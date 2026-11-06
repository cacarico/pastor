//! `head_address` at `pastor setup` of the head's unit. Without it an agent
//! on an `ssh` machine is never told where the head is, so its `pastor task
//! done` fails and every task there ends without a summary. Setup is the
//! moment someone is there to say where the head is, so it asks, or takes
//! `<user>@<this host's name>` when nobody can answer, and says which.

use std::path::Path;

use anyhow::Context;

use crate::config::{PastorConfig, Paths, create_private_dir};

/// The prompt, before the default in brackets.
const PROMPT: &str = "head_address, the ssh destination other machines reach this head by";

/// `<user>@<host>` for this machine: `$USER` (else `$LOGNAME`) and
/// `config::hostname`. `None` when either is unknown.
pub fn from_this_host() -> Option<String> {
    let user = ["USER", "LOGNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.trim().is_empty()))?;
    let host = crate::config::hostname();
    (host != "-").then(|| format!("{}@{host}", user.trim()))
}

/// Settle `head_address` in `paths`' pastor.toml before the head's unit
/// starts, and return the line that says what happened. `ask` is the
/// terminal to ask on, `None` with `--yes` or no terminal; `default` is
/// what an empty answer or no answer takes (`from_this_host`).
///
/// Nothing happens on a machine whose head is elsewhere (client.toml names
/// one): its unit runs a headless serve, and the address is the head's.
/// A `head_address` already set is kept.
pub fn settle(
    paths: &Paths,
    ask: Option<(&mut dyn std::io::BufRead, &mut dyn std::io::Write)>,
    default: Option<String>,
) -> anyhow::Result<Option<String>> {
    if crate::head::load(&crate::head::client_file(paths))?.is_some() {
        return Ok(None);
    }
    let path = paths.config_file();
    if let Some(kept) = PastorConfig::load(&path)?.head_address {
        return Ok(Some(format!("head_address is {kept} (pastor.toml)")));
    }
    let typed = match ask {
        Some((input, out)) => {
            let shown = default.as_deref().unwrap_or("none");
            write!(out, "{PROMPT} [{shown}]: ")
                .and_then(|()| out.flush())
                .context("write head_address prompt")?;
            let mut answer = String::new();
            input.read_line(&mut answer).context("read head_address")?;
            Some(answer.trim().to_string()).filter(|a| !a.is_empty())
        }
        None => None,
    };
    let (address, from) = match (typed, default) {
        (Some(typed), _) => (typed, "as given"),
        (None, Some(default)) => (default, "from this host's name"),
        (None, None) => {
            return Ok(Some(
                "head_address is unset and this host's name is unknown, so agents on ssh machines cannot run pastor task done; set it with pastor config edit".into(),
            ));
        }
    };
    write(&path, &address)?;
    Ok(Some(format!(
        "head_address = \"{address}\" in {} ({from}); agents on ssh machines reach the head there; change it with pastor config edit",
        path.display()
    )))
}

/// Set `head_address` in the pastor.toml at `path`, keeping the rest of the
/// file as it is, and only once the result loads.
fn write(path: &Path, address: &str) -> anyhow::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("parse {}", path.display()))?;
    doc["head_address"] = toml_edit::value(address);
    let text = doc.to_string();
    PastorConfig::parse(path, &text)?;
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths() -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        (tmp, paths)
    }

    fn config(paths: &Paths) -> PastorConfig {
        PastorConfig::load(&paths.config_file()).unwrap()
    }

    /// With nobody to ask (`--yes`, or no terminal), setup takes the
    /// address from this host's name, writes it beside what pastor.toml
    /// already holds, and says where it came from.
    #[test]
    fn with_nobody_to_ask_the_host_name_is_the_default() {
        let (_tmp, paths) = temp_paths();
        std::fs::create_dir_all(paths.config_file().parent().unwrap()).unwrap();
        std::fs::write(
            paths.config_file(),
            "# mine\ntick = \"5s\"\n\n[defaults]\ntimeout = \"1h\"\n",
        )
        .unwrap();
        let line = settle(&paths, None, Some("me@desk".into()))
            .unwrap()
            .unwrap();
        assert!(line.contains("head_address = \"me@desk\""), "{line}");
        assert!(line.contains("from this host's name"), "{line}");
        let text = std::fs::read_to_string(paths.config_file()).unwrap();
        assert!(text.starts_with("# mine\n"), "{text}");
        let c = config(&paths);
        assert_eq!(c.head_address.as_deref(), Some("me@desk"));
        assert_eq!(c.tick, "5s");
    }

    /// At a terminal setup asks, showing the default; an empty answer takes
    /// it, and anything else is used as given.
    #[test]
    fn at_a_terminal_setup_asks_and_an_empty_answer_takes_the_default() {
        for (answer, want, from) in [
            ("\n", "me@desk", "from this host's name"),
            ("me@head.example\n", "me@head.example", "as given"),
        ] {
            let (_tmp, paths) = temp_paths();
            let mut input = std::io::Cursor::new(answer.as_bytes());
            let mut out = Vec::new();
            let line = settle(&paths, Some((&mut input, &mut out)), Some("me@desk".into()))
                .unwrap()
                .unwrap();
            let prompt = String::from_utf8(out).unwrap();
            assert!(prompt.contains(PROMPT), "{prompt}");
            assert!(prompt.contains("[me@desk]"), "{prompt}");
            assert!(line.contains(from), "{line}");
            assert_eq!(config(&paths).head_address.as_deref(), Some(want));
        }
    }

    /// An answer that is no ssh destination is refused, and nothing is
    /// written.
    #[test]
    fn an_answer_that_is_no_ssh_destination_is_refused() {
        let (_tmp, paths) = temp_paths();
        let mut input = std::io::Cursor::new("me at desk\n".as_bytes());
        let mut out = Vec::new();
        let err = settle(&paths, Some((&mut input, &mut out)), None).unwrap_err();
        assert!(format!("{err:#}").contains("head_address"), "{err:#}");
        assert!(!paths.config_file().exists());
    }

    /// A `head_address` already set is kept and named; with no default and
    /// nobody to ask, the line says it stays unset and why that matters.
    #[test]
    fn a_set_address_is_kept_and_an_unknown_host_leaves_it_unset() {
        let (_tmp, paths) = temp_paths();
        std::fs::create_dir_all(paths.config_file().parent().unwrap()).unwrap();
        std::fs::write(paths.config_file(), "head_address = \"a@b\"\n").unwrap();
        let line = settle(&paths, None, Some("me@desk".into()))
            .unwrap()
            .unwrap();
        assert_eq!(line, "head_address is a@b (pastor.toml)");
        assert_eq!(config(&paths).head_address.as_deref(), Some("a@b"));

        let (_tmp, paths) = temp_paths();
        let line = settle(&paths, None, None).unwrap().unwrap();
        assert!(line.contains("unset"), "{line}");
        assert!(line.contains("pastor task done"), "{line}");
        assert!(!paths.config_file().exists());
    }

    /// On a machine whose head is elsewhere the unit runs a headless serve:
    /// setup neither asks nor writes.
    #[test]
    fn a_machine_with_a_head_elsewhere_is_left_alone() {
        let (_tmp, paths) = temp_paths();
        std::fs::create_dir_all(paths.config_file().parent().unwrap()).unwrap();
        std::fs::write(
            crate::head::client_file(&paths),
            "[head]\nssh = \"user@head.example\"\n",
        )
        .unwrap();
        let mut input = std::io::Cursor::new("x@y\n".as_bytes());
        let mut out = Vec::new();
        let got = settle(&paths, Some((&mut input, &mut out)), Some("me@desk".into())).unwrap();
        assert_eq!(got, None);
        assert!(out.is_empty());
        assert!(!paths.config_file().exists());
    }
}
