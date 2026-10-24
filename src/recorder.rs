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
//!
//! Audio always goes to disk as it arrives; memory use is bounded. The
//! silence splitpoint keeps only a short tail of the current track in memory:
//! when the cut lands before the metadata boundary, the overshoot is truncated
//! from the finished track's file and carried into the next one.

use crate::cli::{Config, Overwrite, Xs};
use crate::util::{now_stamp, printable, sanitize, truncate_utf8};
use crate::{cue, decode, frames, id3, split};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Longest filename we generate: 255-byte NAME_MAX minus room for a
/// " (9999)" de-dup suffix.
const NAME_BUDGET: usize = 255 - 8;
/// Audio held while waiting for the first title, so it can join that track.
/// Must exceed the largest metadata interval we accept.
const PREAMBLE_MAX: usize = 2 * 1024 * 1024;
/// Extra bytes kept in the silence tail so the decoder can find frame sync.
const SYNC_MARGIN: usize = 32 * 1024;
/// The cue format allows 99 tracks; titles are bounded so a title flood can't
/// grow the sheet (it is rewritten on every entry).
const MAX_CUE_TRACKS: usize = 99;
const MAX_CUE_TITLE: usize = 255;
/// Stream tail kept to find the unfinished last frame at finish (well above
/// the largest mp3/ADTS frame).
const TAIL_MAX: usize = 32 * 1024;

struct Track {
    title: String,
    path: PathBuf,        // in incomplete/ (the .wav path in --wav mode)
    src: Option<PathBuf>, // --wav: encoded bytes on disk, decoded at finalize
    file: Option<File>,
    proper_start: bool,
    bytes: u64, // bytes in `file` (tag + audio)
    audio: u64, // stream bytes in `file` (no tags)
}

pub struct Recorder {
    root: PathBuf,
    incomplete: PathBuf,
    ext: String,
    mp3like: bool,
    wav: bool,
    no_split: bool,
    no_individual: bool,
    overwrite: Overwrite,
    pattern: Option<String>,
    seq: Option<u32>,
    keep_incomplete: u32,

    no_id3: bool,
    with_id3v1: bool,
    without_id3v2: bool,
    codeset_id3: Option<String>,
    codeset_filesys: Option<String>,

    // crabripper extensions
    truncate_dup: bool,
    log_path: Option<PathBuf>,
    split_time: Option<Duration>,
    split_size: Option<u64>,
    cur_start: Instant,

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
    titled: bool,      // a real (non-empty) title has been seen
    preamble: Vec<u8>, // audio before the first title (bounded)

    // silence-based splitpoint (mp3/aac, individual tracks, --xs != none)
    silence: bool,
    xs_pre: usize,
    xs_post: usize,
    xs_offset_bytes: isize,
    hold: usize,   // silence tail size kept in memory
    ring: Vec<u8>, // tail of the current track's audio

    tail: Vec<u8>, // last stream bytes (mp3/aac), to trim an unfinished frame
}

