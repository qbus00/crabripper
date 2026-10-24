//! Silence-based splitpoint search (mp3/aac): pick the quietest frame boundary
//! within a byte window around the metadata boundary, so track cuts land in a
//! low-energy spot instead of mid-note.
//!
//! This is a simpler model than the C fork's silence-length scan, but it uses the
//! same idea (decode a window, cut at the quietest frame). ICY metadata usually
//! lags the real song change, so the quiet point is typically just before the
//! boundary. `data` is a bounded tail of the stream that may start mid-frame.
//!
//! Byte positions come from our own frame parser; each decoded packet is
//! matched to its frame by length, and any disagreement abandons the search
//! (falling back to the metadata boundary) rather than risk a mid-frame cut.

use crate::{decode, frames};

/// Return the byte offset to cut at: the quietest frame whose start lies
/// within `[boundary - pre, boundary + post]`. Falls back to `boundary`.
pub fn find_split(data: &[u8], boundary: usize, pre: usize, post: usize, ext: &str) -> usize {
    let fallback = boundary.min(data.len());
    let Some(vols) = frame_peaks(data, ext) else {
        return fallback;
    };
    if vols.len() < 2 {
        return fallback;
    }
    let lo = boundary.saturating_sub(pre);
    let hi = boundary.saturating_add(post).min(data.len());

    let mut best: Option<(usize, i32)> = None;
    for &(off, peak) in &vols {
        if off >= lo && off <= hi {
            match best {
                Some((_, p)) if peak >= p => {}
                _ => best = Some((off, peak)),
            }
        }
    }
    best.map(|(o, _)| o).unwrap_or(fallback)
}

/// `(frame start offset in data, peak)` for each decoded frame, or `None` if
/// the decoder's packets don't line up with the parsed frames.
fn frame_peaks(data: &[u8], ext: &str) -> Option<Vec<(usize, i32)>> {
    let frames = frames::frames(data, ext);
    let start = frames.first()?.0;
    let packets = decode::packet_peaks(&data[start..], ext);
    if packets.len() > frames.len() {
        return None;
    }
    let mut out = Vec::with_capacity(packets.len());
    for (&(off, flen), (plen, peak)) in frames.iter().zip(packets) {
        if !frames::packet_matches(flen, plen, ext) {
            return None;
        }
        if let Some(p) = peak {
            out.push((off, p));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: [(&[u8], &str); 2] = [
        (include_bytes!("../testdata/sine.mp3"), "mp3"),
        (include_bytes!("../testdata/sine.aac"), "aac"),
    ];

    #[test]
    fn packets_align_with_frames() {
        for (data, ext) in FIXTURES {
            let v = frame_peaks(data, ext).expect(ext);
            assert!(v.len() > 20, "{ext}: {} frames", v.len());
        }
    }

    /// Cuts always land on a real frame start, even when the analysed tail
    /// begins mid-frame.
    #[test]
    fn cut_is_on_a_frame_start() {
        for (data, ext) in FIXTURES {
            let tail = &data[333..]; // start mid-frame
            let starts: Vec<usize> = frames::frames(tail, ext).iter().map(|f| f.0).collect();
            let cut = find_split(tail, tail.len() - 2000, 6000, 0, ext);
            assert!(starts.contains(&cut), "{ext}: cut {cut} not a frame start");
        }
    }
}
