//! The head's `~/.ssh/authorized_keys`, edited only on lines pastor marks.
//!
//! Every line pastor writes ends in a marker comment, and pastor never
//! touches a line without one, so the user's own keys survive. Edits are
//! serialised by a flock on `<file>.lock` and land by rename, so two machines
//! joining in the same second both keep their line and a reader never sees a
//! half-written file.

use crate::cli::CliError;
use anyhow::Context;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
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
    /// On a machine, the head's key: the head's way in.
    Head,
}

/// Whether `name` can go in a marker and in a `command=` that carries it:
/// a letter or digit, then `[A-Za-z0-9._-]*`. A newline would start a second,
/// unrestricted line; whitespace would split the marker so `has_marker` never
/// finds it again; a quote or a shell character would be live in the
/// command; a leading `-` would be read there as an option.
pub fn valid_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

impl Marker {
    /// The comment that ends the line.
    pub fn render(&self) -> String {
        match self {
            Marker::Machine(n) => format!("pastor:machine:{n}"),
            Marker::Agent(n) => format!("pastor:agent:{n}"),
            Marker::Join(id) => format!("pastor:join:{id}"),
            Marker::Head => "pastor:head".into(),
        }
    }

    /// The name or id this marker carries.
    fn value(&self) -> &str {
        match self {
            Marker::Machine(n) | Marker::Agent(n) | Marker::Join(n) => n,
            Marker::Head => "head",
        }
    }

