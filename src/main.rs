//! streamripper-rs — a work-in-progress Rust port of streamripper.
//!
//! Records shoutcast/icecast streams (HTTP/HTTPS) to per-track files, reading
//! ICY inline metadata to split on track boundaries. Aims for CLI/behaviour
//! parity with the C fork while using idiomatic, modern Rust underneath.

mod cli;
mod conn;
mod cue;
mod http;
mod icy;
mod id3;
mod recorder;
mod relay;
mod source;
mod url;

use cli::{Config, Parsed};
use std::time::{Duration, Instant};

fn main() {
    let parsed = match cli::parse(std::env::args().collect::<Vec<_>>()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    let cfg = match parsed {
        Parsed::Run(c) => c,
        Parsed::Exit(code) => std::process::exit(code),
    };
    if let Err(e) = run(&cfg) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(cfg: &Config) -> Result<(), String> {
    let say = |m: &str| {
        if !cfg.quiet {
            eprintln!("{m}");
        }
    };

    say(&format!("Connecting to {} ...", cfg.url));
    let (conn, info, leftover) = http::open_stream(&cfg.url, cfg)?;
    if info.final_url != cfg.url {
        say(&format!("  -> {}", info.final_url));
    }

    if info.is_hls {
        return Err("HLS (.m3u8) handling is not implemented yet (planned increment)".to_string());
    }

    say(&format!(
        "stream: {}",
        if info.name.is_empty() {
            "(unnamed)"
        } else {
            &info.name
        }
    ));
    say(&format!(
        "content-type: {} -> .{}",
        if info.content_type.is_empty() {
            "(none)"
        } else {
            &info.content_type
        },
        info.ext
    ));
    if let Some(br) = info.bitrate {
        say(&format!("declared bitrate: {br}"));
    }
    say(&format!("metadata interval: {}", info.metaint));

    // Optional relay server.
    let relay = if let Some(port) = cfg.relay_port {
        let ip = cfg.relay_ip.clone().unwrap_or_default();
        match relay::Relay::start(
            &ip,
            port,
            !cfg.no_port_scan,
            cfg.max_relay,
            info.name.clone(),
            info.content_type.clone(),
        ) {
            Ok(r) => {
                let host = if ip.is_empty() || ip == "0.0.0.0" {
                    "127.0.0.1"
                } else {
                    &ip
                };
                say(&format!("relay: http://{host}:{}", r.port));
                if let Some(pl) = &cfg.relay_playlist {
                    let _ = std::fs::write(pl, format!("http://{host}:{}\n", r.port));
                }
                Some(r)
            }
            Err(e) => {
                eprintln!("  ! relay disabled: {e}");
                None
            }
        }
    } else {
        None
    };

    let mut rec = recorder::Recorder::new(cfg, &info.name, &info.ext, info.bitrate.unwrap_or(128))
        .map_err(|e| e.to_string())?;
    let mut src = source::Source::new(conn, leftover);

    let start = Instant::now();
    let deadline = cfg.length.map(|l| start + Duration::from_secs(l));
    let max_bytes = cfg.max_megabytes.map(|m| m * 1024 * 1024);
    let expired = |total: u64| {
        deadline.is_some_and(|d| Instant::now() >= d) || max_bytes.is_some_and(|mb| total >= mb)
    };

    let mut total: u64 = 0;
    let mut audio = Vec::new();

    if info.metaint > 0 {
        let mut meta = Vec::new();
        loop {
            if expired(total) {
                break;
            }
            if !src
                .read_exact_n(info.metaint, &mut audio)
                .map_err(|e| e.to_string())?
            {
                break;
            }
            total += audio.len() as u64;
            rec.write_audio(&audio).map_err(|e| e.to_string())?;
            if let Some(r) = &relay {
                r.broadcast(&audio);
            }

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
        say("(no ICY metadata — recording a single file)");
        while !expired(total) {
            if !src.read_some(&mut audio).map_err(|e| e.to_string())? {
                break;
            }
            total += audio.len() as u64;
            rec.write_audio(&audio).map_err(|e| e.to_string())?;
            if let Some(r) = &relay {
                r.broadcast(&audio);
            }
        }
    }

    rec.finish().map_err(|e| e.to_string())?;
    say(&format!(
        "done ({:.0}s, {} KB).",
        start.elapsed().as_secs_f64(),
        total / 1024
    ));
    Ok(())
}
