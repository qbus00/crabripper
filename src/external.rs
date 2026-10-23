//! `-E` external metadata: run a command and read track titles from its stdout
//! (overriding ICY metadata). Each `TITLE=`/`ARTIST=` pair, or a plain line,
//! becomes a new track title, delivered over a channel.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::thread;

pub fn spawn(cmd: &str) -> std::io::Result<Receiver<String>> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdout(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = channel();

    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut artist = String::new();
        for line in reader.lines().map_while(Result::ok) {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            let title = if let Some((k, v)) = line.split_once('=') {
                match k.trim().to_ascii_uppercase().as_str() {
                    "ARTIST" => {
                        artist = v.trim().to_string();
                        continue;
                    }
                    "TITLE" => {
                        let t = v.trim();
                        if artist.is_empty() {
                            t.to_string()
                        } else {
                            format!("{artist} - {t}")
                        }
                    }
                    _ => continue,
                }
            } else {
                line
            };
            if tx.send(title).is_err() {
                break;
            }
        }
        let _ = child.wait();
    });

    Ok(rx)
}
