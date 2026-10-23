//! streamripper-rs — a work-in-progress Rust port of streamripper.
//!
//! This first slice connects to a shoutcast/icecast stream over HTTP or HTTPS,
//! reads the ICY inline metadata, and splits the audio into one file per track.

mod conn;
mod icy;
mod recorder;
mod url;

use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// (status line, headers as `(lowercased-name, value)`, leftover body bytes).
type ResponseHead = (String, Vec<(String, String)>, Vec<u8>);

struct Args {
    url: String,
    dir: String,
    length: Option<u64>, // seconds, None = until interrupted/EOF
    user_agent: String,
    timeout: u64, // seconds
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n");
            print_help();
            std::process::exit(2);
        }
    };
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> Result<(), String> {
    let u = url::parse(&args.url)?;
    eprintln!(
        "Connecting to {}://{}:{}{} ...",
        u.scheme, u.host, u.port, u.path
    );

    let mut conn =
        conn::connect(&u, Duration::from_secs(args.timeout)).map_err(|e| e.to_string())?;

    let request = format!(
        "GET {} HTTP/1.0\r\n\
         Host: {}\r\n\
         User-Agent: {}\r\n\
         Accept: */*\r\n\
         Icy-MetaData: 1\r\n\
         Connection: close\r\n\
         \r\n",
        u.path, u.host, args.user_agent
    );
    conn.write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    conn.flush().ok();

    let (status, headers, leftover) = read_response_head(&mut conn).map_err(|e| e.to_string())?;
    if !status.contains("200") {
        return Err(format!(
            "server did not return 200 (status line: {status:?})"
        ));
    }

    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    let metaint: usize = header("icy-metaint")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let name = header("icy-name").unwrap_or_default();
    let ctype = header("content-type").unwrap_or_default();
    let ext = ext_for(&ctype);

    eprintln!(
        "stream: {}",
        if name.is_empty() { "(unnamed)" } else { &name }
    );
    eprintln!(
        "content-type: {} -> .{ext}",
        if ctype.is_empty() { "(none)" } else { &ctype }
    );
    eprintln!("metadata interval: {metaint}");

    let mut rec = recorder::Recorder::new(Path::new(&args.dir), ext).map_err(|e| e.to_string())?;
    let mut src = Source::new(conn, leftover);

    let start = Instant::now();
    let deadline = args.length.map(|l| start + Duration::from_secs(l));
    let expired = || deadline.is_some_and(|d| Instant::now() >= d);

    let mut audio = Vec::new();

    if metaint > 0 {
        let mut meta = Vec::new();
        loop {
            if expired() {
                break;
            }
            // One metaint-sized chunk of audio.
            if !src
                .read_exact_n(metaint, &mut audio)
                .map_err(|e| e.to_string())?
            {
                break;
            }
            rec.write_audio(&audio).map_err(|e| e.to_string())?;

            // Length byte, then the metadata block (length * 16 bytes).
            let len_byte = match src.read_byte().map_err(|e| e.to_string())? {
                Some(b) => b,
                None => break,
            };
            let meta_len = len_byte as usize * 16;
            if meta_len > 0 {
                if !src
                    .read_exact_n(meta_len, &mut meta)
                    .map_err(|e| e.to_string())?
                {
                    break;
                }
                if let Some(title) = icy::parse_stream_title(&meta) {
                    if !title.is_empty() {
                        rec.on_title(&title).map_err(|e| e.to_string())?;
                    }
                }
            }
        }
    } else {
        // No inline metadata: a single continuous file.
        eprintln!("(no ICY metadata — recording to a single file)");
        while !expired() {
            if !src.read_some(&mut audio).map_err(|e| e.to_string())? {
                break;
            }
            rec.write_audio(&audio).map_err(|e| e.to_string())?;
        }
    }

    eprintln!("done ({:.0}s).", start.elapsed().as_secs_f64());
    Ok(())
}

/// Buffered reader over a `Conn` that can also carry bytes already read while
/// parsing the response header.
struct Source {
    conn: conn::Conn,
    buf: Vec<u8>,
    pos: usize,
}

