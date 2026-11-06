//! Where bundles are kept on the head, and how one is added.
//!
//! - `<config>/bundles/<id>/`: the bundle as it is now, a plain directory
//!   with its `pastor-bundle.toml`. `bundle add` copies one here; a user may
//!   also write one here by hand and add it in place.
//! - `<state>/bundles/<id>/<hash>/`: each version added, by content hash,
//!   with `versions.json` listing the newest first. The last `KEEP` are
//!   kept, so there is something to roll back to.
//!
//! The content hash is SHA-256 over every file's path, executable bit and
//! bytes, in path order; the author's `version` plays no part. A bundle
//! holds plain files and directories only: a symlink could point anywhere
//! on the head, so it is refused, and a `.git` at the root is left out.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::manifest::Manifest;
use super::secrets;
use crate::config::{Paths, create_private_dir};

/// Versions kept per bundle.
pub const KEEP: usize = 5;

/// A bundle bigger than this is refused: it is copied to every machine.
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;

const VERSIONS_FILE: &str = "versions.json";

/// Why an add or a read failed, with the error code the CLI gives it.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The manifest, a file or the directory's shape is wrong.
    #[error("{0}")]
    Invalid(String),
    /// Something in the bundle looks like a secret.
    #[error("{0}")]
    Secret(String),
    /// `git` could not fetch the ref.
    #[error("{0}")]
    Fetch(String),
    #[error(transparent)]
    Io(#[from] anyhow::Error),
}

impl StoreError {
    pub fn code(&self) -> &'static str {
        match self {
            StoreError::Invalid(_) => super::BUNDLE_INVALID,
            StoreError::Secret(_) => super::BUNDLE_SECRET,
            StoreError::Fetch(_) => super::BUNDLE_FETCH_FAILED,
            StoreError::Io(_) => "runtime_error",
        }
    }
}

/// Where a version came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Origin {
    Dir {
        path: String,
    },
    Git {
        url: String,
        #[serde(rename = "ref")]
        git_ref: String,
        /// What the ref named when it was fetched.
        commit: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
}

/// One kept version, as `versions.json` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub hash: String,
    /// The author's `version` at the time.
    pub version: String,
    pub added_at: DateTime<Utc>,
    pub origin: Origin,
}

/// What `add` reads a bundle from.
#[derive(Debug, Clone)]
pub enum Source {
    Dir(PathBuf),
    Git {
        url: String,
        git_ref: String,
        /// A subdirectory of the repository holding the bundle.
        path: Option<String>,
    },
}

/// What `add` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    pub id: String,
    pub version: String,
    pub hash: String,
    /// The newest version before this add; `None` for a new bundle.
    pub previous_hash: Option<String>,
}

impl Added {
    pub fn unchanged(&self) -> bool {
        self.previous_hash.as_deref() == Some(self.hash.as_str())
    }
}

/// A bundle in `<config>/bundles/`, as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub id: String,
    pub dir: PathBuf,
    pub manifest: Manifest,
}

/// The bundles in `<config>/bundles/`, each loaded or with why not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    valid: BTreeMap<String, Bundle>,
    invalid: BTreeMap<String, String>,
}

impl Catalog {
    /// Every directory under the bundles dir whose name does not start with
    /// `.`; a missing dir is no bundles. A directory whose manifest's `id`
    /// is not its name is invalid, as for a provisioner.
    pub fn load(paths: &Paths) -> anyhow::Result<Catalog> {
        let root = paths.bundles_dir();
        let mut catalog = Catalog::default();
        let entries = match std::fs::read_dir(&root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(catalog),
            Err(e) => return Err(anyhow::anyhow!("read {}: {e}", root.display())),
        };
        for entry in entries {
            let entry = entry?;
            let Some(id) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if id.starts_with('.') || !entry.path().is_dir() {
                continue;
            }
            match load_bundle(&entry.path(), &id) {
                Ok(manifest) => {
                    catalog.valid.insert(
                        id.clone(),
                        Bundle {
                            id,
                            dir: entry.path(),
                            manifest,
                        },
                    );
                }
                Err(why) => {
                    catalog.invalid.insert(id, why);
                }
            }
        }
        Ok(catalog)
    }

