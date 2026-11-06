//! What the head last applied to each machine, in `<state>/bundles/applied.json`:
//! the fingerprint of the plan, when, and whether it worked. `pastor bundle
//! apply` and the keeper (`keep`) both write it, and the keeper, `machine
//! describe` and the launch gate read it, so an apply a person ran by hand and
//! one the head ran on its own are the same to everything that asks.
//!
//! A plan is stale when its fingerprint is not the one last applied. The
//! fingerprint covers everything the head renders for the machine: which
//! bundle versions, the vars filled in, the homes and the items, so a new
//! bundle version, a `bundles` or `bundle_vars` edit, a machine joining a
//! flock and an agent's home moving all change it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::plan::Plan;
use crate::config::{Paths, create_private_dir};

/// The file, under the bundles state dir.
pub fn file(paths: &Paths) -> PathBuf {
    paths.bundle_versions_root().join("applied.json")
}

/// The sha256 of the plan as the head renders it, but for the secret files
/// it names: those change nothing it places.
pub fn fingerprint(plan: &Plan) -> String {
    let plan = Plan {
        secrets: Vec::new(),
        ..plan.clone()
    };
    let json = serde_json::to_vec(&plan).unwrap_or_default();
    Sha256::digest(json)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// One machine's last apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    pub at: DateTime<Utc>,
    /// The fingerprint of the plan the apply was for.
    pub fingerprint: String,
    /// The bundles and versions on the machine after it: id to content
    /// hash. After a failed apply, also those it did not get to take off.
    #[serde(default)]
    pub bundles: BTreeMap<String, String>,
    /// Whether it went through: no failed row, and the machine answered.
    pub ok: bool,
    /// Why it did not, when it did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The machines' last applies, by name. A machine with no entry was never
/// applied to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub machines: BTreeMap<String, Applied>,
}

impl Status {
    /// The file as it is; none (or one that does not read) is nothing
    /// applied anywhere yet.
    pub fn load(paths: &Paths) -> Status {
        std::fs::read_to_string(file(paths))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Whether `plan` is not what `machine` last took: never applied, or
    /// applied for another plan. A failed apply took nothing, whatever it
    /// was for.
    pub fn stale(&self, machine: &str, plan: &Plan) -> bool {
        if plan.items.is_empty() && plan.versions.is_empty() {
            // Nothing wanted: stale only while something is still placed,
            // which an apply that removes it will have recorded. A failed
            // apply that could not remove it leaves it recorded.
            return self
                .machines
                .get(machine)
                .is_some_and(|a| !a.bundles.is_empty());
        }
        match self.machines.get(machine) {
            Some(a) => !a.ok || a.fingerprint != fingerprint(plan),
            None => true,
        }
    }
}

/// Record that `machine` was applied `plan` (or failed to be), under a lock
/// so a person's apply and the head's do not lose each other's entry.
/// `answered` is whether pastor on the machine started the apply, reported
/// on it or not (`push::Failed::started`): one that never got that far
/// (pastor is not on its PATH, the ssh failed) placed nothing, so the plan's
/// bundles are not recorded as on it; one cut short after it started (a
/// timeout, an answer that is no report) may have written, so they are.
pub fn record(
    paths: &Paths,
    machine: &str,
    plan: &Plan,
    ok: bool,
    answered: bool,
    error: Option<String>,
) -> anyhow::Result<Applied> {
    use std::os::unix::io::AsRawFd;
    let root = paths.bundle_versions_root();
    create_private_dir(&root)?;
    let lock = crate::edit::open_lock(&root.join("applied.lock"))?;
    // SAFETY: flock on a descriptor this function owns; blocking is fine,
    // the holder only reads and writes one small file.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    let mut status = Status::load(paths);
    // What is placed on the machine afterwards: the plan's bundles when it
    // went through. A failed apply that answered may have put some of them
    // and took none of the old ones off, so the old stay in the record; a
    // machine whose bundles were taken off and whose removal failed is still
    // holding them. One that never answered placed nothing, and only the old
    // are on it.
    let mut bundles: BTreeMap<String, String> = if ok || answered {
        plan.versions
            .iter()
            .map(|v| (v.bundle.clone(), v.hash.clone()))
            .collect()
    } else {
        BTreeMap::new()
    };
    if !ok && let Some(before) = status.machines.get(machine) {
        for (id, hash) in &before.bundles {
            bundles.entry(id.clone()).or_insert_with(|| hash.clone());
        }
    }
    let applied = Applied {
        at: Utc::now(),
        fingerprint: fingerprint(plan),
        bundles,
        ok,
        error,
    };
    status.machines.insert(machine.to_string(), applied.clone());
    let path = file(paths);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&status)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::plan::{Item, Version, What};

