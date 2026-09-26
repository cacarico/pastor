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

/// Edit `target` in `editor` until the text passes `check` or the user gives
/// up. `target` may be missing (it is created) or a symlink (its target is
/// written, the link stays). `reopen` is asked, with the error, whether to
/// open the editor again on an invalid edit.
pub fn edit(
    target: &Path,
    editor: &str,
    check: &dyn Fn(&str) -> Result<(), String>,
    reopen: &mut dyn FnMut(&str) -> bool,
) -> anyhow::Result<Outcome> {
    let real = resolve(target)?;
    let original = read_or_empty(&real)?;
    let copy = temp_copy(target, &original)?;
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
            match check(&text) {
                Ok(()) => return save(target, &real, &original, &text, &copy),
                Err(e) => Some(e),
            }
        };
        if let Some(e) = &error {
            eprintln!("{} is not valid: {e}", target.display());
            if reopen(e) {
                std::fs::write(&copy, with_marks(target, e, &text))
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
                target.display(),
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

/// Replace the file with `text`: a temp file beside the real one, then a
/// rename, so a reader sees the old file or the new one and never half of
/// it. Refused when the file changed while the editor was open.
fn save(
    target: &Path,
    real: &Path,
    original: &str,
    text: &str,
    copy: &Path,
) -> anyhow::Result<Outcome> {
    if read_or_empty(real)? != original {
        return Err(CliError::err(
            "edit_conflict",
            format!(
                "{} changed while it was being edited; it was left as it is now and the edit is kept in {}",
                target.display(),
                copy.display()
            ),
        ));
    }
    // Only a missing parent is made, private. An existing one keeps its
    // mode: through a symlink it may be a dotfiles dir that is not ours.
    if let Some(parent) = real.parent()
        && !parent.as_os_str().is_empty()
        && std::fs::symlink_metadata(parent).is_err()
    {
        crate::config::create_private_dir(parent)?;
    }
    let mode = std::fs::metadata(real).map_or(0o600, |m| m.permissions().mode() & 0o7777);
    let mut tmp = real.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {}", tmp.display()))?;
    std::fs::rename(&tmp, real).with_context(|| format!("rename to {}", real.display()))?;
    let _ = std::fs::remove_file(copy);
    Ok(Outcome::Saved)
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
