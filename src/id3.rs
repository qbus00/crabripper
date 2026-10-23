//! Minimal ID3 tag writing — ID3v2.3 (prepended) and ID3v1 (appended).
//!
//! Text frames use UTF-16 (with BOM), the Unicode encoding ID3v2.3 supports, so
//! non-Latin metadata (e.g. Polish) survives. ID3v1 is Latin-1 only (its format),
//! so it falls back to '?' for characters it cannot represent.

/// Split a "StreamTitle" into (artist, title) on the first " - ".
pub fn split_artist_title(s: &str) -> (&str, &str) {
    match s.split_once(" - ") {
        Some((a, t)) => (a.trim(), t.trim()),
        None => ("", s.trim()),
    }
}

fn synchsafe(n: u32) -> [u8; 4] {
    [
        ((n >> 21) & 0x7f) as u8,
        ((n >> 14) & 0x7f) as u8,
        ((n >> 7) & 0x7f) as u8,
        (n & 0x7f) as u8,
    ]
}

fn text_frame(id: &[u8; 4], text: &str) -> Vec<u8> {
    // encoding 0x01 = UTF-16 with BOM
    let mut data = vec![0x01u8, 0xFF, 0xFE];
    for u in text.encode_utf16() {
        data.extend_from_slice(&u.to_le_bytes());
    }
    let mut f = Vec::with_capacity(10 + data.len());
    f.extend_from_slice(id);
    f.extend_from_slice(&(data.len() as u32).to_be_bytes()); // v2.3 frame size: plain u32
    f.extend_from_slice(&[0x00, 0x00]); // frame flags
    f.extend_from_slice(&data);
    f
}

/// Build an ID3v2.3 tag for prepending to an mp3/aac track.
pub fn id3v2(artist: &str, title: &str) -> Vec<u8> {
    let mut frames = Vec::new();
    if !title.is_empty() {
        frames.extend(text_frame(b"TIT2", title));
    }
    if !artist.is_empty() {
        frames.extend(text_frame(b"TPE1", artist));
    }
    let mut tag = Vec::with_capacity(10 + frames.len());
    tag.extend_from_slice(b"ID3");
    tag.extend_from_slice(&[0x03, 0x00, 0x00]); // v2.3.0, no flags
    tag.extend_from_slice(&synchsafe(frames.len() as u32));
    tag.extend_from_slice(&frames);
    tag
}

fn latin1_field(dst: &mut [u8], s: &str) {
    for (i, ch) in s.chars().take(dst.len()).enumerate() {
        let c = ch as u32;
        dst[i] = if c < 0x100 { c as u8 } else { b'?' };
    }
}

/// Build a 128-byte ID3v1 tag for appending.
pub fn id3v1(artist: &str, title: &str) -> [u8; 128] {
    let mut t = [0u8; 128];
    t[0..3].copy_from_slice(b"TAG");
    latin1_field(&mut t[3..33], title);
    latin1_field(&mut t[33..63], artist);
    t[127] = 255; // genre: unknown
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_header() {
        let t = id3v2("A", "B");
        assert_eq!(&t[0..3], b"ID3");
        assert_eq!(t[3], 0x03);
        // size is synchsafe: no byte has the high bit set
        assert!(t[6..10].iter().all(|&b| b < 0x80));
    }

    #[test]
    fn v1_is_128() {
        let t = id3v1("Artist", "Title");
        assert_eq!(&t[0..3], b"TAG");
        assert_eq!(t.len(), 128);
    }

    #[test]
    fn split() {
        assert_eq!(split_artist_title("A - B"), ("A", "B"));
        assert_eq!(split_artist_title("solo"), ("", "solo"));
    }
}
