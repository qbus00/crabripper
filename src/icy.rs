//! ICY (shoutcast/icecast) inline-metadata parsing.
//!
//! When a server honours `Icy-MetaData: 1` it interleaves a metadata block into
//! the audio body every `icy-metaint` bytes. The block is `1 + len*16` bytes:
//! a length byte followed by `len*16` bytes of text like
//! `StreamTitle='Artist - Title';StreamUrl='...';` (zero-padded).

/// Extract the raw (undecoded) `StreamTitle` value bytes, so the caller can
/// decode them with the right charset (`--codeset-metadata`).
pub fn stream_title_raw(meta: &[u8]) -> Option<&[u8]> {
    let end = meta
        .iter()
        .rposition(|&b| b != 0)
        .map(|i| i + 1)
        .unwrap_or(0);
    let m = &meta[..end];
    let start = find(m, b"StreamTitle=")? + b"StreamTitle=".len();
    let mut rest = &m[start..];
    while rest.first() == Some(&b' ') {
        rest = &rest[1..];
    }
    if rest.first() == Some(&b'\'') {
        rest = &rest[1..];
    }
    let end = find(rest, b"';")
        .or_else(|| rest.iter().rposition(|&b| b == b'\''))
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn title(m: &[u8]) -> Option<String> {
        stream_title_raw(m).map(|b| String::from_utf8_lossy(b).into_owned())
    }

    #[test]
    fn basic() {
        let m = b"StreamTitle='Heatmiser - Get Lucky';StreamUrl='http://x';\0\0\0";
        assert_eq!(title(m).as_deref(), Some("Heatmiser - Get Lucky"));
    }

    #[test]
    fn empty_title() {
        let m = b"StreamTitle='';StreamUrl='';";
        assert_eq!(title(m).as_deref(), Some(""));
    }

    #[test]
    fn no_title_field() {
        assert_eq!(title(b"StreamUrl='x';"), None);
    }
}
