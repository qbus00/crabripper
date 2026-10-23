//! A small relay server: serve the stream to local players (VLC, mpv, ...) while
//! it is being recorded. Clients receive the de-interleaved audio (no ICY
//! metadata) behind a minimal shoutcast-style header.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub struct Relay {
    clients: Arc<Mutex<Vec<TcpStream>>>,
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

        let clients: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
        let clients_acc = clients.clone();
        let ct = if content_type.is_empty() {
            "audio/mpeg".to_string()
        } else {
            content_type
        };
        let header = format!("ICY 200 OK\r\nicy-name:{name}\r\ncontent-type:{ct}\r\n\r\n");

        thread::spawn(move || {
            for conn in listener.incoming() {
                let mut s = match conn {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                s.set_write_timeout(Some(Duration::from_secs(10))).ok();
                if s.write_all(header.as_bytes()).is_err() {
                    continue;
                }
                let mut guard = clients_acc.lock().unwrap();
                if max != 0 && guard.len() as u32 >= max {
                    continue; // over the limit: drop (closes on scope exit)
                }
                guard.push(s);
            }
        });

        Ok(Relay { clients, port })
    }

    /// Send a chunk to every connected client; drop any that error.
    pub fn broadcast(&self, data: &[u8]) {
        let mut guard = self.clients.lock().unwrap();
        guard.retain_mut(|s| s.write_all(data).is_ok());
    }
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
