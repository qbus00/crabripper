//! Command-line parsing — mirrors the C streamripper's option surface 1:1.
//
// The full option surface is parsed up front; some fields are consumed by
// later increments (relay, HLS, splitpoint, codeset). Allow dead_code module-
// wide until every feature is wired, then this is removed.
#![allow(dead_code)]

use crate::rules::Rules;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Overwrite {
    Always,
    Never,
    Larger,
    Version,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HlsMode {
    Auto,
    Force,
    Off,
}

#[derive(Debug, Clone)]
pub enum HlsVariant {
    Best,
    Worst,
    Bitrate(u32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Xs {
    None,     // --xs-none
    Standard, // default
    V2,       // --xs2
}

#[derive(Debug, Clone)]
pub struct Config {
    pub url: String,

    // output / layout
    pub output_dir: Option<String>, // -d
    pub pattern: Option<String>,    // -D
    pub no_subdir: bool,            // -s
    pub single_file: bool,          // -a
    pub single_file_name: Option<String>,
    pub no_individual: bool,           // -A
    pub overwrite: Overwrite,          // -o
    pub no_overwrite_incomplete: bool, // -t
    pub keep_incomplete: Option<u32>,  // -k
    pub truncate_dup: bool,            // -T
    pub seq: Option<u32>,              // -q [start]

    // run limits
    pub length: Option<u64>,        // -l (seconds)
    pub max_megabytes: Option<u64>, // -M
    pub no_reconnect: bool,         // -c

    // network
    pub user_agent: String,        // -u
    pub timeout: u64,              // -m (seconds)
    pub proxy: Option<String>,     // -p
    pub interface: Option<String>, // -I
    pub http10: bool,              // --http10
    pub ssl_verify: bool,          // --ssl-verify / --no-ssl-verify

    // relay
    pub relay_port: Option<u16>, // -r (Some => relay on)
    pub relay_ip: Option<String>,
    pub max_relay: u32,                 // -R (0 = unlimited)
    pub relay_playlist: Option<String>, // -L
    pub no_port_scan: bool,             // -z

    // HLS
    pub hls_mode: HlsMode,
    pub hls_variant: HlsVariant,

    // tagging / sheets / wav
    pub no_id3: bool,        // -i
    pub with_id3v1: bool,    // --with-id3v1
    pub without_id3v2: bool, // --without-id3v2
    pub no_cue: bool,        // --no-cue
    pub wav: bool,           // --wav

    // splitpoint (mp3)
    pub xs: Xs,
    pub xs_offset: i32,
    pub xs_padding: (i32, i32),
    pub xs_search_window: (i32, i32),
    pub xs_silence_length: i32,

    // metadata sources
    pub rulefile: Option<String>,     // -w
    pub rules: Rules,                 // parsed from rulefile
    pub external_cmd: Option<String>, // -E

    // console
    pub quiet: bool,
    pub to_stderr: bool,
    pub debug: bool,

    // crabripper extensions
    pub log: bool,               // --log: write a played.log of track titles
    pub split_time: Option<u64>, // --split-time=SECS: force a new track every N s
    pub split_size: Option<u64>, // --split-size=MB: force a new track every N MB
    pub config: Option<String>,  // -C / --config=FILE

    // codeset (accepted; conversion is a planned enhancement)
    pub codeset_filesys: Option<String>,
    pub codeset_id3: Option<String>,
    pub codeset_metadata: Option<String>,
    pub codeset_relay: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            url: String::new(),
            output_dir: None,
            pattern: None,
            no_subdir: false,
            single_file: false,
            single_file_name: None,
            no_individual: false,
            overwrite: Overwrite::Larger,
            no_overwrite_incomplete: false,
            keep_incomplete: None,
            truncate_dup: false,
            seq: None,
            length: None,
            max_megabytes: None,
            no_reconnect: false,
            user_agent: "crabripper/0.1".to_string(),
            timeout: 15,
            proxy: None,
            interface: None,
            http10: false,
            ssl_verify: false,
            relay_port: None,
            relay_ip: None,
            max_relay: 1,
            relay_playlist: None,
            no_port_scan: false,
            hls_mode: HlsMode::Auto,
            hls_variant: HlsVariant::Best,
            no_id3: false,
            with_id3v1: false,
            without_id3v2: false,
            no_cue: false,
            wav: false,
            xs: Xs::Standard,
            xs_offset: 0,
            xs_padding: (0, 0),
            xs_search_window: (0, 0),
            xs_silence_length: 0,
            rulefile: None,
            rules: Rules::default(),
            external_cmd: None,
            quiet: false,
            to_stderr: false,
            debug: false,
            log: false,
            split_time: None,
            split_size: None,
            config: None,
            codeset_filesys: None,
            codeset_id3: None,
            codeset_metadata: None,
            codeset_relay: None,
        }
    }
}

/// Outcome of parsing: run with a config, or exit early (help/version).
pub enum Parsed {
    Run(Box<Config>),
    Exit(i32),
}

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Parsed, String> {
    let mut c = Config::default();
    let mut argv: Vec<String> = args.into_iter().collect();
    // drop argv[0] if it looks like the program name (callers pass env::args)
    if !argv.is_empty() {
        argv.remove(0);
    }

    // -C / --config=FILE: splice the file's tokens in front of the CLI args, so
    // the command line overrides the config file. Lines are whitespace-split;
    // '#' comments and blank lines are ignored (no quoting).
    let cfg_path = argv.iter().enumerate().find_map(|(i, a)| {
        if a == "-C" {
            argv.get(i + 1).cloned()
        } else {
            a.strip_prefix("--config=").map(str::to_string)
        }
    });
    if let Some(path) = cfg_path {
        let text = std::fs::read_to_string(&path).map_err(|e| format!("-C {path}: {e}"))?;
        let mut tokens: Vec<String> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            tokens.extend(line.split_whitespace().map(str::to_string));
        }
        tokens.append(&mut argv);
        argv = tokens;
    }

    let mut i = 0;
    let mut url_set = false;
    while i < argv.len() {
        let a = argv[i].clone();
        // helper to fetch the next token as this option's value
        let next = |idx: &mut usize, opt: &str| -> Result<String, String> {
            *idx += 1;
            argv.get(*idx)
                .cloned()
                .ok_or_else(|| format!("option {opt} requires a value"))
        };

        match a.as_str() {
            "-h" | "--help" => {
                print_usage();
                return Ok(Parsed::Exit(0));
            }
            "-v" | "-V" | "--version" => {
                println!("crabripper {VERSION}");
                return Ok(Parsed::Exit(0));
            }
            "-a" => {
                c.single_file = true;
                // -a optionally takes a filename (only if the next arg isn't an option)
                if let Some(n) = argv.get(i + 1) {
                    if !n.starts_with('-') {
                        c.single_file_name = Some(n.clone());
                        i += 1;
                    }
                }
            }
            "-A" => c.no_individual = true,
            "-d" => c.output_dir = Some(next(&mut i, "-d")?),
            "-D" => c.pattern = Some(next(&mut i, "-D")?),
            "-s" => c.no_subdir = true,
            "-r" => {
                // -r optionally takes [[ip:]port]
                c.relay_port = Some(8000);
                if let Some(n) = argv.get(i + 1) {
                    if !n.starts_with('-') {
                        let (ip, port) = parse_ip_port(n)?;
                        c.relay_ip = ip;
                        c.relay_port = Some(port);
                        i += 1;
                    }
                }
            }
            "-R" => c.max_relay = parse_u32(&next(&mut i, "-R")?, "-R")?,
            "-L" => c.relay_playlist = Some(next(&mut i, "-L")?),
            "-z" => c.no_port_scan = true,
            "-p" => c.proxy = Some(next(&mut i, "-p")?),
            "-o" => c.overwrite = parse_overwrite(&next(&mut i, "-o")?)?,
            "-t" => c.no_overwrite_incomplete = true,
            "-c" => c.no_reconnect = true,
            "-l" => c.length = Some(parse_length(&next(&mut i, "-l")?)?),
            "-M" => c.max_megabytes = Some(parse_u64(&next(&mut i, "-M")?, "-M")?),
            "-q" => {
                c.seq = Some(0);
                if let Some(n) = argv.get(i + 1) {
                    if !n.starts_with('-') && n.parse::<u32>().is_ok() {
                        c.seq = Some(n.parse().unwrap());
                        i += 1;
                    }
                }
            }
            "-u" => c.user_agent = next(&mut i, "-u")?,
            "-w" => c.rulefile = Some(next(&mut i, "-w")?),
            "-m" => c.timeout = parse_u64(&next(&mut i, "-m")?, "-m")?,
            "-k" => c.keep_incomplete = Some(parse_u32(&next(&mut i, "-k")?, "-k")?),
            "-I" => c.interface = Some(next(&mut i, "-I")?),
            "-T" => c.truncate_dup = true,
            "-E" => c.external_cmd = Some(next(&mut i, "-E")?),
            "-i" => c.no_id3 = true,
            "--quiet" => c.quiet = true,
            "--stderr" => c.to_stderr = true,
            "--debug" => c.debug = true,
            "--log" => c.log = true,
            "-C" => c.config = Some(next(&mut i, "-C")?),
            "--http10" => c.http10 = true,
            "--hls" => c.hls_mode = HlsMode::Force,
            "--no-hls" => c.hls_mode = HlsMode::Off,
            "--wav" => c.wav = true,
            "--no-cue" => c.no_cue = true,
            "--ssl-verify" => c.ssl_verify = true,
            "--no-ssl-verify" => c.ssl_verify = false,
            "--with-id3v1" => c.with_id3v1 = true,
            "--without-id3v2" => c.without_id3v2 = true,
            "--xs-none" => c.xs = Xs::None,
            "--xs2" => c.xs = Xs::V2,
            _ => {
                // --opt=value forms
                if let Some(v) = a.strip_prefix("--hls-variant=") {
                    c.hls_variant = parse_hls_variant(v)?;
                } else if let Some(v) = a.strip_prefix("--xs-offset=") {
                    c.xs_offset = parse_i32(v, "--xs-offset")?;
                } else if let Some(v) = a.strip_prefix("--xs-padding=") {
                    c.xs_padding = parse_pair(v, "--xs-padding")?;
                } else if let Some(v) = a.strip_prefix("--xs-search-window=") {
                    c.xs_search_window = parse_pair(v, "--xs-search-window")?;
                } else if let Some(v) = a.strip_prefix("--xs-silence-length=") {
                    c.xs_silence_length = parse_i32(v, "--xs-silence-length")?;
                } else if let Some(v) = a.strip_prefix("--codeset-filesys=") {
                    c.codeset_filesys = Some(v.to_string());
                } else if let Some(v) = a.strip_prefix("--codeset-id3=") {
                    c.codeset_id3 = Some(v.to_string());
                } else if let Some(v) = a.strip_prefix("--codeset-metadata=") {
                    c.codeset_metadata = Some(v.to_string());
                } else if let Some(v) = a.strip_prefix("--codeset-relay=") {
                    c.codeset_relay = Some(v.to_string());
                } else if let Some(v) = a.strip_prefix("--split-time=") {
                    c.split_time = Some(parse_length(v)?);
                } else if let Some(v) = a.strip_prefix("--split-size=") {
                    c.split_size = Some(parse_u64(v, "--split-size")?);
                } else if let Some(v) = a.strip_prefix("--config=") {
                    c.config = Some(v.to_string());
                } else if a.starts_with('-') && a != "-" {
                    return Err(format!("unknown option: {a}"));
                } else if !url_set {
                    c.url = a;
                    url_set = true;
                } else {
                    return Err(format!("unexpected argument: {a}"));
                }
            }
        }
        i += 1;
    }

    if c.url.is_empty() {
        return Err("no stream URL given (try -h)".to_string());
    }
    if let Some(rf) = c.rulefile.clone() {
        c.rules = Rules::load(&rf)?;
    }
    Ok(Parsed::Run(Box::new(c)))
}