    /// Fail unless the value is a `valid_name`.
    pub fn check(&self) -> anyhow::Result<()> {
        let v = self.value();
        if !valid_name(v) {
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
/// shell.
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

/// The options that keep a key to its one command: no terminal, no rc files,
/// no forwarding.
pub fn restrict_options() -> &'static str {
    "no-pty,no-user-rc,no-port-forwarding,no-agent-forwarding,no-X11-forwarding"
}

/// A public key kind as OpenSSH names them: `ssh-ed25519`, `ssh-rsa`,
/// `ecdsa-sha2-nistp256` and the like, and the `sk-...@openssh.com` ones.
fn key_kind(kind: &str) -> bool {
    let word = |w: &str| {
        !w.is_empty()
            && w.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    };
    match kind.strip_suffix("@openssh.com") {
        Some(rest) => rest.starts_with("sk-") && word(rest),
        None => (kind.starts_with("ssh-") || kind.starts_with("ecdsa-sha2-")) && word(kind),
    }
}

/// One public key line, checked: its kind, its base64 and, if there is one,
/// a comment without control characters. A line that starts with options (or
/// anything else) is refused: the first word must be a key kind, so a key
/// cannot bring its own `command=` or lift a restriction. Returns the kind and
/// the base64.
pub fn check_public_key(key: &str) -> anyhow::Result<(&str, &str)> {
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
    // Base64: at least a character, then at most two `=` of padding.
    let body = b64.trim_end_matches('=');
    let base64 = !body.is_empty()
        && b64.len() - body.len() <= 2
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/');
    if !key_kind(kind) || !base64 {
        return Err(CliError::err(
            "invalid_key",
            "a public key line is its type (ssh-ed25519, ssh-rsa, ...) and its base64 key, with no options before them",
        ));
    }
    if key.chars().any(|c| c.is_control() && c != '\t') {
        return Err(CliError::err(
            "invalid_key",
            "a public key line holds a control character",
        ));
    }
    Ok((kind, b64))
}

/// `~/.ssh/authorized_keys`, or `PASTOR_AUTHORIZED_KEYS` (for tests, which
/// never edit the real one).
pub fn default_file() -> PathBuf {
    file_in(std::env::var_os("PASTOR_AUTHORIZED_KEYS"), dirs::home_dir())
}

fn file_in(over: Option<std::ffi::OsString>, home: Option<PathBuf>) -> PathBuf {
    if let Some(p) = over {
        return PathBuf::from(p);
    }
    home.unwrap_or_else(|| PathBuf::from("."))
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

/// `line`, with sshd's own `expiry-time` when `until` is given: past it the
/// key gets no session, whether or not the line is still there.
pub fn line_until(
    pastor: &Path,
    command_args: &str,
    key: &str,
    marker: &Marker,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> anyhow::Result<String> {
    marker.check()?;
    let (kind, b64) = check_public_key(key)?;
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
    // `Z`: the time is UTC, whatever the head's zone.
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

/// The flock on `<file>.lock`, held until dropped.
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

/// The directory, made 0700 if it was missing. An existing one is left as the
/// user set it.
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

/// The file as pastor last saw it, to tell whether another program wrote it
/// since.
fn stamp(file: &Path) -> Option<(u64, Option<std::time::SystemTime>)> {
    let m = std::fs::metadata(file).ok()?;
    Some((m.len(), m.modified().ok()))
}

/// Refuse a link: pastor never writes through one, and a rename would
/// replace the link itself and leave its target as it was.
fn refuse_link(file: &Path) -> anyhow::Result<()> {
    if std::fs::symlink_metadata(file).is_ok_and(|m| m.file_type().is_symlink()) {
        // A relative target is relative to the link's directory.
        let target = std::fs::read_link(file)
            .map(|t| match file.parent() {
                Some(dir) if t.is_relative() => dir.join(t),
                _ => t,
            })
            .map(|t| t.display().to_string())
            .unwrap_or_default();
        return Err(CliError::err(
            "authorized_keys_linked",
            format!(
                "{} is a symlink to {target}, which pastor does not write through: edit {target} instead, or point PASTOR_AUTHORIZED_KEYS at it",
                file.display()
            ),
        ));
    }
    Ok(())
}

/// Read, `edit` the lines, and write back by rename if they changed. The
/// caller holds the lock. A missing file is created, even if empty. Lines
/// are kept as they were read, down to a `\r`. Another program does not take
/// the lock, so if the file changed between the read and the rename the edit
/// is made again on what is there now.
fn rewrite(file: &Path, edit: impl Fn(Vec<String>) -> (Vec<String>, bool)) -> anyhow::Result<bool> {
    rewrite_between(file, edit, &mut |_| {})
}

/// Where `rewrite_between` lets a test change the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The new text is written, and not yet renamed into place.
    Written,
    /// The file changed under the edit and it is about to be made again.
    Retry,
}

/// How many times an edit is made again before it gives up.
const TRIES: usize = 8;

/// `rewrite`, with `between` run at each `Step`.
fn rewrite_between(
    file: &Path,
    edit: impl Fn(Vec<String>) -> (Vec<String>, bool),
    between: &mut dyn FnMut(Step),
) -> anyhow::Result<bool> {
    for _ in 0..TRIES {
        // Each time round, as a link may have taken the file's place.
        refuse_link(file)?;
        let seen = stamp(file);
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(anyhow::Error::from(e).context(format!("read {}", file.display())));
            }
        };
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        if lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        let (lines, changed) = edit(lines);
        if !changed && file.exists() {
            return Ok(false);
        }
        let mut out = String::new();
        for l in &lines {
            out.push_str(l);
            out.push('\n');
        }
        let tmp = PathBuf::from(format!(
            "{}.tmp.{}-{}",
            file.display(),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let written = (|| -> anyhow::Result<()> {
            // Never through a link planted at the temp name; 0600 from the
            // start, whatever the umask.
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&tmp)
                .with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(out.as_bytes())?;
            f.sync_all()?;
            Ok(())
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        between(Step::Written);
        // A link may have taken the file's place since the read, and a rename
        // would replace it; one that has the same size and time as the file
        // read would pass the check below.
        if let Err(e) = refuse_link(file) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        if stamp(file) != seen {
            // Written to by someone else since the read: start again.
            let _ = std::fs::remove_file(&tmp);
            between(Step::Retry);
            continue;
        }
        if let Err(e) = std::fs::rename(&tmp, file) {
            let _ = std::fs::remove_file(&tmp);
            return Err(anyhow::Error::from(e).context(format!("rename onto {}", file.display())));
        }
        // The rename is only on disk once its directory is: a crash must not
        // bring back a file without a revoke that was reported done.
        if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty())
            && let Ok(d) = std::fs::File::open(dir)
        {
            let _ = d.sync_all();
        }
        return Ok(changed);
    }
    Err(CliError::err(
        "authorized_keys_busy",
        format!(
            "{} kept changing under the edit, {TRIES} times; something else is writing to it: try again",
            file.display()
        ),
    ))
}

/// Replace the line with `marker`, or append one, built by `line` from
/// `pastor`, `command_args` and the public `key`, so what lands in the file
/// is always one line that ends in the marker and can be found again. Further
/// lines with the same marker are dropped. True if the file changed: a line
/// that is already there, as it would be written, leaves the file alone.
pub fn upsert(
    file: &Path,
    marker: &Marker,
    pastor: &Path,
    command_args: &str,
    key: &str,
) -> anyhow::Result<bool> {
    upsert_until(file, marker, pastor, command_args, key, None)
}

/// `upsert` of a `line_until`.
pub fn upsert_until(
    file: &Path,
    marker: &Marker,
    pastor: &Path,
    command_args: &str,
    key: &str,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> anyhow::Result<bool> {
    let new = line_until(pastor, command_args, key, marker, until)?;
    let _lock = lock(file)?;
    let m = marker.render();
    rewrite(file, |lines| {
        let before = lines.clone();
        let mut out = Vec::with_capacity(lines.len() + 1);
        let mut placed = false;
        for l in lines {
            if has_marker(&l, &m) {
                if !placed {
                    out.push(new.clone());
                    placed = true;
                }
            } else {
                out.push(l);
            }
        }
        if !placed {
            out.push(new.clone());
        }
        let changed = out != before;
        (out, changed)
    })
}

/// Put the public `key`, as it is, on the one line marked `marker`: for a
/// key that may run anything, as the head's on a machine. True if the file
/// changed.
pub fn upsert_key(file: &Path, marker: &Marker, key: &str) -> anyhow::Result<bool> {
    marker.check()?;
    let (kind, b64) = check_public_key(key)?;
    let new = format!("{kind} {b64} {}", marker.render());
    let _lock = lock(file)?;
    let m = marker.render();
    rewrite(file, |lines| {
        let before = lines.clone();
        let mut out: Vec<String> = lines.into_iter().filter(|l| !has_marker(l, &m)).collect();
        out.push(new.clone());
        let changed = out != before;
        (out, changed)
    })
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

/// Whether `l` ends in a marker of pastor's, whoever it names.
fn has_any_marker(l: &str) -> bool {
    l.split_whitespace().last().is_some_and(|w| {
        ["pastor:machine:", "pastor:agent:", "pastor:join:"]
            .iter()
            .any(|p| w.strip_prefix(p).is_some_and(valid_name))
    })
}

/// The lines of `file` that `drop` names, as they are now: what an edit
/// would remove, for a dry run. A file that is not there has none.
fn marked_lines(file: &Path, drop: impl Fn(&str) -> bool) -> anyhow::Result<Vec<String>> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("read {}", file.display())));
        }
    };
    Ok(text
        .lines()
        .filter(|l| drop(l))
        .map(str::to_string)
        .collect())
}

/// Drop the lines matching `drop` under the lock; how many went.
fn remove_where(file: &Path, drop: impl Fn(&str) -> bool) -> anyhow::Result<usize> {
    // A file that is not there has nothing to drop; a link (even a dangling
    // one) is `rewrite`'s to refuse.
    if matches!(std::fs::symlink_metadata(file), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(0);
    }
    let _lock = lock(file)?;
    // The edit may run again when the file changes under it: the last run
    // is the one that counts.
    let removed = std::cell::Cell::new(0);
    rewrite(file, |lines| {
        let n = lines.len();
        let out: Vec<String> = lines.into_iter().filter(|l| !drop(l)).collect();
        removed.set(n - out.len());
        let changed = removed.get() > 0;
        (out, changed)
    })?;
    Ok(removed.get())
}

fn machine_markers(name: &str) -> anyhow::Result<(String, String)> {
    let a = Marker::Machine(name.into());
    a.check()?;
    Ok((a.render(), Marker::Agent(name.into()).render()))
}

/// Drop the Machine and Agent lines of `name`; how many went.
pub fn remove_machine(file: &Path, name: &str) -> anyhow::Result<usize> {
    let (a, b) = machine_markers(name)?;
    remove_where(file, |l| has_marker(l, &a) || has_marker(l, &b))
}

/// The Machine and Agent lines of `name` that `remove_machine` would drop.
pub fn machine_lines(file: &Path, name: &str) -> anyhow::Result<Vec<String>> {
    let (a, b) = machine_markers(name)?;
    marked_lines(file, |l| has_marker(l, &a) || has_marker(l, &b))
}

/// Drop every line pastor marked, whoever it names, and nothing else; how
/// many went.
pub fn remove_all(file: &Path) -> anyhow::Result<usize> {
    remove_where(file, has_any_marker)
}

/// The lines `remove_all` would drop.
pub fn all_lines(file: &Path) -> anyhow::Result<Vec<String>> {
    marked_lines(file, has_any_marker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const FOREIGN: &str = "ssh-ed25519 AAAA me@laptop";

    /// A temp dir with an `authorized_keys` of one line that is not pastor's.
    /// Nothing here touches the real file.
    fn setup() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("authorized_keys");
        std::fs::write(&f, format!("{FOREIGN}\n")).unwrap();
        (d, f)
    }

    fn mk(m: &Marker, key: &str) -> String {
        line(Path::new("/bin/pastor"), "bridge --agent", key, m).unwrap()
    }

    /// `upsert` the way a caller would: the pastor path and args are fixed.
    fn up(f: &Path, m: &Marker, key: &str) -> anyhow::Result<()> {
        upsert(f, m, Path::new("/bin/pastor"), "bridge --agent", key).map(|_| ())
    }

    fn code(e: anyhow::Error) -> String {
        e.downcast_ref::<CliError>().unwrap().code.clone()
    }

    #[test]
    fn the_file_is_the_override_or_the_users_own() {
        assert_eq!(
            file_in(Some("/t/keys".into()), Some("/home/me".into())),
            PathBuf::from("/t/keys")
        );
        assert_eq!(
            file_in(None, Some("/home/me".into())),
            PathBuf::from("/home/me/.ssh/authorized_keys")
        );
    }

    #[test]
    fn line_shape_and_bad_keys() {
        let m = Marker::Join("j1".into());
        let l = mk(&m, "ssh-ed25519 KEY comment");
        assert_eq!(
            l,
            "restrict,command=\"/bin/pastor bridge --agent\",no-pty,no-user-rc,no-port-forwarding,no-agent-forwarding,no-X11-forwarding ssh-ed25519 KEY pastor:join:j1"
        );
        for bad in [
            "a b\nc d",
            "",
            "ssh-ed25519",
            // Options in front of the key, and a first word that is no kind.
            "command=\"id\" ssh-ed25519 AAAA",
            "rm AAAA",
        ] {
            let e = line(Path::new("/p"), "x", bad, &m).unwrap_err();
            assert_eq!(code(e), "invalid_key", "{bad:?}");
        }
    }

    #[test]
    fn args_with_shell_metacharacters_are_rejected() {
        let m = Marker::Join("j".into());
        for bad in [
            "$(x)", "`x`", "a;b", "a|b", "a&b", "'", "a\"b", "a\\b", "a\nb", "a\rb", "a\tb",
        ] {
            let e = line(Path::new("/bin/pastor"), bad, "ssh-ed25519 K", &m).unwrap_err();
            assert_eq!(code(e), "invalid_args", "{bad:?}");
        }
    }

    #[test]
    fn a_relative_or_odd_pastor_path_is_rejected() {
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
            let e = line(Path::new(bad), "bridge", "ssh-ed25519 K", &m).unwrap_err();
            assert_eq!(code(e), "invalid_path", "{bad:?}");
        }
        for ok in ["/bin/pastor", "/opt/pastor-0.9/bin/pastor_x.y", "/pastor"] {
            check_pastor_path(Path::new(ok)).unwrap();
        }
    }

