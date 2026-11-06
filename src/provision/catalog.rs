//! Which provisioners exist: the ones built into the binary
//! (`provisioners/<id>/pastor-provisioner.toml` in the repository, embedded
//! at build time) and those copied by hand into `<data>/provisioners/<id>/`,
//! the directory convention connectors use. A copied one with a built-in's
//! id takes its place, valid or not: whoever copied it meant it to, and
//! falling back to the built-in would hide a broken copy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;

use super::manifest::{MANIFEST_FILE, Manifest};
use crate::config::Paths;
use crate::connector::manifest::check_id;

/// The built-in provisioners, by id, as their manifests read in the
/// repository. A test checks each parses and its id matches its directory.
pub const BUILTIN: &[(&str, &str)] = &[
    (
        "arch",
        include_str!("../../provisioners/arch/pastor-provisioner.toml"),
    ),
    (
        "debian",
        include_str!("../../provisioners/debian/pastor-provisioner.toml"),
    ),
    (
        "macos",
        include_str!("../../provisioners/macos/pastor-provisioner.toml"),
    ),
    (
        "raspberry-pi-os",
        include_str!("../../provisioners/raspberry-pi-os/pastor-provisioner.toml"),
    ),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provisioner {
    pub id: String,
    /// The copied directory, or `None` for a built-in one.
    pub dir: Option<PathBuf>,
    pub manifest: Manifest,
}

impl Provisioner {
    pub fn builtin(&self) -> bool {
        self.dir.is_none()
    }
}

/// A copied provisioner that does not load, kept so a machine naming it
/// gets the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    pub id: String,
    pub dir: PathBuf,
    pub error: String,
}

/// The provisioners a flock may name, read once; `Catalog::load` reads the
/// copied ones again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    valid: BTreeMap<String, Provisioner>,
    invalid: BTreeMap<String, Invalid>,
}

/// Read and validate the manifest in `dir`, which must declare `id` when
/// one is given: the directory name is the id machines name it by.
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

impl Catalog {
    /// Only what ships in the binary.
    pub fn builtin() -> Catalog {
        let mut valid = BTreeMap::new();
        for (id, text) in BUILTIN {
            let manifest =
                Manifest::parse(text).unwrap_or_else(|e| panic!("built-in provisioner {id}: {e}"));
            assert_eq!(manifest.id, *id, "built-in provisioner {id}: id mismatch");
            valid.insert(
                id.to_string(),
                Provisioner {
                    id: id.to_string(),
                    dir: None,
                    manifest,
                },
            );
        }
        Catalog {
            valid,
            invalid: BTreeMap::new(),
        }
    }

    /// The built-ins, then every directory under the provisioners dir, each
    /// valid or invalid with its reason; a copy's id replaces a built-in's.
    /// Entries starting with `.` and plain files are skipped, and a missing
    /// directory is no copies.
    pub fn load(paths: &Paths) -> anyhow::Result<Catalog> {
        let mut catalog = Catalog::builtin();
        let root = paths.provisioners_dir();
        let entries = match std::fs::read_dir(&root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(catalog),
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
            // A symlink to a directory counts; `metadata` follows it. A
            // symlink whose target has gone missing fails `metadata`
            // instead of answering `false`, so it falls through to
            // `load_manifest` and comes back invalid, rather than `is_dir`
            // quietly skipping it and leaving the built-in in its place.
            if matches!(std::fs::metadata(&path), Ok(meta) if !meta.is_dir()) {
                continue;
            }
            match check_id(&id).and_then(|()| load_manifest(&path, Some(&id))) {
                Ok(manifest) => {
                    catalog.invalid.remove(&id);
                    catalog.valid.insert(
                        id.clone(),
                        Provisioner {
                            id,
                            dir: Some(path),
                            manifest,
                        },
                    );
                }
                Err(error) => {
                    catalog.valid.remove(&id);
                    catalog.invalid.insert(
                        id.clone(),
                        Invalid {
                            id,
                            dir: path,
                            error,
                        },
                    );
                }
            }
        }
        Ok(catalog)
    }

