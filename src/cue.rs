//! Cue-sheet rendering for single-file (`-a`) recordings.

use crate::id3::split_artist_title;

/// A cue string field: titles come from the stream, so strip anything that
/// could end the quoted value or the line (and thereby inject directives).
fn field(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '"' => '\'',
            c if (c as u32) < 0x20 || (0x7f..=0x9f).contains(&(c as u32)) => ' ',
            c => c,
        })
        .collect()
}

/// Render a `.cue` sheet: one FILE block per file (a show split by connection
/// drops has several), each with its (title, start_seconds) entries in order.
/// Track numbers run on across files.
pub fn render(files: &[(String, &[(String, u64)])]) -> String {
    let mut s = String::new();
    let mut i = 0;
    for (file, entries) in files.iter().filter(|(_, e)| !e.is_empty()) {
        s.push_str(&format!("FILE \"{}\" MP3\n", field(file)));
        for (title, sec) in entries.iter() {
            i += 1;
            push_track(&mut s, i, title, *sec);
        }
    }
    s
}

fn push_track(s: &mut String, n: usize, title: &str, sec: u64) {
    let (artist, song) = split_artist_title(title);
    s.push_str(&format!("  TRACK {:02} AUDIO\n", n));
    s.push_str(&format!("    TITLE \"{}\"\n", field(song)));
    s.push_str(&format!("    PERFORMER \"{}\"\n", field(artist)));
    // INDEX is MM:SS:FF (FF = frames, 75/sec); we align to whole seconds.
    s.push_str(&format!(
        "    INDEX 01 {:02}:{:02}:00\n",
        sec / 60,
        sec % 60
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_tracks() {
        let e = [("A - B".to_string(), 0), ("C - D".to_string(), 125)];
        let c = render(&[("show.mp3".to_string(), &e[..])]);
        assert!(c.starts_with("FILE \"show.mp3\" MP3\n"));
        assert!(c.contains("TRACK 01 AUDIO"));
        assert!(c.contains("TITLE \"B\""));
        assert!(c.contains("PERFORMER \"C\"")); // artist of track 2
        assert!(c.contains("INDEX 01 02:05:00"));
    }

    #[test]
    fn parts_continue_numbering() {
        let a = [("A - B".to_string(), 0)];
        let b = [("A - B".to_string(), 0), ("C - D".to_string(), 61)];
        let c = render(&[("s_c1.mp3".into(), &a[..]), ("s_c2.mp3".into(), &b[..])]);
        assert!(c.contains("FILE \"s_c2.mp3\" MP3\n  TRACK 02 AUDIO"), "{c}");
        assert!(c.contains("TRACK 03 AUDIO"));
        assert!(c.contains("INDEX 01 01:01:00"));
    }

    #[test]
    fn title_cannot_inject_directives() {
        let e = [("A - x\"\nFILE \"/etc/passwd\" MP3".to_string(), 0)];
        let c = render(&[("show.mp3".to_string(), &e[..])]);
        assert_eq!(c.lines().filter(|l| l.starts_with("FILE")).count(), 1);
    }
}
