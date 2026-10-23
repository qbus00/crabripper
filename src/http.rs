//! HTTP(S) request/response for shoutcast/icecast: builds the ICY request,
//! parses the status line + headers (both `HTTP/1.x` and shoutcast `ICY 200 OK`),
//! follows redirects, and detects content-type / HLS.

use crate::cli::{Config, HlsMode};
use crate::conn::{self, Conn};
use crate::url;
use std::io::{Read, Write};
use std::time::Duration;

type ResponseHead = (String, Vec<(String, String)>, Vec<u8>);

/// A completed one-shot GET (used for HLS playlists and segments).
pub struct Fetch {
    pub body: Vec<u8>,
}

/// GET a URL and read the whole body (HTTP/1.0, Connection: close), following
/// redirects. For playlists and media segments, not for continuous streams.
pub fn fetch(url_str: &str, cfg: &Config) -> Result<Fetch, String> {
    let timeout = Duration::from_secs(cfg.timeout);
    let mut current = url_str.to_string();
    for _ in 0..MAX_REDIRECTS {
        let u = url::parse(&current)?;
        let mut c = conn::connect(&u, timeout).map_err(|e| e.to_string())?;
        let request = format!(
            "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: {}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
            u.path, u.host, cfg.user_agent
        );
        c.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
        c.flush().ok();

        let (status, headers, leftover) = read_response_head(&mut c).map_err(|e| e.to_string())?;
        let code = status_code(&status);
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        if (300..400).contains(&code) {
            if let Some(loc) = get("location") {
                current = resolve_redirect(&loc, &u);
                continue;
            }
            return Err(format!("redirect ({code}) without Location"));
        }
        if code != 200 {
            return Err(format!("GET {current} -> {status:?}"));
        }
        let mut body = leftover;
        let mut buf = [0u8; 16384];
        loop {
            match c.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&buf[..n]),
                Err(e) => return Err(e.to_string()),
            }
        }
        return Ok(Fetch { body });
    }
    Err("too many redirects".to_string())
}

pub struct StreamInfo {
    pub metaint: usize,
    pub name: String,
    pub content_type: String,
    pub ext: String,
    pub is_hls: bool,
    pub bitrate: Option<u32>,
    pub final_url: String,
}

const MAX_REDIRECTS: usize = 6;

/// Connect, send the ICY request, follow redirects, and return the live
/// connection plus parsed stream info and any already-read body bytes.
pub fn open_stream(url_str: &str, cfg: &Config) -> Result<(Conn, StreamInfo, Vec<u8>), String> {
    let timeout = Duration::from_secs(cfg.timeout);
    let mut current = url_str.to_string();

    for _ in 0..MAX_REDIRECTS {
        let u = url::parse(&current)?;
        if u.tls && cfg.ssl_verify {
            eprintln!(
                "  ! --ssl-verify requested but certificate verification is not yet \
                 enforced in this port (connection is still encrypted); treating as off"
            );
        }
        let mut c = conn::connect(&u, timeout).map_err(|e| e.to_string())?;

        let version = if cfg.http10 { "1.0" } else { "1.1" };
        let request = format!(
            "GET {} HTTP/{version}\r\n\
             Host: {}\r\n\
             User-Agent: {}\r\n\
             Accept: */*\r\n\
             Icy-MetaData: 1\r\n\
             Connection: close\r\n\
             \r\n",
            u.path, u.host, cfg.user_agent
        );
        c.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
        c.flush().ok();

        let (status, headers, leftover) = read_response_head(&mut c).map_err(|e| e.to_string())?;
        let code = status_code(&status);
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };

        if (300..400).contains(&code) {
            if let Some(loc) = get("location") {
                current = resolve_redirect(&loc, &u);
                continue;
            }
            return Err(format!("redirect ({code}) without a Location header"));
        }
        if code != 200 {
            return Err(format!("server did not return 200 (status: {status:?})"));
        }

        let content_type = get("content-type").unwrap_or_default();
        let metaint: usize = get("icy-metaint").and_then(|v| v.parse().ok()).unwrap_or(0);
        let name = get("icy-name").unwrap_or_default();
        let bitrate = get("icy-br").and_then(|v| v.parse().ok());
        let is_hls = detect_hls(cfg, &content_type, &u.path);
        let ext = ext_for(&content_type, &u.path);

        let info = StreamInfo {
            metaint,
            name,
            content_type,
            ext,
            is_hls,
            bitrate,
            final_url: current.clone(),
        };
        return Ok((c, info, leftover));
    }
    Err("too many redirects".to_string())
}

fn detect_hls(cfg: &Config, content_type: &str, path: &str) -> bool {
    match cfg.hls_mode {
        HlsMode::Force => true,
        HlsMode::Off => false,
        HlsMode::Auto => {
            let ct = content_type.to_lowercase();
            ct.contains("mpegurl")
                || ct.contains("vnd.apple.mpegurl")
                || path.to_lowercase().ends_with(".m3u8")
        }
    }
}

pub fn ext_for(content_type: &str, path: &str) -> String {
    let c = content_type.to_lowercase();
    let e = if c.contains("aac") {
        "aac"
    } else if c.contains("ogg") {
        "ogg"
    } else if c.contains("flac") {
        "flac"
    } else if c.contains("mpeg") || c.contains("mp3") {
        "mp3"
    } else {
        // fall back to the URL extension, else mp3
        let p = path.to_lowercase();
        if p.ends_with(".aac") {
            "aac"
        } else if p.ends_with(".ogg") {
            "ogg"
        } else if p.ends_with(".flac") {
            "flac"
        } else {
            "mp3"
        }
    };
    e.to_string()
}

fn status_code(status: &str) -> u16 {
    status
        .split_whitespace()
        .find_map(|t| t.parse::<u16>().ok())
        .unwrap_or(0)
}

fn resolve_redirect(location: &str, base: &url::Url) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        location.to_string()
    } else if let Some(rest) = location.strip_prefix('/') {
        format!("{}://{}:{}/{}", base.scheme, base.host, base.port, rest)
    } else {
        // relative to the base path's directory
        let dir = match base.path.rfind('/') {
            Some(i) => &base.path[..=i],
            None => "/",
        };
        format!(
            "{}://{}:{}{}{}",
            base.scheme, base.host, base.port, dir, location
        )
    }
}

/// Read status line + headers; return any body bytes past the `\r\n\r\n`.
pub fn read_response_head(conn: &mut Conn) -> std::io::Result<ResponseHead> {
    let mut data = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = conn.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..n]);
        if let Some(idx) = find_subslice(&data, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&data[..idx]).to_string();
            let leftover = data[idx + 4..].to_vec();
            let mut lines = head.split("\r\n");
            let status = lines.next().unwrap_or("").to_string();
            let mut headers = Vec::new();
            for line in lines {
                if let Some(ci) = line.find(':') {
                    headers.push((
                        line[..ci].trim().to_lowercase(),
                        line[ci + 1..].trim().to_string(),
                    ));
                }
            }
            return Ok((status, headers, leftover));
        }
        if data.len() > 64 * 1024 {
            break;
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "no HTTP/ICY header terminator found",
    ))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