impl Recorder {
    pub fn new(
        cfg: &Config,
        stream_name: &str,
        ext: &str,
        bitrate_kbps: u32,
        no_split: bool,
    ) -> io::Result<Self> {
        let mut root = PathBuf::from(cfg.output_dir.clone().unwrap_or_else(|| ".".to_string()));
        if !cfg.no_subdir {
            // The stream name comes from the server (icy-name).
            let base = sanitize(stream_name);
            let base = if base.is_empty() {
                "Crabripper_rips".to_string()
            } else {
                base
            };
            root.push(component(&base, "", None));
        }
        let incomplete = root.join("incomplete");
        fs::create_dir_all(&incomplete)?;
        let log_path = if cfg.log {
            Some(root.join("played.log"))
        } else {
            None
        };

        // --wav decodes to 16-bit PCM WAV (symphonia: mp3/aac/ogg-vorbis/flac).
        let wav = cfg.wav && matches!(ext, "mp3" | "aac" | "ogg" | "oga" | "flac");
        if cfg.wav && !wav {
            eprintln!("  ! --wav: cannot decode this stream type; recording as-is");
        }

        // Silence-based splitpoint (mp3/aac — frame-contiguous), on by default.
        let silence = matches!(ext, "mp3" | "aac")
            && !cfg.no_individual
            && cfg.xs != Xs::None
            && !wav
            && !no_split;
        // icy-br is server-supplied: clamp it before it sizes anything.
        let bitrate_kbps = bitrate_kbps.clamp(8, 2000);
        let bpms = (bitrate_kbps as usize / 8).max(1); // bytes per millisecond
        let ms = |v: i32| (v.clamp(1, 30_000)) as usize;
        let (pre_ms, post_ms) = cfg.xs_search_window;
        let pre_ms = if pre_ms > 0 { ms(pre_ms) } else { 1500 };
        let post_ms = if post_ms > 0 { ms(post_ms) } else { 1500 };
        let xs_pre = pre_ms * bpms;
        let xs_offset_bytes = cfg.xs_offset.clamp(-30_000, 30_000) as isize * bpms as isize;
        let hold = xs_pre + xs_offset_bytes.min(0).unsigned_abs() + SYNC_MARGIN;

        let (show, show_path) = if cfg.single_file {
            let name = match &cfg.single_file_name {
                // Append the stream's extension unless the name already ends with
                // exactly that extension (so `-a show.aac` on an AAC stream stays
                // `show.aac`, but `-a Krakow.2026-10-24` becomes `...mp3`). Matching
                // on any `.` would wrongly treat a dotted date as an extension.
                Some(n) if ends_with_ext(n, ext) => n.clone(),
                Some(n) => format!("{n}.{ext}"),
                None => format!("sr_{}.{ext}", now_stamp()),
            };
            let path = root.join(fs_component(
                sanitize(&name),
                cfg.codeset_filesys.as_deref(),
            ));
            eprintln!("[show file] {}", printable(&path.display().to_string()));
            (Some(create_show(&path)?), Some(path))
        } else {
            (None, None)
        };

        Ok(Recorder {
            root,
            incomplete,
            ext: ext.to_string(),
            mp3like: matches!(ext, "mp3" | "aac"),
            wav,
            no_split,
            no_individual: cfg.no_individual,
            overwrite: cfg.overwrite,
            pattern: cfg.pattern.clone(),
            seq: cfg.seq,
            keep_incomplete: cfg.keep_incomplete.unwrap_or(0),
            no_id3: cfg.no_id3,
            with_id3v1: cfg.with_id3v1,
            without_id3v2: cfg.without_id3v2,
            codeset_id3: cfg.codeset_id3.clone(),
            codeset_filesys: cfg.codeset_filesys.clone(),
            truncate_dup: cfg.truncate_dup,
            log_path,
            split_time: cfg.split_time.map(Duration::from_secs),
            split_size: cfg.split_size.map(|mb| mb.saturating_mul(1024 * 1024)),
            cur_start: Instant::now(),
            show,
            show_path,
            show_bytes: 0,
            bitrate_kbps,
            no_cue: cfg.no_cue || !matches!(ext, "mp3" | "aac"),
            cue_entries: Vec::new(),
            cur: None,
            completed: 0,
            seen: 0,
            kept: 0,
            titled: false,
            preamble: Vec::new(),
            silence,
            xs_pre,
            xs_post: post_ms * bpms,
            xs_offset_bytes,
            hold,
            ring: Vec::new(),
            tail: Vec::new(),
        })
    }

    /// New `StreamTitle`: finalize the previous track and start a new one.
    pub fn on_title(&mut self, title: &str) -> io::Result<()> {
        // Whole-capture formats (FLAC) are never split: keep the single track.
        if self.no_split && self.cur.is_some() {
            return Ok(());
        }
        // Servers re-send the current title (periodically, and on every
        // reconnect): that is not a track change.
        if self.cur.as_ref().is_some_and(|t| t.title == title) {
            return Ok(());
        }
        self.roll(title, self.silence)
    }

