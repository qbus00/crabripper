//! Cue-sheet rendering for single-file (`-a`) recordings.

use crate::id3::split_artist_title;

/// Render a `.cue` sheet. `entries` is (title, start_seconds) in order.
pub fn render(show_filename: &str, entries: &[(String, u64)]) -> String {
    let mut s = format!("FILE \"{show_filename}\" MP3\n");
    for (i, (title, sec)) in entries.iter().enumerate() {
        let (artist, song) = split_artist_title(title);
        s.push_str(&format!("  TRACK {:02} AUDIO\n", i + 1));
        s.push_str(&format!("    TITLE \"{song}\"\n"));
        s.push_str(&format!("    PERFORMER \"{artist}\"\n"));
        // INDEX is MM:SS:FF (FF = frames, 75/sec); we align to whole seconds.
        s.push_str(&format!(
            "    INDEX 01 {:02}:{:02}:00\n",
            sec / 60,
            sec % 60
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_tracks() {
        let e = vec![("A - B".to_string(), 0), ("C - D".to_string(), 125)];
        let c = render("show.mp3", &e);
        assert!(c.starts_with("FILE \"show.mp3\" MP3\n"));
        assert!(c.contains("TRACK 01 AUDIO"));
        assert!(c.contains("TITLE \"B\""));
        assert!(c.contains("PERFORMER \"C\"")); // artist of track 2
        assert!(c.contains("INDEX 01 02:05:00"));
    }
}
