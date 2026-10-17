//! The head's `~/.ssh/authorized_keys`, edited only on lines pastor marks.
//!
//! Every line pastor writes ends in a marker comment, and pastor never
//! touches a line without one, so the user's own keys survive. Edits are
//! serialised by a flock on `<file>.lock` and land by rename, so two
//! machines joining in the same second both keep their line and a reader
//! never sees a half-written file.

use crate::cli::CliError;
use anyhow::Context;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// Who a marked line belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    /// A machine's own key (the shepherd's way in).
    Machine(String),
    /// The agent key of a machine.
    Agent(String),
    /// A one-time join token's key.
    Join(String),
}

impl Marker {
    /// The comment that ends the line.
    pub fn render(&self) -> String {
        match self {
            Marker::Machine(n) => format!("pastor:machine:{n}"),
            Marker::Agent(n) => format!("pastor:agent:{n}"),
            Marker::Join(id) => format!("pastor:join:{id}"),
        }
    }

    /// The name or id this marker carries.
    fn value(&self) -> &str {
        match self {
            Marker::Machine(n) | Marker::Agent(n) | Marker::Join(n) => n,
        }
    }

    /// Fail unless the value is `[A-Za-z0-9._-]+` and not `.` or `..`. A
    /// newline would start a second, unrestricted line; whitespace would
    /// split the marker so `has_marker` never finds it again.
    pub fn check(&self) -> anyhow::Result<()> {
        let v = self.value();
        if v.is_empty()
            || v == "."
            || v == ".."
            || !v
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        {
            return Err(CliError::err(
                "invalid_machine",
                format!("{v:?} cannot be a pastor marker: use letters, digits, '.', '_' and '-'"),
            ));
        }
        Ok(())
    }
}

/// The pastor path as it goes in `command="..."`, checked: absolute and
/// `[A-Za-z0-9._/-]` only, since sshd runs the command through the user's
/// shell. Shared with `bridge::authorized_key_line`.
pub fn check_pastor_path(pastor: &Path) -> anyhow::Result<String> {
    let pastor = pastor.display().to_string();
    if !pastor.starts_with('/')
        || !pastor
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c))
    {
        return Err(CliError::err(
            "invalid_path",
            format!(
                "{pastor:?} cannot go in an authorized_keys command: it must be absolute, with letters, digits, '.', '_', '/' and '-' only"
            ),
        ));
    }
    Ok(pastor)
}

/// The options that keep a key to its one command: no terminal, no rc
/// files, no forwarding. Shared with `bridge::authorized_key_line`.
pub fn restrict_options() -> &'static str {
    "no-pty,no-user-rc,no-port-forwarding,no-agent-forwarding,no-X11-forwarding"
}

/// `~/.ssh/authorized_keys`, or `PASTOR_AUTHORIZED_KEYS` (for tests).
pub fn default_file() -> PathBuf {
    if let Some(p) = std::env::var_os("PASTOR_AUTHORIZED_KEYS") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ssh")
        .join("authorized_keys")
}

/// One marked line: `restrict,command="<pastor> <args>",<options> <type>
/// <key> <marker>`. The key's own comment is dropped; the marker replaces it.
pub fn line(
    pastor: &Path,
    command_args: &str,
    key: &str,
    marker: &Marker,
) -> anyhow::Result<String> {
    line_until(pastor, command_args, key, marker, None)
}