fn parse_u32(s: &str, opt: &str) -> Result<u32, String> {
    s.parse()
        .map_err(|_| format!("{opt} expects a number, got {s:?}"))
}
fn parse_u64(s: &str, opt: &str) -> Result<u64, String> {
    s.parse()
        .map_err(|_| format!("{opt} expects a number, got {s:?}"))
}
fn parse_i32(s: &str, opt: &str) -> Result<i32, String> {
    s.parse()
        .map_err(|_| format!("{opt} expects a number, got {s:?}"))
}

fn parse_pair(s: &str, opt: &str) -> Result<(i32, i32), String> {
    let (a, b) = s
        .split_once(':')
        .ok_or_else(|| format!("{opt} expects num:num, got {s:?}"))?;
    Ok((parse_i32(a, opt)?, parse_i32(b, opt)?))
}

fn parse_overwrite(s: &str) -> Result<Overwrite, String> {
    match s {
        "always" => Ok(Overwrite::Always),
        "never" => Ok(Overwrite::Never),
        "larger" => Ok(Overwrite::Larger),
        "version" => Ok(Overwrite::Version),
        _ => Err(format!("-o expects always|never|larger|version, got {s:?}")),
    }
}

fn parse_hls_variant(s: &str) -> Result<HlsVariant, String> {
    match s {
        "best" => Ok(HlsVariant::Best),
        "worst" => Ok(HlsVariant::Worst),
        _ => s
            .parse::<u32>()
            .map(HlsVariant::Bitrate)
            .map_err(|_| format!("--hls-variant expects best|worst|<bps>, got {s:?}")),
    }
}

