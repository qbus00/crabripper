//! HTTP(S) request/response for shoutcast/icecast: builds the ICY request,
//! parses the status line + headers (both `HTTP/1.x` and shoutcast `ICY 200 OK`),
//! follows redirects, and detects content-type / HLS.

use crate::cli::{Config, HlsMode};
use crate::conn::{self, Conn};
use crate::url;
use crate::util::printable;
use std::io::{self, Cursor, Read, Write};
use std::time::{Duration, Instant};

type ResponseHead = (String, Vec<(String, String)>, Vec<u8>);

/// Largest `icy-metaint` we accept. Real servers use 8192–65536; anything far
/// beyond that is a broken or hostile server, and the value sizes a buffer.
pub const MAX_METAINT: usize = 1024 * 1024;

const MAX_REDIRECTS: usize = 6;
const MAX_HEAD: usize = 64 * 1024;

/// Size and time bounds for a one-shot GET, so a misbehaving server can
/// neither exhaust memory nor stall the caller indefinitely.
#[derive(Clone, Copy)]
pub struct Limits {
    pub max_bytes: usize,
    pub max_time: Duration,
}

/// A completed one-shot GET (used for HLS playlists and segments).
pub struct Fetch {
    pub body: Vec<u8>,
    /// The URL actually served (after redirects): relative URIs resolve here.
    pub url: String,
}

/// The response body as a byte stream: bytes read along with the headers,
/// then the socket — de-chunked if the server used chunked encoding.
pub type Body = Box<dyn Read + Send>;

/// GET a URL and read the body (HTTP/1.0, Connection: close), following
/// redirects. Honours `Content-Length` (servers often keep the connection open
/// regardless), and enforces `limits`. For playlists and media segments, not
/// for continuous streams.
pub fn fetch(url_str: &str, cfg: &Config, limits: Limits) -> Result<Fetch, String> {
    let timeout = Duration::from_secs(cfg.timeout);
    let started = Instant::now();
    let mut current = url_str.to_string();
    for _ in 0..MAX_REDIRECTS {
        let u = url::parse(&current)?;
        let mut c = conn::connect(&u, timeout, cfg.interface.as_deref(), cfg.ssl_verify)
            .map_err(|e| net_err(&e))?;
        let request = format!(
            "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: {}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
            u.path,
            u.host,
            printable(&cfg.user_agent)
        );
        c.write_all(request.as_bytes()).map_err(|e| net_err(&e))?;
        c.flush().ok();

        let (status, headers, leftover) =
            read_response_head(&mut c, header_budget(timeout)).map_err(|e| net_err(&e))?;
        let code = status_code(&status);
        let get = |name: &str| header(&headers, name);
        if (300..400).contains(&code) {
            if let Some(loc) = get("location") {
                current = follow(&loc, &u, cfg)?;
                continue;
            }
            return Err(format!("redirect ({code}) without Location"));
        }
        if code != 200 {
            return Err(format!("GET {} -> {status:?}", printable(&current)));
        }
        let chunked = is_chunked(&headers)?;
        let content_len: Option<usize> = if chunked {
            None // the length header is meaningless with chunked encoding
        } else {
            get("content-length").and_then(|v| v.parse().ok())
        };
        if content_len.is_some_and(|n| n > limits.max_bytes) {
            return Err(format!(
                "response too large ({} bytes, limit {})",
                content_len.unwrap_or(0),
                limits.max_bytes
            ));
        }
        // Without a length, read to EOF but stop one byte past the limit so an
        // oversized body is detected rather than silently truncated.
        let want = content_len.unwrap_or(limits.max_bytes + 1);
        let mut r = body_reader(c, leftover, chunked);
        let mut body = Vec::with_capacity(content_len.unwrap_or(0));
        let mut buf = [0u8; 16384];
        while body.len() < want {
            if started.elapsed() > limits.max_time {
                return Err(format!("fetch timed out after {:?}", limits.max_time));
            }
            let room = (want - body.len()).min(buf.len());
            match r.read(&mut buf[..room]) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&buf[..n]),
                Err(e) => return Err(net_err(&e)),
            }
        }
        if content_len.is_none() && body.len() > limits.max_bytes {
            return Err(format!("response exceeds {} bytes", limits.max_bytes));
        }
        if content_len.is_some_and(|n| body.len() < n) {
            return Err("connection closed before the full body arrived".to_string());
        }
        return Ok(Fetch { body, url: current });
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

