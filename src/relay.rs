//! A small relay server: serve the stream to local players (VLC, mpv, ...) while
//! it is being recorded. Clients that ask for metadata (`Icy-MetaData: 1`) get
//! ICY-interleaved `StreamTitle` updates so they show the current track; others
//! get the plain audio.
//!
//! The recorder must never wait on a listener: each client has its own writer
//! thread fed through a bounded queue, and a client that falls behind is
//! dropped rather than allowed to stall the recording.

use crate::util::{printable, truncate_utf8};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

const METAINT: usize = 16000;
/// Chunks a client may lag behind before it is disconnected.
const QUEUE_CHUNKS: usize = 64;
/// Connections allowed to be mid-handshake at once.
const MAX_HANDSHAKES: usize = 16;
/// Hard ceiling on listeners, even with `-R 0` ("unlimited").
const MAX_CLIENTS: usize = 64;
/// Total time a client gets to send its request headers.
const HANDSHAKE_TIME: Duration = Duration::from_secs(3);
/// Largest StreamTitle text: the ICY length byte counts 16-byte blocks.
const MAX_META: usize = 255 * 16;

type Chunk = Arc<[u8]>;

pub struct Relay {
    clients: Arc<Mutex<Vec<SyncSender<Chunk>>>>,
    title: Arc<Mutex<String>>,
    pub port: u16,
}

fn handshakes_dec(h: &AtomicUsize) {
    h.fetch_sub(1, Ordering::SeqCst);
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Relay {
    pub fn start(
        ip: &str,
        base_port: u16,
        scan: bool,
        max: u32,
        name: String,
        content_type: String,
    ) -> std::io::Result<Relay> {
        let bind_ip = if ip.is_empty() { "0.0.0.0" } else { ip };
        let (listener, port) = bind(bind_ip, base_port, scan)?;

        let clients: Arc<Mutex<Vec<SyncSender<Chunk>>>> = Arc::new(Mutex::new(Vec::new()));
        let title: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let ct = if content_type.is_empty() {
            "audio/mpeg".to_string()
        } else {
            content_type
        };
        // Upstream header values: no CR/LF may reach our response headers.
        let head = Arc::new((printable(&name), printable(&ct)));
        let handshakes = Arc::new(AtomicUsize::new(0));

        let (clients_acc, title_acc) = (clients.clone(), title.clone());
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(s) = conn else {
                    // e.g. out of file descriptors: back off, don't spin
                    thread::sleep(Duration::from_millis(200));
                    continue;
                };
                if handshakes.fetch_add(1, Ordering::SeqCst) >= MAX_HANDSHAKES {
                    handshakes.fetch_sub(1, Ordering::SeqCst);
                    continue; // drop: too many pending connections
                }
                let (clients, title, head, hs) = (
                    clients_acc.clone(),
                    title_acc.clone(),
                    head.clone(),
                    handshakes.clone(),
                );
                let spawned = thread::Builder::new()
                    .spawn(move || serve(s, max, &clients, &title, &head, &hs));
                if spawned.is_err() {
                    handshakes_dec(&handshakes);
                }
            }
        });

        Ok(Relay {
            clients,
            title,
            port,
        })
    }

    /// Update the title that metadata-aware clients will be told about.
    pub fn set_title(&self, t: &str) {
        *lock(&self.title) = t.to_string();
    }

    /// Queue a chunk for every client without blocking; drop clients that are
    /// too far behind or gone.
    pub fn broadcast(&self, data: &[u8]) {
        let chunk: Chunk = Arc::from(data);
        lock(&self.clients).retain(|tx| match tx.try_send(chunk.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        });
    }
}

