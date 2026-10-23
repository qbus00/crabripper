//! ICY (shoutcast/icecast) inline-metadata parsing.
//!
//! When a server honours `Icy-MetaData: 1` it interleaves a metadata block into
//! the audio body every `icy-metaint` bytes. The block is `1 + len*16` bytes:
//! a length byte followed by `len*16` bytes of text like
//! `StreamTitle='Artist - Title';StreamUrl='...';` (zero-padded).

/// Extract the `StreamTitle` value from a raw metadata block, if present.
pub fn parse_stream_title(meta: &[u8]) -> Option<String> {
    // Trim trailing NUL padding before interpreting as text.
    let end = meta
        .iter()
        .rposition(|&b| b != 0)
        .map(|i| i + 1)
        .unwrap_or(0);
    let text = String::from_utf8_lossy(&meta[..end]);

    let key = "StreamTitle=";
    let start = text.find(key)? + key.len();
    let rest = text[start..].trim_start();
    let rest = rest.strip_prefix('\'').unwrap_or(rest);

    // Value ends at the closing "';" ; fall back to a trailing quote or EOL.
    let end = rest
        .find("';")
        .or_else(|| rest.rfind('\''))
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let m = b"StreamTitle='Heatmiser - Get Lucky';StreamUrl='http://x';\0\0\0";
        assert_eq!(
            parse_stream_title(m).as_deref(),
            Some("Heatmiser - Get Lucky")
        );
    }

    #[test]
    fn empty_title() {
        let m = b"StreamTitle='';StreamUrl='';";
        assert_eq!(parse_stream_title(m).as_deref(), Some(""));
    }

    #[test]
    fn no_title_field() {
        assert_eq!(parse_stream_title(b"StreamUrl='x';"), None);
    }
}
