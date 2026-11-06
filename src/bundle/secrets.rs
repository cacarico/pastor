//! Refusing secrets at `bundle add`. A bundle is copied to every machine of
//! the flocks that name it, so a credential in one would be spread with
//! it. Two kinds of finding: a file named like a credential store, and a
//! line holding a value shaped like a token or a key. Findings name the
//! file and line and say what was seen, never the value itself.
//!
//! MCP servers and hooks take their secrets from the machine's env by name
//! (`env_from`), so a bundle never needs one.

use std::path::Path;

/// One reason to refuse a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The file, relative to the bundle.
    pub path: String,
    /// 1-based; `None` for a finding about the file's name.
    pub line: Option<usize>,
    pub what: &'static str,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(n) => write!(f, "{}:{n}: {}", self.path, self.what),
            None => write!(f, "{}: {}", self.path, self.what),
        }
    }
}

/// What looks like a secret among `files` (paths relative to `dir`).
pub fn scan(dir: &Path, files: &[String]) -> Result<Vec<Finding>, String> {
    let mut found = Vec::new();
    for rel in files {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        if let Some(what) = secret_name(name) {
            found.push(Finding {
                path: rel.clone(),
                line: None,
                what,
            });
            continue;
        }
        let path = dir.join(rel);
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        found.extend(scan_lines(rel, &String::from_utf8_lossy(&bytes)));
    }
    Ok(found)
}

/// What looks like a secret in a file that is not on disk yet: its name,
/// as `rel`, and `text`, its content.
pub fn scan_text(rel: &str, text: &str) -> Vec<Finding> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    if let Some(what) = secret_name(name) {
        return vec![Finding {
            path: rel.to_string(),
            line: None,
            what,
        }];
    }
    scan_lines(rel, text)
}

/// What looks like a secret in `text`, line by line, named `rel`; its name
/// is not judged, as it is no file's.
pub fn scan_lines(rel: &str, text: &str) -> Vec<Finding> {
    text.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            secret_value(line).map(|what| Finding {
                path: rel.to_string(),
                line: Some(i + 1),
                what,
            })
        })
        .collect()
}

/// What looks like a secret in the env variable `name` set to `value`: a
/// value shaped like a token or a key, or a long one under a name that says
/// token, secret, key or password. Says what was seen, never the value.
pub fn env_finding(name: &str, value: &str) -> Option<&'static str> {
    secret_value(&format!("{name} = \"{value}\""))
}

/// Whether the env variable's name says it holds a secret: a name with
/// token, secret, key or password in it. What is shown of it hides the
/// value.
pub fn env_name_looks_secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["token", "secret", "key", "password", "passwd"]
        .iter()
        .any(|w| lower.contains(w))
}

/// A file name that a credential store or a key uses.
fn secret_name(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if lower == ".credentials.json" || lower == "auth.json" {
        return Some("a harness's credentials file");
    }
    if lower.contains("oauth") && lower.contains("token") {
        return Some("an OAuth token file");
    }
    if lower == ".env" || lower.starts_with(".env.") {
        return Some("an env file, which holds secrets");
    }
    if lower.ends_with(".pem") || lower.ends_with(".p12") || lower.ends_with(".pfx") {
        return Some("a key or certificate file");
    }
    // `id_ed25519_sk`, `id_rsa.bak`: ssh's key names, and copies of them;
    // the `.pub` half is not secret.
    if lower.starts_with("id_") && !lower.ends_with(".pub") {
        return Some("an ssh private key");
    }
    if lower == ".netrc" {
        return Some("a netrc file, which holds passwords");
    }
    None
}

/// The kind of token `line` holds, if any.
fn secret_value(line: &str) -> Option<&'static str> {
    if line.contains("-----BEGIN") && line.contains("PRIVATE KEY-----") {
        return Some("a private key");
    }
    let prefixed: [(&str, usize, &str, &'static str); 10] = [
        ("ghp_", 30, ALNUM, "a GitHub token"),
        ("gho_", 30, ALNUM, "a GitHub token"),
        ("ghu_", 30, ALNUM, "a GitHub token"),
        ("ghs_", 30, ALNUM, "a GitHub token"),
        ("ghr_", 30, ALNUM, "a GitHub token"),
        ("github_pat_", 20, WORD, "a GitHub token"),
        ("sk-", 20, WORD, "an API key"),
        ("xoxb-", 10, WORD, "a Slack token"),
        ("xoxp-", 10, WORD, "a Slack token"),
        ("AKIA", 16, UPPER_NUM, "an AWS access key"),
    ];
    for (prefix, min, chars, what) in prefixed {
        for (at, _) in line.match_indices(prefix) {
            // A prefix in the middle of a word (`task-...`) is not one.
            let starts_word = line[..at]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric());
            let run: String = line[at + prefix.len()..]
                .chars()
                .take_while(|c| chars.contains(*c))
                .collect();
            if starts_word && run.len() >= min && !placeholder(&run) {
                return Some(what);
            }
        }
    }
    keyed_value(line)
}

const ALNUM: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const WORD: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-";
const UPPER_NUM: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789+/=_-.";

/// A long random-looking value assigned to a key whose name says token,
/// secret, key or password: `api_key = "..."`, `"token": "..."`. Every
/// such name on the line counts, so one short value before a secret on a
/// minified line does not hide it.
fn keyed_value(line: &str) -> Option<&'static str> {
    // ASCII lowercasing keeps byte offsets, so they index `line` too.
    let lower = line.to_ascii_lowercase();
    let found = ["token", "secret", "key", "password", "passwd"]
        .iter()
        .flat_map(|w| lower.match_indices(w).map(move |(i, _)| i + w.len()))
        .any(|after| secret_after(&line[after..]));
    found.then_some("a long value under a token, secret, key or password name")
}