    /// Finalize the current track and start a new one unconditionally. Used by
    /// the Ogg path (every logical-stream boundary is a real new track, even if
    /// two consecutive tracks share a title).
    pub fn roll_track(&mut self, title: &str) -> io::Result<()> {
        self.roll(title, false)
    }

    /// Start a new track. With `search`, the cut is moved to the quietest frame
    /// near the boundary (silence splitpoint).
    fn roll(&mut self, title: &str, search: bool) -> io::Result<()> {
        self.note_title(title);
        let carry = if search {
            self.silence_cut()?
        } else {
            Vec::new()
        };
        // A track starts "properly" (i.e. can be complete) only at a boundary we
        // witnessed: not the stream's first title — that song was already
        // playing when we joined — and not the initial untitled audio.
        let proper_start = if title.is_empty() {
            self.cur.is_some()
        } else {
            self.titled
        };
        if !title.is_empty() {
            self.titled = true;
        }
        if let Some(prev) = self.cur.take() {
            self.finalize(prev)?;
        }
        self.seen += 1;

        // Cue entry for the single-file show: this track starts at the current
        // position in the show file (minus any audio carried back by the cut).
        if self.show.is_some()
            && !self.no_cue
            && !title.is_empty()
            && self.cue_entries.len() < MAX_CUE_TRACKS
        {
            let pos = self.show_bytes.saturating_sub(carry.len() as u64);
            let sec = pos * 8 / (self.bitrate_kbps as u64 * 1000);
            let t = truncate_utf8(title, MAX_CUE_TITLE).to_string();
            self.cue_entries.push((t, sec));
            self.write_cue(false)?;
        }

        self.ring.clear();
        if self.no_individual {
            // still track boundaries (for the show file / cue sheet)
            self.cur = Some(Track {
                title: title.to_string(),
                path: PathBuf::new(),
                src: None,
                file: None,
                proper_start,
                bytes: 0,
                audio: 0,
            });
            return Ok(());
        }

        let base = sanitize(title);
        let fallback = format!("track_{:03}", self.seen);
        let base = if base.is_empty() {
            fallback.clone()
        } else {
            base
        };
        // A track file that cannot be created (odd filesystem limits, a stale
        // file) costs that track's file, never the whole recording.
        let made = self.new_track_file(title, &base).or_else(|e| {
            eprintln!("  ! cannot create track file ({e}); using {fallback}");
            self.new_track_file(title, &fallback)
        });
        let (path, file, src, bytes) = match made {
            Ok((path, f, src, bytes)) => (path, Some(f), src, bytes),
            Err(e) => {
                eprintln!("  ! cannot create track file ({e}); this track is not saved");
                (PathBuf::new(), None, None, 0)
            }
        };
        self.cur = Some(Track {
            title: title.to_string(),
            path,
            src,
            file,
            proper_start,
            bytes,
            audio: 0,
        });
        if !carry.is_empty() {
            self.write_track(&carry)?;
        }
        if !self.preamble.is_empty() {
            let pre = std::mem::take(&mut self.preamble);
            self.write_track(&pre)?;
        }
        Ok(())
    }

    /// Create the incomplete/ file for a new track: `(path, file, wav part
    /// path, bytes written)`. In --wav mode the encoded audio goes to a `.part`
    /// file that is decoded to `path` at finalize.
    fn new_track_file(
        &self,
        title: &str,
        base: &str,
    ) -> io::Result<(PathBuf, File, Option<PathBuf>, u64)> {
        let path = self.dedup_path(self.incomplete.join(component(
            base,
            self.track_ext(),
            self.codeset_filesys.as_deref(),
        )));
        if self.wav {
            let src = self.part_path(&path);
            let f = self.open_track(&src)?;
            return Ok((path, f, Some(src), 0));
        }
        let mut f = self.open_track(&path)?;
        let mut bytes = 0u64;
        // Prepend ID3v2.3 to mp3/aac tracks (default on; never for wav).
        if self.mp3like && !self.no_id3 && !self.without_id3v2 {
            let (artist, song) = id3::split_artist_title(title);
            let tag = id3::id3v2(artist, song, self.codeset_id3.as_deref());
            f.write_all(&tag)?;
            bytes += tag.len() as u64;
        }
        Ok((path, f, None, bytes))
    }

