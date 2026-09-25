//! A connector's `.env`: secrets and settings the user writes, loaded into every
//! command the connector runs. `Redactor` keeps the secrets' values out of
//! captured output and logs.

use std::path::Path;

use anyhow::Context;

/// `KEY=value` pairs in file order. A missing file is an empty env: most
/// connectors need no settings.
pub fn load(path: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    parse(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

/// The dotenv subset everyone agrees on: `KEY=value`, an optional `export `,
/// `#` comments on their own line, blank lines, and values in single quotes
/// (literal) or double quotes (`\n`, `\"`, `\\` escapes). No interpolation.
/// An unquoted value runs to the end of the line, trimmed; a ` #` starts a
/// comment there.
pub fn parse(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let at = |msg: &str| format!("line {}: {msg}", n + 1);
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| at("expected KEY=value"))?;
        let key = key.trim();
        let key_ok = key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !key_ok {
            return Err(at(&format!("{key:?} is not a variable name")));
        }
        let value = unquote(value.trim()).map_err(|e| at(&e))?;
        // Last one wins, as in a shell; keep one entry per key.
        out.retain(|(k, _)| k != key);
        out.push((key.to_string(), value));
    }
    Ok(out)
}

fn unquote(v: &str) -> Result<String, String> {
    if let Some(rest) = v.strip_prefix('\'') {
        let (inner, after) = rest.split_once('\'').ok_or("unterminated single quote")?;
        trailing_ok(after)?;
        return Ok(inner.to_string());
    }
    if let Some(rest) = v.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = rest.chars();
        loop {
            match chars.next() {
                None => return Err("unterminated double quote".into()),
                Some('"') => break,
                Some('\\') => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(c @ ('"' | '\\' | '$')) => out.push(c),
                    Some(c) => {
                        out.push('\\');
                        out.push(c);
                    }
                    None => return Err("unterminated double quote".into()),
                },
                Some(c) => out.push(c),
            }
        }
        trailing_ok(chars.as_str())?;
        return Ok(out);
    }
    let v = match v.find(" #") {
        Some(i) => &v[..i],
        None => v,
    };
    Ok(v.trim_end().to_string())
}

fn trailing_ok(after: &str) -> Result<(), String> {
    let after = after.trim();
    if after.is_empty() || after.starts_with('#') {
        Ok(())
    } else {
        Err(format!("unexpected {after:?} after the closing quote"))
    }
}

/// Values shorter than this are not redacted: replacing every `1` or `on` in
/// a log would make it unreadable and protect nothing.
pub const MIN_SECRET_LEN: usize = 4;

/// Replaces secret values with `[redacted:NAME]`. Built from the names a
/// manifest declares under `[secrets]` and the values the `.env` gives them,
/// so what gets hidden is chosen by name.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// Longest first, so a secret that contains another is replaced whole.
    secrets: Vec<(String, String)>,
}

impl Redactor {
    pub fn new<'a>(names: impl IntoIterator<Item = &'a str>, env: &[(String, String)]) -> Redactor {
        let mut secrets: Vec<(String, String)> = names
            .into_iter()
            .filter_map(|name| {
                env.iter()
                    .find(|(k, _)| k == name)
                    .filter(|(_, v)| v.len() >= MIN_SECRET_LEN)
                    .map(|(k, v)| (k.clone(), v.clone()))
            })
            .collect();
        secrets.sort_by_key(|s| std::cmp::Reverse(s.1.len()));
        Redactor { secrets }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (name, value) in &self.secrets {
            if out.contains(value.as_str()) {
                out = out.replace(value.as_str(), &format!("[redacted:{name}]"));
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_the_common_subset() {
        let text = r#"
# a comment
SLACK_BOT_TOKEN=xoxb-123
export CHANNEL = C0123   # trailing comment
QUOTED="two words\nand \"a quote\""
LITERAL='no $expansion \n here' # ok
EMPTY=
HASH=a#b
DUP=1
DUP=2
"#;
        let env = parse(text).unwrap();
        assert_eq!(
            env,
            kv(&[
                ("SLACK_BOT_TOKEN", "xoxb-123"),
                ("CHANNEL", "C0123"),
                ("QUOTED", "two words\nand \"a quote\""),
                ("LITERAL", "no $expansion \\n here"),
                ("EMPTY", ""),
                ("HASH", "a#b"),
                ("DUP", "2"),
            ])
        );
    }

    #[test]
    fn reports_the_line_of_a_bad_entry() {
        for (text, needle) in [
            ("OK=1\njust words\n", "line 2: expected KEY=value"),
            ("1BAD=x\n", "line 1: \"1BAD\" is not a variable name"),
            ("A=\"open\n", "line 1: unterminated double quote"),
            ("A='open\n", "line 1: unterminated single quote"),
            ("A=\"x\" y\n", "after the closing quote"),
        ] {
            let err = parse(text).unwrap_err();
            assert!(err.contains(needle), "{needle}: {err}");
        }
    }

    #[test]
    fn a_missing_file_is_empty_and_errors_name_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".env");
        assert!(load(&path).unwrap().is_empty());
        std::fs::write(&path, "nope\n").unwrap();
        let err = load(&path).unwrap_err().to_string();
        assert!(err.contains(".env") && err.contains("line 1"), "{err}");
        std::fs::write(&path, "A=1\n").unwrap();
        assert_eq!(load(&path).unwrap(), kv(&[("A", "1")]));
    }

    #[test]
    fn redacts_declared_secrets_by_name_only() {
        let env = kv(&[
            ("TOKEN", "xoxb-secret-1"),
            ("TOKEN_PREFIX", "xoxb"),
            ("CHANNEL", "C0123ABC"),
            ("SHORT", "on"),
        ]);
        let r = Redactor::new(["TOKEN", "TOKEN_PREFIX", "SHORT", "ABSENT"], &env);
        assert_eq!(
            r.redact("auth xoxb-secret-1 in C0123ABC, xoxb alone, on"),
            "auth [redacted:TOKEN] in C0123ABC, [redacted:TOKEN_PREFIX] alone, on",
            "the longer secret wins, undeclared CHANNEL and short values stay"
        );
        assert!(Redactor::new(["X"], &env).is_empty());
        assert_eq!(Redactor::default().redact("plain"), "plain");
    }
}