/// Connect, send the ICY request, follow redirects, and return the body stream
/// plus parsed stream info.
pub fn open_stream(url_str: &str, cfg: &Config) -> Result<(Body, StreamInfo), String> {
    let timeout = Duration::from_secs(cfg.timeout);
    let mut current = url_str.to_string();

    for _ in 0..MAX_REDIRECTS {
        let u = url::parse(&current)?;
        let mut c = conn::connect(&u, timeout, cfg.interface.as_deref(), cfg.ssl_verify)
            .map_err(|e| net_err(&e))?;

        let version = if cfg.http10 { "1.0" } else { "1.1" };
        let request = format!(
            "GET {} HTTP/{version}\r\n\
             Host: {}\r\n\
             User-Agent: {}\r\n\
             Accept: */*\r\n\
             Icy-MetaData: 1\r\n\
             Connection: close\r\n\
             \r\n",
            u.path,
            u.host,
            printable(&cfg.user_agent)
        );
        c.write_all(request.as_bytes()).map_err(|e| net_err(&e))?;
        c.flush().ok();

        let (status, headers, leftover) =
            read_response_head(&mut c, header_budget(timeout)).map_err(|e| net_err(&e))?;
        let code = status_code(&status);
        let get = |name: &str| header(&headers, name);

        if (300..400).contains(&code) {
            if let Some(loc) = get("location") {
                current = follow(&loc, &u, cfg)?;
                continue;
            }
            return Err(format!("redirect ({code}) without a Location header"));
        }
        if code != 200 {
            return Err(format!("server did not return 200 (status: {status:?})"));
        }
        let chunked = is_chunked(&headers)?;

        let content_type = get("content-type").unwrap_or_default();
        let metaint: usize = match get("icy-metaint") {
            None => 0,
            Some(v) => match v.parse::<usize>() {
                Ok(n) if n <= MAX_METAINT => n,
                _ => {
                    return Err(format!(
                        "server sent an implausible icy-metaint ({}); refusing",
                        printable(&v)
                    ))
                }
            },
        };
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
        return Ok((body_reader(c, leftover, chunked), info));
    }
    Err("too many redirects".to_string())
}

/// Total time allowed for a response's status line + headers to arrive (the
/// per-read timeout alone would let a server trickle them forever).
fn header_budget(timeout: Duration) -> Duration {
    (timeout * 4).max(Duration::from_secs(30))
}

/// Resolve a redirect, refusing an https -> http downgrade unless certificate
/// checks were explicitly disabled.
fn follow(location: &str, from: &url::Url, cfg: &Config) -> Result<String, String> {
    let next = resolve_redirect(location, from);
    if from.tls && cfg.ssl_verify && next.to_ascii_lowercase().starts_with("http://") {
        return Err(format!(
            "refusing redirect from https to plain http ({}); use --no-ssl-verify to allow",
            printable(&next)
        ));
    }
    Ok(next)
}

fn is_chunked(headers: &[(String, String)]) -> Result<bool, String> {
    match header(headers, "transfer-encoding") {
        None => Ok(false),
        Some(v) if v.eq_ignore_ascii_case("chunked") => Ok(true),
        Some(v) if v.eq_ignore_ascii_case("identity") => Ok(false),
        Some(v) => Err(format!("unsupported transfer-encoding: {}", printable(&v))),
    }
}

fn body_reader(c: Conn, leftover: Vec<u8>, chunked: bool) -> Body {
    let r = Cursor::new(leftover).chain(c);
    if chunked {
        Box::new(Chunked {
            r,
            left: 0,
            done: false,
        })
    } else {
        Box::new(r)
    }
}

/// Decoder for `Transfer-Encoding: chunked` bodies (HTTP/1.1 servers and
/// proxies in front of icecast use it). Chunk-size lines are bounded.
struct Chunked<R> {
    r: R,
    left: u64,
    done: bool,
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

impl<R: Read> Chunked<R> {
    /// One CRLF-terminated line (CR stripped), at most 1 KB.
    fn line(&mut self) -> io::Result<String> {
        let mut out = Vec::new();
        let mut b = [0u8; 1];
        loop {
            if self.r.read(&mut b)? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            if b[0] == b'\n' {
                break;
            }
            if out.len() >= 1024 {
                return Err(bad("chunk header line too long"));
            }
            out.push(b[0]);
        }
        if out.last() == Some(&b'\r') {
            out.pop();
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

impl<R: Read> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let line = self.line()?;
            let size = line.split(';').next().unwrap_or("").trim();
            self.left = u64::from_str_radix(size, 16).map_err(|_| bad("bad chunk size"))?;
            if self.left == 0 {
                self.done = true;
                return Ok(0);
            }
        }
        let room = (buf.len() as u64).min(self.left) as usize;
        let n = self.r.read(&mut buf[..room])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= n as u64;
        if self.left == 0 && !self.line()?.is_empty() {
            return Err(bad("missing CRLF after chunk"));
        }
        Ok(n)
    }
}

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

/// Network error text, with a hint when it is a certificate failure.
fn net_err(e: &std::io::Error) -> String {
    let s = e.to_string();
    if s.to_lowercase().contains("certificate") {
        format!("{s} (TLS certificate check failed; use --no-ssl-verify to skip it)")
    } else {
        s
    }
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
pub fn read_response_head(conn: &mut Conn, budget: Duration) -> std::io::Result<ResponseHead> {
    let started = Instant::now();
    let mut data = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if started.elapsed() > budget {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "server is sending its response headers too slowly",
            ));
        }
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
        if data.len() > MAX_HEAD {
            break;
        }
    }
    let msg = if data.is_empty() {
        "server closed the connection without responding (some stations reject \
         ripper User-Agents; try -u)"
    } else {
        "no HTTP/ICY header terminator found"
    };
    Err(std::io::Error::new(std::io::ErrorKind::InvalidData, msg))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dechunk(raw: &[u8]) -> io::Result<Vec<u8>> {
        let mut c = Chunked {
            r: Cursor::new(raw.to_vec()),
            left: 0,
            done: false,
        };
        let mut out = Vec::new();
        c.read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn chunked_decoding() {
        assert_eq!(
            dechunk(b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n").unwrap(),
            b"hello world"
        );
        assert!(dechunk(b"zz\r\nhello").is_err());
        assert!(dechunk(&[b'1'; 2000]).is_err()); // unbounded size line
    }
}
