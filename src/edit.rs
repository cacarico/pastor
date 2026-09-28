//! `pastor job|flock|config edit`: a config file in the user's editor, the
//! way `kubectl edit` does it. The editor gets a temp copy; the copy is
//! checked the way the head loads the file, and only a valid edit replaces
//! it, atomically. An invalid one is reopened with the error on top, or kept
//! aside with the file left as it was.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::cli::CliError;
use crate::config::flock::Flock;
use crate::config::job::{Job, check_name, job_path};
use crate::config::{PastorConfig, Paths};
use crate::connector::{Builtins, Catalog, ConnectorCatalog};

/// What a finished edit did to the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The edit was valid and replaced the file.
    Saved,
    /// The editor left the text as it was; nothing was written.
    Unchanged,
}

/// Lines pastor puts on top of a reopened copy start with this, and are
/// taken off again before the copy is checked.
const MARK: &str = "# pastor:";

/// `$VISUAL`, else `$EDITOR`, else `vi`. Empty values count as unset.
pub fn editor() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "vi".into())
}

/// A config file the edit commands work on, as a request names it: `flock`,
/// `config` or `job:<name>`. The head resolves it under its own config dir,
/// so a CLI on another machine edits the head's file, not its own copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigFile {
    Flock,
    Config,
    Job(String),
}

impl std::str::FromStr for ConfigFile {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<ConfigFile> {
        match s {
            "flock" => Ok(ConfigFile::Flock),
            "config" => Ok(ConfigFile::Config),
            _ => match s.strip_prefix("job:") {
                Some(name) => Ok(ConfigFile::Job(name.to_string())),
                None => Err(CliError::err(
                    "invalid_file",
                    format!(
                        "{s:?} is not a file pastor edits; it takes flock, config or job:<name>"
                    ),
                )),
            },
        }
    }
}

impl std::fmt::Display for ConfigFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigFile::Flock => f.write_str("flock"),
            ConfigFile::Config => f.write_str("config"),
            ConfigFile::Job(name) => write!(f, "job:{name}"),
        }
    }
}

impl ConfigFile {
    /// Where the file is under `paths`. A job file must exist, and its name
    /// is checked before it is joined under the jobs dir: `"../pastor"`
    /// would resolve outside it. flock.toml and pastor.toml may be missing;
    /// an edit creates them.
    pub fn path(&self, paths: &Paths) -> anyhow::Result<PathBuf> {
        match self {
            ConfigFile::Flock => Ok(paths.flock_file()),
            ConfigFile::Config => Ok(paths.config_file()),
            ConfigFile::Job(name) => {
                check_name(name).map_err(|e| CliError::err("job_not_found", e))?;
                let path = job_path(&paths.jobs_dir(), name);
                if !path.exists() {
                    return Err(CliError::err(
                        "job_not_found",
                        format!("no job file {}", path.display()),
                    ));
                }
                Ok(path)
            }
        }
    }

    /// How the head loads the file, as a check of new text: a job against
    /// pastor.toml's `[defaults]` and the connectors installed under `paths`.
    pub fn checker(&self, paths: &Paths) -> anyhow::Result<Check> {
        let path = self.path(paths)?;
        Ok(match self {
            ConfigFile::Flock => Box::new(move |text: &str| {
                Flock::parse(&path, text)
                    .map(|_| ())
                    .map_err(|e| format!("{e:#}"))
            }),
            ConfigFile::Config => Box::new(move |text: &str| {
                PastorConfig::parse(&path, text)
                    .map(|_| ())
                    .map_err(|e| format!("{e:#}"))
            }),
            ConfigFile::Job(name) => {
                let name = name.clone();
                let defaults = PastorConfig::load(&paths.config_file())?.defaults;
                let catalog: Box<dyn Catalog> = match ConnectorCatalog::load(paths) {
                    Ok(c) => Box::new(c),
                    Err(_) => Box::new(Builtins),
                };
                Box::new(move |text: &str| {
                    Job::parse(text, &name, &defaults, catalog.as_ref()).map(|_| ())
                })
            }
        })
    }
}