    fn part_path(&self, wav_path: &Path) -> PathBuf {
        wav_path.with_extension(format!("{}.part", self.ext))
    }

    /// Is this incomplete/ name in use (including, for --wav, its `.part`)?
    /// `symlink_metadata` so a dangling symlink also counts as taken.
    fn taken(&self, p: &Path) -> bool {
        fs::symlink_metadata(p).is_ok()
            || (self.wav && fs::symlink_metadata(self.part_path(p)).is_ok())
    }

    pub fn write_audio(&mut self, data: &[u8]) -> io::Result<()> {
        if self.mp3like {
            self.tail.extend_from_slice(data);
            if self.tail.len() > 2 * TAIL_MAX {
                let excess = self.tail.len() - TAIL_MAX;
                self.tail.drain(..excess);
            }
        }
        if let Some(show) = &mut self.show {
            show.write_all(data)?;
            self.show_bytes += data.len() as u64;
        }
        if self.no_individual {
            return Ok(());
        }
        if self.cur.is_none() {
            // Before the first title: hold the audio so it can join the first
            // (partial) track; give up waiting once it grows too large.
            self.preamble.extend_from_slice(data);
            if self.preamble.len() > PREAMBLE_MAX {
                self.roll("", false)?;
            }
            return Ok(());
        }
        self.write_track(data)?;
        // --split-time / --split-size: force a new track (not for ogg/wav,
        // which have their own boundaries).
        let cur_bytes = self.cur.as_ref().map_or(0, |t| t.bytes);
        if !self.wav && self.ext != "ogg" && self.should_split(cur_bytes) {
            self.roll("", false)?;
        }
        Ok(())
    }

    /// Append to the current track's file (and the silence tail).
    fn write_track(&mut self, data: &[u8]) -> io::Result<()> {
        if let Some(t) = &mut self.cur {
            if let Some(f) = &mut t.file {
                f.write_all(data)?;
                t.bytes += data.len() as u64;
                t.audio += data.len() as u64;
            }
        }
        if self.silence {
            self.ring.extend_from_slice(data);
            if self.ring.len() > 2 * self.hold {
                let excess = self.ring.len() - self.hold;
                self.ring.drain(..excess);
            }
        }
        Ok(())
    }

    /// Silence splitpoint: find the quietest frame near the end of the current
    /// track, truncate the current file there, and return the bytes after the
    /// cut (which belong to the next track).
    fn silence_cut(&mut self) -> io::Result<Vec<u8>> {
        let Some(f) = self.cur.as_mut().and_then(|t| t.file.as_mut()) else {
            return Ok(Vec::new());
        };
        let len = self.ring.len();
        if len == 0 {
            return Ok(Vec::new());
        }
        let boundary = (len as isize + self.xs_offset_bytes).clamp(0, len as isize) as usize;
        let cut =
            split::find_split(&self.ring, boundary, self.xs_pre, self.xs_post, &self.ext).min(len);
        let carry = self.ring[cut..].to_vec();
        if !carry.is_empty() {
            let keep = f.metadata()?.len().saturating_sub(carry.len() as u64);
            f.set_len(keep)?;
            f.seek(SeekFrom::End(0))?;
            if let Some(t) = &mut self.cur {
                t.bytes = keep;
            }
        }
        Ok(carry)
    }

    fn track_ext(&self) -> &str {
        if self.wav {
            "wav"
        } else {
            &self.ext
        }
    }

