//! Audio decoding via symphonia (pure Rust): mp3, aac, ogg-vorbis, flac.
//! Used for `--wav` (all formats) and silence-based splitpoint (mp3/aac).

use std::io::Cursor;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub struct Pcm {
    pub samples: Vec<i16>, // interleaved
    pub channels: u16,
    pub sample_rate: u32,
}

/// Map our output extension to a probe hint symphonia understands.
fn hint_ext(ext: &str) -> &str {
    match ext {
        "oga" => "ogg",
        other => other,
    }
}

type Opened = (Box<dyn FormatReader>, Box<dyn Decoder>, u32);

fn open(data: &[u8], ext: &str) -> Option<Opened> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data.to_vec())), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(hint_ext(ext));
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .ok()?;
    let format = probed.format;
    let track = format.default_track()?.clone();
    let decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .ok()?;
    Some((format, decoder, track.id))
}

/// Decode a complete audio buffer to interleaved 16-bit PCM.
pub fn decode_audio(data: &[u8], ext: &str) -> Pcm {
    let mut out: Vec<i16> = Vec::new();
    let (mut channels, mut sample_rate) = (2u16, 44100u32);

    if let Some((mut format, mut decoder, track_id)) = open(data, ext) {
        let mut sbuf: Option<SampleBuffer<i16>> = None;
        while let Ok(packet) = format.next_packet() {
            if packet.track_id() != track_id {
                continue;
            }
            if let Ok(audio) = decoder.decode(&packet) {
                if sbuf.is_none() {
                    let spec = *audio.spec();
                    channels = spec.channels.count() as u16;
                    sample_rate = spec.rate;
                    sbuf = Some(SampleBuffer::new(audio.capacity() as u64, spec));
                }
                if let Some(sb) = &mut sbuf {
                    sb.copy_interleaved_ref(audio);
                    out.extend_from_slice(sb.samples());
                }
            }
        }
    }
    Pcm {
        samples: out,
        channels,
        sample_rate,
    }
}

/// For silence detection (mp3/aac only — frame-contiguous codecs):
/// `(cumulative byte offset of each frame, peak |sample| in that frame)`.
pub fn frame_volumes(data: &[u8], ext: &str) -> Vec<(usize, i32)> {
    let mut frames = Vec::new();
    if let Some((mut format, mut decoder, track_id)) = open(data, ext) {
        let mut sbuf: Option<SampleBuffer<i16>> = None;
        let mut byte_off = 0usize;
        while let Ok(packet) = format.next_packet() {
            if packet.track_id() != track_id {
                continue;
            }
            let this_off = byte_off;
            byte_off += packet.data.len();
            if let Ok(audio) = decoder.decode(&packet) {
                if sbuf.is_none() {
                    sbuf = Some(SampleBuffer::new(audio.capacity() as u64, *audio.spec()));
                }
                if let Some(sb) = &mut sbuf {
                    sb.copy_interleaved_ref(audio);
                    let peak = sb
                        .samples()
                        .iter()
                        .map(|s| (*s as i32).abs())
                        .max()
                        .unwrap_or(0);
                    frames.push((this_off, peak));
                }
            }
        }
    }
    frames
}