    /// A catalog of just these, for a test.
    #[cfg(test)]
    pub fn of(provisioners: Vec<Provisioner>) -> Catalog {
        Catalog {
            valid: provisioners
                .into_iter()
                .map(|p| (p.id.clone(), p))
                .collect(),
            invalid: BTreeMap::new(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&Provisioner> {
        self.valid.get(id)
    }

    /// Err(reason) when no provisioner has that id, or the one that has it
    /// does not load.
    pub fn check(&self, id: &str) -> Result<(), String> {
        if self.valid.contains_key(id) {
            return Ok(());
        }
        match self.invalid.get(id) {
            Some(bad) => Err(format!("provisioner {id:?} does not load: {}", bad.error)),
            None => Err(format!(
                "provisioner {id:?} is not available: no built-in or copied provisioner has that id"
            )),
        }
    }

    /// The usable provisioners, by id.
    pub fn provisioners(&self) -> impl Iterator<Item = &Provisioner> {
        self.valid.values()
    }

    /// The copied ones that do not load, by id.
    pub fn invalid(&self) -> impl Iterator<Item = &Invalid> {
        self.invalid.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provision::manifest::{AGENT_KINDS, Account, Check, Os};

    fn write(paths: &Paths, id: &str, text: &str) -> PathBuf {
        let dir = paths.provisioners_dir().join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MANIFEST_FILE), text).unwrap();
        dir
    }

    fn manifest(id: &str) -> String {
        format!(
            "id = \"{id}\"\nversion = \"0.1.0\"\n[platform]\nos = \"linux\"\n[[check]]\nid = \"a\"\ncommand = [\"true\"]\n"
        )
    }

    /// One per platform pastor is set up on, each with a github account
    /// check and an install and a login check for every agent kind.
    #[test]
    fn the_built_ins_load_and_check_every_agent_kind() {
        let catalog = Catalog::builtin();
        let ids: Vec<&str> = catalog.provisioners().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["arch", "debian", "macos", "raspberry-pi-os"]);
        assert_eq!(catalog.invalid().count(), 0);
        for p in catalog.provisioners() {
            assert!(p.builtin());
            assert_eq!(p.manifest.id, p.id);
            let checks = &p.manifest.checks;
            assert!(
                checks.iter().any(|c| c.account == Some(Account::Github)),
                "{}: no check for the github account",
                p.id
            );
            for kind in AGENT_KINDS {
                let of: Vec<(&str, &[String])> = checks
                    .iter()
                    .filter(|c| c.agent.as_deref() == Some(kind))
                    .map(|c| (c.id.as_str(), &c.needs[..]))
                    .collect();
                let login = format!("{kind}-login");
                assert_eq!(
                    of,
                    [(kind, &[][..]), (login.as_str(), &[kind.to_string()][..])],
                    "{}: the {kind} checks",
                    p.id
                );
            }
        }
        assert_eq!(
            catalog.get("macos").unwrap().manifest.platform.os,
            Os::Darwin
        );
        for (id, distro) in [
            ("arch", &["arch"][..]),
            ("debian", &["debian", "ubuntu"][..]),
            ("raspberry-pi-os", &["debian", "raspbian"][..]),
        ] {
            let platform = &catalog.get(id).unwrap().manifest.platform;
            assert_eq!(platform.os, Os::Linux, "{id}");
            assert_eq!(platform.distro, distro, "{id}");
        }
        assert!(catalog.check("macos").is_ok());
        let err = catalog.check("windows").unwrap_err();
        assert!(
            err.contains("windows") && err.contains("not available"),
            "{err}"
        );
    }

    /// Pastor never calls sudo, and OS packages are the machine owner's:
    /// a built-in fixes only pastor and the agents' CLIs, and says sudo
    /// nowhere.
    #[test]
    fn the_built_ins_fix_only_pastor_and_the_agents() {
        for (id, text) in BUILTIN {
            assert!(!text.contains("sudo"), "{id} says sudo");
            let manifest = Manifest::parse(text).unwrap();
            for c in &manifest.checks {
                if c.fix.is_some() {
                    assert!(
                        c.id == "pastor" || AGENT_KINDS.contains(&c.id.as_str()),
                        "{id}: {} has a fix",
                        c.id
                    );
                }
            }
            assert!(
                manifest.checks.iter().any(|c| c.fix.is_some()),
                "{id}: no check has a fix"
            );
        }
    }

    /// Every built-in checks the agents' CLIs the same way, so a fix to one
    /// of those checks cannot miss a platform.
    #[test]
    fn the_built_ins_check_the_agents_alike() {
        let catalog = Catalog::builtin();
        let agent_checks = |p: &Provisioner| -> Vec<Check> {
            p.manifest
                .checks
                .iter()
                .filter(|c| c.agent.is_some())
                .cloned()
                .collect()
        };
        let mut all = catalog.provisioners();
        let first = all.next().unwrap();
        for p in all {
            assert_eq!(
                agent_checks(p),
                agent_checks(first),
                "{} and {} differ",
                p.id,
                first.id
            );
        }
    }

    /// Runs `id`'s detect here with the device tree's model at a temp path
    /// holding `model` (none: no device tree), and gives its exit code.
    fn detect_with_model(id: &str, model: Option<&str>) -> Option<i32> {
        const MODEL: &str = "/proc/device-tree/model";
        let detect = Catalog::builtin()
            .get(id)
            .unwrap()
            .manifest
            .platform
            .detect
            .clone()
            .unwrap_or_else(|| panic!("{id} has no detect"));
        assert!(detect.iter().any(|w| w.contains(MODEL)), "{detect:?}");
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("model");
        if let Some(model) = model {
            std::fs::write(&path, format!("{model}\0")).unwrap();
        }
        let argv: Vec<String> = detect
            .iter()
            .map(|w| w.replace(MODEL, &path.to_string_lossy()))
            .collect();
        std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .unwrap()
            .code()
    }

    /// Raspberry Pi OS is Debian to os-release, so only `detect` tells a
    /// Pi from any other Debian machine: the device tree's model names it.
    /// The two built-ins split them, so whichever comes first by id, a
    /// Debian machine gets exactly one. No device tree is exit 1 for the
    /// Pi, not an error.
    #[test]
    fn debian_and_raspberry_pi_os_split_by_the_board_model() {
        let pi = "Raspberry Pi 4 Model B Rev 1.4";
        let vm = "QEMU Virtual Machine";
        assert_eq!(detect_with_model("raspberry-pi-os", None), Some(1));
        assert_eq!(detect_with_model("raspberry-pi-os", Some(pi)), Some(0));
        assert_eq!(detect_with_model("raspberry-pi-os", Some(vm)), Some(1));
        assert_eq!(detect_with_model("debian", None), Some(0));
        assert_eq!(detect_with_model("debian", Some(pi)), Some(1));
        assert_eq!(detect_with_model("debian", Some(vm)), Some(0));
    }

    /// Each login check against a stand-in for its CLI that answers as the
    /// real one does: exit 0 when logged in, 1 when the machine needs a
    /// login, anything else when the CLI could not say. The stand-in exits
    /// 9 when called with other arguments.
    #[test]
    fn each_login_check_reads_its_cli() {
        let catalog = Catalog::builtin();
        let checks = &catalog.get("arch").unwrap().manifest.checks;
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = format!("{}:/usr/bin:/bin", bin.display());
        // (kind, the arguments the check passes, what the CLI prints and
        // exits with, the exit the check gives, what its first line says)
        let cases: &[(&str, &str, &str, i32, &str)] = &[
            (
                "claude",
                "auth status --text",
                "echo 'Login method: Claude Max account'",
                0,
                "Login method",
            ),
            (
                "claude",
                "auth status --text",
                "echo 'Not logged in. Run claude auth login to authenticate.'; exit 1",
                1,
                "Not logged in",
            ),
            (
                "codex",
                "login status",
                "echo 'Logged in using ChatGPT' >&2",
                0,
                "",
            ),
            (
                "codex",
                "login status",
                "echo 'Not logged in' >&2; exit 1",
                1,
                "",
            ),
            (
                "agy",
                "models",
                "echo 'Fetching available models...'; echo gemini-4-argon",
                0,
                "",
            ),
            (
                "agy",
                "models",
                "echo 'Fetching available models...'; echo 'Error: Please sign in to view available models. Launch the CLI without arguments to sign in.'; exit 1",
                1,
                "Error: Please sign in",
            ),
            (
                "agy",
                "models",
                "echo 'Fetching available models...'; echo 'Error: dial tcp: lookup example.org: no such host'; exit 1",
                2,
                "Error: dial tcp",
            ),
            (
                "opencode",
                "auth list",
                "printf '%s\\n' 'Credentials ~/.local/share/opencode/auth.json' '1 credentials'",
                0,
                "1 credentials",
            ),
            (
                "opencode",
                "auth list",
                "printf '%s\\n' 'Credentials ~/.local/share/opencode/auth.json' '10 credentials'",
                0,
                "10 credentials",
            ),
            (
                "opencode",
                "auth list",
                "printf '%s\\n' 'Credentials ~/.local/share/opencode/auth.json' '0 credentials'",
                1,
                "no provider credentials",
            ),
            (
                "opencode",
                "auth list",
                "printf '%s\\n' 'Credentials ~/.local/share/opencode/auth.json' '0 credentials' Environment 'Anthropic ANTHROPIC_API_KEY' '1 environment variable'",
                0,
                "1 environment variable",
            ),
            (
                "opencode",
                "auth list",
                "echo 'No authenticated integrations'",
                1,
                "no provider credentials",
            ),
            (
                "opencode",
                "auth list",
                "echo 'Anthropic   oauth   stored'",
                0,
                "",
            ),
            (
                "opencode",
                "auth list",
                "echo 'Error: config is not valid JSON' >&2; exit 3",
                2,
                "Error: config is not valid JSON",
            ),
        ];
        for (kind, args, answer, want, line) in cases {
            let cli = bin.join(kind);
            std::fs::write(
                &cli,
                format!("#!/bin/sh\n[ \"$*\" = \"{args}\" ] || exit 9\n{answer}\n"),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
            let login = format!("{kind}-login");
            let check = checks.iter().find(|c| c.id == login).unwrap();
            let out = std::process::Command::new(&check.command[0])
                .args(&check.command[1..])
                .env("PATH", &path)
                .current_dir(tmp.path())
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            assert_eq!(
                out.status.code(),
                Some(*want),
                "{kind} answering {answer:?}: {text}{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let first = text.lines().next().unwrap_or("");
            assert!(first.contains(line), "{kind} answering {answer:?}: {text}");
        }
    }

    /// The repository's `provisioners/` directory and `BUILTIN` name the same
    /// set, so a manifest added on disk is not forgotten in the binary.
    #[test]
    fn the_repository_directory_matches_the_embedded_list() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("provisioners");
        let mut on_disk: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        on_disk.sort();
        let embedded: Vec<&str> = BUILTIN.iter().map(|(id, _)| *id).collect();
        assert_eq!(on_disk, embedded);
        for (id, text) in BUILTIN {
            let file = std::fs::read_to_string(root.join(id).join(MANIFEST_FILE)).unwrap();
            assert_eq!(&file, text, "{id}: the embedded manifest is stale");
        }
    }

    #[test]
    fn a_missing_directory_is_the_built_ins_only() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let catalog = Catalog::load(&paths).unwrap();
        assert_eq!(catalog, Catalog::builtin());
    }

    #[test]
    fn a_copied_provisioner_is_read_beside_the_built_ins() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let dir = write(&paths, "nixos", &manifest("nixos"));
        // Not provisioners: a plain file, a dot entry, a directory with no manifest.
        std::fs::write(paths.provisioners_dir().join("notes.txt"), "x").unwrap();
        std::fs::create_dir_all(paths.provisioners_dir().join(".scratch")).unwrap();
        std::fs::create_dir_all(paths.provisioners_dir().join("empty")).unwrap();
        let catalog = Catalog::load(&paths).unwrap();
        let ids: Vec<&str> = catalog.provisioners().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["arch", "debian", "macos", "nixos", "raspberry-pi-os"]);
        let nixos = catalog.get("nixos").unwrap();
        assert_eq!(nixos.dir.as_deref(), Some(dir.as_path()));
        assert!(!nixos.builtin());
        assert!(catalog.check("nixos").is_ok());
        let invalid: Vec<&str> = catalog.invalid().map(|i| i.id.as_str()).collect();
        assert_eq!(invalid, ["empty"]);
        let err = catalog.check("empty").unwrap_err();
        assert!(
            err.contains("does not load") && err.contains(MANIFEST_FILE),
            "{err}"
        );
    }

    /// A copy with a built-in's id replaces it, and a broken copy makes the
    /// id unusable rather than quietly falling back to the built-in.
    #[test]
    fn a_copy_replaces_a_built_in_of_the_same_id() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        write(
            &paths,
            "arch",
            &manifest("arch").replace("os = \"linux\"", "os = \"freebsd\""),
        );
        let catalog = Catalog::load(&paths).unwrap();
        let arch = catalog.get("arch").unwrap();
        assert!(!arch.builtin());
        assert_eq!(arch.manifest.platform.os, Os::Freebsd);

        write(&paths, "arch", "id = \"arch\"\n");
        let catalog = Catalog::load(&paths).unwrap();
        assert!(catalog.get("arch").is_none());
        let err = catalog.check("arch").unwrap_err();
        assert!(err.contains("does not load"), "{err}");
    }

    /// Copilot 4281285713: a copied override whose target has gone missing
    /// must make its id invalid, not quietly leave the built-in in place.
    #[test]
    #[cfg(unix)]
    fn a_broken_symlink_override_makes_the_id_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        std::fs::create_dir_all(paths.provisioners_dir()).unwrap();
        std::os::unix::fs::symlink(
            paths.provisioners_dir().join("ghost"),
            paths.provisioners_dir().join("arch"),
        )
        .unwrap();
        let catalog = Catalog::load(&paths).unwrap();
        assert!(catalog.get("arch").is_none());
        let err = catalog.check("arch").unwrap_err();
        assert!(err.contains("does not load"), "{err}");
    }

    #[test]
    fn a_copy_must_match_its_directory_name_and_the_id_alphabet() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        write(&paths, "mine", &manifest("other"));
        write(&paths, "Bad", &manifest("Bad"));
        let catalog = Catalog::load(&paths).unwrap();
        let err = catalog.check("mine").unwrap_err();
        assert!(err.contains("does not match the directory name"), "{err}");
        let err = catalog.check("Bad").unwrap_err();
        assert!(err.contains("must match"), "{err}");
    }
}