    fn plan(hash: &str) -> Plan {
        Plan {
            machine: "m".into(),
            versions: vec![Version {
                bundle: "b".into(),
                hash: hash.into(),
                modes: Default::default(),
            }],
            items: vec![Item {
                bundle: "b".into(),
                home: Some("~/.claude".into()),
                path: "~/.claude/skills/x".into(),
                what: What::Link {
                    target: format!("b/{hash}/x"),
                },
            }],
            skipped: vec![],
            secrets: vec!["~/.claude/.credentials.json".into()],
        }
    }

    /// The secret files a plan names change nothing it places, so they
    /// leave its fingerprint alone.
    #[test]
    fn the_secret_files_do_not_change_the_fingerprint() {
        let none = Plan {
            secrets: vec![],
            ..plan("h1")
        };
        assert_eq!(fingerprint(&none), fingerprint(&plan("h1")));
        assert_ne!(fingerprint(&plan("h2")), fingerprint(&plan("h1")));
    }

    #[test]
    fn a_plan_is_stale_until_its_fingerprint_is_applied() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let status = Status::load(&paths);
        assert!(status.stale("m", &plan("one")), "never applied");
        let applied = record(&paths, "m", &plan("one"), true, true, None).unwrap();
        assert_eq!(applied.bundles["b"], "one");
        let status = Status::load(&paths);
        assert!(!status.stale("m", &plan("one")));
        assert!(status.stale("m", &plan("two")), "a new version");
        assert!(status.stale("other", &plan("one")), "another machine");
        // A failed apply took nothing, even for the same plan.
        record(&paths, "m", &plan("one"), false, true, Some("boom".into())).unwrap();
        let status = Status::load(&paths);
        assert!(status.stale("m", &plan("one")));
        assert_eq!(status.machines["m"].error.as_deref(), Some("boom"));
        // Other machines' entries are kept.
        record(&paths, "n", &plan("one"), true, true, None).unwrap();
        assert_eq!(Status::load(&paths).machines.len(), 2);
    }

    #[test]
    fn an_empty_plan_is_stale_only_while_something_is_placed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let empty = Plan {
            machine: "m".into(),
            ..Default::default()
        };
        assert!(!Status::load(&paths).stale("m", &empty));
        record(&paths, "m", &plan("one"), true, true, None).unwrap();
        assert!(Status::load(&paths).stale("m", &empty), "to be removed");
        // A removal that failed leaves them placed, and the machine stale.
        record(&paths, "m", &empty, false, true, Some("down".into())).unwrap();
        assert!(Status::load(&paths).stale("m", &empty), "still placed");
        record(&paths, "m", &empty, true, true, None).unwrap();
        assert!(!Status::load(&paths).stale("m", &empty));
        // A machine nothing was placed on is not held by an apply that failed.
        record(&paths, "n", &empty, false, true, Some("down".into())).unwrap();
        assert!(!Status::load(&paths).stale("n", &empty));
    }

    /// An apply the machine never answered placed nothing: the plan's
    /// bundles are not recorded as on it, so taking them off the machine in
    /// the flock leaves nothing to remove. What an earlier apply placed stays
    /// recorded through it; one that answered with a failure records the
    /// plan's bundles too, since it may have put some.
    #[test]
    fn an_apply_the_machine_never_answered_placed_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let empty = Plan {
            machine: "m".into(),
            ..Default::default()
        };
        let a = record(
            &paths,
            "m",
            &plan("one"),
            false,
            false,
            Some("pastor: not found".into()),
        )
        .unwrap();
        assert!(a.bundles.is_empty(), "{a:?}");
        assert!(
            !Status::load(&paths).stale("m", &empty),
            "nothing to take off"
        );

        let a = record(
            &paths,
            "m",
            &plan("one"),
            false,
            true,
            Some("a row failed".into()),
        )
        .unwrap();
        assert_eq!(a.bundles["b"], "one", "it may have put some");

        // Placed earlier, then an apply that did not answer: still placed.
        record(&paths, "n", &plan("one"), true, true, None).unwrap();
        let a = record(&paths, "n", &plan("two"), false, false, Some("down".into())).unwrap();
        assert_eq!(a.bundles["b"], "one", "{a:?}");
    }
}
