//! streamripper-rs — a work-in-progress Rust port of streamripper.
//!
//! Records shoutcast/icecast streams (HTTP/HTTPS) to per-track files, reading
//! ICY inline metadata to split on track boundaries. Aims for CLI/behaviour
//! parity with the C fork while using idiomatic, modern Rust underneath.

mod cli;
mod conn;
mod cue;
mod decode;
mod hls;
mod http;
mod icy;
mod id3;
mod ogg;
mod recorder;
mod relay;
mod source;
mod split;
mod url;
mod util;
mod wav;

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

    if cfg.debug {
        eprintln!(
            "[debug] url={} ua={:?} timeout={}s reconnect={}",
            cfg.url, cfg.user_agent, cfg.timeout, !cfg.no_reconnect
        );
    }
    // Flags accepted for CLI parity but not yet implemented — warn rather than
    // silently ignore.
    let codeset = cfg.codeset_filesys.is_some()
        || cfg.codeset_id3.is_some()
        || cfg.codeset_metadata.is_some()
        || cfg.codeset_relay.is_some();
    for (set, name) in [
        (cfg.rulefile.is_some(), "-w (metadata rulefile)"),
        (cfg.external_cmd.is_some(), "-E (external metadata command)"),
        (cfg.interface.is_some(), "-I (interface binding)"),
        (codeset, "--codeset-* (charset conversion)"),
    ] {
        if set {
            eprintln!("  ! {name} is accepted for compatibility but not implemented yet; ignoring");
        }
    }

    say(&format!("Connecting to {} ...", cfg.url));
    let (conn, info, leftover) = http::open_stream(&cfg.url, cfg)?;
    if info.final_url != cfg.url {
        say(&format!("  -> {}", info.final_url));
    }

    if info.is_hls {
        say("HLS (.m3u8) stream detected");
        drop(conn); // the HLS ripper re-fetches the playlist as a whole body
        return hls::rip(&info.final_url, cfg);
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

    // FLAC/Ogg specifics: native FLAC -> .flac; FLAC carried in Ogg -> .oga
    // (detected by sniffing the first bytes). FLAC is captured whole (never split).
    let mut ext = info.ext.clone();
    let mut no_split = ext == "flac";
    if ext == "ogg" && leftover_has_flac(&leftover) {
        ext = "oga".to_string();
        no_split = true;
        say("Ogg stream carries FLAC -> .oga (captured whole)");
    }

    let mut rec =
        recorder::Recorder::new(cfg, &info.name, &ext, info.bitrate.unwrap_or(128), no_split)
            .map_err(|e| e.to_string())?;

    let start = Instant::now();
    let deadline = cfg.length.map(|l| start + Duration::from_secs(l));
    let max_bytes = cfg.max_megabytes.map(|m| m * 1024 * 1024);
    let expired = |total: u64| {
        deadline.is_some_and(|d| Instant::now() >= d) || max_bytes.is_some_and(|mb| total >= mb)
    };

    let mut total: u64 = 0;
    // The first connection is already open; on a drop, reconnect and continue the
    // same recording — unless -c (no_reconnect) or the time/size limit is reached.
    let mut next = Some((conn, leftover));
    loop {
        let (conn, leftover) = match next.take() {
            Some(cl) => cl,
            None => {
                if cfg.no_reconnect || expired(total) {
                    break;
                }
                say("stream dropped — reconnecting in 2s ...");
                std::thread::sleep(Duration::from_secs(2));
                match http::open_stream(&cfg.url, cfg) {
                    Ok((c, _, l)) => (c, l),
                    Err(e) => {
                        eprintln!("  ! reconnect failed: {e}");
                        continue;
                    }
                }
            }
        };
        let mut src = source::Source::new(conn, leftover);
        match rip_stream(
            cfg, &info, &ext, &mut rec, &relay, &mut total, &expired, &mut src,
        )? {
            StopReason::Done => break,
            StopReason::Disconnected => {
                if cfg.no_reconnect || expired(total) {
                    break;
                }
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

enum StopReason {
    Done,         // -l / -M reached — stop for good
    Disconnected, // stream ended or dropped — reconnect unless -c
}

/// One connection's worth of ripping. Returns why it stopped.
#[allow(clippy::too_many_arguments)]
fn rip_stream(
    cfg: &Config,
    info: &http::StreamInfo,
    ext: &str,
    rec: &mut recorder::Recorder,
    relay: &Option<relay::Relay>,
    total: &mut u64,
    expired: &dyn Fn(u64) -> bool,
    src: &mut source::Source,
) -> Result<StopReason, String> {
    let say = |m: &str| {
        if !cfg.quiet {
            eprintln!("{m}");
        }
    };
    let mut audio = Vec::new();

    if ext == "ogg" {
        say("(ogg — splitting on logical-stream boundaries)");
        let mut splitter = ogg::OggSplitter::new();
        loop {
            if expired(*total) {
                return Ok(StopReason::Done);
            }
            if !src.read_some(&mut audio).map_err(|e| e.to_string())? {
                return Ok(StopReason::Disconnected);
            }
            *total += audio.len() as u64;
            for ev in splitter.push(&audio) {
                if let Some(title) = ev.title {
                    rec.roll_track(&title).map_err(|e| e.to_string())?;
                }
                rec.write_audio(&ev.bytes).map_err(|e| e.to_string())?;
                if let Some(r) = relay {
                    r.broadcast(&ev.bytes);
                }
            }
        }
    } else if info.metaint > 0 {
        let mut meta = Vec::new();
        loop {
            if expired(*total) {
                return Ok(StopReason::Done);
            }
            if !src
                .read_exact_n(info.metaint, &mut audio)
                .map_err(|e| e.to_string())?
            {
                return Ok(StopReason::Disconnected);
            }
            *total += audio.len() as u64;
            rec.write_audio(&audio).map_err(|e| e.to_string())?;
            if let Some(r) = relay {
                r.broadcast(&audio);
            }
            let len_byte = match src.read_byte().map_err(|e| e.to_string())? {
                Some(b) => b,
                None => return Ok(StopReason::Disconnected),
            };
            let meta_len = len_byte as usize * 16;
            if meta_len > 0 {
                if !src
                    .read_exact_n(meta_len, &mut meta)
                    .map_err(|e| e.to_string())?
                {
                    return Ok(StopReason::Disconnected);
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
        loop {
            if expired(*total) {
                return Ok(StopReason::Done);
            }
            if !src.read_some(&mut audio).map_err(|e| e.to_string())? {
                return Ok(StopReason::Disconnected);
            }
            *total += audio.len() as u64;
            rec.write_audio(&audio).map_err(|e| e.to_string())?;
            if let Some(r) = relay {
                r.broadcast(&audio);
            }
        }
    }
}

/// Sniff the first bytes of an Ogg body for the FLAC signature (FLAC-in-Ogg).
fn leftover_has_flac(b: &[u8]) -> bool {
    let n = b.len().min(64);
    b[..n].windows(4).any(|w| w == b"FLAC")
}
