//! Minimal 16-bit PCM WAV header writer (samples are streamed by the caller).

use std::io::{self, Write};

/// Write a 44-byte PCM WAV header for `data_len` bytes of 16-bit samples.
/// WAV sizes are 32-bit; longer data saturates the fields at the maximum
/// (the common convention players accept) instead of wrapping.
pub fn write_header<W: Write>(
    w: &mut W,
    channels: u16,
    sample_rate: u32,
    data_len: u64,
) -> io::Result<()> {
    let bits: u16 = 16;
    let channels = channels.max(1);
    let block_align: u16 = channels.saturating_mul(bits / 8);
    let byte_rate: u32 = sample_rate.saturating_mul(block_align as u32);
    let data_len32: u32 = data_len.min((u32::MAX - 36) as u64) as u32;
    let riff_len: u32 = 36 + data_len32;

    w.write_all(b"RIFF")?;
    w.write_all(&riff_len.to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?; // PCM fmt chunk size
    w.write_all(&1u16.to_le_bytes())?; // audio format = PCM
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&block_align.to_le_bytes())?;
    w.write_all(&bits.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_len32.to_le_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_saturates() {
        let mut v = Vec::new();
        write_header(&mut v, 2, 44100, 10_000_000_000).unwrap();
        assert_eq!(v.len(), 44);
        assert_eq!(u32::from_le_bytes(v[4..8].try_into().unwrap()), u32::MAX);
    }
}
