//! Every error code pastor's code uses is in the manual's table.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Where a code is written right after: the constructors of an error with a
/// code, and the `(code, message)` pairs that become one.
const CODE_SITES: [&str; 7] = [
    "CliError::err(",
    "IpcResponse::error(",
    "ReopenError::new(",
    "code:",
    " fail(",
    "Err((",
    "unwrap_or((",
];

/// Where a `(code, message)` pair may start, in a match arm or a closure.
/// Only a pair whose message is not a plain literal counts, so a pair of
/// words (`("machine", "stays")`) does not.
const PAIR_SITES: [&str; 3] = ["=> (", "|| (", "|| {"];

#[test]
fn every_error_code_is_in_the_manual() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manual = std::fs::read_to_string(root.join("docs/manual.md")).unwrap();
    let documented = table_codes(&manual);
    assert!(
        documented.len() > 50,
        "only {} codes in the table",
        documented.len()
    );
    let used = source_codes(&root.join("src"));
    let missing: Vec<_> = used
        .iter()
        .filter(|(code, _)| !documented.contains(*code))
        .map(|(code, file)| format!("{code} ({file})"))
        .collect();
    assert!(
        missing.is_empty(),
        "add these codes to the table under ### Errors in docs/manual.md: {missing:?}"
    );
}

#[test]
fn every_code_in_the_manual_is_used() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manual = std::fs::read_to_string(root.join("docs/manual.md")).unwrap();
    let used = source_codes(&root.join("src"));
    let stale: Vec<_> = table_codes(&manual)
        .into_iter()
        .filter(|c| !used.contains_key(c))
        .collect();
    assert!(
        stale.is_empty(),
        "codes in the manual no error site emits: {stale:?}"
    );
}

#[test]
fn the_scan_finds_codes_in_each_form() {
    let text = r#"
/// The code of a thing.
pub const THING: &str = "thing_code";
const OTHER: &str = "not_a_code";
fn a() {
    CliError::err("cli_code", "m");
    IpcResponse::error(
        crate::x::THING,
        m,
    );
    fail("fail_code", &m);
    Err(("pair_code".into(), format!("m")));
    x => ("arm_code", message.clone()),
    x => ("machine", "stays"),
}
impl E {
    fn code(&self) -> &str {
        match self {
            E::A => "enum_code",
        }
    }
}
"#;
    let mut found = BTreeMap::new();
    scan(text, "f.rs", &consts(text), &mut found);
    let found: Vec<_> = found.into_keys().collect();
    assert_eq!(
        found,
        [
            "arm_code",
            "cli_code",
            "enum_code",
            "fail_code",
            "pair_code",
            "thing_code"
        ]
    );
}

/// The codes in the manual's table: the first cell of each row, in
/// backticks.
fn table_codes(manual: &str) -> BTreeSet<String> {
    let from = manual
        .find("### Errors")
        .expect("no ### Errors in the manual");
    manual[from..]
        .lines()
        .skip(1)
        .take_while(|l| !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("| `"))
        .map(|l| l[..l.find('`').unwrap()].to_string())
        .collect()
}

/// Every code the code under `dir` uses, with a file that uses it. herdr's
/// own codes (`src/herdr/`) are herdr's, not pastor's.
fn source_codes(dir: &Path) -> BTreeMap<String, String> {
    let files: Vec<(String, String)> = rust_files(dir)
        .into_iter()
        .filter(|f| !f.starts_with(dir.join("herdr")))
        .map(|f| {
            let text = non_test(&std::fs::read_to_string(&f).unwrap()).to_string();
            (f.strip_prefix(dir).unwrap().display().to_string(), text)
        })
        .collect();
    let mut all_consts = BTreeMap::new();
    for (_, text) in &files {
        all_consts.extend(consts(text));
    }
    let mut out = BTreeMap::new();
    for (name, text) in &files {
        scan(text, name, &all_consts, &mut out);
    }
    out
}

/// The codes in one file: those at a code site, the arms of a `fn code`,
/// and the constants documented as a code.
fn scan(
    text: &str,
    file: &str,
    known: &BTreeMap<String, (String, bool)>,
    out: &mut BTreeMap<String, String>,
) {
    let mut add = |code: &str| {
        out.entry(code.to_string())
            .or_insert_with(|| file.to_string());
    };
    for site in CODE_SITES {
        for (at, _) in text.match_indices(site) {
            let rest = text[at + site.len()..].trim_start();
            if let Some(code) = literal(rest) {
                add(code);
            } else if let Some(name) = constant(rest) {
                let (code, _) = known
                    .get(name)
                    .unwrap_or_else(|| panic!("{file}: no constant {name}"));
                add(code);
            }
        }
    }
    for site in PAIR_SITES {
        for (at, _) in text.match_indices(site) {
            let rest = text[at + site.len()..].trim_start();
            let rest = rest.strip_prefix('(').map(str::trim_start).unwrap_or(rest);
            let Some(code) = literal(rest) else { continue };
            let after = rest[code.len() + 2..]
                .trim_start_matches(".into()")
                .trim_start_matches(".to_string()");
            if let Some(message) = after.strip_prefix(',')
                && !message.trim_start().starts_with('"')
            {
                add(code);
            }
        }
    }
    for (at, _) in text.match_indices("fn code(") {
        let body = &text[at..];
        let body = &body[..body.find("\n    }\n").unwrap_or(body.len())];
        for arm in body.split("=> ").skip(1) {
            if let Some(code) = literal(arm) {
                add(code);
            }
        }
    }
    for (code, is_code) in consts(text).into_values() {
        if is_code {
            add(&code);
        }
    }
}

/// The `&str` constants of a file: name to value, and whether its doc
/// comment calls it a code.
fn consts(text: &str) -> BTreeMap<String, (String, bool)> {
    let mut out = BTreeMap::new();
    let mut doc = String::new();
    for line in text.lines().map(str::trim) {
        if let Some(d) = line.strip_prefix("///") {
            doc.push_str(d);
            continue;
        }
        let decl = line.strip_prefix("pub ").unwrap_or(line);
        if let Some(decl) = decl.strip_prefix("const ")
            && let Some((name, value)) = decl.split_once(": &str = ")
            && let Some(code) = literal(value)
        {
            out.insert(name.to_string(), (code.to_string(), doc.contains("code")));
        }
        doc.clear();
    }
    out
}

/// The code-shaped string literal `s` starts with: lower case words joined
/// by `_`.
fn literal(s: &str) -> Option<&str> {
    let body = s.strip_prefix('"')?;
    let end = body.find('"')?;
    let code = &body[..end];
    let shaped = code.starts_with(|c: char| c.is_ascii_lowercase())
        && code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    shaped.then_some(code)
}

/// The constant `s` starts with, as the last segment of a path such as
/// `crate::task::UNKNOWN_PRIORITY`.
fn constant(s: &str) -> Option<&str> {
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':'))
        .unwrap_or(s.len());
    let name = s[..end].rsplit("::").next()?;
    let shaped = name.len() > 1
        && name.starts_with(|c: char| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    shaped.then_some(name)
}

/// A file without its `mod tests`, whose codes are made up.
fn non_test(text: &str) -> &str {
    match text.find("\n#[cfg(test)]\nmod tests") {
        Some(at) => &text[..at],
        None => text,
    }
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out.sort();
    out
}
