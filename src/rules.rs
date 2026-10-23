//! `-w` metadata rulefiles: regex rules applied to each title to clean it up.
//!
//! Supported directives (one per line; `#` comments and blanks ignored):
//!   match   <regex>          # remember a pattern for the next `replace`
//!   replace <replacement>    # regex-replace the matched pattern ($1.. backrefs)
//!   exclude <regex>          # if the title matches, drop it (treat as no title)

use regex::Regex;

#[derive(Debug, Clone, Default)]
pub struct Rules {
    ops: Vec<Op>,
}

#[derive(Debug, Clone)]
enum Op {
    Replace(Regex, String),
    Exclude(Regex),
}

impl Rules {
    pub fn load(path: &str) -> Result<Rules, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("-w {path}: {e}"))?;
        let mut ops = Vec::new();
        let mut pending: Option<Regex> = None;
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (kw, arg) = match line.split_once(char::is_whitespace) {
                Some((k, a)) => (k, a.trim()),
                None => (line, ""),
            };
            let n = i + 1;
            match kw {
                "match" => {
                    pending = Some(Regex::new(arg).map_err(|e| format!("-w line {n}: {e}"))?);
                }
                "replace" | "subst" => {
                    let re = pending
                        .take()
                        .ok_or_else(|| format!("-w line {n}: replace without a preceding match"))?;
                    ops.push(Op::Replace(re, unquote(arg)));
                }
                "exclude" => {
                    ops.push(Op::Exclude(
                        Regex::new(arg).map_err(|e| format!("-w line {n}: {e}"))?,
                    ));
                }
                other => return Err(format!("-w line {n}: unknown directive {other:?}")),
            }
        }
        Ok(Rules { ops })
    }

    /// Apply the rules to a title. Returns "" if an `exclude` rule matched.
    pub fn apply(&self, title: &str) -> String {
        let mut s = title.to_string();
        for op in &self.ops {
            match op {
                Op::Exclude(re) => {
                    if re.is_match(&s) {
                        return String::new();
                    }
                }
                Op::Replace(re, rep) => {
                    s = re.replace_all(&s, rep.as_str()).into_owned();
                }
            }
        }
        s
    }
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn match_replace_and_exclude() {
        let mut f = tempfile();
        writeln!(
            f.0,
            "# clean up\nmatch \\s*\\(live\\)\nreplace \nexclude ^ADVERT"
        )
        .unwrap();
        let r = Rules::load(&f.1).unwrap();
        assert_eq!(r.apply("Band - Song (live)"), "Band - Song");
        assert_eq!(r.apply("ADVERT break"), "");
        std::fs::remove_file(&f.1).ok();
    }

    fn tempfile() -> (std::fs::File, String) {
        let p = std::env::temp_dir().join(format!("sr_rules_{}.txt", std::process::id()));
        let f = std::fs::File::create(&p).unwrap();
        (f, p.to_string_lossy().into_owned())
    }
}