    /// At shutdown: the in-progress track stays in `incomplete/`.
    pub fn finish(&mut self) -> io::Result<()> {
        if let Some(show) = &mut self.show {
            show.flush()?;
        }
        if !self.no_individual && self.cur.is_none() && !self.preamble.is_empty() {
            self.roll("", false)?;
        }
        // A stop (-l, Ctrl-C, end of stream) rarely lands on a frame boundary:
        // drop the unfinished last frame so decoders don't hit a broken one.
        let cut = if self.mp3like {
            frames::partial_tail(&self.tail, &self.ext) as u64
        } else {
            0
        };
        if cut > 0 {
            if let Some(show) = &mut self.show {
                self.show_bytes -= cut; // the tail is part of what it holds
                show.set_len(self.show_bytes)?;
            }
            if let Some(t) = self.cur.as_mut().filter(|t| t.audio >= cut) {
                if let Some(f) = &mut t.file {
                    t.bytes -= cut;
                    t.audio -= cut;
                    f.set_len(t.bytes)?;
                }
            }
        }
        if let Some(t) = &mut self.cur {
            if let Some(f) = &mut t.file {
                f.flush()?;
            }
            // --wav: decode the in-progress last track (it stays in incomplete/).
            if let Some(src) = t.src.take() {
                drop(t.file.take());
                wav_from_part(&src, &t.path, &self.ext);
            }
        }
        self.write_cue(true)
    }

    fn finalize(&mut self, mut prev: Track) -> io::Result<()> {
        if let Some(src) = prev.src.take() {
            drop(prev.file.take());
            match wav_from_part(&src, &prev.path, &self.ext) {
                Some(n) => prev.bytes = n,
                None => return Ok(()), // decode failed: encoded file kept in incomplete/
            }
        } else if self.mp3like && !self.no_id3 && self.with_id3v1 {
            // Append ID3v1 to mp3/aac tracks before closing, if requested.
            if let Some(f) = &mut prev.file {
                let (artist, song) = id3::split_artist_title(&prev.title);
                f.write_all(&id3::id3v1(artist, song, self.codeset_id3.as_deref()))?;
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
        self.move_completed(&prev.path, &prev.title, prev.bytes)
    }

    /// Move a finalized track from `incomplete/` to the destination, applying the
    /// name pattern/sequence and overwrite mode.
    fn move_completed(&mut self, from: &Path, title: &str, bytes: u64) -> io::Result<()> {
        let seqno = self.seq.map(|start| start.saturating_add(self.completed));
        let base = self.dest_basename(title, seqno);
        let mut dest = self.root.join(component(
            &base,
            self.track_ext(),
            self.codeset_filesys.as_deref(),
        ));
        // A track named like the show being recorded (or its cue sheet) must
        // never replace it, whatever -o says.
        if self.is_show_file(&dest) {
            dest = self.root.join(component(
                &format!("{base} (track)"),
                self.track_ext(),
                self.codeset_filesys.as_deref(),
            ));
        }
        let outcome = self.resolve_overwrite(&dest, bytes).and_then(|d| match d {
            Some(final_dest) => move_file(from, &final_dest).map(|_| Some(final_dest)),
            None => Ok(None),
        });
        match outcome {
            Ok(Some(final_dest)) => {
                self.completed += 1;
                eprintln!(
                    "[track {:>3}] {}",
                    self.completed,
                    printable(&final_dest.display().to_string())
                );
            }
            Ok(None) => {
                let _ = fs::remove_file(from);
            }
            Err(e) => eprintln!(
                "  ! could not complete {} ({e}); left in incomplete/",
                printable(&from.display().to_string())
            ),
        }
        Ok(())
    }

    fn is_show_file(&self, p: &Path) -> bool {
        self.show_path
            .as_ref()
            .is_some_and(|s| p == s || p == s.with_extension("cue"))
    }

    /// Rewrite the cue sheet for the single-file show (called as entries are
    /// added, so it survives an interrupted recording).
    fn write_cue(&self, announce: bool) -> io::Result<()> {
        if self.no_cue || self.cue_entries.is_empty() {
            return Ok(());
        }
        if let Some(show_path) = &self.show_path {
            let show_name = show_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("show")
                .to_string();
            let cue_path = show_path.with_extension("cue");
            fs::write(&cue_path, cue::render(&show_name, &self.cue_entries))?;
            if announce {
                eprintln!("[cue] {}", printable(&cue_path.display().to_string()));
            }
        }
        Ok(())
    }

    // ---- crabripper extensions ----

    /// Reset the per-track clock and, with --log, append the title to played.log.
    fn note_title(&mut self, title: &str) {
        self.cur_start = Instant::now();
        if let Some(p) = &self.log_path {
            if !title.is_empty() {
                if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p) {
                    // one line per title: no embedded newlines from the stream
                    let _ = writeln!(f, "{}  {}", now_stamp(), printable(title));
                }
            }
        }
    }

    /// Without -T, avoid clobbering an existing incomplete file by versioning it.
    fn dedup_path(&self, path: PathBuf) -> PathBuf {
        if self.truncate_dup || !self.taken(&path) {
            return path;
        }
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        for n in 1..10_000 {
            let name = if ext.is_empty() {
                format!("{stem} ({n})")
            } else {
                format!("{stem} ({n}).{ext}")
            };
            let cand = dir.join(name);
            if !self.taken(&cand) {
                return cand;
            }
        }
        path
    }

    /// Create a track file in incomplete/: never follow a symlink. Without -T
    /// the name was de-duplicated, so require it to be new (atomic O_EXCL).
    fn open_track(&self, path: &Path) -> io::Result<File> {
        if self.truncate_dup {
            refuse_symlink(path)?;
            File::create(path)
        } else {
            OpenOptions::new().write(true).create_new(true).open(path)
        }
    }

    /// --split-time / --split-size: has the current track hit a forced boundary?
    fn should_split(&self, cur_bytes: u64) -> bool {
        self.split_time
            .is_some_and(|d| self.cur_start.elapsed() >= d)
            || self.split_size.is_some_and(|sz| cur_bytes >= sz)
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
                    let cand = self.root.join(fs_component(
                        format!("{stem} ({n}).{ext}"),
                        self.codeset_filesys.as_deref(),
                    ));
                    if !cand.exists() && !self.is_show_file(&cand) {
                        return Ok(Some(cand));
                    }
                }
                // never fall back to overwriting the original
                Err(io::Error::other("no free versioned name"))
            }
        }
    }
}

