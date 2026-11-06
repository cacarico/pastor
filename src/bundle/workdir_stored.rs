//! Stored workdir bundles: a bundle with `target = "workdir"` in its
//! manifest, kept by `bundle add` like any other and named by a flock's or
//! a job's `workdir_bundles`. Before each start of such a task, the head
//! turns the newest version kept into one workdir set for the task's kind
//! of agent (`set_of`), whose `id` is the bundle's, and places it in the
//! agent's folder like the task's own set.
//!
//! A stored set never names a profile: only the task's own set carries the
//! task's, so two sets in one folder never both own the permission keys.

use std::collections::BTreeMap;
use std::path::Path;

use super::manifest::{Manifest, Target};
use super::workdir::{PLACED_KINDS, Set};
use crate::config::Paths;
use crate::config::flock::Flock;

/// Whether pastor places workdir sets for agents of `kind` (claude, agy).
pub fn places(kind: &str) -> bool {
    PLACED_KINDS.contains(&kind)
}

/// The set bundle `m`, in `dir`, gives an agent of `kind`, checked as a set
/// is, or `None` when it has nothing for that kind. `machine` picks the
/// `per_machine` instructions; `vars` fill `{name}` where a set fills its
/// own. `Err` says what is wrong, as `bundle add` would.
pub fn set_of(
    m: &Manifest,
    dir: &Path,
    kind: &str,
    machine: Option<&str>,
    vars: &BTreeMap<String, String>,
) -> Result<Option<serde_json::Value>, String> {
    let wants =
        |kinds: &Option<Vec<String>>| kinds.as_ref().is_none_or(|k| k.iter().any(|x| x == kind));
    let read = |src: &str| -> Result<String, String> {
        std::fs::read_to_string(dir.join(src))
            .map_err(|e| format!("{src}: {e}: a workdir bundle's files are text"))
    };
    let mut skills = Vec::new();
    for s in m.skills.iter().filter(|s| wants(&s.kinds)) {
        let name = s
            .src
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&s.src)
            .to_string();
        let mut files = BTreeMap::new();
        read_tree(&dir.join(&s.src), "", &mut files).map_err(|e| format!("skill {name}: {e}"))?;
        skills.push(serde_json::json!({"name": name, "files": files}));
    }
    let mut files = Vec::new();
    for f in m.files.iter().filter(|f| wants(&f.kinds)) {
        let mut file = serde_json::json!({
            "path": f.path.clone().unwrap_or_default(),
            "content": read(&f.src)?,
            "render": f.render.unwrap_or(false),
        });
        if let Some(mode) = &f.mode {
            file["mode"] = mode.clone().into();
        }
        files.push(file);
    }
    let mut instructions = None;
    if let Some(ins) = m.instructions.as_ref().filter(|i| wants(&i.kinds)) {
        let mut text = read(&ins.src)?;
        if let (Some(per), Some(machine)) = (&ins.per_machine, machine) {
            // A machine with no file of its own gets the shared text; a file
            // that is there and cannot be read is an error.
            let src = per.replace("{machine}", machine);
            match std::fs::read_to_string(dir.join(&src)) {
                Ok(more) => text = format!("{}\n\n{}", text.trim_end(), more),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{src}: {e}: a workdir bundle's files are text")),
            }
        }
        instructions = Some(text);
    }
    let settings = match m.settings.get(kind) {
        Some(s) => Some(
            serde_json::from_str::<serde_json::Value>(&read(&s.src)?)
                .map_err(|e| format!("settings.{kind}: {}: {e}", s.src))?,
        ),
        None => None,
    };
    if skills.is_empty() && files.is_empty() && instructions.is_none() && settings.is_none() {
        return Ok(None);
    }
    let mut doc = serde_json::json!({
        "id": m.id,
        "kind": kind,
        "skills": skills,
        "files": files,
        "vars": vars,
    });
    if let Some(text) = instructions {
        doc["instructions"] = text.into();
    }
    if let Some(settings) = settings {
        doc["settings"] = settings;
    }
    let set = Set::from_doc(&doc).map_err(|e| match e.downcast_ref::<crate::cli::CliError>() {
        Some(c) => format!("as a {kind} set: {} ({})", c.message, c.code),
        None => format!("as a {kind} set: {e:#}"),
    })?;
    Ok(Some(set.to_doc()))
}

