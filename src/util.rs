//! Small shared, dependency-free helpers.

use std::time::{SystemTime, UNIX_EPOCH};

/// Is `c` a control character (C0, DEL or C1)? These must never reach a
/// filename, a terminal, or a header line verbatim.
fn is_control(c: char) -> bool {
    let u = c as u32;
    u < 0x20 || (0x7f..=0x9f).contains(&u)
}

/// Make a string safe to use as a single filename component: no path
/// separators, no control characters, and never `.` / `..` (which would
/// resolve to the current or parent directory). Returns "" for such names so
/// callers fall back to their default name.
pub fn sanitize(s: &str) -> String {
    let out = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if is_control(c) => '_',
            c => c,
        })
        .collect::<String>()
        .trim()
        .to_string();
    if out.chars().all(|c| c == '.') {
        String::new()
    } else {
        out
    }
}

/// Server-supplied text made safe to print to a terminal or write into a
/// single header/log line: control characters (incl. ESC, CR, LF) become '?'.
pub fn printable(s: &str) -> String {
    s.chars()
        .map(|c| if is_control(c) { '?' } else { c })
        .collect()
}

/// The longest prefix of `s` that is at most `max` bytes and ends on a char
/// boundary.
pub fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// UTC timestamp `YYYY-MM-DD_HHMMSS` from the system clock.
pub fn now_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86400, secs % 86400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}_{h:02}{mi:02}{s:02}")
}

/// Howard Hinnant's days-since-epoch -> (year, month, day), UTC.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rejects_traversal_and_controls() {
        assert_eq!(sanitize(".."), "");
        assert_eq!(sanitize("."), "");
        assert_eq!(sanitize(" ... "), "");
        assert_eq!(sanitize("../etc/passwd"), ".._etc_passwd");
        assert_eq!(sanitize("a\x1b[31mb"), "a_[31mb");
        assert_eq!(sanitize("a\u{85}b"), "a_b"); // C1 control
        assert_eq!(sanitize("...And Justice"), "...And Justice");
    }

    #[test]
    fn printable_strips_escapes() {
        assert_eq!(printable("x\x1b]0;pwn\x07y\r\nz"), "x?]0;pwn?y??z");
    }

    #[test]
    fn truncate_on_char_boundary() {
        assert_eq!(truncate_utf8("żółw", 3), "ż"); // ż=2 bytes, ó would split
        assert_eq!(truncate_utf8("abc", 10), "abc");
    }
}
