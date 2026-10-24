//! crabripper — a Rust port of streamripper.
//!
//! Records shoutcast/icecast streams (HTTP/HTTPS) to per-track files, reading
//! ICY inline metadata to split on track boundaries. Aims for CLI/behaviour
//! parity with the C fork while using idiomatic, modern Rust underneath.

mod cli;
mod codeset;
mod conn;
mod cue;
mod decode;
mod external;
mod frames;
mod hls;
mod http;
mod icy;
mod id3;
mod ogg;
mod recorder;
mod relay;
mod rules;
mod source;
mod split;
mod url;
mod util;
mod wav;

use cli::{Config, Parsed};
use std::time::{Duration, Instant};
use util::printable;

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
        eprintln!("error: {}", printable(&e));
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
            "[debug] url={} ua={:?} timeout={}s reconnect={} ssl_verify={}",
            cfg.url, cfg.user_agent, cfg.timeout, !cfg.no_reconnect, cfg.ssl_verify
        );
    }
    if cfg.codeset_relay.is_some() {
        // The relay re-sends titles as UTF-8 text; nothing to transcode.
        say("note: --codeset-relay has no effect (the relay carries no metadata)");
    }

    // The -l clock starts now (connecting counts), and arms the watchdog that
    // cuts every socket once the run time is over.
    let start = Instant::now();
    let deadline = cfg
        .length
        .and_then(|l| start.checked_add(Duration::from_secs(l)));
    if let Some(d) = deadline {
        conn::set_deadline(d);
    }

    say(&format!("Connecting to {} ...", printable(&cfg.url)));
    let (body, info) = http::open_stream(&cfg.url, cfg).map_err(|e| {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            "run time limit (-l) reached before the stream started".to_string()
        } else {
            e
        }
    })?;
    if info.final_url != cfg.url {
        say(&format!("  -> {}", printable(&info.final_url)));
    }

    if info.is_hls {
        say("HLS (.m3u8) stream detected");
        drop(body); // the HLS ripper re-fetches the playlist as a whole body
        return hls::rip(&info.final_url, cfg, deadline);
    }

    say(&format!(
        "stream: {}",
        if info.name.is_empty() {
            "(unnamed)".to_string()
        } else {
            printable(&info.name)
        }
    ));
    say(&format!(
        "content-type: {} -> .{}",
        if info.content_type.is_empty() {
            "(none)".to_string()
        } else {
            printable(&info.content_type)
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
    let mut src = source::Source::new(body);
    let mut ext = info.ext.clone();
    let mut no_split = ext == "flac";
    if ext == "ogg" && has_flac(src.peek(64).unwrap_or_default()) {
        ext = "oga".to_string();
        no_split = true;
        say("Ogg stream carries FLAC -> .oga (captured whole)");
    }

    let mut rec =
        recorder::Recorder::new(cfg, &info.name, &ext, info.bitrate.unwrap_or(128), no_split)
            .map_err(|e| e.to_string())?;

    // -E external metadata source (overrides ICY metadata when present). Held
    // for the whole run; dropping it kills the command.
    let external = match &cfg.external_cmd {
        Some(cmd) => {
            say(&format!("external metadata: {cmd}"));
            Some(external::spawn(cmd).map_err(|e| format!("-E: {e}"))?)
        }
        None => None,
    };
    let ext_rx = external.as_ref().map(|x| &x.rx);

    let max_bytes = cfg.max_megabytes.map(|m| m.saturating_mul(1024 * 1024));
    let expired = |total: u64| {
        deadline.is_some_and(|d| Instant::now() >= d) || max_bytes.is_some_and(|mb| total >= mb)
    };

    let mut total: u64 = 0;
    // The first connection is already open; on a drop, reconnect and continue the
    // same recording — unless -c (no_reconnect) or the time/size limit is reached.
    let mut next = Some((src, info.metaint));
    loop {
        let (mut src, metaint) = match next.take() {
            Some(cl) => cl,
            None => {
                if cfg.no_reconnect || expired(total) {
                    break;
                }
                say("stream dropped — reconnecting in 2s ...");
                std::thread::sleep(Duration::from_secs(2));
                match http::open_stream(&cfg.url, cfg) {
                    // The new connection may use a different metadata interval.
                    Ok((b, i)) => (source::Source::new(b), i.metaint),
                    Err(e) => {
                        eprintln!("  ! reconnect failed: {}", printable(&e));
                        continue;
                    }
                }
            }
        };
        match rip_stream(
            cfg, metaint, &ext, &mut rec, &relay, &mut total, &expired, &mut src, ext_rx,
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
    drop(external);
    say(&format!(
        "done ({:.0}s, {} KB).",
        start.elapsed().as_secs_f64(),
        total / 1024
    ));
    Ok(())
}

enum StopReason {
    Done,         // -l / -M reached — stop for good
    Disconnected, // stream ended, dropped or errored — reconnect unless -c
}

/// One connection's worth of ripping. Returns why it stopped. Network errors
/// end the connection (and trigger a reconnect); only local errors (disk) are
/// fatal.
#[allow(clippy::too_many_arguments)]
fn rip_stream(
    cfg: &Config,
    metaint: usize,
    ext: &str,
    rec: &mut recorder::Recorder,
    relay: &Option<relay::Relay>,
    total: &mut u64,
    expired: &dyn Fn(u64) -> bool,
    src: &mut source::Source,
    ext_meta: Option<&std::sync::mpsc::Receiver<String>>,
) -> Result<StopReason, String> {
    let say = |m: &str| {
        if !cfg.quiet {
            eprintln!("{m}");
        }
    };
    // A read failure (reset, timeout, TLS close) is a disconnect, not the end —
    // unless the run time is up (the watchdog closes sockets at the deadline).
    let lost = |e: std::io::Error, total: u64| {
        if expired(total) {
            return StopReason::Done;
        }
        say(&format!(
            "  ! connection error: {}",
            printable(&e.to_string())
        ));
        StopReason::Disconnected
    };
    let mut audio = Vec::new();

    if ext == "ogg" {
        say("(ogg — splitting on logical-stream boundaries)");
        let mut splitter = ogg::OggSplitter::new();
        loop {
            if expired(*total) {
                return Ok(StopReason::Done);
            }
            match src.read_some(&mut audio) {
                Ok(true) => {}
                Ok(false) => return Ok(StopReason::Disconnected),
                Err(e) => return Ok(lost(e, *total)),
            }
            *total += audio.len() as u64;
            for ev in splitter.push(&audio) {
                if let Some(title) = ev.title {
                    if let Some(r) = relay {
                        r.set_title(&title);
                    }
                    rec.roll_track(&title).map_err(|e| e.to_string())?;
                }
                rec.write_audio(&ev.bytes).map_err(|e| e.to_string())?;
                if let Some(r) = relay {
                    r.broadcast(&ev.bytes);
                }
            }
        }
    } else if metaint > 0 {
        let mut meta = Vec::new();
        loop {
            if expired(*total) {
                return Ok(StopReason::Done);
            }
            let full = src.read_exact_n(metaint, &mut audio);
            // Keep whatever audio arrived, even if the connection then dropped.
            if !audio.is_empty() {
                *total += audio.len() as u64;
                rec.write_audio(&audio).map_err(|e| e.to_string())?;
                if let Some(r) = relay {
                    r.broadcast(&audio);
                }
            }
            match full {
                Ok(true) => {}
                Ok(false) => return Ok(StopReason::Disconnected),
                Err(e) => return Ok(lost(e, *total)),
            }
            drain_external(ext_meta, cfg, rec, relay)?;
            let len_byte = match src.read_byte() {
                Ok(Some(b)) => b,
                Ok(None) => return Ok(StopReason::Disconnected),
                Err(e) => return Ok(lost(e, *total)),
            };
            let meta_len = len_byte as usize * 16;
            if meta_len > 0 {
                match src.read_exact_n(meta_len, &mut meta) {
                    Ok(true) => {}
                    Ok(false) => return Ok(StopReason::Disconnected),
                    Err(e) => return Ok(lost(e, *total)),
                }
                // ICY metadata — skipped when an external source (-E) is driving.
                if ext_meta.is_none() {
                    if let Some(raw) = icy::stream_title_raw(&meta) {
                        let title = codeset::decode(raw, cfg.codeset_metadata.as_deref());
                        let title = cfg.rules.apply(&title);
                        if !title.is_empty() {
                            if let Some(r) = relay {
                                r.set_title(&title);
                            }
                            rec.on_title(&title).map_err(|e| e.to_string())?;
                        }
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
            match src.read_some(&mut audio) {
                Ok(true) => {}
                Ok(false) => return Ok(StopReason::Disconnected),
                Err(e) => return Ok(lost(e, *total)),
            }
            *total += audio.len() as u64;
            rec.write_audio(&audio).map_err(|e| e.to_string())?;
            if let Some(r) = relay {
                r.broadcast(&audio);
            }
            drain_external(ext_meta, cfg, rec, relay)?;
        }
    }
}

/// Apply any pending titles from the external metadata process (-E).
fn drain_external(
    ext_meta: Option<&std::sync::mpsc::Receiver<String>>,
    cfg: &Config,
    rec: &mut recorder::Recorder,
    relay: &Option<relay::Relay>,
) -> Result<(), String> {
    if let Some(rx) = ext_meta {
        while let Ok(t) = rx.try_recv() {
            let t = cfg.rules.apply(&t);
            if !t.is_empty() {
                if let Some(r) = relay {
                    r.set_title(&t);
                }
                rec.on_title(&t).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

/// Sniff the first bytes of an Ogg body for the FLAC signature (FLAC-in-Ogg).
fn has_flac(b: &[u8]) -> bool {
    b.windows(4).any(|w| w == b"FLAC")
}
