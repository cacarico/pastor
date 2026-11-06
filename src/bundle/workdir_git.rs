//! Keeping the files a workdir set placed out of git. In a repo checkout a
//! placed file would show in `git status` and an agent would commit it, so
//! pastor lists each one in the repo's `info/exclude`, in a block of its own:
//!
//! ```text
//! # pastor:workdir
//! /sub/dir/.claude/settings.local.json
//! # /pastor:workdir
//! ```
//!
//! - Git reads one `info/exclude` for every worktree of a repo, so a line
//!   covers a task's worktree and the clone alike. The file is
//!   `info/exclude` under `git rev-parse --git-common-dir`, which names the
//!   shared git dir from a linked worktree, as its real path: two paths to
//!   one repo are one file. Pastor never writes through a symlink there:
//!   `.git`, `info` or `exclude` being one is a failed row.
//! - A path the repo already ignores gets no line. A path git tracks is
//!   never written over (`tracked`): its row is a conflict, unless pastor
//!   placed it earlier, and then the set goes on managing it.
//! - What each directory asked for is in `exclude.json` beside its ledger
//!   (`<state>/workdirs/<key>/`), by set. The block holds the lines of
//!   every ledger that names the same file, so a line goes only when no set
//!   in any directory of the repo still has its path.
//! - Text outside the block is never touched. A machine without `git`
//!   leaves the files placed, with a `skipped` row saying they are not
//!   excluded.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::{Deserialize, Serialize};

use super::apply::{Action, Row};
use super::workdir::LEDGERS;

const OPEN: &str = "# pastor:workdir";
const CLOSE: &str = "# /pastor:workdir";

/// What `git` says about a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Repo {
    /// Inside a work tree: its top, and the exclude file git reads there.
    In { top: PathBuf, exclude: PathBuf },
    /// Not in a repo.
    None,
    /// `git` is not on this machine, and the directory holds a `.git`.
    NoGit,
}

fn git(program: &str, dir: &Path, args: &[&str]) -> std::io::Result<Output> {
    Command::new(program)
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
}

/// The repo `dir` is in. `program` is the `git` to run.
pub fn find(program: &str, dir: &Path) -> Repo {
    let out = match git(
        program,
        dir,
        &["rev-parse", "--show-toplevel", "--git-common-dir"],
    ) {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return match dir.join(".git").symlink_metadata() {
                Ok(_) => Repo::NoGit,
                Err(_) => Repo::None,
            };
        }
        Err(_) => return Repo::None,
    };
    if !out.status.success() {
        return Repo::None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let (Some(top), Some(common)) = (lines.next(), lines.next()) else {
        return Repo::None;
    };
    let top = PathBuf::from(top);
    let common = PathBuf::from(common);
    // The common dir is relative to where git ran, when it is.
    let common = if common.is_absolute() {
        common
    } else {
        dir.join(common)
    };
    let common = common.canonicalize().unwrap_or(common);
    Repo::In {
        top: top.canonicalize().unwrap_or(top),
        exclude: common.join("info/exclude"),
    }
}

/// Does git track `path`? With no repo, or no `git`, it does not.
pub fn tracked(program: &str, dir: &Path, path: &Path) -> bool {
    git(
        program,
        dir,
        &["ls-files", "--error-unmatch", "--"]
            .iter()
            .copied()
            .chain([path.to_str().unwrap_or_default()])
            .collect::<Vec<_>>(),
    )
    .is_ok_and(|o| o.status.success())
}

/// Does the repo already ignore `path`, by a `.gitignore` or a global file?
fn ignored(program: &str, dir: &Path, path: &Path) -> bool {
    git(
        program,
        dir,
        &[
            "check-ignore",
            "-q",
            "--",
            path.to_str().unwrap_or_default(),
        ],
    )
    .is_ok_and(|o| o.status.code() == Some(0))
}

/// The paths pastor's ledger in `ledger_dir` lists for the set `id`, or for
/// every set.
fn ledger_paths(ledger_dir: &Path, id: Option<&str>) -> BTreeMap<String, Vec<PathBuf>> {
    ledger_paths_where(ledger_dir, id, |_| true)
}

