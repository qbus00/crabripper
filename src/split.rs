//! Silence-based splitpoint search (mp3): pick the quietest mp3-frame boundary
//! within a byte window around the metadata boundary, so track cuts land in a
//! low-energy spot instead of mid-note.
//!
//! This is a simpler model than the C fork's silence-length scan, but it uses the
//! same idea (decode a window, cut at the quietest frame) and the same minimp3
//! decoder. ICY metadata usually lags the real song change, so the quiet point is
//! typically just before the boundary.

use crate::decode;

/// Return the byte offset to cut at: the quietest decoded frame whose start lies
/// within `[boundary - pre, boundary + post]`. Falls back to `boundary`.
pub fn find_split(data: &[u8], boundary: usize, pre: usize, post: usize, ext: &str) -> usize {
    let frames = decode::frame_volumes(data, ext);
    if frames.len() < 2 {
        return boundary.min(data.len());
    }
    let lo = boundary.saturating_sub(pre);
    let hi = (boundary + post).min(data.len());

    let mut best: Option<(usize, i32)> = None;
    for &(off, peak) in &frames {
        if off >= lo && off <= hi {
            match best {
                Some((_, p)) if peak >= p => {}
                _ => best = Some((off, peak)),
            }
        }
    }
    best.map(|(o, _)| o)
        .unwrap_or_else(|| boundary.min(data.len()))
}