impl Source {
    fn new(conn: conn::Conn, leftover: Vec<u8>) -> Self {
        Source {
            conn,
            buf: leftover,
            pos: 0,
        }
    }

    /// Ensure at least one unread byte is buffered. Returns false on EOF.
    fn fill(&mut self) -> std::io::Result<bool> {
        if self.pos < self.buf.len() {
            return Ok(true);
        }
        let mut tmp = [0u8; 16384];
        let n = self.conn.read(&mut tmp)?;
        if n == 0 {
            return Ok(false);
        }
        self.buf.clear();
        self.buf.extend_from_slice(&tmp[..n]);
        self.pos = 0;
        Ok(true)
    }

    /// Read exactly `n` bytes into `out`. Returns false if EOF hit first.
    fn read_exact_n(&mut self, n: usize, out: &mut Vec<u8>) -> std::io::Result<bool> {
        out.clear();
        out.reserve(n);
        while out.len() < n {
            if !self.fill()? {
                return Ok(false);
            }
            let avail = &self.buf[self.pos..];
            let take = (n - out.len()).min(avail.len());
            out.extend_from_slice(&avail[..take]);
            self.pos += take;
        }
        Ok(true)
    }

    fn read_byte(&mut self) -> std::io::Result<Option<u8>> {
        if !self.fill()? {
            return Ok(None);
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(Some(b))
    }

    /// Read whatever is currently available (used when there is no metadata).
    fn read_some(&mut self, out: &mut Vec<u8>) -> std::io::Result<bool> {
        out.clear();
        if !self.fill()? {
            return Ok(false);
        }
        out.extend_from_slice(&self.buf[self.pos..]);
        self.pos = self.buf.len();
        Ok(true)
    }
}

/// Read the response status line + headers, returning any body bytes that were
/// read past the `\r\n\r\n` terminator so they aren't lost.
fn read_response_head(conn: &mut conn::Conn) -> std::io::Result<ResponseHead> {
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

fn ext_for(content_type: &str) -> &'static str {
    let c = content_type.to_lowercase();
    if c.contains("aac") {
        "aac"
    } else if c.contains("ogg") {
        "ogg"
    } else if c.contains("flac") {
        "flac"
    } else {
        "mp3" // audio/mpeg, or unknown
    }
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        url: String::new(),
        dir: ".".to_string(),
        length: None,
        user_agent: format!("streamripper-rs/{VERSION}"),
        timeout: 30,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-d" => a.dir = it.next().ok_or("-d needs a directory")?,
            "-l" => {
                a.length = Some(
                    it.next()
                        .ok_or("-l needs seconds")?
                        .parse()
                        .map_err(|_| "-l expects a number of seconds")?,
                )
            }
            "-u" => a.user_agent = it.next().ok_or("-u needs a value")?,
            "-m" => {
                a.timeout = it
                    .next()
                    .ok_or("-m needs seconds")?
                    .parse()
                    .map_err(|_| "-m expects a number of seconds")?
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("streamripper-rs {VERSION}");
                std::process::exit(0);
            }
            s if !s.starts_with('-') && a.url.is_empty() => a.url = s.to_string(),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if a.url.is_empty() {
        return Err("no stream URL given".to_string());
    }
    Ok(a)
}

fn print_help() {
    eprintln!(
        "streamripper-rs {VERSION} — record a shoutcast/icecast stream to per-track files\n\n\
         USAGE:\n    streamripper-rs <URL> [options]\n\n\
         OPTIONS:\n\
         \x20   -d <dir>     output directory (default: current directory)\n\
         \x20   -l <secs>    stop after this many seconds (default: run until stopped)\n\
         \x20   -u <agent>   HTTP User-Agent (default: streamripper-rs/<version>)\n\
         \x20   -m <secs>    connect/read timeout in seconds (default: 30)\n\
         \x20   -h, --help       show this help\n\
         \x20   -V, --version    show version\n\n\
         EXAMPLE:\n\
         \x20   streamripper-rs http://stream4.nadaje.com:9680/radiokrakow-s3 -d ./rec -l 60"
    );
}