    #[test]
    fn valid_name_rejects_shell_metacharacters() {
        for bad in [
            "", ".", "..", "a b", "a\tb", "a\nb", "a\rb", "a;b", "a|b", "a&b", "a$b", "$(x)",
            "`x`", "a'b", "a\"b", "a\\b", "a/b", "a*b", "a>b", "a<b", "a#b", "é", "a,b", "a=b",
            "-x y", "-x", "--agent", "-lead", "_x", ".x", "-",
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        for ok in ["pi-1", "a.b_c", "9", "Build-01", "x", "a-", "a..b"] {
            assert!(valid_name(ok), "{ok:?}");
        }
    }

    /// Every entry point refuses a marker that is not a valid name, so no
    /// edit ever runs with it and the file stays as it was.
    #[test]
    fn a_bad_marker_is_refused_everywhere() {
        let (_d, f) = setup();
        for value in [
            "a\nssh-rsa X",
            "a b",
            "",
            ".",
            "..",
            "a/b",
            "a\"b",
            "-x",
            "--agent",
        ] {
            for m in [
                Marker::Machine(value.into()),
                Marker::Agent(value.into()),
                Marker::Join(value.into()),
            ] {
                let e = line(Path::new("/bin/pastor"), "bridge", "ssh-ed25519 K", &m).unwrap_err();
                assert_eq!(code(e), "invalid_machine", "{m:?}");
                let e = up(&f, &m, "ssh-ed25519 K x").unwrap_err();
                assert_eq!(code(e), "invalid_machine", "{m:?}");
                let e = remove(&f, &m).unwrap_err();
                assert_eq!(code(e), "invalid_machine", "{m:?}");
            }
            let e = remove_machine(&f, value).unwrap_err();
            assert_eq!(code(e), "invalid_machine");
        }
        assert_eq!(std::fs::read_to_string(&f).unwrap(), format!("{FOREIGN}\n"));
    }

    /// `upsert` says whether the file changed: a first line and a new key
    /// do, the same line again does not.
    #[test]
    fn upsert_says_whether_it_changed_the_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("authorized_keys");
        let m = Marker::Agent("pi-1".into());
        let put =
            |key: &str| upsert(&f, &m, Path::new("/bin/pastor"), "bridge --agent", key).unwrap();
        assert!(put("ssh-ed25519 AAAAone"));
        assert!(!put("ssh-ed25519 AAAAone"), "the same line is no change");
        assert!(put("ssh-ed25519 AAAAtwo"));
        assert_eq!(std::fs::read_to_string(&f).unwrap().lines().count(), 1);
    }