/// A check of a file's new text: why it would not load, if it would not.
pub type Check = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// A stable digest of a file's text, for `put`'s conflict check across the
/// socket. FNV-1a, not `DefaultHasher`: that one may change between Rust
/// releases, and the CLI and the head need not be built by the same one.
/// It guards against a lost update, not an attacker.
pub fn hash(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The text of `target` (or of the file its symlink points to), empty when
/// it is missing, with its `hash`: what an edit starts from.
pub fn get(target: &Path) -> anyhow::Result<(String, String)> {
    let text = read_or_empty(&resolve(target)?)?;
    let h = hash(&text);
    Ok((text, h))
}

/// Check `text` with `check` and, when it passes, replace `target` with it,
/// atomically: the one check-and-write that both the head (`FilePut`) and
/// an edit with no head go through. Refused with `invalid_edit` (the
/// message is the check's error alone) or with `edit_conflict` when the
/// file's hash is no longer `base_hash`.
///
/// `target` may be missing (it is created) or a symlink (its target is
/// written, the link stays). The write is a temp file beside the real one,
/// then a rename, so a reader sees the old file or the new one and never
/// half of it. The conflict check runs right before the rename, both under
/// an advisory lock on `.<file>.lock` beside it, so two pastor edits never
/// interleave; a writer that does not take the lock can still slip in
/// between the two.
pub fn put(
    target: &Path,
    text: &str,
    base_hash: &str,
    check: &dyn Fn(&str) -> Result<(), String>,
) -> anyhow::Result<()> {
    check(text).map_err(|e| CliError::err("invalid_edit", e))?;
    let real = resolve(target)?;
    let (dir, name) = beside(&real);
    // Only a missing parent is made, private. An existing one keeps its
    // mode: through a symlink it may be a dotfiles dir that is not ours.
    if std::fs::symlink_metadata(dir).is_err() {
        crate::config::create_private_dir(dir)?;
    }
    let _lock = lock(&dir.join(format!(".{name}.lock")))?;
    let mode = std::fs::metadata(&real).map_or(0o600, |m| m.permissions().mode() & 0o7777);
    let tmp = write_temp(&real, text, mode)?;
    let replaced = if hash(&read_or_empty(&real)?) != base_hash {
        Err(CliError::err(
            "edit_conflict",
            format!(
                "{} changed while it was being edited; it was left as it is now",
                target.display()
            ),
        ))
    } else {
        std::fs::rename(&tmp, &real).with_context(|| format!("rename to {}", real.display()))
    };
    if let Err(e) = replaced {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Edit `target` in `editor` with no head: the file as it is here, saved
/// through `put` with `check`.
pub async fn edit_here(
    target: &Path,
    editor: &str,
    check: &dyn Fn(&str) -> Result<(), String>,
    reopen: &mut dyn FnMut(&str) -> bool,
) -> anyhow::Result<Outcome> {
    let (original, base) = get(target)?;
    edit(
        target,
        &original,
        editor,
        async |text: &str| put(target, text, &base, check),
        reopen,
    )
    .await
}

/// Edit `original` in `editor` until `save` takes it or the user gives up.
/// `save` refuses an invalid edit with `invalid_edit`, whose message is
/// shown and `reopen` is asked about, and a stale one with
/// `edit_conflict`; `put` does both, here or on the head. `label` names the
/// file in messages and the temp copy. The editor always runs here, on the
/// caller's machine.
pub async fn edit(
    label: &Path,
    original: &str,
    editor: &str,
    mut save: impl AsyncFnMut(&str) -> anyhow::Result<()>,
    reopen: &mut dyn FnMut(&str) -> bool,
) -> anyhow::Result<Outcome> {
    let copy = temp_copy(label, original)?;
    let mut last_rejected: Option<String> = None;
    loop {
        if let Err(e) = run_editor(editor, &copy) {
            let _ = std::fs::remove_file(&copy);
            return Err(e);
        }
        let raw =
            std::fs::read_to_string(&copy).with_context(|| format!("read {}", copy.display()))?;
        // Only a reopened copy carries the error block; on the first pass a
        // leading `# pastor:` line is the user's own and stays.
        let text = if last_rejected.is_some() {
            strip_marks(&raw)
        } else {
            raw
        };
        if text == original {
            let _ = std::fs::remove_file(&copy);
            return Ok(Outcome::Unchanged);
        }
        let error = if last_rejected.as_deref() == Some(text.as_str()) {
            // Saved as it was reopened: the user is done trying.
            None
        } else {
            match save(&text).await {
                Ok(()) => {
                    let _ = std::fs::remove_file(&copy);
                    return Ok(Outcome::Saved);
                }
                Err(e) => match e.downcast::<CliError>() {
                    Ok(e) if e.code == "invalid_edit" => Some(e.message),
                    Ok(e) if e.code == "edit_conflict" => {
                        return Err(CliError::err(
                            "edit_conflict",
                            format!("{}, and the edit is kept in {}", e.message, copy.display()),
                        ));
                    }
                    Ok(e) => return Err(e.into()),
                    Err(e) => return Err(e),
                },
            }
        };
        if let Some(e) = &error {
            eprintln!("{} is not valid: {e}", label.display());
            if reopen(e) {
                std::fs::write(&copy, with_marks(label, e, &text))
                    .with_context(|| format!("write {}", copy.display()))?;
                last_rejected = Some(text);
                continue;
            }
        }
        std::fs::write(&copy, &text).with_context(|| format!("write {}", copy.display()))?;
        let why = error.map_or_else(
            || "the edit is still not valid".to_string(),
            |e| format!("the edit is not valid: {e}"),
        );
        return Err(CliError::err(
            "invalid_edit",
            format!(
                "{}: {why}; the file is unchanged and the edit is kept in {}",
                label.display(),
                copy.display()
            ),
        ));
    }
}

/// The file to read and write: `target` itself, or where its symlink points.
fn resolve(target: &Path) -> anyhow::Result<PathBuf> {
    if std::fs::symlink_metadata(target).is_err() {
        return Ok(target.to_path_buf());
    }
    std::fs::canonicalize(target).with_context(|| format!("resolve {}", target.display()))
}

fn read_or_empty(path: &Path) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// A private copy in the temp dir, named after the file so the editor knows
/// it is TOML. Not beside the file: a stray `*.toml` in the jobs dir is a job.
fn temp_copy(target: &Path, text: &str) -> anyhow::Result<PathBuf> {
    let name = target
        .file_name()
        .map_or_else(|| "edit.toml".into(), |n| n.to_string_lossy().into_owned());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path =
        std::env::temp_dir().join(format!("pastor-edit-{}-{stamp}-{name}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("create {}", path.display()))?;
    f.write_all(text.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Run the editor on `file` as git does: through `sh`, so `$EDITOR` may
/// carry arguments (`code --wait`).
fn run_editor(editor: &str, file: &Path) -> anyhow::Result<()> {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(editor)
        .arg(file)
        .status()
        .map_err(|e| CliError::err("editor_failed", format!("cannot run {editor}: {e}")))?;
    if !status.success() {
        return Err(CliError::err(
            "editor_failed",
            format!("{editor} exited with {status}; nothing was changed"),
        ));
    }
    Ok(())
}

/// The directory `real` is in, and its file name.
fn beside(real: &Path) -> (&Path, String) {
    let dir = real
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = real
        .file_name()
        .map_or_else(|| "edit".into(), |n| n.to_string_lossy().into_owned());
    (dir, name)
}

/// The edit lock for the real file `real`: the one `put` holds while it
/// writes, so any other writer of the file can take turns with it.
pub(crate) fn lock_file(real: &Path) -> anyhow::Result<std::fs::File> {
    let (dir, name) = beside(real);
    lock(&dir.join(format!(".{name}.lock")))
}

/// An exclusive `flock` on `path`, created if missing, held until the file
/// is dropped. The lock file is left in place: removing it would let a
/// waiter lock a file nobody else sees any more.
fn lock(path: &Path) -> anyhow::Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    loop {
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(f);
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e).with_context(|| format!("lock {}", path.display()));
        }
    }
}

/// `text` in a new file beside `real`, with `mode`, ready to be renamed
/// over it. The name is unique and the file is created exclusively without
/// following a symlink, so nothing planted at a temp name can redirect the
/// write or the chmod. Hidden and not `*.toml`, so no loader picks it up.
/// Removed again if anything fails.
fn write_temp(real: &Path, text: &str, mode: u32) -> anyhow::Result<PathBuf> {
    let (dir, name) = beside(real);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    for attempt in 0..16 {
        let tmp = dir.join(format!(
            ".{name}.{}-{stamp}-{attempt}.tmp",
            std::process::id()
        ));
        let mut f = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("create {}", tmp.display())),
        };
        let written = f
            .write_all(text.as_bytes())
            .and_then(|()| f.set_permissions(std::fs::Permissions::from_mode(mode)))
            .and_then(|()| f.sync_all());
        if let Err(e) = written {
            drop(f);
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("write {}", tmp.display()));
        }
        return Ok(tmp);
    }
    Err(anyhow::anyhow!(
        "no free temp name for {} in {}",
        name,
        dir.display()
    ))
}

/// `text` with the error on top, as comments pastor takes off again.
fn with_marks(target: &Path, error: &str, text: &str) -> String {
    let mut out = format!(
        "{MARK} {} is not valid and was not saved:\n",
        target.display()
    );
    for line in error.lines() {
        out.push_str(&format!("{MARK}   {line}\n"));
    }
    out.push_str(&format!(
        "{MARK} fix it and save, or quit without saving to give up.\n"
    ));
    out.push_str(text);
    out
}

/// `text` without the leading lines `with_marks` added.
fn strip_marks(text: &str) -> String {
    let mut rest = text;
    while rest.starts_with(MARK) {
        rest = rest.split_once('\n').map_or("", |(_, r)| r);
    }
    rest.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_come_off_and_only_leading_ones() {
        let marked = with_marks(Path::new("j.toml"), "bad\nworse", "a = 1\n# pastor: mine\n");
        assert!(marked.starts_with("# pastor: j.toml is not valid"));
        assert!(marked.contains("# pastor:   worse\n"));
        assert_eq!(strip_marks(&marked), "a = 1\n# pastor: mine\n");
        assert_eq!(strip_marks("a = 1\n"), "a = 1\n");
    }
}
