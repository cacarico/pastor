//! Putting connectors in place and taking them away. `install` clones a GitHub
//! repo into a managed directory; `link` symlinks a directory the user works
//! in. Neither touches the connector's `.env` or state, and removing a connector
//! leaves them too: they are the user's.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use super::manifest::Manifest;
use super::{Connector, load_manifest};
use crate::config::{Paths, create_private_dir};

/// Where `owner/repo` is cloned from. Overridable (a mirror, or a local
/// directory of repos in tests) with `PASTOR_CONNECTOR_GIT_BASE`.
pub const DEFAULT_GIT_BASE: &str = "https://github.com";

/// `owner/repo[/subdir]`, resolved against a git base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallSource {
    pub url: String,
    pub subdir: Option<PathBuf>,
}

impl InstallSource {
    pub fn parse(spec: &str, base: &str) -> Result<InstallSource, String> {
        let bad = || format!("{spec:?} is not owner/repo or owner/repo/subdir");
        let mut parts = spec.trim_matches('/').splitn(3, '/');
        let owner = parts.next().filter(|s| !s.is_empty()).ok_or_else(bad)?;
        let repo = parts.next().filter(|s| !s.is_empty()).ok_or_else(bad)?;
        let segment_ok = |s: &str| {
            s != "."
                && s != ".."
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        };
        if !segment_ok(owner) || !segment_ok(repo) {
            return Err(bad());
        }
        let subdir = match parts.next() {
            None => None,
            Some(sub) => {
                let p = PathBuf::from(sub);
                if !p.components().all(|c| matches!(c, Component::Normal(_))) {
                    return Err(format!("{spec:?}: subdir must stay inside the repo"));
                }
                Some(p)
            }
        };
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        Ok(InstallSource {
            url: format!("{}/{owner}/{repo}.git", base.trim_end_matches('/')),
            subdir,
        })
    }
}

pub fn git_base() -> String {
    std::env::var("PASTOR_CONNECTOR_GIT_BASE").unwrap_or_else(|_| DEFAULT_GIT_BASE.into())
}

/// Removes a half-done install whatever happens.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn git(args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("git")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("run git (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Clone, validate, ask, move into place. `confirm` sees the validated
/// manifest before anything lands in the connectors dir and may refuse.
pub fn install(
    paths: &Paths,
    source: &InstallSource,
    git_ref: Option<&str>,
    confirm: impl FnOnce(&Manifest) -> anyhow::Result<bool>,
) -> anyhow::Result<Connector> {
    // `git checkout` would read a ref that starts with `-` as an option.
    if let Some(r) = git_ref
        && (r.is_empty() || r.starts_with('-'))
    {
        bail!("ref {r:?} is not a branch, tag or commit");
    }
    let root = paths.connectors_dir();
    create_private_dir(&root)?;
    // Dot-named, so discovery never sees a half-done install.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let scratch = Scratch(root.join(format!(".install-{}-{nanos}", std::process::id())));
    let checkout = scratch.0.join("checkout");
    let checkout_s = checkout.to_string_lossy();
    match git_ref {
        // A shallow clone cannot check out an arbitrary commit.
        Some(r) => {
            git(&["clone", "--quiet", "--", &source.url, &checkout_s])?;
            git(&["-C", &checkout_s, "checkout", "--quiet", r])?;
        }
        None => git(&[
            "clone",
            "--quiet",
            "--depth",
            "1",
            "--",
            &source.url,
            &checkout_s,
        ])?,
    }
    // Move the real directory, never a link: renaming a symlinked subdir
    // would move the link, and dropping the scratch clone would then delete
    // its target. A link that resolves outside the clone is refused, since
    // the install would copy someone else's directory.
    let root_real = std::fs::canonicalize(&checkout)
        .with_context(|| format!("resolve {}", checkout.display()))?;
    let dir = match &source.subdir {
        Some(s) => {
            let dir = std::fs::canonicalize(checkout.join(s))
                .with_context(|| format!("subdir {} is not in the repository", s.display()))?;
            if !dir.starts_with(&root_real) {
                bail!(
                    "subdir {} resolves to {}, outside the repository; refusing to install it",
                    s.display(),
                    dir.display()
                );
            }
            dir
        }
        None => root_real,
    };
    let manifest = load_manifest(&dir, None).map_err(anyhow::Error::msg)?;
    let target = root.join(&manifest.id);
    if target.symlink_metadata().is_ok() {
        bail!(
            "connector {:?} is already in {}; uninstall or unlink it first",
            manifest.id,
            target.display()
        );
    }
    if !confirm(&manifest)? {
        bail!("install of {:?} cancelled", manifest.id);
    }
    std::fs::rename(&dir, &target)
        .with_context(|| format!("move {} to {}", dir.display(), target.display()))?;
    Ok(Connector {
        id: manifest.id.clone(),
        dir: target,
        linked: false,
        manifest,
    })
}

/// Symlink `path` into the connectors dir under its manifest's id, for
/// developing a connector in place.
pub fn link(paths: &Paths, path: &Path) -> anyhow::Result<Connector> {
    let dir = std::fs::canonicalize(path).with_context(|| format!("{}", path.display()))?;
    let manifest = load_manifest(&dir, None).map_err(anyhow::Error::msg)?;
    let root = paths.connectors_dir();
    create_private_dir(&root)?;
    let target = root.join(&manifest.id);
    if target.symlink_metadata().is_ok() {
        bail!(
            "connector {:?} is already in {}; uninstall or unlink it first",
            manifest.id,
            target.display()
        );
    }
    std::os::unix::fs::symlink(&dir, &target)
        .with_context(|| format!("link {} to {}", target.display(), dir.display()))?;
    Ok(Connector {
        id: manifest.id.clone(),
        dir,
        linked: true,
        manifest,
    })
}

