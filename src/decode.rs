//! Audio decoding via symphonia (pure Rust): mp3, aac, ogg-vorbis, flac.
//! Used for `--wav` (all formats) and silence-based splitpoint (mp3/aac).
//!
//! Decoding runs on untrusted stream data, so every entry point is isolated
//! with `catch_unwind`: a decoder panic degrades to "no result" instead of
//! taking the recording down.

use crate::wav;
use std::fs::File;
use std::io::{self, BufWriter, Cursor, Seek, SeekFrom, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use symphonia::core::audio::{AudioBufferRef, SampleBuffer, SignalSpec};
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Map our output extension to a probe hint symphonia understands.
fn hint_ext(ext: &str) -> &str {
    match ext {
        "oga" => "ogg",
        other => other,
    }
}

type Opened = (Box<dyn FormatReader>, Box<dyn Decoder>, u32);

fn open(src: Box<dyn MediaSource>, ext: &str) -> Option<Opened> {
    let mss = MediaSourceStream::new(src, Default::default());
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

/// A sample buffer that is (re)allocated whenever a decoded packet's spec or
/// size outgrows it — symphonia asserts if the buffer is too small.
#[derive(Default)]
struct Samples {
    buf: Option<SampleBuffer<i16>>,
    spec: Option<SignalSpec>,
}

impl Samples {
    fn copy(&mut self, audio: AudioBufferRef) -> &[i16] {
        let spec = *audio.spec();
        let needed = audio.frames() * spec.channels.count();
        let fits =
            self.spec == Some(spec) && self.buf.as_ref().is_some_and(|b| b.capacity() >= needed);
        if !fits {
            self.buf = Some(SampleBuffer::new(audio.capacity() as u64, spec));
            self.spec = Some(spec);
        }
        let sb = self.buf.as_mut().expect("allocated above");
        sb.copy_interleaved_ref(audio);
        sb.samples()
    }
}

/// For silence detection (mp3/aac only — frame-contiguous codecs): one entry
/// per packet, in order: `(packet payload length, peak |sample|)`, the peak
/// being `None` if that packet failed to decode. The caller maps packets to
/// byte positions with its own frame parser (packet lengths differ from frame
/// lengths for ADTS, which strips the header).
pub fn packet_peaks(data: &[u8], ext: &str) -> Vec<(usize, Option<i32>)> {
    let src: Box<dyn MediaSource> = Box::new(Cursor::new(data.to_vec()));
    catch_unwind(AssertUnwindSafe(|| {
        let mut out = Vec::new();
        let Some((mut format, mut decoder, track_id)) = open(src, ext) else {
            return out;
        };
        let mut samples = Samples::default();
        while let Ok(packet) = format.next_packet() {
            if packet.track_id() != track_id {
                continue;
            }
            let peak = decoder.decode(&packet).ok().map(|audio| {
                samples
                    .copy(audio)
                    .iter()
                    .map(|s| (*s as i32).abs())
                    .max()
                    .unwrap_or(0)
            });
            out.push((packet.data.len(), peak));
        }
        out
    }))
    .unwrap_or_default()
}

/// Decode the encoded file `src` into a 16-bit PCM WAV at `dst`, streaming:
/// memory use is one packet, not the whole track. Packets whose channel count
/// or rate differ from the first decoded packet are skipped (WAV cannot change
/// format mid-file). Returns the number of PCM data bytes written.
pub fn decode_to_wav(src: &Path, dst: &Path, ext: &str) -> io::Result<u64> {
    let input = File::open(src)?;
    let out = File::create(dst)?;
    catch_unwind(AssertUnwindSafe(move || -> io::Result<u64> {
        let mut w = BufWriter::new(out);
        let (mut channels, mut rate) = (2u16, 44100u32);
        wav::write_header(&mut w, channels, rate, 0)?; // patched below
        let mut data_bytes = 0u64;
        if let Some((mut format, mut decoder, track_id)) = open(Box::new(input), ext) {
            let mut samples = Samples::default();
            let mut first: Option<(u16, u32)> = None;
            while let Ok(packet) = format.next_packet() {
                if packet.track_id() != track_id {
                    continue;
                }
                let Ok(audio) = decoder.decode(&packet) else {
                    continue;
                };
                let spec = *audio.spec();
                let this = (spec.channels.count() as u16, spec.rate);
                match first {
                    None => {
                        first = Some(this);
                        (channels, rate) = this;
                    }
                    Some(f) if f != this => continue,
                    _ => {}
                }
                let pcm = samples.copy(audio);
                let mut bytes = Vec::with_capacity(pcm.len() * 2);
                for s in pcm {
                    bytes.extend_from_slice(&s.to_le_bytes());
                }
                w.write_all(&bytes)?;
                data_bytes += bytes.len() as u64;
            }
        }
        if data_bytes == 0 {
            // Nothing decodable (wrong codec, garbage): report it, so the
            // caller keeps the encoded audio instead of an empty WAV.
            return Err(io::Error::other("no decodable audio"));
        }
        w.seek(SeekFrom::Start(0))?;
        wav::write_header(&mut w, channels, rate, data_bytes)?;
        w.flush()?;
        Ok(data_bytes)
    }))
    .unwrap_or_else(|_| Err(io::Error::other("audio decoder panicked")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_to_wav_file() {
        let dir = std::env::temp_dir().join(format!("crab_dec_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.mp3");
        std::fs::write(&src, include_bytes!("../testdata/sine.mp3")).unwrap();
        let dst = dir.join("out.wav");
        let n = decode_to_wav(&src, &dst, "mp3").unwrap();
        let wavb = std::fs::read(&dst).unwrap();
        assert!(n > 100_000, "decoded {n} bytes"); // ~2s stereo 44.1k
        assert_eq!(wavb.len() as u64, 44 + n);
        assert_eq!(&wavb[0..4], b"RIFF");
        std::fs::remove_dir_all(&dir).ok();
    }
}
