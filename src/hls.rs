//! HLS (`.m3u8`) recording: follow a master playlist to a variant, then poll the
//! media playlist and concatenate each new segment into one output file until the
//! time limit or `#EXT-X-ENDLIST`. AES-128 encrypted segments are not yet handled.

use crate::cli::{Config, HlsVariant};
use crate::http;
use crate::util::{now_stamp, sanitize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

pub fn rip(url: &str, cfg: &Config) -> Result<(), String> {
    let say = |m: &str| {
        if !cfg.quiet {
            eprintln!("{m}");
        }
    };

    // Fetch the playlist; follow a master playlist to the chosen variant.
    let first = http::fetch(url, cfg)?;
    let text = String::from_utf8_lossy(&first.body).to_string();
    if !text.contains("#EXTM3U") {
        return Err("not an HLS playlist (missing #EXTM3U)".to_string());
    }
    let media_url = if text.contains("#EXT-X-STREAM-INF") {
        let v = pick_variant(&text, &cfg.hls_variant).ok_or("no variant in master playlist")?;
        let resolved = resolve_url(url, &v);
        say(&format!("HLS master -> variant: {resolved}"));
        resolved
    } else {
        url.to_string()
    };

    // Output: a single file.
    let outdir = PathBuf::from(cfg.output_dir.clone().unwrap_or_else(|| ".".to_string()));
    fs::create_dir_all(&outdir).map_err(|e| e.to_string())?;
    let name = match &cfg.single_file_name {
        Some(n) => n.clone(),
        None => format!("hls_{}.ts", now_stamp()),
    };
    let path = outdir.join(sanitize(&name));
    say(&format!("[hls file] {}", path.display()));
    let mut file = File::create(&path).map_err(|e| e.to_string())?;

    let start = Instant::now();
    let deadline = cfg.length.map(|l| start + Duration::from_secs(l));
    let expired = || deadline.is_some_and(|d| Instant::now() >= d);

    let mut seen: HashSet<String> = HashSet::new();
    let mut key_cache: HashMap<String, [u8; 16]> = HashMap::new();
    let mut total: u64 = 0;
    let mut nsegs: u64 = 0;

    loop {
        if expired() {
            break;
        }
        let mp = http::fetch(&media_url, cfg)?;
        let mptext = String::from_utf8_lossy(&mp.body).to_string();
        let target = parse_target_duration(&mptext).unwrap_or(3);
        let entries = parse_media(&mptext)?; // errors on SAMPLE-AES / unsupported

        let mut got_new = false;
        for entry in entries {
            if expired() {
                break;
            }
            let segurl = resolve_url(&media_url, &entry.url);
            if !seen.insert(segurl.clone()) {
                continue;
            }
            let data = http::fetch(&segurl, cfg)?.body;
            let out = match &entry.key {
                Some(k) => {
                    let key_url = resolve_url(&media_url, &k.uri);
                    let key = match key_cache.get(&key_url) {
                        Some(k) => *k,
                        None => {
                            let kb = http::fetch(&key_url, cfg)?.body;
                            if kb.len() != 16 {
                                return Err(format!("HLS key is {} bytes, expected 16", kb.len()));
                            }
                            let mut arr = [0u8; 16];
                            arr.copy_from_slice(&kb);
                            key_cache.insert(key_url, arr);
                            arr
                        }
                    };
                    let iv = k.iv.unwrap_or_else(|| iv_from_seq(entry.seq));
                    aes128_cbc_decrypt(&key, &iv, &data).ok_or("HLS AES-128 decryption failed")?
                }
                None => data,
            };
            file.write_all(&out).map_err(|e| e.to_string())?;
            total += out.len() as u64;
            nsegs += 1;
            got_new = true;
        }

        if mptext.contains("#EXT-X-ENDLIST") {
            say("HLS: #EXT-X-ENDLIST (VOD end)");
            break;
        }
        if expired() {
            break;
        }
        thread::sleep(Duration::from_secs(if got_new { 1 } else { target.max(1) }));
    }

    file.flush().map_err(|e| e.to_string())?;
    say(&format!(
        "done ({:.0}s, {nsegs} segments, {} KB).",
        start.elapsed().as_secs_f64(),
        total / 1024
    ));
    Ok(())
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
            seq += 1;
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

fn parse_hex_iv(v: &str) -> Option<[u8; 16]> {
    let h = v.trim().trim_start_matches("0x").trim_start_matches("0X");
    if h.len() != 32 {
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
