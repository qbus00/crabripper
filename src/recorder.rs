//! Track recorder with streamripper-style output layout:
//! - per-stream subdirectory (unless `-s`)
//! - tracks written to `incomplete/` while recording, moved to the destination
//!   once a following track proves them complete (the first, partial, track and
//!   the in-progress last track stay in `incomplete/`)
//! - overwrite modes (`-o`), single-file show (`-a`), no-individual (`-A`),
//!   keep-incomplete (`-k`), sequence numbers (`-q`), name patterns (`-D`).
//!
//! ID3 tags (v2.3 prepended to mp3/aac tracks, optional v1 appended) and cue
//! sheets for single-file (`-a`) recordings are written here too.

use crate::cli::{Config, Overwrite};
use crate::util::{now_stamp, sanitize};
use crate::{cue, id3};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

struct Track {
    title: String,
    path: PathBuf, // in incomplete/
    file: Option<File>,
    proper_start: bool,
    bytes: u64,
}

pub struct Recorder {
    root: PathBuf,
    incomplete: PathBuf,
    ext: String,
    mp3like: bool,
    no_individual: bool,
    overwrite: Overwrite,
    pattern: Option<String>,
    seq: Option<u32>,
    keep_incomplete: u32,

    no_id3: bool,
    with_id3v1: bool,
    without_id3v2: bool,

    show: Option<File>,
    show_path: Option<PathBuf>,
    show_bytes: u64,
    bitrate_kbps: u32,
    no_cue: bool,
    cue_entries: Vec<(String, u64)>,

    cur: Option<Track>,
    completed: u32,
    seen: u32,
    kept: u32,
}

impl Recorder {
    pub fn new(cfg: &Config, stream_name: &str, ext: &str, bitrate_kbps: u32) -> io::Result<Self> {
        let mut root = PathBuf::from(cfg.output_dir.clone().unwrap_or_else(|| ".".to_string()));
        if !cfg.no_subdir {
            let base = sanitize(stream_name);
            let base = if base.is_empty() {
                "Streamripper_rips".to_string()
            } else {
                base
            };
            root.push(base);
        }
        let incomplete = root.join("incomplete");
        fs::create_dir_all(&incomplete)?;

        let (show, show_path) = if cfg.single_file {
            let name = match &cfg.single_file_name {
                Some(n) if n.contains('.') => n.clone(),
                Some(n) => format!("{n}.{ext}"),
                None => format!("sr_{}.{ext}", now_stamp()),
            };
            let path = root.join(sanitize(&name));
            eprintln!("[show file] {}", path.display());
            (Some(File::create(&path)?), Some(path))
        } else {
            (None, None)
        };

        Ok(Recorder {
            root,
            incomplete,
            ext: ext.to_string(),
            mp3like: matches!(ext, "mp3" | "aac"),
            no_individual: cfg.no_individual,
            overwrite: cfg.overwrite,
            pattern: cfg.pattern.clone(),
            seq: cfg.seq,
            keep_incomplete: cfg.keep_incomplete.unwrap_or(0),
            no_id3: cfg.no_id3,
            with_id3v1: cfg.with_id3v1,
            without_id3v2: cfg.without_id3v2,
            show,
            show_path,
            show_bytes: 0,
            bitrate_kbps: bitrate_kbps.max(1),
            no_cue: cfg.no_cue,
            cue_entries: Vec::new(),
            cur: None,
            completed: 0,
            seen: 0,
            kept: 0,
        })
    }

    /// New `StreamTitle`: finalize the previous track and start a new one.
    pub fn on_title(&mut self, title: &str) -> io::Result<()> {
        let changed = match &self.cur {
            Some(t) => t.title != title,
            None => true,
        };
        if !changed {
            return Ok(());
        }
        if let Some(prev) = self.cur.take() {
            self.finalize(prev)?;
        }
        self.seen += 1;
        let proper_start = self.seen > 1;

        // Cue entry for the single-file show: this track starts at the current
        // position in the show file.
        if self.show.is_some() && !self.no_cue && !title.is_empty() {
            let sec = self.show_bytes * 8 / (self.bitrate_kbps as u64 * 1000);
            self.cue_entries.push((title.to_string(), sec));
        }

        if self.no_individual {
            // still track boundaries (for the show file / cue sheet)
            self.cur = Some(Track {
                title: title.to_string(),
                path: PathBuf::new(),
                file: None,
                proper_start,
                bytes: 0,
            });
        } else {
            let base = sanitize(title);
            let base = if base.is_empty() {
                format!("track_{:03}", self.seen)
            } else {
                base
            };
            let path = self.incomplete.join(format!("{base}.{}", self.ext));
            let mut f = File::create(&path)?;
            let mut bytes = 0u64;
            // Prepend ID3v2.3 to mp3/aac tracks (default on).
            if self.mp3like && !self.no_id3 && !self.without_id3v2 {
                let (artist, song) = id3::split_artist_title(title);
                let tag = id3::id3v2(artist, song);
                f.write_all(&tag)?;
                bytes += tag.len() as u64;
            }
            self.cur = Some(Track {
                title: title.to_string(),
                path,
                file: Some(f),
                proper_start,
                bytes,
            });
        }
        Ok(())
    }