    #[test]
    fn upsert_replaces_same_marker() {
        let (_d, f) = setup();
        let m = Marker::Machine("m1".into());
        up(&f, &m, "ssh-ed25519 ONE").unwrap();
        up(&f, &m, "ssh-ed25519 TWO").unwrap();
        let t = std::fs::read_to_string(&f).unwrap();
        assert_eq!(t.matches("pastor:machine:m1").count(), 1);
        assert!(t.contains("TWO") && !t.contains("ONE"));
        // A duplicate left by hand collapses to the one line.
        std::fs::write(
            &f,
            format!(
                "{FOREIGN}\n{}\n{}\n",
                mk(&m, "ssh-ed25519 A"),
                mk(&m, "ssh-ed25519 B")
            ),
        )
        .unwrap();
        up(&f, &m, "ssh-ed25519 C").unwrap();
        let t = std::fs::read_to_string(&f).unwrap();
        assert_eq!(t.matches("pastor:machine:m1").count(), 1, "{t}");
        assert_eq!(t.lines().count(), 2);
    }

    #[test]
    fn foreign_lines_untouched() {
        let (_d, f) = setup();
        // A line that merely mentions a marker, and one with no trailing
        // newline, are the user's.
        let mention = "ssh-ed25519 BBBB see pastor:join:j and more";
        std::fs::write(&f, format!("{FOREIGN}\n{mention}\n# a note")).unwrap();
        let m = Marker::Join("j".into());
        up(&f, &m, "ssh-ed25519 K").unwrap();
        let t = std::fs::read_to_string(&f).unwrap();
        assert!(t.contains(FOREIGN) && t.contains(mention) && t.contains("# a note"));
        assert!(remove(&f, &m).unwrap());
        assert!(!remove(&f, &m).unwrap());
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            format!("{FOREIGN}\n{mention}\n# a note\n")
        );
    }

    #[test]
    fn remove_machine_drops_both() {
        let (_d, f) = setup();
        for m in [
            Marker::Machine("a".into()),
            Marker::Agent("a".into()),
            Marker::Machine("b".into()),
            Marker::Join("a".into()),
        ] {
            up(&f, &m, "ssh-ed25519 K").unwrap();
        }
        remove_machine(&f, "a").unwrap();
        let t = std::fs::read_to_string(&f).unwrap();
        assert!(!t.contains("pastor:machine:a") && !t.contains("pastor:agent:a"));
        assert!(t.contains("pastor:machine:b") && t.contains(FOREIGN));
        assert!(t.contains("pastor:join:a"), "a join id is not a machine");
    }

    #[test]
    fn concurrent_edits_keep_every_line() {
        let (_d, f) = setup();
        let hs: Vec<_> = (0..50)
            .map(|i| {
                let f = f.clone();
                std::thread::spawn(move || {
                    let m = Marker::Join(format!("j{i}"));
                    up(&f, &m, "ssh-ed25519 K").unwrap();
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
    fn file_mode_is_0600() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join(".ssh").join("authorized_keys");
        let m = Marker::Agent("x".into());
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        up(&f, &m, "ssh-ed25519 K").unwrap();
        assert_eq!(mode(&f), 0o600);
        assert_eq!(mode(f.parent().unwrap()), 0o700, "a new dir is private");
        // A file the user had at a looser mode is replaced at 0600.
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        up(&f, &m, "ssh-ed25519 L").unwrap();
        assert_eq!(mode(&f), 0o600);
        // No temp file is left beside it.
        let names: Vec<_> = std::fs::read_dir(f.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.contains(".tmp.")), "{names:?}");
    }

    /// What `upsert` writes is built by `line`, so it is one line that ends
    /// in the marker: a key or args that would add a second line, or hide the
    /// marker, are refused and the file stays as it was.
    #[test]
    fn upsert_writes_only_a_line_it_built() {
        let (_d, f) = setup();
        let m = Marker::Join("j".into());
        let e = up(
            &f,
            &m,
            "ssh-ed25519 K pastor:join:j\nssh-ed25519 EVIL attacker",
        )
        .unwrap_err();
        assert_eq!(code(e), "invalid_key");
        let e = upsert(
            &f,
            &m,
            Path::new("/bin/pastor"),
            "bridge\nx",
            "ssh-ed25519 K",
        )
        .unwrap_err();
        assert_eq!(code(e), "invalid_args");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), format!("{FOREIGN}\n"));
        // What it did write is found again, so it can be revoked.
        up(&f, &m, "ssh-ed25519 K").unwrap();
        assert!(remove(&f, &m).unwrap());
    }

    /// A linked file is refused, naming the target, and neither the link nor
    /// the target changes: a rename would replace the link and leave the
    /// target without the edit.
    #[test]
    fn a_symlinked_file_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real_keys");
        std::fs::write(&real, format!("{FOREIGN}\n")).unwrap();
        let link = d.path().join("authorized_keys");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let m = Marker::Agent("x".into());
        for (f, target) in [
            (&link, "real_keys"),
            (&d.path().join("dangling"), "nowhere"),
        ] {
            if f != &link {
                std::os::unix::fs::symlink(d.path().join("nowhere"), f).unwrap();
            }
            for result in [
                up(f, &m, "ssh-ed25519 K"),
                remove(f, &m).map(drop),
                remove_machine(f, "x").map(drop),
            ] {
                let e = result.unwrap_err();
                let said = e.to_string();
                assert!(
                    said.contains(&format!("symlink to {}", d.path().join(target).display())),
                    "{said}"
                );
                assert_eq!(code(e), "authorized_keys_linked");
            }
            assert!(
                std::fs::symlink_metadata(f)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            format!("{FOREIGN}\n")
        );
        assert!(!d.path().join("nowhere").exists());
    }

    /// The user's own lines keep their bytes, line endings included.
    #[test]
    fn lines_keep_their_endings() {
        let (_d, f) = setup();
        let own = "ssh-ed25519 AAAA me@laptop\r\nssh-rsa BBBB other\r\n";
        std::fs::write(&f, own).unwrap();
        let m = Marker::Join("j".into());
        up(&f, &m, "ssh-ed25519 K").unwrap();
        let t = std::fs::read_to_string(&f).unwrap();
        assert!(t.starts_with(own), "{t:?}");
        assert!(remove(&f, &m).unwrap());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), own);
    }

    /// A line another program appends while pastor edits is not lost: the
    /// edit is made again on what is there.
    #[test]
    fn a_line_appended_during_an_edit_is_kept() {
        let (_d, f) = setup();
        let m = Marker::Join("j".into());
        let new = mk(&m, "ssh-ed25519 K");
        let mut appended = false;
        rewrite_between(
            &f,
            |mut lines| {
                lines.push(new.clone());
                (lines, true)
            },
            &mut |step| {
                if step == Step::Written && !appended {
                    appended = true;
                    let mut t = std::fs::read_to_string(&f).unwrap();
                    t.push_str("ssh-ed25519 LATE ssh-copy-id\n");
                    std::fs::write(&f, t).unwrap();
                }
            },
        )
        .unwrap();
        let t = std::fs::read_to_string(&f).unwrap();
        assert!(t.contains("LATE ssh-copy-id"), "{t}");
        assert_eq!(t.matches("pastor:join:j").count(), 1, "{t}");
        assert!(t.contains(FOREIGN));
    }

    /// A relative link target is printed from the link's directory, so the
    /// message names a file that can be found.
    #[test]
    fn a_relative_link_names_its_target_in_full() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("real_keys"), "").unwrap();
        let link = d.path().join("authorized_keys");
        std::os::unix::fs::symlink("real_keys", &link).unwrap();
        let e = up(&link, &Marker::Agent("x".into()), "ssh-ed25519 K").unwrap_err();
        let said = e.to_string();
        assert!(
            said.contains(&format!(
                "symlink to {}",
                d.path().join("real_keys").display()
            )),
            "{said}"
        );
        assert!(said.contains(&format!(
            "edit {} instead",
            d.path().join("real_keys").display()
        )));
    }

    /// A link that takes the file's place while the edit is made is refused,
    /// in both places it can be seen: just before the rename (even when the
    /// link's target has the size and time of the file read, which the
    /// stamp cannot tell), and when the edit starts again.
    #[test]
    fn a_link_that_appears_mid_edit_is_refused() {
        let m = Marker::Join("j".into());
        let new = mk(&m, "ssh-ed25519 K");
        let edit = |mut lines: Vec<String>| {
            lines.push(new.clone());
            (lines, true)
        };
        let swap_for_a_link = |f: &Path| {
            // The same bytes and the same time, so only the link gives it away.
            let other = f.with_file_name("elsewhere");
            std::fs::copy(f, &other).unwrap();
            let mtime = std::fs::metadata(f).unwrap().modified().unwrap();
            std::fs::File::options()
                .write(true)
                .open(&other)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
            std::fs::remove_file(f).unwrap();
            std::os::unix::fs::symlink(&other, f).unwrap();
        };
        // Before the rename.
        let (d, f) = setup();
        let e = rewrite_between(&f, edit, &mut |step| {
            if step == Step::Written {
                swap_for_a_link(&f);
            }
        })
        .unwrap_err();
        assert_eq!(code(e), "authorized_keys_linked");
        assert!(
            std::fs::symlink_metadata(&f)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("elsewhere")).unwrap(),
            format!("{FOREIGN}\n"),
            "the target is as it was"
        );
        assert!(
            std::fs::read_dir(d.path()).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp.")),
            "no temp file is left"
        );
        // When the edit starts again: the file is appended to (so it is read
        // again) and then replaced by a link before the second time round.
        let (_d, f) = setup();
        let mut first = true;
        let edits = std::cell::Cell::new(0);
        let counted = |lines: Vec<String>| {
            edits.set(edits.get() + 1);
            edit(lines)
        };
        let e = rewrite_between(&f, counted, &mut |step| match step {
            Step::Written if first => {
                first = false;
                let mut t = std::fs::read_to_string(&f).unwrap();
                t.push_str("ssh-ed25519 LATE x\n");
                std::fs::write(&f, t).unwrap();
            }
            Step::Retry => swap_for_a_link(&f),
            Step::Written => {}
        })
        .unwrap_err();
        assert_eq!(code(e), "authorized_keys_linked");
        assert_eq!(edits.get(), 1, "the edit is not made again through a link");
    }

    /// A file that keeps changing under the edit ends it with an error of
    /// its own, after a bounded number of tries.
    #[test]
    fn a_file_that_never_settles_is_busy() {
        let (_d, f) = setup();
        let m = Marker::Join("j".into());
        let new = mk(&m, "ssh-ed25519 K");
        let mut tries = 0;
        let e = rewrite_between(
            &f,
            |mut lines| {
                lines.push(new.clone());
                (lines, true)
            },
            &mut |step| {
                if step == Step::Written {
                    tries += 1;
                    let mut t = std::fs::read_to_string(&f).unwrap();
                    t.push_str("ssh-ed25519 MORE x\n");
                    std::fs::write(&f, t).unwrap();
                }
            },
        )
        .unwrap_err();
        assert_eq!(code(e), "authorized_keys_busy");
        assert_eq!(tries, TRIES);
        assert!(!std::fs::read_to_string(&f).unwrap().contains("pastor:join"));
    }
}