fn parse_ip_port(s: &str) -> Result<(Option<String>, u16), String> {
    // forms: "port" or "ip:port"
    if let Some((ip, port)) = s.rsplit_once(':') {
        let p = port
            .parse()
            .map_err(|_| format!("bad relay port in {s:?}"))?;
        Ok((Some(ip.to_string()), p))
    } else {
        let p = s.parse().map_err(|_| format!("bad relay port in {s:?}"))?;
        Ok((None, p))
    }
}

/// Parse `-l`: plain seconds (3600), a unit (90s/12m/2h), or hh:mm[:ss].
pub fn parse_length(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Some((h, rest)) = s.split_once(':') {
        // hh:mm[:ss]
        let h: u64 = h.parse().map_err(|_| format!("bad hours in {s:?}"))?;
        let (m, sec) = match rest.split_once(':') {
            Some((m, sec)) => (
                m.parse::<u64>()
                    .map_err(|_| format!("bad minutes in {s:?}"))?,
                sec.parse::<u64>()
                    .map_err(|_| format!("bad seconds in {s:?}"))?,
            ),
            None => (
                rest.parse::<u64>()
                    .map_err(|_| format!("bad minutes in {s:?}"))?,
                0,
            ),
        };
        return Ok(h * 3600 + m * 60 + sec);
    }
    let (num, mult) = match s.chars().last() {
        Some('s') | Some('S') => (&s[..s.len() - 1], 1),
        Some('m') | Some('M') => (&s[..s.len() - 1], 60),
        Some('h') | Some('H') => (&s[..s.len() - 1], 3600),
        _ => (s, 1),
    };
    num.trim()
        .parse::<u64>()
        .map(|n| n * mult)
        .map_err(|_| format!("bad -l value: {s:?}"))
}