/// `line`, with `expiry-time` when `until` is set, so sshd itself refuses
/// the key from then on (OpenSSH 8.2 and later), whether or not anything
/// took the line away yet.
pub fn line_until(
    pastor: &Path,
    command_args: &str,
    key: &str,
    marker: &Marker,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> anyhow::Result<String> {
    marker.check()?;
    let lines: Vec<&str> = key.lines().filter(|l| !l.trim().is_empty()).collect();
    let [key] = lines.as_slice() else {
        return Err(CliError::err(
            "invalid_key",
            "the key must be exactly one public key line",
        ));
    };
    let mut parts = key.split_whitespace();
    let (Some(kind), Some(b64)) = (parts.next(), parts.next()) else {
        return Err(CliError::err(
            "invalid_key",
            "a public key line is its type and its key, like ssh-ed25519 AAAA...",
        ));
    };
    let pastor = check_pastor_path(pastor)?;
    // sshd hands command= to the user's shell, so anything it reads as
    // syntax is live: only words, `=`, paths and spaces go through.
    if !command_args
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._ =/-".contains(c))
    {
        return Err(CliError::err(
            "invalid_args",
            format!(
                "{command_args:?} cannot go in an authorized_keys command: use letters, digits, '.', '_', '=', '/', '-' and spaces"
            ),
        ));
    }
    let expiry = until
        .map(|t| format!("expiry-time=\"{}\",", t.format("%Y%m%d%H%M%SZ")))
        .unwrap_or_default();
    Ok(format!(
        "restrict,{expiry}command=\"{pastor} {command_args}\",{} {kind} {b64} {}",
        restrict_options(),
        marker.render()
    ))
}

/// Whether `l` is a line with this marker: it is the last word.
fn has_marker(l: &str, marker: &str) -> bool {
    l.split_whitespace().last() == Some(marker)
}

/// The flock on `<file>.lock`, held until dropped. Also serialises pastor's
/// own `known_hosts` (`bootstrap::push`, `machine remove`).
pub(crate) fn lock(file: &Path) -> anyhow::Result<std::fs::File> {
    let mut name = file.as_os_str().to_owned();
    name.push(".lock");
    let path = PathBuf::from(name);
    if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
        ensure_dir(dir)?;
    }
    let f = crate::edit::open_lock(&path)?;
    loop {
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(f);
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(anyhow::Error::from(e).context(format!("lock {}", path.display())));
        }
    }
}

/// The directory, made 0700 if it was missing. An existing one is left as
/// the user set it.
fn ensure_dir(dir: &Path) -> anyhow::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))
}

/// Read, `edit` the lines, and write back by rename if they changed. The
/// caller holds the lock. A missing file is created, even if empty.
fn rewrite(
    file: &Path,
    edit: impl FnOnce(Vec<String>) -> (Vec<String>, bool),
) -> anyhow::Result<bool> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(anyhow::Error::from(e).context(format!("read {}", file.display()))),
    };
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let (lines, changed) = edit(lines);
    if !changed && file.exists() {
        return Ok(false);
    }
    let tmp = PathBuf::from(format!("{}.tmp.{}", file.display(), std::process::id()));
    let mut out = String::new();
    for l in &lines {
        out.push_str(l);
        out.push('\n');
    }
    let result = (|| -> anyhow::Result<()> {
        // A stale temp of this pid is ours to replace.
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(out.as_bytes())?;
        f.sync_all()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp, file).with_context(|| format!("rename onto {}", file.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    Ok(changed)
}

/// Replace the line with `marker`, or append `line`. Other markers' lines
/// with the same marker beyond the first are dropped.
pub fn upsert(file: &Path, marker: &Marker, line: &str) -> anyhow::Result<()> {
    marker.check()?;
    let _lock = lock(file)?;
    let m = marker.render();
    rewrite(file, |lines| {
        let mut out = Vec::with_capacity(lines.len() + 1);
        let mut placed = false;
        for l in lines {
            if has_marker(&l, &m) {
                if !placed {
                    out.push(line.to_string());
                    placed = true;
                }
            } else {
                out.push(l);
            }
        }
        if !placed {
            out.push(line.to_string());
        }
        (out, true)
    })?;
    Ok(())
}

/// Drop the line with `marker`. True if there was one.
pub fn remove(file: &Path, marker: &Marker) -> anyhow::Result<bool> {
    marker.check()?;
    let _lock = lock(file)?;
    let m = marker.render();
    rewrite(file, |lines| {
        let n = lines.len();
        let out: Vec<String> = lines.into_iter().filter(|l| !has_marker(l, &m)).collect();
        let changed = out.len() != n;
        (out, changed)
    })
}

/// Whether a line not marked with one of `except` holds the key `blob` (the
/// base64 word of a public key). A missing file holds none.
pub fn holds_key(file: &Path, blob: &str, except: &[Marker]) -> anyhow::Result<bool> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(anyhow::Error::from(e).context(format!("read {}", file.display()))),
    };
    let except: Vec<String> = except.iter().map(Marker::render).collect();
    Ok(text.lines().any(|l| {
        !except.iter().any(|m| has_marker(l, m)) && l.split_whitespace().any(|w| w == blob)
    }))
}

