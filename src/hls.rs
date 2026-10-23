//! HLS (`.m3u8`) recording: follow a master playlist to a variant, then poll the
//! media playlist and concatenate each new segment into one output file until the
//! time limit or `#EXT-X-ENDLIST`. AES-128 encrypted segments are not yet handled.

use crate::cli::{Config, HlsVariant};
use crate::http;
use crate::util::{now_stamp, sanitize};
use std::collections::HashSet;
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
    let mut total: u64 = 0;
    let mut nsegs: u64 = 0;

    loop {
        if expired() {
            break;
        }
        let mp = http::fetch(&media_url, cfg)?;
        let mptext = String::from_utf8_lossy(&mp.body).to_string();
        if mptext.contains("#EXT-X-KEY") && mptext.to_uppercase().contains("METHOD=AES") {
            return Err("AES-128 encrypted HLS is not supported yet".to_string());
        }
        let target = parse_target_duration(&mptext).unwrap_or(3);

        let mut got_new = false;
        for seg in parse_segments(&mptext) {
            if expired() {
                break;
            }
            let segurl = resolve_url(&media_url, &seg);
            if !seen.insert(segurl.clone()) {
                continue;
            }
            let data = http::fetch(&segurl, cfg)?;
            file.write_all(&data.body).map_err(|e| e.to_string())?;
            total += data.body.len() as u64;
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

fn parse_segments(mp: &str) -> Vec<String> {
    mp.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect()
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
