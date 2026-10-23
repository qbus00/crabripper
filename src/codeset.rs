//! Character-set conversion for the `--codeset-*` options (via encoding_rs).
//! ICY metadata and filenames on some stations are not UTF-8 (e.g. Polish radio
//! often sends Windows-1250 / ISO-8859-2).

/// Decode bytes using the named codeset (e.g. "cp1250", "iso-8859-2", "utf-8").
/// Unknown labels fall back to a lossy UTF-8 decode.
pub fn decode(bytes: &[u8], label: Option<&str>) -> String {
    match label.and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes())) {
        Some(enc) => enc.decode(bytes).0.into_owned(),
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Encode a string into the named codeset's bytes. Unknown labels → UTF-8.
pub fn encode(s: &str, label: Option<&str>) -> Vec<u8> {
    match label.and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes())) {
        Some(enc) => enc.encode(s).0.into_owned(),
        None => s.as_bytes().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cp1250_roundtrip() {
        // "Łódź" in Windows-1250
        let utf8 = "Łódź";
        let bytes = encode(utf8, Some("cp1250"));
        assert_ne!(bytes, utf8.as_bytes()); // not the same as UTF-8
        assert_eq!(decode(&bytes, Some("cp1250")), utf8);
    }

    #[test]
    fn unknown_label_is_utf8() {
        assert_eq!(decode(b"hi", Some("bogus")), "hi");
        assert_eq!(decode("Łódź".as_bytes(), None), "Łódź");
    }
}