pub fn print_usage() {
    let u = "\
Usage: crabripper URL [OPTIONS]   (Rust port; aims for streamripper CLI parity)
Opts: -h             - Print this listing
      -v             - Print version info and quit
      -a [file]      - Rip to single file, default name is timestamped
      -A             - Don't write individual tracks
      -d dir         - The destination directory
      -D pattern     - Write files using specified pattern
      -s             - Don't create a directory for each stream
      -r [[ip:]port] - Create relay server on base ip:port, default port 8000
      -R #connect    - Max connections to relay, default 1, -R 0 is no limit
      -L file        - Create a relay playlist file
      -z             - Don't scan for free ports if base port is not avail
      -p url         - Use HTTP proxy server at <url>
      -o (always|never|larger|version) - When to write tracks in complete
      -t             - Don't overwrite tracks in incomplete
      -c             - Don't auto-reconnect
      -l length      - How long to run: seconds (3600), unit (90s/12m/2h),
                       or hh:mm[:ss] (1:30 or 1:30:00); else runs forever
      -M megabytes   - Stop ripping after this many megabytes
      -q [start]     - Add sequence number to output file
      -u useragent   - Use a different UserAgent than \"crabripper\"
      -w rulefile    - Parse metadata using rules in file
      -m timeout     - Seconds before force-closing a stalled connection
      -k count       - Leave <count> tracks in incomplete
      -I interface   - Rip from specified interface
      -T             - Truncate duplicated tracks in incomplete
      -E command     - Run external command to fetch metadata
      --quiet        - Don't print ripping status to console
      --stderr       - Print ripping status to stderr
      --debug        - Verbose debugging trace
      --http10       - Use HTTP/1.0
      --hls / --no-hls        - Force / disable HLS (.m3u8) handling
      --hls-variant=V         - HLS master variant: best|worst|<bps>
      --wav          - Decode tracks and write .wav files
      --no-cue       - Don't write .cue sheet files
TLS opts:
      --ssl-verify / --no-ssl-verify   - Verify the TLS cert (default: off)
ID3 opts (mp3/aac):
      -i                      - Don't add any ID3 tags
      --with-id3v1            - Add ID3V1 tags
      --without-id3v2         - Don't add ID3V2 tags
Splitpoint opts (mp3):
      --xs-none / --xs2
      --xs-offset=num  --xs-padding=num:num
      --xs-search-window=num:num  --xs-silence-length=num
Codeset opts:
      --codeset-filesys= / --codeset-id3= / --codeset-metadata= / --codeset-relay=
crabripper extensions:
      --log            - Write a timestamped played.log of track titles
      --split-time=SECS - Force a new track every N seconds (or 90s/12m/2h)
      --split-size=MB  - Force a new track every N megabytes
      -C file / --config=file - Read options from a file (CLI overrides it)";
    eprintln!("{u}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(args: &[&str]) -> Config {
        let mut v = vec!["crabripper".to_string()];
        v.extend(args.iter().map(|s| s.to_string()));
        match parse(v).unwrap() {
            Parsed::Run(c) => *c,
            Parsed::Exit(_) => panic!("unexpected exit"),
        }
    }

    #[test]
    fn length_forms() {
        assert_eq!(parse_length("3600").unwrap(), 3600);
        assert_eq!(parse_length("90s").unwrap(), 90);
        assert_eq!(parse_length("12m").unwrap(), 720);
        assert_eq!(parse_length("2h").unwrap(), 7200);
        assert_eq!(parse_length("1:30").unwrap(), 5400);
        assert_eq!(parse_length("1:30:00").unwrap(), 5400);
        assert_eq!(parse_length("0:00:45").unwrap(), 45);
    }

    #[test]
    fn basic_opts() {
        let c = cfg(&[
            "http://x/y",
            "-d",
            "rec",
            "-s",
            "-A",
            "-a",
            "show.mp3",
            "-l",
            "1:00",
        ]);
        assert_eq!(c.url, "http://x/y");
        assert_eq!(c.output_dir.as_deref(), Some("rec"));
        assert!(c.no_subdir && c.no_individual && c.single_file);
        assert_eq!(c.single_file_name.as_deref(), Some("show.mp3"));
        assert_eq!(c.length, Some(3600));
    }

    #[test]
    fn relay_and_variant() {
        let c = cfg(&[
            "url",
            "-r",
            "127.0.0.1:9000",
            "-R",
            "0",
            "--hls-variant=128000",
        ]);
        assert_eq!(c.relay_port, Some(9000));
        assert_eq!(c.relay_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(c.max_relay, 0);
        matches!(c.hls_variant, HlsVariant::Bitrate(128000));
    }

    #[test]
    fn a_without_name() {
        let c = cfg(&["url", "-a", "-A"]);
        assert!(c.single_file && c.single_file_name.is_none() && c.no_individual);
    }
}