/// Every file under `dir`, by its path below it, as text.
fn read_tree(dir: &Path, prefix: &str, out: &mut BTreeMap<String, String>) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let path = entry.path();
        if path.is_dir() {
            read_tree(&path, &rel, out)?;
        } else {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("{rel}: {e}: a workdir bundle's files are text"))?;
            out.insert(rel, text);
        }
    }
    Ok(())
}

/// The machines `m` has a `per_machine` instructions file for in `dir`: the
/// names that fill `{machine}` in a path that is there. A path is there when
/// anything is at it, a file that cannot be read included.
fn per_machine_names(m: &Manifest, dir: &Path) -> Vec<String> {
    let Some(per) = m.instructions.as_ref().and_then(|i| i.per_machine.as_ref()) else {
        return Vec::new();
    };
    // `{machine}` stands for a whole file or folder name: what comes before
    // it in its folder is a prefix, and what follows it up to the next `/`
    // a suffix.
    let Some((before, after)) = per.split_once("{machine}") else {
        return Vec::new();
    };
    let (folder, prefix) = before.rsplit_once('/').unwrap_or(("", before));
    let suffix = after.split('/').next().unwrap_or_default();
    let Ok(entries) = std::fs::read_dir(dir.join(folder)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let machine = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
            (!machine.is_empty()).then(|| machine.to_string())
        })
        .filter(|machine| {
            std::fs::symlink_metadata(dir.join(per.replace("{machine}", machine))).is_ok()
        })
        .collect();
    names.sort();
    names
}

/// `bundle add`'s check of a workdir bundle: it gives a set for at least
/// one kind pastor places, and each set it gives is one pastor would
/// place, also with each `per_machine` file the bundle holds appended, as
/// the machine it is for would get it. The vars are left as written, as a
/// set leaves an unknown one.
pub fn check(m: &Manifest, dir: &Path) -> Result<(), String> {
    let machines = per_machine_names(m, dir);
    let mut any = false;
    for kind in PLACED_KINDS {
        any |= set_of(m, dir, kind, None, &BTreeMap::new())?.is_some();
        for machine in &machines {
            set_of(m, dir, kind, Some(machine), &BTreeMap::new())?;
        }
    }
    if any {
        Ok(())
    } else {
        Err(format!(
            "a workdir bundle carries nothing for {}: name one in `kinds`",
            PLACED_KINDS.join(" or ")
        ))
    }
}