/// `ledger_paths`, of the entries the exclude file keeps out of git: all
/// but a merge into one of `workdir::SHARED_FILES` that was there before
/// pastor (`created` false), which stays the user's to commit.
fn excluded_paths(ledger_dir: &Path) -> BTreeMap<String, Vec<PathBuf>> {
    let dir = std::fs::read_to_string(ledger_dir.join("ledger.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["dir"].as_str().map(PathBuf::from));
    ledger_paths_where(ledger_dir, None, |e| {
        let merged = e["created"].as_bool() == Some(false);
        let shared = dir
            .as_deref()
            .zip(e["path"].as_str())
            .is_some_and(|(d, p)| is_shared(d, Path::new(p)));
        !(merged && shared)
    })
}

/// Whether `path` is one of `workdir::SHARED_FILES` of `dir`.
fn is_shared(dir: &Path, path: &Path) -> bool {
    path.strip_prefix(dir).is_ok_and(|rel| {
        crate::bundle::workdir::SHARED_FILES
            .iter()
            .any(|f| rel == Path::new(f))
    })
}

/// A worktree of the repo at `top`, other than `top`, where `rel` is a file
/// git does not track and nothing but pastor's own lines (`ours`, in
/// `exclude`) ignores: the copy an exclude line for `rel` would hide there.
/// Git reads one exclude file for every worktree, so a line pastor adds for
/// a shared file it created in one worktree hides a user's own untracked
/// copy in another until the line goes.
fn hidden_copy(
    program: &str,
    paths: &crate::config::Paths,
    top: &Path,
    dir: &Path,
    exclude: &Path,
    ours: &BTreeSet<String>,
    rel: &Path,
) -> Option<PathBuf> {
    let out = git(program, top, &["worktree", "list", "--porcelain"]).ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // One block per worktree, a blank line between: its path, then what
    // marks it (`bare`, `prunable`).
    for block in text.split("\n\n") {
        let mut lines = block.lines();
        let Some(path) = lines.find_map(|l| l.strip_prefix("worktree ")) else {
            continue;
        };
        if block
            .lines()
            .any(|l| l == "bare" || l.starts_with("prunable"))
        {
            continue;
        }
        let wt = PathBuf::from(path);
        let wt = wt.canonicalize().unwrap_or(wt);
        if wt == top || wt.join(rel).symlink_metadata().is_err() {
            continue;
        }
        if tracked(program, &wt, rel) || ignored_beyond(program, &wt, exclude, ours, rel) {
            continue;
        }
        // A copy pastor made there too (a set per task, a worktree per
        // task) is not the user's, and is excluded by the same line.
        if let Ok(below) = dir.strip_prefix(top)
            && placed_by_pastor(paths, &wt.join(below), &wt.join(rel))
        {
            continue;
        }
        return Some(wt);
    }
    None
}

/// Whether the ledger of the directory `dir` (a worktree's, or below one)
/// lists `file` as one pastor created, as `excluded_paths` reads it.
fn placed_by_pastor(paths: &crate::config::Paths, dir: &Path, file: &Path) -> bool {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let ledger = crate::bundle::workdir::ledger_dir(paths, &dir);
    let Ok(text) = std::fs::read_to_string(ledger.join("ledger.json")) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let file = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    v["entries"].as_array().into_iter().flatten().any(|e| {
        e["created"].as_bool() == Some(true)
            && e["path"].as_str().is_some_and(|p| {
                let p = Path::new(p);
                p == file || p.canonicalize().is_ok_and(|p| p == file)
            })
    })
}

/// Whether the repo ignores `rel` in the worktree `wt` by anything but
/// pastor's own lines in `exclude`.
fn ignored_beyond(
    program: &str,
    wt: &Path,
    exclude: &Path,
    ours: &BTreeSet<String>,
    rel: &Path,
) -> bool {
    let Ok(out) = git(
        program,
        wt,
        &["check-ignore", "-v", "--", rel.to_str().unwrap_or_default()],
    ) else {
        return false;
    };
    if out.status.code() != Some(0) {
        return false;
    }
    // `<source>:<line>:<pattern>\t<path>`; the source may hold a colon.
    let text = String::from_utf8_lossy(&out.stdout);
    let Some((left, _)) = text.split_once('\t') else {
        return true;
    };
    let mut at = 0;
    while let Some(i) = left[at..].find(':') {
        let colon = at + i;
        let rest = &left[colon + 1..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && rest[digits..].starts_with(':') {
            let source = Path::new(&left[..colon]);
            let pattern = &rest[digits + 1..];
            let same =
                source == exclude || source.canonicalize().ok() == exclude.canonicalize().ok();
            return !(same && ours.contains(pattern));
        }
        at = colon + 1;
    }
    true
}

/// The row that says a shared file got no exclude line, and why.
fn held_back(id: &str, dir: &Path, path: &Path, worktree: &Path) -> Row {
    let rel = path.strip_prefix(dir).unwrap_or(path);
    Row {
        bundle: id.to_string(),
        home: None,
        path: rel.display().to_string(),
        key: None,
        action: Action::Skipped,
        detail: Some(format!(
            "not excluded: {} is an untracked file in the worktree {}, which a line would hide there",
            rel.display(),
            worktree.display()
        )),
        old: None,
        backup: None,
    }
}

fn ledger_paths_where(
    ledger_dir: &Path,
    id: Option<&str>,
    keep: impl Fn(&serde_json::Value) -> bool,
) -> BTreeMap<String, Vec<PathBuf>> {
    let mut out: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(ledger_dir.join("ledger.json")) else {
        return out;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return out;
    };
    for e in v["entries"].as_array().into_iter().flatten() {
        let (Some(bundle), Some(path)) = (e["bundle"].as_str(), e["path"].as_str()) else {
            continue;
        };
        if id.is_none_or(|i| i == bundle) && keep(e) {
            out.entry(bundle.to_string())
                .or_default()
                .push(PathBuf::from(path));
        }
    }
    out
}

/// A pattern for git's ignore files that matches exactly `path`: its
/// wildcards and a trailing space escaped.
fn pattern(rel: &str) -> String {
    let mut out = String::from("/");
    for c in rel.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    if out.ends_with(' ') {
        out.insert(out.len() - 1, '\\');
    }
    out
}

/// What a directory asked of the exclude file, by set.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    /// The exclude file the lines are in, when there are any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(default)]
    sets: BTreeMap<String, SetLines>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SetLines {
    /// The set's `git_exclude`: off, it has no lines.
    exclude: bool,
    #[serde(default)]
    lines: Vec<String>,
}

const RECORD: &str = "exclude.json";

fn read_record(dir: &Path) -> Record {
    std::fs::read_to_string(dir.join(RECORD))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_record(dir: &Path, rec: &Record) -> anyhow::Result<()> {
    let path = dir.join(RECORD);
    if rec.sets.is_empty() && rec.file.is_none() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        };
    }
    let tmp = dir.join(format!("{RECORD}.tmp"));
    std::fs::write(&tmp, serde_json::to_string_pretty(rec)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// The lines every ledger under `root` has recorded for `file`.
fn wanted_in(root: &Path, file: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Ok(dirs) = std::fs::read_dir(root) else {
        return out;
    };
    for d in dirs.flatten() {
        let rec = read_record(&d.path());
        if rec.file.as_deref().map(Path::new) == Some(file) {
            out.extend(rec.sets.into_values().flat_map(|s| s.lines));
        }
    }
    out
}

/// `text` without pastor's block, then with a block of `lines` at the end
/// when there are any. Everything else is left exactly as it is.
fn with_block(text: &str, lines: &BTreeSet<String>) -> String {
    let mut out = Vec::new();
    let mut inside = false;
    for l in text.lines() {
        match (inside, l.trim_end()) {
            (false, OPEN) => inside = true,
            (true, CLOSE) => inside = false,
            (true, _) => {}
            (false, _) => out.push(l.to_string()),
        }
    }
    // A block that was never closed is not pastor's to keep.
    let mut s = out.join("\n");
    if !s.is_empty() {
        s.push('\n');
    }
    if !lines.is_empty() {
        s.push_str(OPEN);
        s.push('\n');
        for l in lines {
            s.push_str(l);
            s.push('\n');
        }
        s.push_str(CLOSE);
        s.push('\n');
    }
    s
}

/// The lines in `text`'s pastor block.
fn block_lines(text: &str) -> BTreeSet<String> {
    let mut inside = false;
    let mut out = BTreeSet::new();
    for l in text.lines() {
        match (inside, l.trim_end()) {
            (false, OPEN) => inside = true,
            (true, CLOSE) => inside = false,
            (true, l) => {
                out.insert(l.to_string());
            }
            _ => {}
        }
    }
    out
}

/// Make `file`'s block hold `lines`, under a lock shared by every ledger,
/// and say what it added and took out.
fn rewrite(
    file: &Path,
    lines: &BTreeSet<String>,
    dry_run: bool,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    if !dry_run {
        for p in file.parent().into_iter().chain([file]) {
            if p.symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
            {
                anyhow::bail!(
                    "{} is a symlink; pastor does not write through it",
                    p.display()
                );
            }
        }
    }
    let old = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => anyhow::bail!("{}: {e}", file.display()),
    };
    let had = block_lines(&old);
    let added: Vec<String> = lines.difference(&had).cloned().collect();
    let removed: Vec<String> = had.difference(lines).cloned().collect();
    if dry_run {
        return Ok((added, removed));
    }
    // The caller holds the lock.
    let new = with_block(&old, lines);
    if new != old {
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = file.with_extension("pastor-tmp");
        std::fs::write(&tmp, &new)?;
        if let Ok(meta) = std::fs::metadata(file) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, file)?;
    }
    Ok((added, removed))
}

/// One lock for every ledger's exclude work, so two directories of one repo
/// never rewrite its file at once.
fn lock(root: &Path) -> anyhow::Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    crate::config::create_private_dir(root)?;
    let f = crate::edit::open_lock(&root.join("exclude.lock"))?;
    loop {
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(f);
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(anyhow::Error::from(e).context("lock the exclude file"));
        }
    }
}

