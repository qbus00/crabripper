//! HLS (`.m3u8`) recording: follow a master playlist to a variant, then poll the
//! media playlist and concatenate each new segment into one output file until the
//! time limit or `#EXT-X-ENDLIST`. AES-128 encrypted segments are decrypted.
//!
//! Everything here comes from the server, so downloads are size/time bounded,
//! the poll interval is clamped, and transient fetch failures are retried
//! rather than ending a scheduled recording.

use crate::cli::{Config, HlsVariant};
use crate::http::{self, Limits};
use crate::util::{now_stamp, printable, sanitize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

const PLAYLIST: Limits = Limits {
    max_bytes: 2 * 1024 * 1024,
    max_time: Duration::from_secs(30),
};
const KEY: Limits = Limits {
    max_bytes: 1024,
    max_time: Duration::from_secs(30),
};
const SEGMENT: Limits = Limits {
    max_bytes: 16 * 1024 * 1024,
    max_time: Duration::from_secs(120),
};
/// Longest wait between playlist polls, whatever TARGETDURATION says.
const MAX_POLL: u64 = 30;

pub fn rip(url: &str, cfg: &Config, deadline: Option<Instant>) -> Result<(), String> {
    let say = |m: &str| {
        if !cfg.quiet {
            eprintln!("{m}");
        }
    };
    let start = Instant::now();

    // Fetch the playlist; follow a master playlist to the chosen variant.
    // Relative URIs resolve against the URL actually served (post-redirect).
    let first = http::fetch(url, cfg, PLAYLIST)?;
    let text = String::from_utf8_lossy(&first.body).to_string();
    if !text.contains("#EXTM3U") {
        return Err("not an HLS playlist (missing #EXTM3U)".to_string());
    }
    let media_url = if text.contains("#EXT-X-STREAM-INF") {
        let v = pick_variant(&text, &cfg.hls_variant).ok_or("no variant in master playlist")?;
        let resolved = resolve_url(&first.url, &v);
        say(&format!("HLS master -> variant: {}", printable(&resolved)));
        resolved
    } else {
        first.url.clone()
    };

    // Output: a single file (overwritten, but never through a symlink).
    let outdir = PathBuf::from(cfg.output_dir.clone().unwrap_or_else(|| ".".to_string()));
    fs::create_dir_all(&outdir).map_err(|e| e.to_string())?;
    let name = match &cfg.single_file_name {
        Some(n) => n.clone(),
        None => format!("hls_{}.ts", now_stamp()),
    };
    let name = sanitize(&name);
    let path = outdir.join(if name.is_empty() {
        "hls.ts".to_string()
    } else {
        name
    });
    say(&format!(
        "[hls file] {}",
        printable(&path.display().to_string())
    ));
    if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!(
            "refusing to write through a symlink: {}",
            printable(&path.display().to_string())
        ));
    }
    let mut file = File::create(&path).map_err(|e| e.to_string())?;

    let expired = || deadline.is_some_and(|d| Instant::now() >= d);
    // Sleep `secs`, but never past the deadline.
    let nap = |secs: u64| {
        let mut d = Duration::from_secs(secs.clamp(1, MAX_POLL));
        if let Some(dl) = deadline {
            d = d.min(dl.saturating_duration_since(Instant::now()));
        }
        thread::sleep(d);
    };

    let mut seen: HashSet<String> = HashSet::new();
    let mut key_cache: HashMap<String, [u8; 16]> = HashMap::new();
    let mut total: u64 = 0;
    let mut nsegs: u64 = 0;
    let mut skipped: u64 = 0;

    loop {
        if expired() {
            break;
        }
        let (mptext, base) = match http::fetch(&media_url, cfg, PLAYLIST) {
            Ok(mp) => (String::from_utf8_lossy(&mp.body).to_string(), mp.url),
            Err(e) => {
                if expired() {
                    break;
                }
                // A blip on the playlist must not end a scheduled recording.
                say(&format!("  ! HLS playlist fetch failed ({e}); retrying"));
                nap(2);
                continue;
            }
        };
        let target = parse_target_duration(&mptext).unwrap_or(3);
        let entries = parse_media(&mptext)?; // unsupported METHOD: cannot record at all
        let current: HashSet<String> = entries.iter().map(|e| resolve_url(&base, &e.url)).collect();
        let current_keys: HashSet<String> = entries
            .iter()
            .filter_map(|e| e.key.as_ref().map(|k| resolve_url(&base, &k.uri)))
            .collect();

        let mut got_new = false;
        for entry in entries {
            if expired() {
                break;
            }
            let segurl = resolve_url(&base, &entry.url);
            if !seen.insert(segurl.clone()) {
                continue;
            }
            // Any per-segment problem (fetch, key, decryption) loses that
            // segment only, never the recording.
            let seg = fetch_retry(&segurl, cfg, SEGMENT).and_then(|data| match &entry.key {
                None => Ok(data),
                Some(k) => {
                    let key_url = resolve_url(&base, &k.uri);
                    let key = match key_cache.get(&key_url) {
                        Some(k) => *k,
                        None => {
                            let key = fetch_key(&key_url, &base, cfg)?;
                            key_cache.insert(key_url, key);
                            key
                        }
                    };
                    let iv = k.iv.unwrap_or_else(|| iv_from_seq(entry.seq));
                    aes128_cbc_decrypt(&key, &iv, &data)
                        .ok_or_else(|| "AES-128 decryption failed".to_string())
                }
            });
            let out = match seg {
                Ok(out) => out,
                Err(e) => {
                    if expired() {
                        break;
                    }
                    say(&format!("  ! HLS segment skipped ({})", printable(&e)));
                    skipped += 1;
                    continue;
                }
            };
            file.write_all(&out).map_err(|e| e.to_string())?;
            total += out.len() as u64;
            nsegs += 1;
            got_new = true;
        }
        // Only remember segments/keys still in the playlist window (bounded
        // memory for 24/7 recordings).
        seen.retain(|u| current.contains(u));
        key_cache.retain(|u, _| current_keys.contains(u));

        if mptext.contains("#EXT-X-ENDLIST") {
            say("HLS: #EXT-X-ENDLIST (VOD end)");
            break;
        }
        if expired() {
            break;
        }
        nap(if got_new { 1 } else { target });
    }
    if skipped > 0 {
        say(&format!(
            "  ! {skipped} HLS segment(s) could not be recorded"
        ));
    }

    file.flush().map_err(|e| e.to_string())?;
    say(&format!(
        "done ({:.0}s, {nsegs} segments, {} KB).",
        start.elapsed().as_secs_f64(),
        total / 1024
    ));
    Ok(())
}

