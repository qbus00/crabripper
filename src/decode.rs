//! MP3 decoding via the vendored minimp3 (C), used for `--wav` output and
//! silence-based splitpoint detection. MP3 only — matching streamripper's
//! "mp3 only" splitpoint/wav features.

use std::os::raw::{c_int, c_void};

extern "C" {
    fn sr_mp3_new() -> *mut c_void;
    fn sr_mp3_free(d: *mut c_void);
    fn sr_mp3_decode(
        d: *mut c_void,
        buf: *const u8,
        len: c_int,
        pcm: *mut i16,
        channels: *mut c_int,
        hz: *mut c_int,
        frame_bytes: *mut c_int,
    ) -> c_int;
}

const MAX_SAMPLES_PER_FRAME: usize = 1152 * 2;

pub struct Pcm {
    pub samples: Vec<i16>, // interleaved
    pub channels: u16,
    pub sample_rate: u32,
}

/// Decode a complete MP3 byte buffer to interleaved 16-bit PCM.
pub fn decode_mp3(data: &[u8]) -> Pcm {
    let mut out: Vec<i16> = Vec::new();
    let mut pcm = [0i16; MAX_SAMPLES_PER_FRAME];
    let mut channels = 2i32;
    let mut sample_rate = 44100i32;
    let mut pos = 0usize;

    unsafe {
        let dec = sr_mp3_new();
        if dec.is_null() {
            return Pcm {
                samples: out,
                channels: 2,
                sample_rate: 44100,
            };
        }
        while pos < data.len() {
            let mut c = 0i32;
            let mut h = 0i32;
            let mut fb = 0i32;
            let got = sr_mp3_decode(
                dec,
                data[pos..].as_ptr(),
                (data.len() - pos) as c_int,
                pcm.as_mut_ptr(),
                &mut c,
                &mut h,
                &mut fb,
            );
            if fb == 0 {
                break; // need more data / end of stream
            }
            pos += fb as usize;
            if got > 0 {
                if c > 0 {
                    channels = c;
                }
                if h > 0 {
                    sample_rate = h;
                }
                let n = got as usize * channels.max(1) as usize;
                out.extend_from_slice(&pcm[..n.min(pcm.len())]);
            }
        }
        sr_mp3_free(dec);
    }

    Pcm {
        samples: out,
        channels: channels.max(1) as u16,
        sample_rate: sample_rate.max(1) as u32,
    }
}
