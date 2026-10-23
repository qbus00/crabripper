//! A small relay server: serve the stream to local players (VLC, mpv, ...) while
//! it is being recorded. Clients that ask for metadata (`Icy-MetaData: 1`) get
//! ICY-interleaved `StreamTitle` updates so they show the current track; others
//! get the plain audio.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const METAINT: usize = 16000;

struct Client {
    stream: TcpStream,
    meta: bool,
    since_meta: usize,
    last_title: String,
}

pub struct Relay {
    clients: Arc<Mutex<Vec<Client>>>,
    title: Arc<Mutex<String>>,
    pub port: u16,
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

        let clients: Arc<Mutex<Vec<Client>>> = Arc::new(Mutex::new(Vec::new()));
        let title: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let clients_acc = clients.clone();
        let ct = if content_type.is_empty() {
            "audio/mpeg".to_string()
        } else {
            content_type
        };

        thread::spawn(move || {
            for conn in listener.incoming() {
                let mut s = match conn {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let wants_meta = request_wants_meta(&mut s);
                s.set_write_timeout(Some(Duration::from_secs(10))).ok();
                let mut hdr = format!("ICY 200 OK\r\nicy-name:{name}\r\ncontent-type:{ct}\r\n");
                if wants_meta {
                    hdr.push_str(&format!("icy-metaint:{METAINT}\r\n"));
                }
                hdr.push_str("\r\n");
                if s.write_all(hdr.as_bytes()).is_err() {
                    continue;
                }
                let mut guard = clients_acc.lock().unwrap();
                if max != 0 && guard.len() as u32 >= max {
                    continue;
                }
                guard.push(Client {
                    stream: s,
                    meta: wants_meta,
                    since_meta: 0,
                    last_title: String::new(),
                });
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
        *self.title.lock().unwrap() = t.to_string();
    }

    /// Send a chunk to every connected client; drop any that error.
    pub fn broadcast(&self, data: &[u8]) {
        let title = self.title.lock().unwrap().clone();
        let mut guard = self.clients.lock().unwrap();
        guard.retain_mut(|c| write_client(c, data, &title).is_ok());
    }
}

fn write_client(c: &mut Client, mut data: &[u8], title: &str) -> std::io::Result<()> {
    if !c.meta {
        return c.stream.write_all(data);
    }
    // Interleave an ICY metadata block every METAINT audio bytes.
    while !data.is_empty() {
        let room = METAINT - c.since_meta;
        let n = room.min(data.len());
        c.stream.write_all(&data[..n])?;
        c.since_meta += n;
        data = &data[n..];
        if c.since_meta == METAINT {
            if title != c.last_title {
                let text = format!("StreamTitle='{}';", title.replace('\'', ""));
                let mut bytes = text.into_bytes();
                let blocks = bytes.len().div_ceil(16);
                bytes.resize(blocks * 16, 0);
                c.stream.write_all(&[blocks as u8])?;
                c.stream.write_all(&bytes)?;
                c.last_title = title.to_string();
            } else {
                c.stream.write_all(&[0u8])?; // no metadata update this interval
            }
            c.since_meta = 0;
        }
    }
    Ok(())
}

/// Read the client's request headers and report whether it asked for metadata.
fn request_wants_meta(s: &mut TcpStream) -> bool {
    s.set_read_timeout(Some(Duration::from_secs(3))).ok();
    let mut data = Vec::new();
    let mut buf = [0u8; 1024];
    while data.len() < 4096 {
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