/// The items of a set about to be placed in `dir` that git tracks and pastor
/// did not place before: not to be written over. Their paths.
pub fn tracked_new(
    program: &str,
    repo: &Repo,
    dir: &Path,
    ledger_dir: &Path,
    id: &str,
    paths: &[PathBuf],
) -> Vec<PathBuf> {
    if !matches!(repo, Repo::In { .. }) {
        return Vec::new();
    }
    let ours = ledger_paths(ledger_dir, Some(id));
    let ours = ours.get(id).cloned().unwrap_or_default();
    paths
        .iter()
        .filter(|p| !ours.contains(p) && tracked(program, dir, p))
        .cloned()
        .collect()
}

/// A `skipped` row saying a set's files are not excluded.
pub fn not_excluded(id: &str, dir: &Path, why: &str) -> Row {
    Row {
        bundle: id.to_string(),
        home: None,
        path: ".git".to_string(),
        key: None,
        action: Action::Skipped,
        detail: Some(format!("not excluded: {why} ({})", dir.display())),
        old: None,
        backup: None,
    }
}

/// Bring the exclude file in line with what `dir`'s ledger now lists.
///
/// `id` is the set just placed, with its `git_exclude`; none after a
/// removal. Every set the directory's record names is recomputed from the
/// ledger, so a removed set's lines go and the others stay. A dry run
/// writes nothing and names the lines `paths` would add.
#[allow(clippy::too_many_arguments)]
pub fn exclude(
    program: &str,
    paths: &crate::config::Paths,
    ledger_dir: &Path,
    dir: &Path,
    repo: &Repo,
    set: Option<(&str, bool)>,
    placing: &[PathBuf],
    dry_run: bool,
) -> anyhow::Result<Vec<Row>> {
    let root = paths.state_dir.join(LEDGERS);
    let who = set.map(|(id, _)| id).unwrap_or_default();
    let mut rows = Vec::new();
    let top_exclude = match repo {
        Repo::In { top, exclude } => Some((top.as_path(), exclude.as_path())),
        _ => None,
    };
    if dry_run {
        let (Some((top, file)), Some((_, true))) = (top_exclude, set) else {
            return Ok(rows);
        };
        let had = std::fs::read_to_string(file)
            .map(|t| block_lines(&t))
            .unwrap_or_default();
        let new: Vec<String> = placing
            .iter()
            .filter(|p| !tracked(program, dir, p))
            .filter(|p| {
                // A shared file in `placing` is one pastor would create.
                if !is_shared(dir, p) {
                    return true;
                }
                let Ok(rel) = p.strip_prefix(top) else {
                    return true;
                };
                match hidden_copy(program, paths, top, dir, file, &had, rel) {
                    Some(wt) => {
                        rows.push(held_back(who, dir, p, &wt));
                        false
                    }
                    None => true,
                }
            })
            .filter_map(|p| {
                let line = pattern(&p.strip_prefix(top).ok()?.to_string_lossy());
                // As the real run: a line of the block stays, any other
                // path the repo ignores needs none.
                (!had.contains(&line) && !ignored(program, dir, p)).then_some(line)
            })
            .collect();
        if !new.is_empty() {
            rows.push(change(who, file, &new, &[]));
        }
        return Ok(rows);
    }
    if let Some((top, _)) = top_exclude
        && top
            .join(".git")
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
    {
        anyhow::bail!(
            "{} is a symlink; pastor does not write through it",
            top.join(".git").display()
        );
    }
    let _lock = lock(&root)?;
    let mut rec = read_record(ledger_dir);
    let previous = rec.file.clone();
    if let Some((id, on)) = set {
        rec.sets
            .entry(id.to_string())
            .or_insert(SetLines {
                exclude: on,
                lines: Vec::new(),
            })
            .exclude = on;
    }
    // Every set the ledger names keeps its record, even one whose only
    // paths are shared files left to the user.
    let named = ledger_paths(ledger_dir, None);
    rec.sets.retain(|id, _| named.contains_key(id));
    let listed = excluded_paths(ledger_dir);
    for (id, s) in rec.sets.iter_mut() {
        let mut lines = BTreeSet::new();
        if let (true, Some((top, file))) = (s.exclude, top_exclude) {
            let mine = block_lines(&std::fs::read_to_string(file).unwrap_or_default());
            for p in listed.get(id).into_iter().flatten() {
                let Ok(rel) = p.strip_prefix(top) else {
                    continue;
                };
                if rel.to_string_lossy().chars().any(char::is_control) {
                    anyhow::bail!(
                        "{} has a control character in its name, which an exclude line cannot hold",
                        p.display()
                    );
                }
                let line = pattern(&rel.to_string_lossy());
                if rel.starts_with(".git") || tracked(program, dir, p) {
                    continue;
                }
                // A shared file here is one pastor created (`excluded_paths`).
                // Another worktree's untracked copy would be hidden by the
                // line, so there is none; the next place looks again.
                if is_shared(dir, p)
                    && let Some(wt) = hidden_copy(program, paths, top, dir, file, &mine, rel)
                {
                    if set.is_some_and(|(sid, _)| sid == id) {
                        rows.push(held_back(id, dir, p, &wt));
                    }
                    continue;
                }
                // A line already in the block is ours and stays; any other
                // path the repo ignores needs none.
                if mine.contains(&line) || !ignored(program, dir, p) {
                    lines.insert(line);
                }
            }
        }
        s.lines = lines.into_iter().collect();
    }
    rec.file = top_exclude
        .filter(|_| rec.sets.values().any(|s| !s.lines.is_empty()))
        .map(|(_, f)| f.display().to_string());
    write_record(ledger_dir, &rec)?;
    let mut files: Vec<PathBuf> = Vec::new();
    files.extend(previous.map(PathBuf::from));
    files.extend(rec.file.clone().map(PathBuf::from));
    files.sort();
    files.dedup();
    for file in files {
        let (added, removed) = rewrite(&file, &wanted_in(&root, &file), false)?;
        if !added.is_empty() || !removed.is_empty() {
            rows.push(change(who, &file, &added, &removed));
        }
    }
    Ok(rows)
}