/// The set bundle `id` gives an agent of `kind` on `machine` in flock
/// `flock`, from the newest version kept, with the flock's `bundle_vars`
/// and `{flock}`. `None` when it has nothing for that kind. `Err` when the
/// bundle has no version kept, is not a workdir bundle, or the flock lacks
/// a var it declares.
pub fn stored_set(
    paths: &Paths,
    fleet: &Flock,
    id: &str,
    kind: &str,
    machine: &str,
    flock: &str,
) -> Result<Option<serde_json::Value>, String> {
    let versions = super::store::read_versions(paths, id).map_err(|e| format!("{e:#}"))?;
    let Some(newest) = versions.first() else {
        return Err(format!(
            "bundle {id} has no version kept; add it with `pastor bundle add`"
        ));
    };
    let dir = paths.bundle_versions_dir(id).join(&newest.hash);
    let m = Manifest::load(&dir).map_err(|e| format!("bundle {id} at {}: {e}", newest.hash))?;
    if m.target() != Target::Workdir {
        return Err(format!(
            "bundle {id} is a home bundle (its target is {}): name it in `bundles`, not `workdir_bundles`",
            m.target()
        ));
    }
    let given = fleet.entry(flock).map(|f| &f.bundle_vars);
    let mut vars = BTreeMap::new();
    for name in m.var_names() {
        match given.and_then(|g| g.get(name)) {
            Some(v) => {
                vars.insert(name.to_string(), v.clone());
            }
            None => {
                return Err(format!(
                    "flock {flock}: bundle {id} needs bundle_vars {name}"
                ));
            }
        }
    }
    vars.insert("flock".into(), flock.to_string());
    set_of(&m, &dir, kind, Some(machine), &vars)
        .map_err(|e| format!("bundle {id} at {}: {e}", newest.hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::tests::write_workdir_example;

    fn vars() -> BTreeMap<String, String> {
        [
            (
                "ticket_url".to_string(),
                "https://tickets.example.com".to_string(),
            ),
            ("flock".to_string(), "ops".to_string()),
        ]
        .into()
    }

    /// One set per kind, with what that kind takes: the claude set has the
    /// agent file (`kinds = ["claude"]`), the agy set does not; the skill's
    /// files, the instructions with the machine's own appended, the vars,
    /// and no profile.
    #[test]
    fn a_workdir_bundle_gives_a_set_per_kind() {
        let tmp = tempfile::tempdir().unwrap();
        write_workdir_example(tmp.path());
        let m = Manifest::load(tmp.path()).unwrap();
        let claude = set_of(&m, tmp.path(), "claude", Some("build-01"), &vars())
            .unwrap()
            .unwrap();
        assert_eq!(claude["id"], "repo-notes");
        assert_eq!(claude["kind"], "claude");
        assert!(claude.get("profile").is_none());
        assert_eq!(claude["files"][0]["path"], ".claude/agents/checker.md");
        assert_eq!(claude["files"][0]["render"], true);
        assert_eq!(claude["skills"][0]["name"], "reviewing-a-pr");
        assert_eq!(claude["skills"][0]["files"]["notes/style.md"], "Short.\n");
        let text = claude["instructions"].as_str().unwrap();
        assert!(text.contains("{ticket_url}"), "filled when placed: {text}");
        assert!(text.ends_with("This one has the GPUs.\n"), "{text}");
        assert_eq!(claude["vars"]["flock"], "ops");
        assert_eq!(
            claude["settings"]["permissions"]["additionalDirectories"][0],
            "{home}/notes"
        );

        let agy = set_of(&m, tmp.path(), "agy", Some("other"), &vars())
            .unwrap()
            .unwrap();
        assert_eq!(agy["files"].as_array().unwrap().len(), 0);
        assert!(agy.get("settings").is_none(), "no agy settings");
        assert!(!agy["instructions"].as_str().unwrap().contains("GPUs"));
    }

    /// A kind the bundle has nothing for gives no set.
    #[test]
    fn a_kind_with_nothing_gives_no_set() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f"), "x").unwrap();
        std::fs::write(
            tmp.path().join(crate::bundle::MANIFEST_FILE),
            "id = \"b\"\nversion = \"1\"\ntarget = \"workdir\"\n[[file]]\nsrc = \"f\"\npath = \".claude/agents/f.md\"\nkinds = [\"claude\"]\n",
        )
        .unwrap();
        let m = Manifest::load(tmp.path()).unwrap();
        assert!(
            set_of(&m, tmp.path(), "agy", None, &vars())
                .unwrap()
                .is_none()
        );
        assert!(
            set_of(&m, tmp.path(), "claude", None, &vars())
                .unwrap()
                .is_some()
        );
    }

    /// `bundle add` checks each `per_machine` file the bundle holds, as that
    /// machine would get it: one that would make the set invalid is refused,
    /// naming the file, even though no machine is named at add.
    #[test]
    fn add_checks_every_per_machine_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_workdir_example(tmp.path());
        let m = Manifest::load(tmp.path()).unwrap();
        check(&m, tmp.path()).unwrap();

        std::fs::write(
            tmp.path().join("instructions/machines/build-02.md"),
            "<!-- pastor:own -->\nnot allowed\n",
        )
        .unwrap();
        let err = check(&m, tmp.path()).unwrap_err();
        assert!(err.contains("how pastor marks its blocks"), "{err}");

        // A machine with no file of its own is not an error.
        std::fs::remove_file(tmp.path().join("instructions/machines/build-02.md")).unwrap();
        check(&m, tmp.path()).unwrap();
    }

    /// A `per_machine` file that cannot be read is an error, at add and
    /// where it is placed, not a file dropped without a word. A machine with
    /// none is still fine.
    #[test]
    fn an_unreadable_per_machine_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        write_workdir_example(tmp.path());
        let m = Manifest::load(tmp.path()).unwrap();
        // Not text.
        std::fs::write(
            tmp.path().join("instructions/machines/build-01.md"),
            [0xff, 0xfe, 0x00],
        )
        .unwrap();
        let err = check(&m, tmp.path()).unwrap_err();
        assert!(err.contains("build-01.md"), "{err}");
        let err = set_of(&m, tmp.path(), "claude", Some("build-01"), &vars()).unwrap_err();
        assert!(err.contains("build-01.md"), "{err}");
        // A machine whose own file is not there gets the shared text.
        assert!(
            set_of(&m, tmp.path(), "claude", Some("other"), &vars())
                .unwrap()
                .is_some()
        );
    }

    /// `{machine}` may stand for a folder name too, and the check finds the
    /// files below it.
    #[test]
    fn add_finds_per_machine_files_in_a_folder_per_machine() {
        let tmp = tempfile::tempdir().unwrap();
        write_workdir_example(tmp.path());
        let manifest = tmp.path().join(crate::bundle::MANIFEST_FILE);
        let text = std::fs::read_to_string(&manifest).unwrap().replace(
            "instructions/machines/{machine}.md",
            "hosts/{machine}/notes.md",
        );
        std::fs::write(&manifest, text).unwrap();
        let m = Manifest::load(tmp.path()).unwrap();
        std::fs::create_dir_all(tmp.path().join("hosts/gpu-1")).unwrap();
        std::fs::write(tmp.path().join("hosts/gpu-1/notes.md"), [0xff, 0xfe]).unwrap();
        assert_eq!(per_machine_names(&m, tmp.path()), ["gpu-1"]);
        let err = check(&m, tmp.path()).unwrap_err();
        assert!(err.contains("hosts/gpu-1/notes.md"), "{err}");
    }

    fn paths(tmp: &Path) -> Paths {
        Paths::new(tmp.join("config"), tmp.join("state"))
    }

    /// From the store: the newest version kept, the flock's vars and
    /// `{flock}`. A flock short of a var, a home bundle and a bundle with
    /// no version kept are each an error naming it.
    #[test]
    fn a_stored_set_comes_from_the_newest_version() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let src = tmp.path().join("src");
        write_workdir_example(&src);
        crate::bundle::store::add(&paths, &crate::bundle::store::Source::Dir(src.clone())).unwrap();
        let fleet = Flock::parse(
            Path::new("flock.toml"),
            "[[flock]]\nname = \"ops\"\ndefault = true\nmachines = { m = 1 }\nbundle_vars = { ticket_url = \"https://t.example.com\" }\n\n\
             [[flock]]\nname = \"bare\"\nmachines = { m = 1 }\n\n\
             [[machine]]\nname = \"m\"\nlocal = true\n",
        )
        .unwrap();
        let set = stored_set(&paths, &fleet, "repo-notes", "claude", "m", "ops")
            .unwrap()
            .unwrap();
        assert_eq!(set["vars"]["ticket_url"], "https://t.example.com");
        assert_eq!(set["vars"]["flock"], "ops");

        let err = stored_set(&paths, &fleet, "repo-notes", "claude", "m", "bare").unwrap_err();
        assert!(err.contains("needs bundle_vars ticket_url"), "{err}");
        let err = stored_set(&paths, &fleet, "nothing", "claude", "m", "ops").unwrap_err();
        assert!(err.contains("no version kept"), "{err}");

        let home = tmp.path().join("home");
        crate::bundle::manifest::tests::write_example(&home);
        crate::bundle::store::add(&paths, &crate::bundle::store::Source::Dir(home)).unwrap();
        let err = stored_set(&paths, &fleet, "team-rules", "claude", "m", "ops").unwrap_err();
        assert!(err.contains("is a home bundle"), "{err}");
    }
}