/// Decode an encoded `.part` file to the WAV at `dst`, removing the part on
/// success. On failure the encoded file is kept (and reported). Returns the
/// WAV file size.
fn wav_from_part(src: &Path, dst: &Path, ext: &str) -> Option<u64> {
    match decode::decode_to_wav(src, dst, ext) {
        Ok(n) => {
            let _ = fs::remove_file(src);
            Some(n + 44)
        }
        Err(e) => {
            eprintln!(
                "  ! --wav: could not decode {} ({e}); kept the encoded file",
                printable(&src.display().to_string())
            );
            let _ = fs::remove_file(dst);
            None
        }
    }
}

fn refuse_symlink(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(io::Error::other(format!(
            "refusing to write through a symlink: {}",
            path.display()
        )));
    }
    Ok(())
}

/// The single-file show is overwritten if it exists, but never via a symlink.
fn create_show(path: &Path) -> io::Result<File> {
    refuse_symlink(path)?;
    File::create(path)
}

/// Does `name` already end with `.<ext>` (case-insensitive)? Used to avoid
/// double-suffixing a single-file (`-a`) name that already carries its extension.
fn ends_with_ext(name: &str, ext: &str) -> bool {
    name.len() > ext.len()
        && name.as_bytes()[name.len() - ext.len() - 1] == b'.'
        && name[name.len() - ext.len()..].eq_ignore_ascii_case(ext)
}