/// Fetch a 16-byte AES key. With certificate checks on, a key referenced from
/// an https playlist must itself come over https (not in cleartext).
fn fetch_key(key_url: &str, playlist: &str, cfg: &Config) -> Result<[u8; 16], String> {
    if cfg.ssl_verify
        && playlist.to_ascii_lowercase().starts_with("https://")
        && !key_url.to_ascii_lowercase().starts_with("https://")
    {
        return Err("refusing to fetch an HLS key over plain http".to_string());
    }
    let kb = fetch_retry(key_url, cfg, KEY)?;
    kb.as_slice()
        .try_into()
        .map_err(|_| format!("HLS key is {} bytes, expected 16", kb.len()))
}

#[derive(Clone)]
struct KeyInfo {
    uri: String,
    iv: Option<[u8; 16]>,
}

struct SegEntry {
    url: String,
    seq: u64,
    key: Option<KeyInfo>,
}

/// Parse a media playlist into segments, tracking media sequence and the active
/// `#EXT-X-KEY`. Errors on encryption schemes we can't handle (SAMPLE-AES, etc.).
fn parse_media(mp: &str) -> Result<Vec<SegEntry>, String> {
    let mut seq = mp
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("#EXT-X-MEDIA-SEQUENCE:")
                .and_then(|v| v.trim().parse::<u64>().ok())
        })
        .unwrap_or(0);
    let mut key: Option<KeyInfo> = None;
    let mut out = Vec::new();

    for line in mp.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let attrs = parse_attrs(rest);
            match attrs.get("METHOD").map(|s| s.as_str()) {
                Some("NONE") | None => key = None,
                Some("AES-128") => {
                    let uri = attrs.get("URI").cloned().unwrap_or_default();
                    let iv = attrs.get("IV").and_then(|v| parse_hex_iv(v));
                    key = Some(KeyInfo { uri, iv });
                }
                Some(other) => {
                    return Err(format!("HLS encryption METHOD={other} is not supported"));
                }
            }
        } else if !line.is_empty() && !line.starts_with('#') {
            out.push(SegEntry {
                url: line.to_string(),
                seq,
                key: key.clone(),
            });
            seq = seq.wrapping_add(1);
        }
    }
    Ok(out)
}

/// Parse `KEY=VALUE,KEY="QUOTED",...` attribute lists.
fn parse_attrs(s: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // key
        let kstart = i;
        while i < bytes.len() && bytes[i] != b'=' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let key = s[kstart..i].trim().to_string();
        i += 1; // skip '='
        let val = if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let vstart = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            let v = s[vstart..i].to_string();
            i += 1; // skip closing quote
            v
        } else {
            let vstart = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            s[vstart..i].trim().to_string()
        };
        map.insert(key.to_ascii_uppercase(), val);
        while i < bytes.len() && (bytes[i] == b',' || bytes[i] == b' ') {
            i += 1;
        }
    }
    map
}