/// One client: handshake, register, then write queued chunks until the queue
/// is dropped (client too slow) or the socket fails.
fn serve(
    mut s: TcpStream,
    max: u32,
    clients: &Mutex<Vec<SyncSender<Chunk>>>,
    title: &Mutex<String>,
    head: &(String, String),
    handshakes: &AtomicUsize,
) {
    let wants_meta = request_wants_meta(&mut s);
    let rx: Option<Receiver<Chunk>> = {
        let mut g = lock(clients);
        let cap = if max == 0 {
            MAX_CLIENTS
        } else {
            (max as usize).min(MAX_CLIENTS)
        };
        if g.len() >= cap {
            None
        } else {
            let (tx, rx) = sync_channel(QUEUE_CHUNKS);
            g.push(tx);
            Some(rx)
        }
    };
    handshakes_dec(handshakes);
    let Some(rx) = rx else { return }; // relay full: close

    s.set_write_timeout(Some(Duration::from_secs(10))).ok();
    let mut hdr = format!(
        "ICY 200 OK\r\nicy-name:{}\r\ncontent-type:{}\r\n",
        head.0, head.1
    );
    if wants_meta {
        hdr.push_str(&format!("icy-metaint:{METAINT}\r\n"));
    }
    hdr.push_str("\r\n");
    if s.write_all(hdr.as_bytes()).is_err() {
        return;
    }
    let mut since_meta = 0usize;
    let mut last_title = String::new();
    for chunk in rx.iter() {
        let t = lock(title).clone();
        let res = if wants_meta {
            write_meta(&mut s, &chunk, &mut since_meta, &mut last_title, &t)
        } else {
            s.write_all(&chunk)
        };
        if res.is_err() {
            return; // dropping rx unregisters us on the next broadcast
        }
    }
}

/// Interleave an ICY metadata block every METAINT audio bytes.
fn write_meta(
    s: &mut TcpStream,
    mut data: &[u8],
    since_meta: &mut usize,
    last_title: &mut String,
    title: &str,
) -> std::io::Result<()> {
    while !data.is_empty() {
        let n = (METAINT - *since_meta).min(data.len());
        s.write_all(&data[..n])?;
        *since_meta += n;
        data = &data[n..];
        if *since_meta == METAINT {
            if title != last_title.as_str() {
                s.write_all(&meta_block(title))?;
                *last_title = title.to_string();
            } else {
                s.write_all(&[0u8])?; // no metadata update this interval
            }
            *since_meta = 0;
        }
    }
    Ok(())
}

/// `len_byte + StreamTitle='...';` padded to 16-byte blocks; the title is
/// sanitized and truncated so the block count always fits the length byte.
fn meta_block(title: &str) -> Vec<u8> {
    let clean = printable(title).replace('\'', "");
    let room = MAX_META - "StreamTitle='';".len();
    let text = format!("StreamTitle='{}';", truncate_utf8(&clean, room));
    let blocks = text.len().div_ceil(16);
    let mut out = Vec::with_capacity(1 + blocks * 16);
    out.push(blocks as u8);
    out.extend_from_slice(text.as_bytes());
    out.resize(1 + blocks * 16, 0);
    out
}

/// Read the client's request headers and report whether it asked for metadata.
fn request_wants_meta(s: &mut TcpStream) -> bool {
    let started = std::time::Instant::now();
    let mut data = Vec::new();
    let mut buf = [0u8; 1024];
    while data.len() < 4096 {
        // a total budget, not per read: a client dribbling bytes can't hold
        // a handshake slot for long
        let left = HANDSHAKE_TIME.saturating_sub(started.elapsed());
        if left.is_zero() {
            break;
        }
        s.set_read_timeout(Some(left)).ok();
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                if data.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&data).to_lowercase();
    text.contains("icy-metadata:1") || text.contains("icy-metadata: 1")
}

fn bind(ip: &str, base: u16, scan: bool) -> std::io::Result<(TcpListener, u16)> {
    let tries: u16 = if scan { 20 } else { 1 };
    let mut last_err = None;
    for off in 0..tries {
        let port = base.saturating_add(off);
        match TcpListener::bind((ip, port)) {
            Ok(l) => return Ok((l, port)),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("could not bind relay port")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_block_always_fits() {
        let b = meta_block(&"x".repeat(10_000));
        assert_eq!(b.len(), 1 + b[0] as usize * 16);
        assert!(b[0] as usize * 16 <= MAX_META);
        let b = meta_block("a'b\r\nc");
        assert_eq!(&b[1..21], b"StreamTitle='ab??c';");
    }
}