fn change(id: &str, file: &Path, added: &[String], removed: &[String]) -> Row {
    let mut said = Vec::new();
    if !added.is_empty() {
        said.push(format!("excluded {}", added.join(", ")));
    }
    if !removed.is_empty() {
        said.push(format!("no longer excluded {}", removed.join(", ")));
    }
    Row {
        bundle: id.to_string(),
        home: None,
        path: file.display().to_string(),
        key: None,
        action: if added.is_empty() {
            Action::Removed
        } else {
            Action::Updated
        },
        detail: Some(said.join("; ")),
        old: None,
        backup: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_matches_exactly_that_path() {
        assert_eq!(pattern(".claude/a.json"), "/.claude/a.json");
        assert_eq!(pattern("odd[1]*?.md"), "/odd\\[1\\]\\*\\?.md");
        assert_eq!(pattern("a b "), "/a b\\ ");
    }

    #[test]
    fn the_block_is_replaced_and_the_rest_of_the_file_kept() {
        let lines: BTreeSet<String> = ["/a".to_string(), "/b".to_string()].into();
        let text = "# git ls-files --others\n*.log\n";
        let with = with_block(text, &lines);
        assert_eq!(
            with,
            "# git ls-files --others\n*.log\n# pastor:workdir\n/a\n/b\n# /pastor:workdir\n"
        );
        assert_eq!(block_lines(&with), lines);
        let fewer: BTreeSet<String> = ["/b".to_string()].into();
        assert_eq!(
            with_block(&with, &fewer),
            "# git ls-files --others\n*.log\n# pastor:workdir\n/b\n# /pastor:workdir\n"
        );
        assert_eq!(with_block(&with, &BTreeSet::new()), text);
    }
}