/// Why the code in a directory about to be linked could change under pastor:
/// the directory or its manifest is writable by group or others, or owned by
/// a uid other than `euid`. A linked connector runs from that directory, and
/// hooks re-read their connector on every event, so whoever can change it
/// changes what runs next. Empty when there is nothing to say.
pub fn link_warnings(dir: &Path, euid: u32) -> Vec<String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let mut out = Vec::new();
    for path in [dir.to_path_buf(), dir.join(super::manifest::MANIFEST_FILE)] {
        let Ok(md) = std::fs::metadata(&path) else {
            continue;
        };
        let mode = md.permissions().mode();
        let shown = path.display();
        if mode & 0o002 != 0 {
            out.push(format!("{shown} is world-writable"));
        } else if mode & 0o020 != 0 {
            out.push(format!("{shown} is group-writable"));
        }
        if md.uid() != euid {
            out.push(format!("{shown} is owned by uid {}, not you", md.uid()));
        }
    }
    out
}

enum Kind {
    Linked,
    Managed,
}

fn kind_of(paths: &Paths, id: &str) -> anyhow::Result<(PathBuf, Kind)> {
    super::manifest::check_id(id).map_err(anyhow::Error::msg)?;
    let path = paths.connectors_dir().join(id);
    let md = match path.symlink_metadata() {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "no connector {id:?} in {}",
                paths.connectors_dir().display()
            )
        }
        Err(e) => return Err(e).with_context(|| format!("{}", path.display())),
    };
    let kind = if md.file_type().is_symlink() {
        Kind::Linked
    } else {
        Kind::Managed
    };
    Ok((path, kind))
}

/// Delete a managed checkout. A linked connector is refused: its directory is
/// the user's, and `unlink` is the command that leaves it alone.
pub fn uninstall(paths: &Paths, id: &str) -> anyhow::Result<()> {
    match kind_of(paths, id)? {
        (_, Kind::Linked) => {
            bail!("connector {id:?} is linked; use `pastor connector unlink {id}`")
        }
        (path, Kind::Managed) => {
            std::fs::remove_dir_all(&path).with_context(|| format!("remove {}", path.display()))
        }
    }
}

/// Remove the symlink only; the linked directory is untouched.
pub fn unlink(paths: &Paths, id: &str) -> anyhow::Result<()> {
    match kind_of(paths, id)? {
        (_, Kind::Managed) => {
            bail!(
                "connector {id:?} is installed, not linked; use `pastor connector uninstall {id}`"
            )
        }
        (path, Kind::Linked) => {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ref is handed to `git checkout`, which would read `-...` as an
    /// option; it is refused before anything is cloned.
    #[test]
    fn a_ref_that_looks_like_an_option_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let src = InstallSource::parse("o/r", "file:///nonexistent").unwrap();
        for r in ["-b", "--orphan=x", ""] {
            let err = install(&paths, &src, Some(r), |_| Ok(true)).unwrap_err();
            assert!(format!("{err:#}").contains("ref"), "{r}: {err:#}");
        }
    }

    /// `link` runs code from a directory pastor does not manage; one that
    /// others can write to, or that someone else owns, is worth a warning,
    /// since a change there runs at the next event.
    #[test]
    fn link_warns_about_a_directory_others_can_change() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("conn");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("pastor-connector.toml"), "").unwrap();
        let mode =
            |p: &Path, m| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
        mode(&dir, 0o755);
        mode(&dir.join("pastor-connector.toml"), 0o644);
        let me = std::fs::metadata(&dir).unwrap().uid();
        assert_eq!(link_warnings(&dir, me), Vec::<String>::new());

        mode(&dir, 0o777);
        mode(&dir.join("pastor-connector.toml"), 0o664);
        let w = link_warnings(&dir, me).join("\n");
        assert!(w.contains("conn is world-writable"), "{w}");
        assert!(w.contains("pastor-connector.toml is group-writable"), "{w}");

        mode(&dir, 0o755);
        mode(&dir.join("pastor-connector.toml"), 0o644);
        let w = link_warnings(&dir, me + 1).join("\n");
        assert!(w.contains(&format!("owned by uid {me}")), "{w}");
    }

    #[test]
    fn parses_install_sources() {
        let s = InstallSource::parse("cacarico/pastor/connectors/slack", DEFAULT_GIT_BASE).unwrap();
        assert_eq!(s.url, "https://github.com/cacarico/pastor.git");
        assert_eq!(s.subdir, Some(PathBuf::from("connectors/slack")));
        let s = InstallSource::parse("o/r.git", "file:///tmp/repos/").unwrap();
        assert_eq!(s.url, "file:///tmp/repos/o/r.git");
        assert_eq!(s.subdir, None);
        for bad in [
            "",
            "owner",
            "owner/",
            "../x",
            "o/r/../../etc",
            "o/r//abs",
            "a b/c",
        ] {
            assert!(
                InstallSource::parse(bad, DEFAULT_GIT_BASE).is_err(),
                "{bad}"
            );
        }
    }
}