/// Build a `base.ext` filename component that fits NAME_MAX (with room for a
/// de-dup suffix) after encoding in the filesystem codeset. Titles come from
/// the stream and can be several KB; the base is cut on a char boundary.
fn component(base: &str, ext: &str, codeset: Option<&str>) -> OsString {
    let suffix = if ext.is_empty() {
        String::new()
    } else {
        format!(".{ext}")
    };
    let budget = NAME_BUDGET.saturating_sub(suffix.len());
    let mut b = truncate_utf8(base, budget).to_string();
    let encoded_len = |s: &str| match codeset {
        Some(cs) => crate::codeset::encode(s, Some(cs)).len(),
        None => s.len(),
    };
    while encoded_len(&b) > budget {
        b.pop();
    }
    let b = b.trim_end();
    // Truncation + trimming can re-create "." / ".." (e.g. ".." + spaces + "x"):
    // never return a name that resolves to a directory.
    let b = if b.chars().all(|c| c == '.') { "_" } else { b };
    fs_component(format!("{b}{suffix}"), codeset)
}

/// Encode a filename component in the filesystem codeset (`--codeset-filesys`).
/// Without a codeset (or off Unix) the UTF-8 name is used as-is.
fn fs_component(name: String, codeset: Option<&str>) -> OsString {
    match codeset {
        Some(cs) => {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                let bytes = crate::codeset::encode(&name, Some(cs));
                // Stateful encodings (ISO-2022-*) can emit '/' or NUL bytes;
                // such a name would be a path, not a component: keep UTF-8.
                if bytes.iter().any(|&b| b == b'/' || b == 0) {
                    return OsString::from(name);
                }
                std::ffi::OsStr::from_bytes(&bytes).to_os_string()
            }
            #[cfg(not(unix))]
            {
                OsString::from(name)
            }
        }
        None => OsString::from(name),
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

#[cfg(test)]
mod tests {
    use super::*;

    const MP3: &[u8] = include_bytes!("../testdata/sine.mp3");

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("crab_rec_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg(dir: &Path) -> Config {
        Config {
            output_dir: Some(dir.to_string_lossy().into_owned()),
            no_subdir: true,
            ..Config::default()
        }
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn single_file_extension_rules() {
        // dotted date must still get the stream extension appended
        assert!(!ends_with_ext("Krakow.2026-10-24_Sat_113130", "mp3"));
        // name already carrying the right extension is left alone
        assert!(ends_with_ext("Trojka.2026-10-24.aac", "aac"));
        assert!(ends_with_ext("show.MP3", "mp3")); // case-insensitive
        assert!(!ends_with_ext("show.aac", "mp3")); // wrong extension -> append
        assert!(!ends_with_ext("mp3", "mp3")); // no dot
    }

    #[test]
    fn long_titles_fit_name_max() {
        let title = "Zespół Pieśni i Tańca - ".to_string() + &"Żółć gęślą jaźń ".repeat(40);
        let c = component(&sanitize(&title), "mp3", None);
        assert!(c.len() <= NAME_BUDGET, "{} bytes", c.len());
        assert!(c.to_string_lossy().ends_with(".mp3"));
        let c = component(&title, "mp3", Some("cp1250"));
        assert!(c.len() <= NAME_BUDGET);
    }

    /// Default (silence-split) mode: a re-sent identical title must not cut
    /// the track, and the audio before the first title joins that track.
    #[test]
    fn repeated_title_does_not_split() {
        let dir = tmpdir("repeat");
        let mut r = Recorder::new(&cfg(&dir), "s", "mp3", 64, false).unwrap();
        assert!(r.silence);
        r.write_audio(&MP3[..4000]).unwrap(); // before any title
        for chunk in MP3[4000..].chunks(2000) {
            r.on_title("Band - Song").unwrap();
            r.write_audio(chunk).unwrap();
        }
        r.finish().unwrap();
        let inc = files(&dir.join("incomplete"));
        assert_eq!(inc, vec!["Band - Song.mp3"], "{inc:?}");
        let size = fs::metadata(dir.join("incomplete/Band - Song.mp3"))
            .unwrap()
            .len();
        assert!(
            size >= MP3.len() as u64,
            "lost audio: {size} < {}",
            MP3.len()
        );
        assert!(files(&dir).is_empty()); // first track is partial: not moved
        fs::remove_dir_all(&dir).ok();
    }

    /// The silence cut moves audio between tracks but never loses or
    /// duplicates any of it, and every byte is on disk without finish().
    #[test]
    fn silence_cut_conserves_audio() {
        let dir = tmpdir("cut");
        let mut r = Recorder::new(&cfg(&dir), "s", "mp3", 64, false).unwrap();
        r.no_id3 = true;
        let mut fed = 0;
        for (i, chunk) in MP3.chunks(3000).enumerate() {
            if i % 2 == 0 {
                r.on_title(&format!("A - T{i}")).unwrap();
            }
            r.write_audio(chunk).unwrap();
            fed += chunk.len();
        }
        // no finish(): data must already be on disk
        let total: u64 = [dir.clone(), dir.join("incomplete")]
            .iter()
            .flat_map(|d| fs::read_dir(d).unwrap())
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert_eq!(total, fed as u64);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dotdot_stream_name_stays_inside_output_dir() {
        let dir = tmpdir("dotdot");
        let c = Config {
            no_subdir: false,
            ..cfg(&dir)
        };
        let r = Recorder::new(&c, "..", "mp3", 128, false).unwrap();
        assert_eq!(r.root, dir.join("Crabripper_rips"));
        // ".." padded so length-truncation + trimming leaves ".." again
        for pad in [" ", "\u{3000}"] {
            let name = format!("..{}x", pad.repeat(400));
            let r = Recorder::new(&c, &name, "mp3", 128, false).unwrap();
            assert!(r.root.starts_with(&dir) && r.root != dir, "{:?}", r.root);
            assert!(r
                .root
                .components()
                .all(|c| !matches!(c, std::path::Component::ParentDir)));
        }
        fs::remove_dir_all(&dir).ok();
    }

    /// Stopping mid-frame leaves no unfinished frame at the end of the show
    /// or of the in-progress track.
    #[test]
    fn finish_drops_unfinished_frame() {
        let dir = tmpdir("tail");
        let c = Config {
            single_file: true,
            single_file_name: Some("Show".into()),
            ..cfg(&dir)
        };
        let mut r = Recorder::new(&c, "s", "mp3", 64, false).unwrap();
        r.no_id3 = true;
        let fr = frames::frames(MP3, "mp3");
        let (pos, len) = fr[fr.len() - 2];
        r.on_title("A - B").unwrap();
        r.write_audio(&MP3[..pos + len / 2]).unwrap();
        r.finish().unwrap();
        let size = |p: &str| fs::metadata(dir.join(p)).unwrap().len();
        assert_eq!(size("Show.mp3"), pos as u64);
        assert_eq!(size("incomplete/A - B.mp3"), pos as u64);
        fs::remove_dir_all(&dir).ok();
    }

    /// A stream title equal to the show's name must not replace the show.
    #[test]
    fn track_never_replaces_show_file() {
        let dir = tmpdir("show");
        let c = Config {
            single_file: true,
            single_file_name: Some("Show".into()),
            overwrite: Overwrite::Always,
            ..cfg(&dir)
        };
        let mut r = Recorder::new(&c, "s", "mp3", 64, false).unwrap();
        for t in ["A", "Show", "B"] {
            r.on_title(t).unwrap();
            r.write_audio(&MP3[..3000]).unwrap();
        }
        r.finish().unwrap();
        let show = fs::metadata(dir.join("Show.mp3")).unwrap().len();
        assert_eq!(show, 9000, "show file was replaced");
        assert!(dir.join("Show (track).mp3").exists());
        fs::remove_dir_all(&dir).ok();
    }
}