    pub fn write_audio(&mut self, data: &[u8]) -> io::Result<()> {
        if let Some(show) = &mut self.show {
            show.write_all(data)?;
            self.show_bytes += data.len() as u64;
        }
        if !self.no_individual {
            if self.cur.is_none() {
                self.on_title("")?;
            }
            if let Some(t) = &mut self.cur {
                if let Some(f) = &mut t.file {
                    f.write_all(data)?;
                    t.bytes += data.len() as u64;
                }
            }
        }
        Ok(())
    }

    /// At shutdown: the in-progress track stays in `incomplete/`.
    pub fn finish(&mut self) -> io::Result<()> {
        if let Some(show) = &mut self.show {
            show.flush()?;
        }
        if let Some(t) = &mut self.cur {
            if let Some(f) = &mut t.file {
                f.flush()?;
            }
        }
        // Cue sheet for the single-file show.
        if !self.no_cue && !self.cue_entries.is_empty() {
            if let Some(show_path) = &self.show_path {
                let show_name = show_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("show")
                    .to_string();
                let cue_path = show_path.with_extension("cue");
                fs::write(&cue_path, cue::render(&show_name, &self.cue_entries))?;
                eprintln!("[cue] {}", cue_path.display());
            }
        }
        Ok(())
    }

    fn finalize(&mut self, mut prev: Track) -> io::Result<()> {
        // Append ID3v1 to mp3/aac tracks before closing, if requested.
        if self.mp3like && !self.no_id3 && self.with_id3v1 {
            if let Some(f) = &mut prev.file {
                let (artist, song) = id3::split_artist_title(&prev.title);
                f.write_all(&id3::id3v1(artist, song))?;
            }
        }
        drop(prev.file.take()); // close the incomplete file

        if self.no_individual || prev.path.as_os_str().is_empty() {
            return Ok(());
        }
        // The first (partial) track, and the first `keep_incomplete` tracks, stay.
        if !prev.proper_start {
            return Ok(());
        }
        if self.kept < self.keep_incomplete {
            self.kept += 1;
            return Ok(());
        }

        let seqno = self.seq.map(|start| start + self.completed);
        let base = self.dest_basename(&prev.title, seqno);
        let dest = self.root.join(format!("{base}.{}", self.ext));
        match self.resolve_overwrite(&dest, prev.bytes)? {
            Some(final_dest) => {
                move_file(&prev.path, &final_dest)?;
                self.completed += 1;
                eprintln!("[track {:>3}] {}", self.completed, final_dest.display());
            }
            None => {
                let _ = fs::remove_file(&prev.path);
            }
        }
        Ok(())
    }

    fn dest_basename(&self, title: &str, seqno: Option<u32>) -> String {
        let mut base = match &self.pattern {
            Some(p) => apply_pattern(p, title, seqno.unwrap_or(0)),
            None => sanitize(title),
        };
        if base.is_empty() {
            base = format!("track_{:03}", self.completed + 1);
        }
        if self.pattern.is_none() {
            if let Some(n) = seqno {
                base = format!("{n:04}_{base}");
            }
        }
        base
    }

    fn resolve_overwrite(&self, dest: &Path, new_bytes: u64) -> io::Result<Option<PathBuf>> {
        if !dest.exists() {
            return Ok(Some(dest.to_path_buf()));
        }
        match self.overwrite {
            Overwrite::Always => Ok(Some(dest.to_path_buf())),
            Overwrite::Never => Ok(None),
            Overwrite::Larger => {
                let existing = fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
                Ok(if new_bytes > existing {
                    Some(dest.to_path_buf())
                } else {
                    None
                })
            }
            Overwrite::Version => {
                let stem = dest.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
                let ext = dest
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&self.ext);
                for n in 1..10_000 {
                    let cand = self.root.join(format!("{stem} ({n}).{ext}"));
                    if !cand.exists() {
                        return Ok(Some(cand));
                    }
                }
                Ok(Some(dest.to_path_buf()))
            }
        }
    }
}

/// Move within the same tree; fall back to copy+remove across filesystems.
fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    fs::copy(from, to)?;
    fs::remove_file(from)
}

/// Pattern tokens: %A artist, %T title, %q sequence, %% literal percent.
/// The title is split into artist/title on the first " - ".
fn apply_pattern(pattern: &str, title: &str, seqno: u32) -> String {
    let (artist, song) = match title.split_once(" - ") {
        Some((a, t)) => (a.trim(), t.trim()),
        None => ("", title.trim()),
    };
    let mut out = String::new();
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('A') => out.push_str(artist),
            Some('T') => out.push_str(song),
            Some('q') => out.push_str(&format!("{seqno:04}")),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    sanitize(&out)
}