/// Drop the Machine and Agent lines of `name`.
pub fn remove_machine(file: &Path, name: &str) -> anyhow::Result<()> {
    let a = Marker::Machine(name.into());
    let b = Marker::Agent(name.into());
    a.check()?;
    let _lock = lock(file)?;
    let (a, b) = (a.render(), b.render());
    rewrite(file, |lines| {
        let n = lines.len();
        let out: Vec<String> = lines
            .into_iter()
            .filter(|l| !has_marker(l, &a) && !has_marker(l, &b))
            .collect();
        let changed = out.len() != n;
        (out, changed)
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOREIGN: &str = "ssh-ed25519 AAAA me@laptop";

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("authorized_keys");
        std::fs::write(&f, format!("{FOREIGN}\n")).unwrap();
        (d, f)
    }

    fn mk(m: &Marker, key: &str) -> String {
        line(Path::new("/bin/pastor"), "bridge --agent", key, m).unwrap()
    }

    #[test]
    fn line_shape_and_bad_keys() {
        let m = Marker::Join("j1".into());
        let l = mk(&m, "ssh-ed25519 KEY comment");
        assert!(l.starts_with("restrict,command=\"/bin/pastor bridge --agent\","));
        assert!(l.ends_with(" ssh-ed25519 KEY pastor:join:j1"));
        let e = line(Path::new("/p"), "x", "a b\nc d", &m).unwrap_err();
        assert_eq!(e.downcast_ref::<CliError>().unwrap().code, "invalid_key");
    }

    /// sshd's own clock stops the key: `expiry-time` in UTC, as OpenSSH
    /// reads it (`YYYYMMDDHHMMSSZ`).
    #[test]
    fn line_until_carries_the_expiry() {
        let m = Marker::Join("j1".into());
        let t = chrono::DateTime::parse_from_rfc3339("2026-10-17T13:04:05+02:00")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let l = line_until(
            Path::new("/bin/pastor"),
            "bridge",
            "ssh-ed25519 K",
            &m,
            Some(t),
        )
        .unwrap();
        assert!(
            l.starts_with(
                "restrict,expiry-time=\"20261017110405Z\",command=\"/bin/pastor bridge\","
            ),
            "{l}"
        );
        assert!(!mk(&m, "ssh-ed25519 K").contains("expiry-time"));
    }

    fn code(e: anyhow::Error) -> String {
        e.downcast_ref::<CliError>().unwrap().code.clone()
    }

    /// Every entry point refuses the marker, so no edit ever runs with it.
    fn marker_rejected_everywhere(value: &str) {
        let (_d, f) = setup();
        for m in [
            Marker::Machine(value.into()),
            Marker::Agent(value.into()),
            Marker::Join(value.into()),
        ] {
            let e = line(Path::new("/bin/pastor"), "bridge", "ssh-ed25519 K", &m).unwrap_err();
            assert_eq!(code(e), "invalid_machine", "{m:?}");
            let e = upsert(&f, &m, "ssh-ed25519 K x").unwrap_err();
            assert_eq!(code(e), "invalid_machine", "{m:?}");
            let e = remove(&f, &m).unwrap_err();
            assert_eq!(code(e), "invalid_machine", "{m:?}");
        }
        let e = remove_machine(&f, value).unwrap_err();
        assert_eq!(code(e), "invalid_machine");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), format!("{FOREIGN}\n"));
    }

    #[test]
    fn marker_with_newline_rejected() {
        // A newline in the marker would be a second, unrestricted key line.
        marker_rejected_everywhere("a\nssh-rsa X");
    }

    #[test]
    fn marker_with_space_rejected() {
        // A space would split the marker, so `remove` could never find it.
        marker_rejected_everywhere("a b");
        for bad in ["", ".", "..", "a\tb", "a/b", "a\"b", "a\\b"] {
            marker_rejected_everywhere(bad);
        }
        for ok in ["pi-1", "a.b_c", "...", ".hidden", "9"] {
            Marker::Machine(ok.into()).check().unwrap();
        }
    }

    #[test]
    fn args_with_shell_metachar_rejected() {
        let m = Marker::Join("j".into());
        for bad in [
            "$(x)", "`x`", "a;b", "a|b", "a&b", "'", "a\"b", "a\\b", "a\nb", "a\rb", "a\tb",
        ] {
            let e = line(Path::new("/bin/pastor"), bad, "ssh-ed25519 K", &m).unwrap_err();
            assert_eq!(code(e), "invalid_args", "{bad:?}");
        }
        let l = line(
            Path::new("/bin/pastor"),
            "bridge --agent --machine pi-1 --dir=/srv/x.y",
            "ssh-ed25519 K",
            &m,
        )
        .unwrap();
        assert!(l.contains("--dir=/srv/x.y\","), "{l}");
    }

    #[test]
    fn relative_or_tilde_path_rejected() {
        let m = Marker::Join("j".into());
        for bad in [
            "~/bin/pastor",
            "bin/pastor",
            "pastor",
            "/a b/pastor",
            "/bin/$HOME/pastor",
            "/bin/pastor;id",
            "/bin/pa\"stor",
            "/bin/pa\\stor",
            "/bin/pa\nstor",
        ] {
            let p = Path::new(bad);
            let e = line(p, "bridge", "ssh-ed25519 K", &m).unwrap_err();
            assert_eq!(code(e), "invalid_path", "{bad:?}");
            let e = crate::bridge::authorized_key_line(p, "pi-1", "ssh-ed25519 K").unwrap_err();
            assert_eq!(code(e), "invalid_path", "{bad:?}");
        }
        for ok in ["/bin/pastor", "/opt/pastor-0.9/bin/pastor_x.y", "/pastor"] {
            check_pastor_path(Path::new(ok)).unwrap();
        }
    }

    #[test]
    fn upsert_replaces_same_marker() {
        let (_d, f) = setup();
        let m = Marker::Machine("m1".into());
        upsert(&f, &m, &mk(&m, "ssh-ed25519 ONE")).unwrap();
        upsert(&f, &m, &mk(&m, "ssh-ed25519 TWO")).unwrap();
        let t = std::fs::read_to_string(&f).unwrap();
        assert_eq!(t.matches("pastor:machine:m1").count(), 1);
        assert!(t.contains("TWO") && !t.contains("ONE"));
    }

    #[test]
    fn foreign_lines_untouched() {
        let (_d, f) = setup();
        let m = Marker::Join("j".into());
        upsert(&f, &m, &mk(&m, "ssh-ed25519 K")).unwrap();
        assert!(std::fs::read_to_string(&f).unwrap().contains(FOREIGN));
        assert!(remove(&f, &m).unwrap());
        assert!(!remove(&f, &m).unwrap());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), format!("{FOREIGN}\n"));
    }

    #[test]
    fn remove_machine_drops_both() {
        let (_d, f) = setup();
        for m in [
            Marker::Machine("a".into()),
            Marker::Agent("a".into()),
            Marker::Machine("b".into()),
        ] {
            upsert(&f, &m, &mk(&m, "ssh-ed25519 K")).unwrap();
        }
        remove_machine(&f, "a").unwrap();
        let t = std::fs::read_to_string(&f).unwrap();
        assert!(!t.contains("pastor:machine:a") && !t.contains("pastor:agent:a"));
        assert!(t.contains("pastor:machine:b") && t.contains(FOREIGN));
    }

    #[test]
    fn concurrent_edits_keep_every_line() {
        let (_d, f) = setup();
        let hs: Vec<_> = (0..50)
            .map(|i| {
                let f = f.clone();
                std::thread::spawn(move || {
                    let m = Marker::Join(format!("j{i}"));
                    upsert(&f, &m, &mk(&m, "ssh-ed25519 K")).unwrap();
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let t = std::fs::read_to_string(&f).unwrap();
        assert_eq!(t.matches("pastor:join:").count(), 50);
        assert_eq!(t.lines().count(), 51);
        assert!(t.contains(FOREIGN));
    }

    #[test]
    fn file_mode_is_0600_and_new_dir_0700() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join(".ssh").join("authorized_keys");
        let m = Marker::Agent("x".into());
        upsert(&f, &m, &mk(&m, "ssh-ed25519 K")).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&f), 0o600);
        assert_eq!(mode(f.parent().unwrap()), 0o700);
    }
}