/// Fetch with one retry after a short pause (segments/keys are small and a
/// single transient failure is common on CDNs).
fn fetch_retry(url: &str, cfg: &Config, limits: Limits) -> Result<Vec<u8>, String> {
    match http::fetch(url, cfg, limits) {
        Ok(f) => Ok(f.body),
        Err(_) => {
            thread::sleep(Duration::from_secs(1));
            http::fetch(url, cfg, limits).map(|f| f.body)
        }
    }
}

fn parse_hex_iv(v: &str) -> Option<[u8; 16]> {
    let h = v.trim().trim_start_matches("0x").trim_start_matches("0X");
    // ASCII hex only: byte-slicing below must never split a UTF-8 character.
    if h.len() != 32 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut iv = [0u8; 16];
    for (i, b) in iv.iter_mut().enumerate() {
        *b = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(iv)
}

/// Default HLS IV: the 128-bit big-endian media sequence number.
fn iv_from_seq(seq: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&seq.to_be_bytes());
    iv
}

fn aes128_cbc_decrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    use cbc::cipher::block_padding::Pkcs7;
    use cbc::cipher::{BlockDecryptMut, KeyIvInit};
    type Dec = cbc::Decryptor<aes::Aes128>;
    Dec::new(key.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .ok()
}

fn parse_target_duration(mp: &str) -> Option<u64> {
    mp.lines().find_map(|l| {
        l.trim()
            .strip_prefix("#EXT-X-TARGETDURATION:")
            .and_then(|v| v.trim().parse().ok())
    })
}

/// Find `key` (e.g. "BANDWIDTH=") anywhere in a line and parse the number after it.
fn extract_attr_u64(line: &str, key: &str) -> Option<u64> {
    let idx = line.find(key)? + key.len();
    let digits: String = line[idx..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn pick_variant(master: &str, want: &HlsVariant) -> Option<String> {
    let lines: Vec<&str> = master.lines().collect();
    let mut variants: Vec<(u64, String)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim().starts_with("#EXT-X-STREAM-INF") {
            let bw = extract_attr_u64(lines[i], "BANDWIDTH=").unwrap_or(0);
            let mut j = i + 1;
            while j < lines.len()
                && (lines[j].trim().is_empty() || lines[j].trim().starts_with('#'))
            {
                j += 1;
            }
            if j < lines.len() {
                variants.push((bw, lines[j].trim().to_string()));
                i = j;
            }
        }
        i += 1;
    }
    if variants.is_empty() {
        return None;
    }
    variants.sort_by_key(|(bw, _)| *bw);
    let chosen = match want {
        HlsVariant::Best => variants.last(),
        HlsVariant::Worst => variants.first(),
        HlsVariant::Bitrate(target) => {
            let t = *target as u64;
            variants
                .iter()
                .rev()
                .find(|(bw, _)| *bw <= t)
                .or_else(|| variants.first())
        }
    };
    chosen.map(|(_, uri)| uri.clone())
}

/// Resolve a playlist-relative URI against the playlist's URL.
fn resolve_url(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    let (prefix, path) = match base.find("://") {
        Some(i) => {
            let after = &base[i + 3..];
            match after.find('/') {
                Some(j) => (&base[..i + 3 + j], &after[j..]),
                None => (base, "/"),
            }
        }
        None => (base, "/"),
    };
    // drop any query string from the base path when taking its directory
    let path = path.split('?').next().unwrap_or(path);
    if let Some(r) = rel.strip_prefix('/') {
        format!("{prefix}/{r}")
    } else {
        let dir = match path.rfind('/') {
            Some(k) => &path[..=k],
            None => "/",
        };
        format!("{prefix}{dir}{rel}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve() {
        let b = "https://h.com/a/b/playlist.m3u8?x=1";
        assert_eq!(resolve_url(b, "seg1.ts"), "https://h.com/a/b/seg1.ts");
        assert_eq!(resolve_url(b, "/x/seg.ts"), "https://h.com/x/seg.ts");
        assert_eq!(resolve_url(b, "https://c/z.ts"), "https://c/z.ts");
    }

    #[test]
    fn hex_iv_rejects_non_ascii() {
        // 32 bytes, but multi-byte chars: must not panic on char boundaries
        assert_eq!(parse_hex_iv("0xaa€€€€€€€€€€"), None);
        let iv = parse_hex_iv("0x000102030405060708090A0B0C0D0E0F").unwrap();
        assert_eq!(iv[15], 0x0f);
    }

    #[test]
    fn variant_pick() {
        let m = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=64000\nlo.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=256000\nhi.m3u8\n";
        assert_eq!(
            pick_variant(m, &HlsVariant::Best).as_deref(),
            Some("hi.m3u8")
        );
        assert_eq!(
            pick_variant(m, &HlsVariant::Worst).as_deref(),
            Some("lo.m3u8")
        );
        assert_eq!(
            pick_variant(m, &HlsVariant::Bitrate(100000)).as_deref(),
            Some("lo.m3u8")
        );
    }
}
