//! Plugins: a directory with `pastor-plugin.toml` and commands, providing a
//! connector, event hooks, or both. Each lives at `<data>/plugins/<id>/`,
//! either a managed checkout (`plugin install`) or a symlink to a directory
//! the user develops in (`plugin link`).

pub mod env;
pub mod manifest;

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::config::Paths;
use env::Redactor;
use manifest::{MANIFEST_FILE, Manifest};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    pub id: String,
    /// The directory commands run in: the link's target for a linked plugin.
    pub dir: PathBuf,
    pub linked: bool,
    pub manifest: Manifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovered {
    Valid(Box<Plugin>),
    Invalid {
        id: String,
        dir: PathBuf,
        linked: bool,
        error: String,
    },
}

impl Discovered {
    pub fn id(&self) -> &str {
        match self {
            Discovered::Valid(p) => &p.id,
            Discovered::Invalid { id, .. } => id,
        }
    }
}

/// Read and validate the manifest in `dir`, which must declare `id`: the
/// directory name is the id pastor, jobs and hooks know the plugin by.
pub fn load_manifest(dir: &Path, id: Option<&str>) -> Result<Manifest, String> {
    let path = dir.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let m = Manifest::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if let Some(id) = id
        && m.id != id
    {
        return Err(format!(
            "{}: id {:?} does not match the directory name {id:?}",
            path.display(),
            m.id
        ));
    }
    Ok(m)
}

/// Every plugin under the plugins dir, sorted by id, each valid or invalid
/// with its reason. Entries starting with `.` are pastor's own scratch (an
/// install in progress) and skipped. A missing directory is no plugins.
pub fn discover(paths: &Paths) -> anyhow::Result<Vec<Discovered>> {
    let root = paths.plugins_dir();
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("read {}", root.display())),
    };
    for entry in entries {
        let entry = entry?;
        let Some(id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if id.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let linked = entry.file_type()?.is_symlink();
        let dir = if linked {
            match std::fs::canonicalize(&path) {
                Ok(d) => d,
                Err(e) => {
                    out.push(Discovered::Invalid {
                        id,
                        dir: path,
                        linked,
                        error: format!("link target is gone: {e}"),
                    });
                    continue;
                }
            }
        } else if path.is_dir() {
            path
        } else {
            continue;
        };
        match manifest::check_id(&id).and_then(|_| load_manifest(&dir, Some(&id))) {
            Ok(manifest) => out.push(Discovered::Valid(Box::new(Plugin {
                id,
                dir,
                linked,
                manifest,
            }))),
            Err(error) => out.push(Discovered::Invalid {
                id,
                dir,
                linked,
                error,
            }),
        }
    }
    out.sort_by(|a, b| a.id().cmp(b.id()));
    Ok(out)
}

impl Plugin {
    /// The plugin's `.env`, as loaded into its commands.
    pub fn env(&self, paths: &Paths) -> anyhow::Result<Vec<(String, String)>> {
        env::load(&paths.plugin_env_file(&self.id))
    }

    /// Declared secrets the `.env` does not set, or sets empty.
    pub fn missing_secrets(&self, env: &[(String, String)]) -> Vec<String> {
        self.manifest
            .secrets
            .keys()
            .filter(|name| !env.iter().any(|(k, v)| k == *name && !v.is_empty()))
            .cloned()
            .collect()
    }

    pub fn redactor(&self, env: &[(String, String)]) -> Redactor {
        Redactor::new(self.manifest.secrets.keys().map(String::as_str), env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_plugin(dir: &Path, id: &str, extra: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            format!(
                "id = \"{id}\"\nversion = \"0.1.0\"\n[connector]\ncommand = [\"true\"]\n{extra}"
            ),
        )
        .unwrap();
    }

    #[test]
    fn discovers_managed_linked_and_broken_plugins() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        assert!(discover(&paths).unwrap().is_empty(), "no dir, no plugins");
        let root = paths.plugins_dir();
        write_plugin(&root.join("alpha"), "alpha", "");
        write_plugin(&root.join("mismatch"), "other", "");
        std::fs::create_dir_all(root.join(".install-1")).unwrap();
        std::fs::write(root.join("stray-file"), "").unwrap();
        let dev = tmp.path().join("dev/beta");
        write_plugin(&dev, "beta", "");
        std::os::unix::fs::symlink(&dev, root.join("beta")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("gone"), root.join("dangling")).unwrap();

        let found = discover(&paths).unwrap();
        let ids: Vec<&str> = found.iter().map(Discovered::id).collect();
        assert_eq!(ids, vec!["alpha", "beta", "dangling", "mismatch"]);
        let Discovered::Valid(alpha) = &found[0] else {
            panic!("{:?}", found[0])
        };
        assert!(!alpha.linked);
        let Discovered::Valid(beta) = &found[1] else {
            panic!("{:?}", found[1])
        };
        assert!(beta.linked);
        assert_eq!(beta.dir, std::fs::canonicalize(&dev).unwrap());
        let Discovered::Invalid { error, .. } = &found[2] else {
            panic!()
        };
        assert!(error.contains("link target is gone"), "{error}");
        let Discovered::Invalid { error, .. } = &found[3] else {
            panic!()
        };
        assert!(
            error.contains("does not match the directory name"),
            "{error}"
        );
    }

    #[test]
    fn missing_secrets_and_redaction_follow_the_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let dir = paths.plugins_dir().join("slack");
        write_plugin(&dir, "slack", "[secrets.TOKEN]\n[secrets.SIGNING]\n");
        let Discovered::Valid(p) = discover(&paths).unwrap().remove(0) else {
            panic!()
        };
        assert!(p.env(&paths).unwrap().is_empty());
        assert_eq!(p.missing_secrets(&[]), vec!["SIGNING", "TOKEN"]);
        let env_file = paths.plugin_env_file("slack");
        std::fs::create_dir_all(env_file.parent().unwrap()).unwrap();
        std::fs::write(&env_file, "TOKEN=xoxb-9999\nSIGNING=\nOTHER=abcdef\n").unwrap();
        let env = p.env(&paths).unwrap();
        assert_eq!(p.missing_secrets(&env), vec!["SIGNING"]);
        assert_eq!(
            p.redactor(&env).redact("xoxb-9999 abcdef"),
            "[redacted:TOKEN] abcdef"
        );
    }
}