    /// A catalog of just these, for a test.
    #[cfg(test)]
    pub fn of(bundles: Vec<Bundle>) -> Catalog {
        Catalog {
            valid: bundles.into_iter().map(|b| (b.id.clone(), b)).collect(),
            invalid: BTreeMap::new(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&Bundle> {
        self.valid.get(id)
    }

    /// This catalog with `bundle` in place of any of its id.
    pub fn with(mut self, bundle: Bundle) -> Catalog {
        self.invalid.remove(&bundle.id);
        self.valid.insert(bundle.id.clone(), bundle);
        self
    }

    /// Err(reason) when no bundle has that id, or the one that has it does
    /// not load.
    pub fn check(&self, id: &str) -> Result<&Bundle, String> {
        if let Some(b) = self.valid.get(id) {
            return Ok(b);
        }
        match self.invalid.get(id) {
            Some(why) => Err(format!("bundle {id:?} does not load: {why}")),
            None => Err(format!(
                "bundle {id:?} is not in the bundles dir; add it with `pastor bundle add`"
            )),
        }
    }

    pub fn bundles(&self) -> impl Iterator<Item = &Bundle> {
        self.valid.values()
    }

    pub fn invalid(&self) -> impl Iterator<Item = (&String, &String)> {
        self.invalid.iter()
    }
}

fn load_bundle(dir: &Path, id: &str) -> Result<Manifest, String> {
    let manifest = Manifest::load(dir)?;
    if manifest.id != id {
        return Err(format!(
            "its manifest says id {:?}; the directory must be named after it",
            manifest.id
        ));
    }
    Ok(manifest)
}

/// The files of the bundle in `dir`, relative and sorted: plain files and
/// directories only, a top-level `.git` left out.
pub fn files(dir: &Path) -> Result<Vec<String>, StoreError> {
    let mut out = Vec::new();
    let mut size = 0;
    walk(dir, "", &mut out, &mut size)?;
    out.sort();
    Ok(out)
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>, size: &mut u64) -> Result<(), StoreError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| StoreError::Invalid(format!("{}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| StoreError::Invalid(format!("{}: {e}", dir.display())))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = format!("{prefix}{name}");
        if prefix.is_empty() && name == ".git" {
            continue;
        }
        let meta = entry
            .path()
            .symlink_metadata()
            .map_err(|e| StoreError::Invalid(format!("{rel}: {e}")))?;
        if meta.file_type().is_symlink() {
            return Err(StoreError::Invalid(format!(
                "{rel} is a symlink; a bundle holds plain files only"
            )));
        } else if meta.is_dir() {
            walk(&entry.path(), &format!("{rel}/"), out, size)?;
        } else if meta.is_file() {
            *size += meta.len();
            if *size > MAX_BYTES {
                return Err(StoreError::Invalid(format!(
                    "the bundle is over {} MiB",
                    MAX_BYTES / 1024 / 1024
                )));
            }
            out.push(rel);
        } else {
            return Err(StoreError::Invalid(format!(
                "{rel} is not a plain file or directory"
            )));
        }
    }
    Ok(())
}

/// The content hash of `files` in `dir`, as 64 hex digits.
pub fn hash(dir: &Path, files: &[String]) -> anyhow::Result<String> {
    let mut h = Sha256::new();
    for rel in files {
        let path = dir.join(rel);
        let bytes = std::fs::read(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        let exec = path.metadata()?.permissions().mode() & 0o111 != 0;
        h.update(rel.as_bytes());
        h.update([0, u8::from(exec)]);
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Validate the bundle in `dir`: its manifest and files, no symlink, no
/// secret. Gives the manifest, the file list and the hash.
pub fn inspect(dir: &Path) -> Result<(Manifest, Vec<String>, String), StoreError> {
    let manifest = Manifest::load(dir).map_err(StoreError::Invalid)?;
    let files = files(dir)?;
    let found = secrets::scan(dir, &files).map_err(StoreError::Invalid)?;
    if !found.is_empty() {
        let list: Vec<String> = found.iter().map(|f| f.to_string()).collect();
        return Err(StoreError::Secret(format!(
            "bundle {} looks like it holds a secret, so it is not added: {}; give MCP servers and hooks their secrets by variable name (env_from), from the machine's env",
            manifest.id,
            list.join("; ")
        )));
    }
    let hash = hash(dir, &files)?;
    Ok((manifest, files, hash))
}

/// Add the bundle `source` names: validate it, copy it to the bundles dir
/// and keep it as a version. Adding what is already the newest version
/// changes nothing.
pub fn add(paths: &Paths, source: &Source) -> Result<Added, StoreError> {
    add_checked(paths, source, &|_| Ok(()))
}

/// What a bundle about to be added is checked by: the bundle as it would
/// be in the bundles dir, read from where it is now. Err refuses it.
pub type Fits<'a> = &'a dyn Fn(&Bundle) -> Result<(), String>;

/// `add`, with `fits` asked once the bundle is valid and before anything is
/// copied; a refusal is `bundle_invalid`.
pub fn add_checked(paths: &Paths, source: &Source, fits: Fits<'_>) -> Result<Added, StoreError> {
    match source {
        Source::Dir(dir) => {
            let dir = std::fs::canonicalize(dir)
                .map_err(|e| StoreError::Invalid(format!("{}: {e}", dir.display())))?;
            let origin = Origin::Dir {
                path: dir.to_string_lossy().into_owned(),
            };
            add_from(paths, &dir, origin, fits)
        }
        Source::Git { url, git_ref, path } => {
            if git_ref.is_empty() || git_ref.starts_with('-') {
                return Err(StoreError::Invalid(format!(
                    "ref {git_ref:?} is not a branch, tag or commit"
                )));
            }
            let root = paths.bundle_versions_root();
            create_private_dir(&root)?;
            let scratch = Scratch(root.join(format!(".fetch-{}", unique())));
            let checkout = scratch.0.join("checkout");
            let co = checkout.to_string_lossy().into_owned();
            git(&["init", "--quiet", &co])?;
            git(&[
                "-C", &co, "fetch", "--quiet", "--depth", "1", "--", url, git_ref,
            ])?;
            git(&["-C", &co, "checkout", "--quiet", "FETCH_HEAD"])?;
            let commit = git(&["-C", &co, "rev-parse", "HEAD"])?;
            let dir = match path {
                Some(sub) => {
                    let root_real =
                        std::fs::canonicalize(&checkout).map_err(anyhow::Error::from)?;
                    let dir = std::fs::canonicalize(checkout.join(sub)).map_err(|_| {
                        StoreError::Invalid(format!(
                            "path {sub} is not in the repository at {git_ref}"
                        ))
                    })?;
                    if !dir.starts_with(&root_real) {
                        return Err(StoreError::Invalid(format!(
                            "path {sub} leads outside the repository"
                        )));
                    }
                    dir
                }
                None => checkout.clone(),
            };
            let origin = Origin::Git {
                url: url.clone(),
                git_ref: git_ref.clone(),
                commit,
                path: path.clone(),
            };
            add_from(paths, &dir, origin, fits)
        }
    }
}

fn add_from(
    paths: &Paths,
    dir: &Path,
    origin: Origin,
    fits: Fits<'_>,
) -> Result<Added, StoreError> {
    let (manifest, files, hash) = inspect(dir)?;
    fits(&Bundle {
        id: manifest.id.clone(),
        dir: dir.to_path_buf(),
        manifest: manifest.clone(),
    })
    .map_err(StoreError::Invalid)?;
    let id = manifest.id.clone();
    let mut versions = read_versions(paths, &id)?;
    let previous_hash = versions.first().map(|v| v.hash.clone());
    let current = paths.bundles_dir().join(&id);
    create_private_dir(&paths.bundles_dir())?;
    let in_place = std::fs::canonicalize(&current).is_ok_and(|c| c == dir);
    if !in_place {
        let fresh = paths.bundles_dir().join(format!(".add-{id}-{}", unique()));
        let fresh = Scratch(fresh);
        copy_files(dir, &fresh.0, &files)?;
        let old = Scratch(paths.bundles_dir().join(format!(".old-{id}-{}", unique())));
        if current.symlink_metadata().is_ok() {
            std::fs::rename(&current, &old.0).map_err(anyhow::Error::from)?;
        }
        std::fs::rename(&fresh.0, &current).map_err(anyhow::Error::from)?;
    }
    let vroot = paths.bundle_versions_dir(&id);
    create_private_dir(&vroot)?;
    let snapshot = vroot.join(&hash);
    if !snapshot.exists() {
        let tmp = Scratch(vroot.join(format!(".tmp-{}", unique())));
        copy_files(dir, &tmp.0, &files)?;
        std::fs::rename(&tmp.0, &snapshot).map_err(anyhow::Error::from)?;
    }
    if previous_hash.as_deref() != Some(hash.as_str()) {
        versions.retain(|v| v.hash != hash);
        versions.insert(
            0,
            Version {
                hash: hash.clone(),
                version: manifest.version.clone(),
                added_at: Utc::now(),
                origin,
            },
        );
        for gone in versions.split_off(KEEP.min(versions.len())) {
            let _ = std::fs::remove_dir_all(vroot.join(&gone.hash));
        }
        write_versions(paths, &id, &versions)?;
    }
    Ok(Added {
        id,
        version: manifest.version,
        hash,
        previous_hash,
    })
}

/// The kept versions of `id`, newest first; none when it has none.
pub fn read_versions(paths: &Paths, id: &str) -> anyhow::Result<Vec<Version>> {
    let file = paths.bundle_versions_dir(id).join(VERSIONS_FILE);
    match std::fs::read_to_string(&file) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(anyhow::anyhow!("{}: {e}", file.display())),
    }
}

fn write_versions(paths: &Paths, id: &str, versions: &[Version]) -> anyhow::Result<()> {
    let file = paths.bundle_versions_dir(id).join(VERSIONS_FILE);
    let tmp = file.with_extension("json.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(serde_json::to_string_pretty(versions)?.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

/// Remove `id`: the bundle and every version kept. Gives how many versions
/// went.
pub fn remove(paths: &Paths, id: &str) -> anyhow::Result<usize> {
    let versions = read_versions(paths, id)?.len();
    let current = paths.bundles_dir().join(id);
    if current.symlink_metadata().is_ok() {
        std::fs::remove_dir_all(&current)?;
    }
    let vroot = paths.bundle_versions_dir(id);
    if vroot.exists() {
        std::fs::remove_dir_all(&vroot)?;
    }
    Ok(versions)
}

/// Copy `files` from `from` into a new directory `to`, keeping each file's
/// executable bit.
fn copy_files(from: &Path, to: &Path, files: &[String]) -> anyhow::Result<()> {
    create_private_dir(to)?;
    for rel in files {
        let dst = to.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(from.join(rel), &dst).map_err(|e| anyhow::anyhow!("copy {rel}: {e}"))?;
    }
    Ok(())
}

/// Removes a half-done copy or fetch whatever happens.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn unique() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    format!(
        "{}-{}-{nanos}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// Runs git and returns its stdout, trimmed.
fn git(args: &[&str]) -> Result<String, StoreError> {
    let out = Command::new("git")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| StoreError::Fetch(format!("run git (is it installed?): {e}")))?;
    if !out.status.success() {
        // `-C <dir> <subcommand> ...`: name the subcommand, not the dir.
        let sub = if args.first() == Some(&"-C") {
            args.get(2)
        } else {
            args.first()
        };
        return Err(StoreError::Fetch(format!(
            "git {}: {}",
            sub.unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::MANIFEST_FILE;

    fn paths(tmp: &Path) -> Paths {
        Paths::new(tmp.join("config"), tmp.join("state"))
    }

    fn bundle(dir: &Path, id: &str, note: &str) {
        std::fs::create_dir_all(dir.join("skills/s")).unwrap();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            format!("id = \"{id}\"\nversion = \"1\"\n[[skill]]\nsrc = \"skills/s\"\n"),
        )
        .unwrap();
        std::fs::write(dir.join("skills/s/SKILL.md"), note).unwrap();
    }

    /// A first add copies the bundle in and keeps it as a version; adding
    /// it again changes nothing; a change is a new version, with the
    /// previous hash.
    #[test]
    fn add_copies_keeps_versions_and_knows_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let src = tmp.path().join("src");
        bundle(&src, "team-rules", "one");
        let first = add(&p, &Source::Dir(src.clone())).unwrap();
        assert_eq!(first.previous_hash, None);
        assert_eq!(first.hash.len(), 64);
        let current = p.bundles_dir().join("team-rules");
        assert_eq!(
            std::fs::read_to_string(current.join("skills/s/SKILL.md")).unwrap(),
            "one"
        );
        assert!(
            p.bundle_versions_dir("team-rules")
                .join(&first.hash)
                .join(MANIFEST_FILE)
                .is_file()
        );

        let again = add(&p, &Source::Dir(src.clone())).unwrap();
        assert!(again.unchanged(), "{again:?}");
        assert_eq!(read_versions(&p, "team-rules").unwrap().len(), 1);

        std::fs::write(src.join("skills/s/SKILL.md"), "two").unwrap();
        let second = add(&p, &Source::Dir(src.clone())).unwrap();
        assert_eq!(second.previous_hash.as_deref(), Some(first.hash.as_str()));
        assert_ne!(second.hash, first.hash);
        assert_eq!(
            std::fs::read_to_string(current.join("skills/s/SKILL.md")).unwrap(),
            "two"
        );
        let v = read_versions(&p, "team-rules").unwrap();
        assert_eq!(
            v.iter().map(|v| v.hash.as_str()).collect::<Vec<_>>(),
            [second.hash.as_str(), first.hash.as_str()]
        );
        assert!(matches!(&v[0].origin, Origin::Dir { path } if path.ends_with("src")));
        // No scratch left behind.
        let left: Vec<String> = std::fs::read_dir(p.bundles_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["team-rules"]);
    }

    /// Only the last `KEEP` versions stay, their directories with them.
    #[test]
    fn the_last_versions_are_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let src = tmp.path().join("src");
        let mut hashes = Vec::new();
        for i in 0..KEEP + 2 {
            bundle(&src, "b", &format!("v{i}"));
            hashes.push(add(&p, &Source::Dir(src.clone())).unwrap().hash);
        }
        let kept: Vec<String> = read_versions(&p, "b")
            .unwrap()
            .into_iter()
            .map(|v| v.hash)
            .collect();
        let want: Vec<String> = hashes.iter().rev().take(KEEP).cloned().collect();
        assert_eq!(kept, want);
        for gone in &hashes[..2] {
            assert!(!p.bundle_versions_dir("b").join(gone).exists());
        }
    }

    /// A bundle written by hand in the bundles dir is added in place.
    #[test]
    fn a_bundle_in_the_bundles_dir_is_added_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let dir = p.bundles_dir().join("laptop");
        bundle(&dir, "laptop", "mine");
        let added = add(&p, &Source::Dir(dir.clone())).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("skills/s/SKILL.md")).unwrap(),
            "mine"
        );
        assert_eq!(read_versions(&p, "laptop").unwrap()[0].hash, added.hash);
    }

    /// The hash covers paths, bytes and the executable bit, and nothing
    /// else: not the author's version, not `.git`.
    #[test]
    fn the_hash_is_the_content() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        bundle(&a, "b", "x");
        let h = |d: &Path| hash(d, &files(d).unwrap()).unwrap();
        let first = h(&a);
        std::fs::create_dir_all(a.join(".git")).unwrap();
        std::fs::write(a.join(".git/HEAD"), "ref").unwrap();
        assert_eq!(h(&a), first);
        let script = a.join("skills/s/SKILL.md");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let exec = h(&a);
        assert_ne!(exec, first);
        std::fs::rename(a.join("skills/s"), a.join("skills/t")).unwrap();
        assert_ne!(h(&a), exec);
    }

    #[test]
    fn a_symlink_or_a_secret_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let src = tmp.path().join("src");
        bundle(&src, "b", "x");
        std::os::unix::fs::symlink("/etc/hostname", src.join("skills/s/host")).unwrap();
        let err = add(&p, &Source::Dir(src.clone())).unwrap_err();
        assert_eq!(err.code(), crate::bundle::BUNDLE_INVALID);
        assert!(
            err.to_string().contains("skills/s/host is a symlink"),
            "{err}"
        );
        std::fs::remove_file(src.join("skills/s/host")).unwrap();

        std::fs::write(
            src.join("skills/s/SKILL.md"),
            format!("export TOKEN=ghp_{}\n", "a1B2".repeat(9)),
        )
        .unwrap();
        let err = add(&p, &Source::Dir(src)).unwrap_err();
        assert_eq!(err.code(), crate::bundle::BUNDLE_SECRET);
        let said = err.to_string();
        assert!(
            said.contains("skills/s/SKILL.md:1: a GitHub token"),
            "{said}"
        );
        assert!(!p.bundles_dir().join("b").exists());
    }

    /// `--git` fetches the ref, records the commit, and takes a
    /// subdirectory with `path`.
    #[test]
    fn add_from_a_git_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let repo = tmp.path().join("repo");
        bundle(&repo.join("bundles/ops"), "ops", "at v1");
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "--quiet"]);
        run(&["add", "."]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.org",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "v1",
        ]);
        run(&["tag", "v1"]);
        let commit = run(&["rev-parse", "HEAD"]);
        std::fs::write(repo.join("bundles/ops/skills/s/SKILL.md"), "after v1").unwrap();
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.org",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-am",
            "v2",
        ]);

        let url = format!("file://{}", repo.display());
        let source = Source::Git {
            url: url.clone(),
            git_ref: "v1".into(),
            path: Some("bundles/ops".into()),
        };
        let added = add(&p, &source).unwrap();
        assert_eq!(added.id, "ops");
        let current = p.bundles_dir().join("ops");
        assert_eq!(
            std::fs::read_to_string(current.join("skills/s/SKILL.md")).unwrap(),
            "at v1"
        );
        assert!(!current.join(".git").exists());
        let v = &read_versions(&p, "ops").unwrap()[0];
        assert_eq!(
            v.origin,
            Origin::Git {
                url: url.clone(),
                git_ref: "v1".into(),
                commit,
                path: Some("bundles/ops".into())
            }
        );
        let no_fetch: Vec<String> = std::fs::read_dir(p.bundle_versions_root())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        assert!(no_fetch.is_empty(), "{no_fetch:?}");

        let bad = Source::Git {
            url: url.clone(),
            git_ref: "v9".into(),
            path: None,
        };
        assert_eq!(
            add(&p, &bad).unwrap_err().code(),
            crate::bundle::BUNDLE_FETCH_FAILED
        );
        let opt = Source::Git {
            url,
            git_ref: "--upload-pack=x".into(),
            path: None,
        };
        assert_eq!(
            add(&p, &opt).unwrap_err().code(),
            crate::bundle::BUNDLE_INVALID
        );
    }

    /// A bundle `fits` refuses is not copied and keeps no version.
    #[test]
    fn a_refused_bundle_is_not_added() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let src = tmp.path().join("src");
        bundle(&src, "b", "x");
        let err = add_checked(&p, &Source::Dir(src), &|b| {
            Err(format!("{} does not fit", b.id))
        })
        .unwrap_err();
        assert_eq!(err.code(), crate::bundle::BUNDLE_INVALID);
        assert_eq!(err.to_string(), "b does not fit");
        assert!(!p.bundles_dir().join("b").exists());
        assert!(read_versions(&p, "b").unwrap().is_empty());
    }

    #[test]
    fn the_catalog_reads_the_bundles_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        assert_eq!(Catalog::load(&p).unwrap(), Catalog::default());
        bundle(&p.bundles_dir().join("good"), "good", "x");
        bundle(&p.bundles_dir().join("misnamed"), "other", "x");
        std::fs::create_dir_all(p.bundles_dir().join(".add-good-1")).unwrap();
        let c = Catalog::load(&p).unwrap();
        assert_eq!(
            c.bundles().map(|b| b.id.as_str()).collect::<Vec<_>>(),
            ["good"]
        );
        assert!(
            c.check("misnamed")
                .unwrap_err()
                .contains("must be named after it")
        );
        assert!(c.check("absent").unwrap_err().contains("pastor bundle add"));
    }

    #[test]
    fn remove_takes_the_bundle_and_its_versions() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let src = tmp.path().join("src");
        bundle(&src, "b", "x");
        add(&p, &Source::Dir(src)).unwrap();
        assert_eq!(remove(&p, "b").unwrap(), 1);
        assert!(!p.bundles_dir().join("b").exists());
        assert!(!p.bundle_versions_dir("b").exists());
    }
}
