//! Track recorder: writes audio to disk, starting a new file whenever the ICY
//! `StreamTitle` changes. This mirrors streamripper's metadata-driven splitting
//! (silence-based refinement is a planned enhancement).

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub struct Recorder {
    dir: PathBuf,
    ext: String,
    current: Option<(String, File)>, // (title, open file)
    count: u32,
}

impl Recorder {
    pub fn new(dir: &Path, ext: &str) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        Ok(Recorder {
            dir: dir.to_path_buf(),
            ext: ext.to_string(),
            current: None,
            count: 0,
        })
    }

    /// Called when a new `StreamTitle` is seen. Opens a fresh output file if the
    /// title actually changed (or if nothing is open yet).
    pub fn on_title(&mut self, title: &str) -> io::Result<()> {
        let changed = match &self.current {
            Some((cur, _)) => cur != title,
            None => true,
        };
        if !changed {
            return Ok(());
        }
        self.count += 1;
        let base = sanitize(title);
        let base = if base.is_empty() {
            format!("track_{:03}", self.count)
        } else {
            base
        };
        let path = self.dir.join(format!("{base}.{}", self.ext));
        let file = File::create(&path)?;
        eprintln!("[track {:>3}] {}", self.count, path.display());
        self.current = Some((title.to_string(), file));
        Ok(())
    }

    /// Append audio bytes to the current track (starting a default track if no
    /// metadata has arrived yet).
    pub fn write_audio(&mut self, data: &[u8]) -> io::Result<()> {
        if self.current.is_none() {
            self.on_title("")?;
        }
        if let Some((_, f)) = &mut self.current {
            f.write_all(data)?;
        }
        Ok(())
    }
}

/// Make a title safe to use as a filename.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}