/// Whether the value after the first `=` or `:` in `rest` looks like a
/// secret: 32 or more token characters, with letters and digits, and not
/// a placeholder or a UUID.
fn secret_after(rest: &str) -> bool {
    let Some(at) = rest.find(['=', ':']) else {
        return false;
    };
    let value = rest[at + 1..].trim_start().trim_start_matches(['"', '\'']);
    let token: String = value.chars().take_while(|c| TOKEN.contains(*c)).collect();
    let mixed =
        token.chars().any(|c| c.is_ascii_digit()) && token.chars().any(|c| c.is_ascii_alphabetic());
    token.len() >= 32 && mixed && !placeholder(&token) && !uuid(&token)
}

/// A value written to stand for one, as docs do: eight or more of one
/// character in a row (`sk-ant-xxxxxxxx`).
fn placeholder(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.windows(8).any(|w| w.iter().all(|b| *b == w[0]))
}

/// `550e8400-e29b-41d4-a716-446655440000`: an id, not a secret.
fn uuid(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12])
        && parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_file_names_are_refused() {
        for name in [
            ".credentials.json",
            "auth.json",
            "antigravity-oauth-token",
            ".env",
            ".env.local",
            "server.pem",
            "id_ed25519",
            "id_ed25519_sk",
            "id_rsa.bak",
            ".netrc",
        ] {
            assert!(secret_name(name).is_some(), "{name}");
        }
        for name in [
            "SKILL.md",
            "id_ed25519.pub",
            "id_rsa_sk.pub",
            "environment.md",
            "tokens.md",
            "notify.sh",
            "settings.json",
        ] {
            assert!(secret_name(name).is_none(), "{name}");
        }
    }

    /// 36 characters, letters and digits.
    const MIXED: &str = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6a7b8";

    #[test]
    fn token_shaped_values_are_refused() {
        let gh = format!("ghp_{}", "a1B2".repeat(9));
        let cases = [
            format!("export GH_TOKEN={gh}"),
            format!("\"url\": \"https://x:{gh}@github.com\""),
            format!("github_pat_{}", "x1_".repeat(10)),
            format!("ANTHROPIC_API_KEY=sk-ant-{}", "abc123".repeat(5)),
            format!("SLACK=xoxb-{}", "1234-abcd".repeat(3)),
            "aws = AKIAABCDEFGHIJKLMNOP".to_string(),
            "-----BEGIN OPENSSH PRIVATE KEY-----".to_string(),
            format!("api_key = \"{}\"", "Zm9vYmFy0123".repeat(3)),
            format!("\"client_secret\": \"{}\"", "a9".repeat(20)),
            // A short value under the first name does not hide a secret
            // under a later one, on a minified or inline line.
            format!(
                "{{\"apiKeyHelper\": \"~/bin/x\", \"env\": {{\"SERVICE_TOKEN\": \"{MIXED}\"}}}}"
            ),
            format!("{{\"env\":{{\"KEYMAP\":\"us\",\"GH_SECRET\":\"{MIXED}\"}}}}"),
            format!("env = {{ KEYBOARD = \"us\", API_TOKEN = \"{MIXED}\" }}"),
        ];
        for line in &cases {
            assert!(secret_value(line).is_some(), "{line}");
        }
        let clean = [
            "args = [\"--token-env\", \"TICKETS_TOKEN\"]",
            "env_from = [\"TICKETS_TOKEN\"]",
            "\"key\": \"${TICKETS_TOKEN}\"",
            "task-0123456789abcdefghij0123456789",
            "\"model\": \"claude-sonnet-4-5-20250929\"",
            "keyboard: 2026.10.27.1",
            "\"apiKeyHelper\": \"~/.local/bin/get-the-key-from-the-vault\"",
            "sk-short",
            // Under 32 characters is not long enough to be a secret.
            "\"api_key\": \"abc123def456ghi789jkl012mno345p\"",
            // A placeholder in docs, and an id.
            "Set ANTHROPIC_API_KEY=sk-ant-xxxxxxxxxxxxxxxxxxxxxxxx",
            "Primary key: 550e8400-e29b-41d4-a716-446655440000",
        ];
        for line in clean {
            assert!(secret_value(line).is_none(), "{line}");
        }
    }

    /// A finding names the file and line and what it looks like, never the
    /// value.
    #[test]
    fn a_finding_names_file_and_line_not_the_value() {
        let tmp = tempfile::tempdir().unwrap();
        let gh = format!("ghp_{}", "a1B2".repeat(9));
        std::fs::create_dir_all(tmp.path().join("settings")).unwrap();
        std::fs::write(
            tmp.path().join("settings/claude.json"),
            format!("{{\n\"env\": {{\"T\": \"{gh}\"}}\n}}\n"),
        )
        .unwrap();
        std::fs::write(tmp.path().join(".env"), "A=1\n").unwrap();
        std::fs::write(tmp.path().join("ok.md"), "nothing here\n").unwrap();
        let files = ["settings/claude.json", ".env", "ok.md"].map(String::from);
        let found = scan(tmp.path(), &files).unwrap();
        let said: Vec<String> = found.iter().map(|f| f.to_string()).collect();
        assert_eq!(
            said,
            [
                "settings/claude.json:2: a GitHub token",
                ".env: an env file, which holds secrets"
            ]
        );
        assert!(!said.concat().contains(&gh));
    }
}
