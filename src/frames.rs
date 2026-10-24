//! Minimal MPEG-audio (mp3) and ADTS (aac) frame-header parsing.
//!
//! The silence splitter analyses a rolling tail of the stream that can start
//! mid-frame. These helpers locate real frame boundaries (a sync word whose
//! frame length chains to the next sync word), so cut points line up exactly.

/// Byte length of the MPEG-1/2/2.5 Layer I/II/III frame whose header is `h`.
fn mpeg_frame_len(h: &[u8]) -> Option<usize> {
    if h.len() < 4 || h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 3; // 0: MPEG-2.5, 1: reserved, 2: MPEG-2, 3: MPEG-1
    let layer = (h[1] >> 1) & 3; // 1: III, 2: II, 3: I, 0: reserved
    let br_idx = (h[2] >> 4) as usize;
    let sr_idx = ((h[2] >> 2) & 3) as usize;
    let pad = ((h[2] >> 1) & 1) as u32;
    if version == 1 || layer == 0 || br_idx == 0 || br_idx == 15 || sr_idx == 3 {
        return None;
    }
    const SR: [[u32; 3]; 4] = [
        [11025, 12000, 8000],
        [0, 0, 0],
        [22050, 24000, 16000],
        [44100, 48000, 32000],
    ];
    // kbps, indexed [layer I, II, III][br_idx]
    const BR_V1: [[u32; 15]; 3] = [
        [
            0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
        ],
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
        ],
        [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ],
    ];
    // MPEG-2/2.5: [layer I, layer II+III][br_idx]
    const BR_V2: [[u32; 15]; 2] = [
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
        ],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ];
    let sr = SR[version as usize][sr_idx];
    let li = match layer {
        3 => 0,
        2 => 1,
        _ => 2,
    };
    let br = 1000
        * if version == 3 {
            BR_V1[li][br_idx]
        } else {
            BR_V2[usize::from(li != 0)][br_idx]
        };
    let len = match layer {
        3 => (12 * br / sr + pad) * 4,           // Layer I
        1 if version != 3 => 72 * br / sr + pad, // Layer III, MPEG-2/2.5
        _ => 144 * br / sr + pad,                // Layer II, Layer III MPEG-1
    };
    (len >= 4).then_some(len as usize)
}

/// Byte length of the ADTS frame whose header is `h`.
fn adts_frame_len(h: &[u8]) -> Option<usize> {
    if h.len() < 7 || h[0] != 0xFF || h[1] & 0xF6 != 0xF0 || (h[2] >> 2) & 0xF > 12 {
        return None;
    }
    let len = ((h[3] as usize & 3) << 11) | ((h[4] as usize) << 3) | (h[5] as usize >> 5);
    (len >= 7).then_some(len)
}

fn frame_len(h: &[u8], ext: &str) -> Option<usize> {
    match ext {
        "aac" => adts_frame_len(h),
        _ => mpeg_frame_len(h),
    }
}

/// Offset of the first genuine frame in `data`: a header whose length leads to
/// another valid header (twice), or runs exactly to the end of the buffer.
pub fn first_frame(data: &[u8], ext: &str) -> Option<usize> {
    (0..data.len()).find(|&i| {
        let mut pos = i;
        for _ in 0..3 {
            match frame_len(&data[pos..], ext) {
                Some(len) => pos += len,
                None => return false,
            }
            if pos >= data.len() {
                return true; // chained to the end of what we have
            }
        }
        true
    })
}

/// `(offset, length)` of each complete consecutive frame from `first_frame`.
pub fn frames(data: &[u8], ext: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let Some(mut pos) = first_frame(data, ext) else {
        return out;
    };
    while let Some(len) = frame_len(&data[pos..], ext) {
        if pos + len > data.len() {
            break;
        }
        out.push((pos, len));
        pos += len;
    }
    out
}

/// Does a decoder packet of `packet_len` bytes correspond to a frame of
/// `frame_len` bytes? mp3 packets are whole frames; ADTS packets exclude the
/// 7-byte header (9 with CRC).
pub fn packet_matches(frame_len: usize, packet_len: usize, ext: &str) -> bool {
    match ext {
        "aac" => frame_len == packet_len + 7 || frame_len == packet_len + 9,
        _ => frame_len == packet_len,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MP3: &[u8] = include_bytes!("../testdata/sine.mp3");
    const AAC: &[u8] = include_bytes!("../testdata/sine.aac");

    #[test]
    fn finds_frames_after_garbage() {
        for (data, ext) in [(MP3, "mp3"), (AAC, "aac")] {
            let start = first_frame(data, ext).expect("sync");
            let mut junk = vec![0x12u8, 0xFF, 0xFB, 0x00, 0x77];
            junk.extend_from_slice(&data[start..]);
            assert_eq!(first_frame(&junk, ext), Some(5), "{ext}");
            assert!(frames(data, ext).len() > 20, "{ext}");
        }
    }
}
