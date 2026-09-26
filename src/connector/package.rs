//! Installed connectors: a directory with `pastor-connector.toml` and
//! commands, providing a connector command, event hooks, or both. Each lives
//! at `<data>/connectors/<id>/`, either a managed checkout (`connector
//! install`) or a symlink to a directory the user develops in (`connector
//! link`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use serde_json::Value;

use super::env::{self, Redactor};
use super::manifest::{self, MANIFEST_FILE, Manifest};
use crate::config::Paths;
use crate::connector::{self, Builtins, Catalog, ItemSource};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connector {
    pub id: String,
    /// The directory commands run in: the link's target for a linked connector.
    pub dir: PathBuf,
    pub linked: bool,
    pub manifest: Manifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovered {
    Valid(Box<Connector>),
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
/// directory name is the id pastor, jobs and hooks know the connector by.
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

/// Every connector under the connectors dir, sorted by id, each valid or invalid
/// with its reason. Entries starting with `.` are pastor's own scratch (an
/// install in progress) and skipped. A missing directory is no connectors.
pub fn discover(paths: &Paths) -> anyhow::Result<Vec<Discovered>> {
    let root = paths.connectors_dir();
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
            Ok(manifest) => out.push(Discovered::Valid(Box::new(Connector {
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

impl Connector {
    /// The connector's `.env`, as loaded into its commands. A declared secret
    /// with a line break is refused: output is redacted one line at a time,
    /// so no line would ever contain the whole value and it would reach the
    /// logs in pieces.
    pub fn env(&self, paths: &Paths) -> anyhow::Result<Vec<(String, String)>> {
        let file = paths.connector_env_file(&self.id);
        let env = env::load(&file)?;
        for (name, value) in &env {
            if self.manifest.secrets.contains_key(name) && value.contains(['\n', '\r']) {
                anyhow::bail!(
                    "{}: secret {name} contains a line break; it could not be redacted from output line by line",
                    file.display()
                );
            }
        }
        Ok(env)
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

    /// The environment every command of this connector gets, connector command or hook:
    /// its `.env`, then `PASTOR_CONNECTOR_ID`, `PASTOR_JOB` (when there is a
    /// job), `PASTOR_CONFIG_DIR`, `PASTOR_STATE_DIR` and
    /// `PASTOR_CONNECTOR_STATE_DIR`, the job's scratch dir, created 0700. The
    /// job's scratch belongs to the connector the job uses: without a job, or
    /// for a hook hearing about a job it does not `own`, the dir is the
    /// connector's own `@<id>`. Plus the redactor for what the command prints.
    pub fn command_env(
        &self,
        paths: &Paths,
        job: Option<&str>,
        owns_job: bool,
    ) -> anyhow::Result<(Vec<(String, String)>, Redactor)> {
        let dotenv = self.env(paths)?;
        let redactor = self.redactor(&dotenv);
        let scope = job
            .filter(|_| owns_job)
            .map(str::to_string)
            .unwrap_or_else(|| format!("@{}", self.id));
        let state_dir = paths.connector_state_dir(&scope);
        crate::config::create_private_dir(&state_dir)?;
        let dir = |p: &Path| p.to_string_lossy().into_owned();
        let mut env = dotenv;
        env.push(("PASTOR_CONNECTOR_ID".into(), self.id.clone()));
        if let Some(job) = job {
            env.push(("PASTOR_JOB".into(), job.to_string()));
        }
        env.push(("PASTOR_CONFIG_DIR".into(), dir(&paths.config_dir)));
        env.push(("PASTOR_STATE_DIR".into(), dir(&paths.state_dir)));
        env.push(("PASTOR_CONNECTOR_STATE_DIR".into(), dir(&state_dir)));
        Ok((env, redactor))
    }
}

/// The built-in connectors plus every valid connector that has a connector command,
/// read once from the connectors dir. Connectors change only through `connector
/// install|link|uninstall|unlink`, so a daemon rebuilds this on reload rather
/// than watching the directory.
pub struct ConnectorCatalog {
    paths: Paths,
    connectors: BTreeMap<String, Arc<Connector>>,
    /// id -> why it is unusable, so a job naming it says so.
    invalid: BTreeMap<String, String>,
    /// Sources handed out so far, keyed by (connector, job): a stream connector
    /// must be one process however often the scheduler asks for it.
    sources: Mutex<HashMap<SourceKey, Arc<dyn ItemSource>>>,
}

/// (connector id, job name).
type SourceKey = (String, Option<String>);

impl ConnectorCatalog {
    pub fn load(paths: &Paths) -> anyhow::Result<ConnectorCatalog> {
        let mut connectors = BTreeMap::new();
        let mut invalid = BTreeMap::new();
        for d in discover(paths)? {
            match d {
                Discovered::Valid(p) => {
                    connectors.insert(p.id.clone(), Arc::from(p));
                }
                Discovered::Invalid { id, error, .. } => {
                    invalid.insert(id, error);
                }
            }
        }
        Ok(ConnectorCatalog {
            paths: paths.clone(),
            connectors,
            invalid,
            sources: Mutex::default(),
        })
    }

    pub fn connector(&self, id: &str) -> Option<&Arc<Connector>> {
        self.connectors.get(id)
    }

    fn source_keyed(&self, id: &str, job: Option<&str>) -> Option<Arc<dyn ItemSource>> {
        if let Some(b) = connector::builtin(id) {
            return Some(b);
        }
        let connector = self.connectors.get(id)?;
        connector.manifest.connector.as_ref()?;
        let key = (id.to_string(), job.map(str::to_string));
        let mut sources = self.sources.lock().unwrap_or_else(|p| p.into_inner());
        let src = sources.entry(key).or_insert_with(|| {
            connector::process::source(
                connector.clone(),
                self.paths.clone(),
                job.map(str::to_string),
            )
        });
        Some(src.clone())
    }
}

impl Catalog for ConnectorCatalog {
    fn source(&self, id: &str) -> Option<Arc<dyn ItemSource>> {
        self.source_keyed(id, None)
    }

    /// `PASTOR_JOB`, the run log and the scratch dir are the job's. What the
    /// scheduler uses.
    fn source_for_job(&self, id: &str, job: &str) -> Option<Arc<dyn ItemSource>> {
        self.source_keyed(id, Some(job))
    }

    fn retain_jobs(&self, keep: &HashSet<(String, String)>) {
        let mut sources = self.sources.lock().unwrap_or_else(|p| p.into_inner());
        sources.retain(|(id, job), _| match job {
            Some(job) => keep.contains(&(id.clone(), job.clone())),
            None => true,
        });
    }

    fn check(&self, id: &str, config: &Value) -> Result<(), String> {
        if connector::builtin(id).is_some() {
            return Builtins.check(id, config);
        }
        if let Some(p) = self.connectors.get(id) {
            return p.manifest.check_config(config);
        }
        if let Some(why) = self.invalid.get(id) {
            return Err(format!("connector {id:?} is invalid: {why}"));
        }
        Builtins.check(id, config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_connector(dir: &Path, id: &str, extra: &str) {
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
    fn discovers_managed_linked_and_broken_connectors() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        assert!(
            discover(&paths).unwrap().is_empty(),
            "no dir, no connectors"
        );
        let root = paths.connectors_dir();
        write_connector(&root.join("alpha"), "alpha", "");
        write_connector(&root.join("mismatch"), "other", "");
        std::fs::create_dir_all(root.join(".install-1")).unwrap();
        std::fs::write(root.join("stray-file"), "").unwrap();
        let dev = tmp.path().join("dev/beta");
        write_connector(&dev, "beta", "");
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
    fn the_catalog_checks_config_and_reuses_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let root = paths.connectors_dir();
        write_connector(
            &root.join("slack"),
            "slack",
            "[connector.config.channel]\nrequired = true\n",
        );
        write_connector(&root.join("clock"), "clock", "");
        write_connector(&root.join("broken"), "nope", "");
        std::fs::create_dir_all(root.join("ntfy")).unwrap();
        std::fs::write(
            root.join("ntfy").join(MANIFEST_FILE),
            "id = \"ntfy\"\nversion = \"0.1.0\"\n[[events]]\non = [\"task.done\"]\ncommand = [\"x\"]\n",
        )
        .unwrap();
        let cat = ConnectorCatalog::load(&paths).unwrap();
        let cfg = serde_json::json!({"channel": "C1"});
        assert!(cat.check("slack", &cfg).is_ok());
        let err = cat.check("slack", &serde_json::json!({})).unwrap_err();
        assert!(err.contains("requires connector.channel"), "{err}");
        assert!(cat.check("clock", &serde_json::json!({})).is_ok());
        assert_eq!(cat.source("clock").unwrap().id(), "clock");
        let err = cat.check("broken", &cfg).unwrap_err();
        assert!(
            err.contains("is invalid") && err.contains("does not match"),
            "{err}"
        );
        let err = cat.check("ntfy", &cfg).unwrap_err();
        assert!(err.contains("no connector"), "{err}");
        assert!(cat.source("ntfy").is_none());
        let err = cat.check("asana", &cfg).unwrap_err();
        assert!(err.contains("not available"), "{err}");

        let a = cat.source_for_job("slack", "j1").unwrap();
        let b = cat.source_for_job("slack", "j1").unwrap();
        let c = cat.source_for_job("slack", "j2").unwrap();
        assert!(Arc::ptr_eq(&a, &b), "one source per (connector, job)");
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(a.id(), "slack");

        // Only the jobs the scheduler still has keep their sources.
        cat.retain_jobs(&HashSet::from([("slack".to_string(), "j1".to_string())]));
        assert!(Arc::ptr_eq(&a, &cat.source_for_job("slack", "j1").unwrap()));
        assert!(!Arc::ptr_eq(
            &c,
            &cat.source_for_job("slack", "j2").unwrap()
        ));
    }

    #[test]
    fn missing_secrets_and_redaction_follow_the_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let dir = paths.connectors_dir().join("slack");
        write_connector(&dir, "slack", "[secrets.TOKEN]\n[secrets.SIGNING]\n");
        let Discovered::Valid(p) = discover(&paths).unwrap().remove(0) else {
            panic!()
        };
        assert!(p.env(&paths).unwrap().is_empty());
        assert_eq!(p.missing_secrets(&[]), vec!["SIGNING", "TOKEN"]);
        let env_file = paths.connector_env_file("slack");
        std::fs::create_dir_all(env_file.parent().unwrap()).unwrap();
        std::fs::write(&env_file, "TOKEN=xoxb-9999\nSIGNING=\nOTHER=abcdef\n").unwrap();
        let env = p.env(&paths).unwrap();
        assert_eq!(p.missing_secrets(&env), vec!["SIGNING"]);
        assert_eq!(
            p.redactor(&env).redact("xoxb-9999 abcdef"),
            "[redacted:TOKEN] abcdef"
        );
    }

    /// Output is redacted a line at a time, so a secret with a line break in
    /// it could never match and would reach the logs whole. Such a `.env` is
    /// refused when it is loaded; an undeclared multiline setting is fine.
    #[test]
    fn a_multiline_declared_secret_is_refused_at_load() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let dir = paths.connectors_dir().join("slack");
        write_connector(&dir, "slack", "[secrets.TOKEN]\n");
        let Discovered::Valid(p) = discover(&paths).unwrap().remove(0) else {
            panic!()
        };
        let env_file = paths.connector_env_file("slack");
        std::fs::create_dir_all(env_file.parent().unwrap()).unwrap();
        for value in [r#""part-one\npart-two""#, "\"part-one\rpart-two\""] {
            std::fs::write(&env_file, format!("TOKEN={value}\n")).unwrap();
            let err = format!("{:#}", p.env(&paths).unwrap_err());
            assert!(
                err.contains("TOKEN") && err.contains("line break"),
                "{value}: {err}"
            );
            assert!(p.command_env(&paths, None, false).is_err());
        }
        std::fs::write(&env_file, "TOKEN=xoxb-9999\nNOTE=\"a\\nb\"\n").unwrap();
        let env = p.env(&paths).unwrap();
        assert_eq!(
            p.redactor(&env).redact("auth xoxb-9999"),
            "auth [redacted:TOKEN]"
        );
    }
}
