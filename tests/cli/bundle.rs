use crate::helpers::*;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A config dir with one local machine in flock `ops`, and nothing else.
struct BundleEnv {
    tmp: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
}

const FLOCK: &str = "[[flock]]\nname = \"ops\"\ndefault = true\nmachines = { here = 1 }\n\
     bundles = [\"team-rules\"]\nbundle_vars = { ticket_url = \"https://tickets.example.com\" }\n\
     [[machine]]\nname = \"here\"\nlocal = true\n";

fn bundle_env(flock: &str) -> BundleEnv {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(tmp.path().join("h")).unwrap();
    std::fs::write(
        config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n",
    )
    .unwrap();
    std::fs::write(config.join("flock.toml"), flock).unwrap();
    BundleEnv { tmp, config, state }
}

impl BundleEnv {
    fn command(&self) -> Command {
        let mut c = pastor();
        c.env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .env("PASTOR_DATA_DIR", self.tmp.path().join("d"))
            .env("HOME", self.tmp.path().join("h"))
            .env("XDG_CONFIG_HOME", self.tmp.path().join("x"));
        c
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.command().args(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> std::process::Output {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        serde_json::from_slice(&self.ok(args).stdout).unwrap()
    }

    /// A `team-rules` bundle with a skill and a var, outside the config
    /// dir.
    fn write_bundle(&self, skill: &str) -> PathBuf {
        let dir = self.tmp.path().join("src/team-rules");
        let put = |rel: &str, text: &str| {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        put(
            "pastor-bundle.toml",
            "id = \"team-rules\"\nversion = \"2026.10.27.1\"\ndescription = \"the team's skills\"\n\
             [vars]\nticket_url = { description = \"the ticket tracker\" }\n\
             [[skill]]\nsrc = \"skills/reviewing-a-pr\"\n",
        );
        put("skills/reviewing-a-pr/SKILL.md", skill);
        dir
    }
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// Add from a directory, again unchanged, list, show, and remove once no
/// flock names it; all work with no head, and `--json` says the same.
#[test]
fn bundle_add_list_show_and_remove() {
    let env = bundle_env(FLOCK);
    let src = env.write_bundle("Read the diff first.\n");

    let added = env.json(&["bundle", "add", path(&src), "--json"]);
    assert_eq!(added["id"], "team-rules");
    assert_eq!(added["version"], "2026.10.27.1");
    let hash = added["hash"].as_str().unwrap().to_string();
    assert_eq!(hash.len(), 64);
    assert_eq!(added["previous_hash"], serde_json::Value::Null);
    assert_eq!(added["flocks"], serde_json::json!(["ops"]));
    assert_eq!(added["machines_affected"], serde_json::json!(["here"]));
    assert_eq!(added["warnings"], serde_json::json!([]));
    assert!(
        env.config
            .join("bundles/team-rules/skills/reviewing-a-pr/SKILL.md")
            .is_file()
    );

    let out = env.ok(&["bundle", "add", path(&src)]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("already the newest version"),
        "{out:?}"
    );

    std::fs::write(
        src.join("skills/reviewing-a-pr/SKILL.md"),
        "Read the tests first.\n",
    )
    .unwrap();
    let out = env.ok(&["bundle", "add", path(&src)]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.starts_with("added team-rules 2026.10.27.1 (")
            && said.contains(&format!("replacing {}", &hash[..12])),
        "{said}"
    );

    let list = env.json(&["bundle", "list", "--json"]);
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["id"], "team-rules");
    assert_eq!(list[0]["versions"], 2);
    assert_eq!(list[0]["flocks"], serde_json::json!(["ops"]));
    assert_eq!(list[0]["target"], "home");
    let out = env.ok(&["bundle", "list"]);
    let table = String::from_utf8_lossy(&out.stdout);
    assert!(table.starts_with("ID "), "{table}");
    assert!(
        table.contains("team-rules  2026.10.27.1  home") && table.contains("the team's skills"),
        "{table}"
    );

    let show = env.json(&["bundle", "show", "team-rules", "--json"]);
    assert_eq!(
        show["manifest"]["vars"]["ticket_url"]["description"],
        "the ticket tracker"
    );
    assert_eq!(show["versions"].as_array().unwrap().len(), 2);
    assert_eq!(show["versions"][1]["hash"], hash.as_str());
    assert_eq!(show["versions"][0]["origin"]["type"], "dir");
    assert_eq!(show["changed_since_add"], false);
    let out = env.ok(&["bundle", "show", "team-rules"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("  skills: skills/reviewing-a-pr\n"), "{text}");
    assert!(text.contains("  named by: flock ops\n"), "{text}");

    let out = env.run(&["bundle", "remove", "team-rules"]);
    assert_eq!(error_code(&out), "bundle_in_use");
    assert!(String::from_utf8_lossy(&out.stderr).contains("flock ops"));

    std::fs::write(
        env.config.join("flock.toml"),
        "[[machine]]\nname = \"here\"\nlocal = true\n",
    )
    .unwrap();
    let removed = env.json(&["bundle", "remove", "team-rules", "--json"]);
    assert_eq!(removed["versions_removed"], 2);
    assert_eq!(
        env.json(&["bundle", "list", "--json"]),
        serde_json::json!([])
    );

    assert_eq!(
        error_code(&env.run(&["bundle", "show", "team-rules"])),
        "unknown_bundle"
    );
    assert_eq!(
        error_code(&env.run(&["bundle", "remove", "team-rules"])),
        "unknown_bundle"
    );
}

/// A bundle holding something token-shaped is refused, with the file and
/// line and not the value; nothing is kept.
#[test]
fn bundle_add_refuses_a_secret() {
    let env = bundle_env(FLOCK);
    let token = format!("ghp_{}", "a1B2".repeat(9));
    let src = env.write_bundle(&format!("Use this:\nGH_TOKEN={token}\n"));
    let out = env.run(&["bundle", "add", path(&src)]);
    assert_eq!(error_code(&out), "bundle_secret");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("skills/reviewing-a-pr/SKILL.md:2: a GitHub token"),
        "{err}"
    );
    assert!(!err.contains(&token), "{err}");
    assert!(!env.config.join("bundles/team-rules").exists());
}

/// `--git` with `--ref` fetches that ref; `--ref` alone, or neither a
/// directory nor `--git`, is a usage error.
#[test]
fn bundle_add_from_a_git_ref() {
    let env = bundle_env(FLOCK);
    let src = env.write_bundle("At the tag.\n");
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&src)
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
    };
    git(&["init", "--quiet"]);
    git(&["add", "."]);
    let who = [
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.org",
        "-c",
        "commit.gpgsign=false",
    ];
    git(&[&who[..], &["commit", "--quiet", "-m", "one"]].concat());
    git(&["tag", "v1"]);
    let url = format!("file://{}", src.display());
    let added = env.json(&["bundle", "add", "--git", &url, "--ref", "v1", "--json"]);
    assert_eq!(added["id"], "team-rules");
    let show = env.json(&["bundle", "show", "team-rules", "--json"]);
    let origin = &show["versions"][0]["origin"];
    assert_eq!(
        (origin["type"].as_str(), origin["ref"].as_str()),
        (Some("git"), Some("v1"))
    );
    assert_eq!(origin["commit"].as_str().unwrap().len(), 40);

    assert_eq!(
        env.run(&["bundle", "add", "--ref", "v1"]).status.code(),
        Some(2)
    );
    assert_eq!(env.run(&["bundle", "add"]).status.code(), Some(2));
    assert_eq!(
        env.run(&["bundle", "add", "--git", &url]).status.code(),
        Some(2)
    );
}

/// A flock.toml that names a bundle the bundles dir lacks, or leaves a var
/// of one unset, does not load for a head.
#[test]
fn flock_toml_naming_a_missing_bundle_does_not_load() {
    let env = bundle_env(FLOCK);
    let out = env.run(&["tick"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("flock ops: bundle \\\"team-rules\\\" is not in the bundles dir"),
        "{err}"
    );

    let src = env.write_bundle("x\n");
    env.ok(&["bundle", "add", path(&src)]);
    std::fs::write(
        env.config.join("flock.toml"),
        FLOCK.replace(
            "bundle_vars = { ticket_url = \"https://tickets.example.com\" }\n",
            "",
        ),
    )
    .unwrap();
    let out = env.run(&["tick"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("bundle team-rules needs bundle_vars ticket_url"),
        "{err}"
    );
}

/// Adding and removing change what a flock's machines get, so an agent
/// pastor started may not; reading is fine.
#[test]
fn an_agent_may_read_bundles_but_not_change_them() {
    let env = bundle_env(FLOCK);
    let src = env.write_bundle("x\n");
    let as_agent = |args: &[&str]| {
        env.command()
            .args(args)
            .env("PASTOR_TASK", "t-1")
            .output()
            .unwrap()
    };
    assert_eq!(
        error_code(&as_agent(&["bundle", "add", path(&src)])),
        "agent_refused"
    );
    env.ok(&["bundle", "add", path(&src)]);
    assert!(as_agent(&["bundle", "list"]).status.success());
    assert!(as_agent(&["bundle", "show", "team-rules"]).status.success());
    assert_eq!(
        error_code(&as_agent(&["bundle", "remove", "team-rules"])),
        "agent_refused"
    );
}

/// A new version that declares a var the flocks naming it do not give is
/// refused before anything is copied, naming the flock and the var; once
/// flock.toml gives it, the same version goes in.
#[test]
fn bundle_add_refuses_a_version_flock_toml_does_not_fit() {
    let env = bundle_env(FLOCK);
    let src = env.write_bundle("v1\n");
    let first = env.json(&["bundle", "add", path(&src), "--json"]);
    let manifest = src.join("pastor-bundle.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(
        &manifest,
        text.replace(
            "[vars]\n",
            "[vars]\nregion = { description = \"where it runs\" }\n",
        ),
    )
    .unwrap();
    let out = env.run(&["bundle", "add", path(&src)]);
    assert_eq!(error_code(&out), "bundle_invalid");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("does not fit flock.toml")
            && err.contains("flock ops: bundle team-rules needs bundle_vars region"),
        "{err}"
    );
    let show = env.json(&["bundle", "show", "team-rules", "--json"]);
    assert_eq!(show["versions"].as_array().unwrap().len(), 1, "{show}");
    assert_eq!(show["versions"][0]["hash"], first["hash"]);
    // The flock gives it first, which flock.toml allows; then the add goes in.
    std::fs::write(
        env.config.join("flock.toml"),
        FLOCK.replace("ticket_url = ", "region = \"eu\", ticket_url = "),
    )
    .unwrap();
    env.ok(&["tick", "--dry-run"]);
    env.ok(&["bundle", "add", path(&src)]);
    env.ok(&["tick", "--dry-run"]);
}

/// A version whose target is not the one flock.toml names the bundle as is
/// refused: a home bundle named in `bundles` may not become a workdir one.
/// A workdir bundle is listed with its target.
#[test]
fn bundle_add_refuses_a_version_that_changes_its_target_under_a_flock() {
    let env = bundle_env(FLOCK);
    let src = env.write_bundle("v1\n");
    env.ok(&["bundle", "add", path(&src)]);
    let manifest = src.join("pastor-bundle.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(
        &manifest,
        text.replace("[vars]\n", "target = \"workdir\"\n[vars]\n"),
    )
    .unwrap();
    let out = env.run(&["bundle", "add", path(&src)]);
    assert_eq!(error_code(&out), "bundle_invalid");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("flock ops: bundle team-rules goes into tasks' working directories"),
        "{err}"
    );
    // Named as a workdir bundle, it goes in.
    std::fs::write(
        env.config.join("flock.toml"),
        FLOCK.replace("bundles = [", "workdir_bundles = ["),
    )
    .unwrap();
    env.ok(&["bundle", "add", path(&src)]);
    let list = env.json(&["bundle", "list", "--json"]);
    assert_eq!(list[0]["target"], "workdir", "{list}");
    assert_eq!(list[0]["flocks"], serde_json::json!(["ops"]));
    env.ok(&["tick", "--dry-run"]);
}

/// `bundle remove` refuses a bundle still named anywhere, saying by whom
/// and under which key: a flock's `workdir_bundles` and a job's `[dispatch]
/// workdir_bundles` as much as a flock's `bundles`. Without that, every
/// queued task of the job would fail its start on a bundle no longer kept.
#[test]
fn bundle_remove_names_the_flocks_and_jobs_that_still_name_it() {
    let env = bundle_env(&FLOCK.replace("bundles = [", "workdir_bundles = ["));
    let src = env.write_bundle("v1\n");
    let manifest = src.join("pastor-bundle.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(
        &manifest,
        text.replace("[vars]\n", "target = \"workdir\"\n[vars]\n"),
    )
    .unwrap();
    env.ok(&["bundle", "add", path(&src)]);
    let jobs = env.config.join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("nightly.toml"),
        "[dispatch]\nprompt = \"p\"\nworkdir_bundles = [\"team-rules\"]\n",
    )
    .unwrap();
    std::fs::write(jobs.join("other.toml"), "[dispatch]\nprompt = \"p\"\n").unwrap();

    let out = env.run(&["bundle", "remove", "team-rules"]);
    assert_eq!(error_code(&out), "bundle_in_use");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("flock ops (workdir_bundles)"), "{err}");
    assert!(
        err.contains("job nightly ([dispatch] workdir_bundles)"),
        "{err}"
    );
    assert!(!err.contains("job other"), "{err}");

    // Named by the job alone, it is still refused.
    std::fs::write(
        env.config.join("flock.toml"),
        "[[machine]]\nname = \"here\"\nlocal = true\n",
    )
    .unwrap();
    let out = env.run(&["bundle", "remove", "team-rules"]);
    assert_eq!(error_code(&out), "bundle_in_use");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("bundle team-rules is named by job nightly"),
        "{err}"
    );

    std::fs::remove_file(jobs.join("nightly.toml")).unwrap();
    env.ok(&["bundle", "remove", "team-rules"]);
}

/// A flock already short of a var the bundle needs does not refuse a new
/// version, which did not cause it, but the add says so, text and JSON.
#[test]
fn bundle_add_warns_of_a_gap_flock_toml_already_has() {
    let env = bundle_env(FLOCK);
    env.ok(&["bundle", "add", path(&env.write_bundle("v1\n"))]);
    let short = FLOCK.replace(
        "\nbundle_vars = { ticket_url = \"https://tickets.example.com\" }",
        "",
    );
    assert_ne!(short, FLOCK);
    std::fs::write(env.config.join("flock.toml"), &short).unwrap();
    let gap = "flock ops: bundle team-rules needs bundle_vars ticket_url";

    let out = env.ok(&["bundle", "add", path(&env.write_bundle("v2\n"))]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("warning: flock.toml already lacks what its bundles need")
            && err.contains(gap),
        "{err}"
    );
    let added = env.json(&["bundle", "add", path(&env.write_bundle("v3\n")), "--json"]);
    let warnings = added["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap().contains(gap)),
        "{added}"
    );

    // With the var set again, no warning.
    std::fs::write(env.config.join("flock.toml"), FLOCK).unwrap();
    let out = env.ok(&["bundle", "add", path(&env.write_bundle("v4\n"))]);
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("already lacks"),
        "{out:?}"
    );
}

/// `bundle apply` reaches a local machine as it would an ssh one: the
/// rendered set unpacked in a run dir, and `pastor bundle apply --local`
/// run there. A dry run writes nothing; the second apply changes nothing;
/// an agent may only dry-run.
#[test]
fn bundle_apply_places_a_bundle_in_the_agents_home() {
    let env = bundle_env(&FLOCK.replace(
        "default = true\n",
        "default = true\nagent = \"claude-main\"\n",
    ));
    std::fs::write(
        env.config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n\
         [agents.claude-main]\nkind = \"claude\"\nenv = { CLAUDE_CONFIG_DIR = \"~/.claude-main\" }\n",
    )
    .unwrap();
    let src = env.write_bundle("Read the diff first.\n");
    let manifest = src.join("pastor-bundle.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(
        &manifest,
        format!("{text}[instructions]\nsrc = \"team.md\"\n"),
    )
    .unwrap();
    std::fs::write(src.join("team.md"), "File tickets at {ticket_url}.\n").unwrap();
    env.ok(&["bundle", "add", path(&src)]);
    let home = env.tmp.path().join("h");
    std::fs::create_dir_all(home.join(".claude-main")).unwrap();
    std::fs::write(home.join(".claude-main/CLAUDE.md"), "My own rules.\n").unwrap();

    let dry = env.json(&["bundle", "apply", "here", "--dry-run", "--json"]);
    assert_eq!(dry[0]["machine"], "here");
    assert_eq!(dry[0]["ok"], true, "{dry}");
    let added = dry[0]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["action"] == "added")
        .count();
    assert_eq!(added, 3, "the version, the skill and the block: {dry}");
    assert!(!home.join(".claude-main/skills").exists());
    // Nor anything of the run it was sent in.
    assert!(!home.join(".cache/pastor").exists());
    assert!(!env.state.join("applied").exists());
    // A dry run is not an apply: the head records nothing.
    assert!(!env.state.join("bundles/applied.json").exists());

    let out = env.ok(&["bundle", "apply", "here"]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.starts_with("here: 3 added\n"), "{said}");
    assert!(
        said.contains("added           ~/.claude-main/skills/reviewing-a-pr  team-rules"),
        "{said}"
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".claude-main/skills/reviewing-a-pr/SKILL.md")).unwrap(),
        "Read the diff first.\n"
    );
    let md = std::fs::read_to_string(home.join(".claude-main/CLAUDE.md")).unwrap();
    assert!(
        md.starts_with("My own rules.\n\n<!-- pastor:bundle:team-rules -->\n"),
        "{md}"
    );
    assert!(
        md.contains("File tickets at https://tickets.example.com."),
        "{md}"
    );
    assert!(env.state.join("applied/ledger.json").is_file());
    // The head recorded what it applied, for `machine describe` and the
    // launch gate.
    let recorded: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(env.state.join("bundles/applied.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(recorded["machines"]["here"]["ok"], true, "{recorded}");
    assert!(
        recorded["machines"]["here"]["bundles"]["team-rules"].is_string(),
        "{recorded}"
    );
    assert_eq!(
        std::fs::read_dir(env.state.join("applied/backup"))
            .unwrap()
            .count(),
        1
    );
    // The run dir on the machine is gone again, and its empty parents.
    assert!(!home.join(".cache/pastor").exists());

    let out = env.ok(&["bundle", "apply"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "here: 2 kept, nothing to change\n"
    );

    // `machine describe` says what the machine is meant to have, how it
    // takes a change and when it last did: not stale after the apply.
    let described = env.json(&["machine", "describe", "here", "--json"]);
    let config = &described["config"];
    assert_eq!(config["apply_mode"], "manual", "{described}");
    assert_eq!(config["stale"], false, "{config}");
    assert_eq!(config["bundles"][0]["id"], "team-rules", "{config}");
    assert_eq!(config["bundles"][0]["version"], "2026.10.27.1", "{config}");
    assert_eq!(config["last_apply"]["ok"], true, "{config}");
    assert_eq!(config["homes"][0]["path"], "~/.claude-main", "{config}");
    // With no head nothing has read the home.
    assert!(config["homes"][0]["state"].is_null(), "{config}");
    // A new version of the bundle makes it stale, and the apply mode holds.
    let src = env.write_bundle("Read the diff twice.\n");
    let manifest = src.join("pastor-bundle.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(
        &manifest,
        format!("{text}[instructions]\nsrc = \"team.md\"\n"),
    )
    .unwrap();
    env.ok(&["bundle", "add", path(&src)]);
    let described = env.json(&["machine", "describe", "here", "--json"]);
    assert_eq!(described["config"]["stale"], true, "{described}");
    assert_ne!(
        described["config"]["bundles"][0]["applied_hash"],
        serde_json::Value::Null
    );
    env.ok(&["bundle", "apply"]);
    let described = env.json(&["machine", "describe", "here", "--json"]);
    assert_eq!(described["config"]["stale"], false, "{described}");

    let as_agent = |args: &[&str]| {
        env.command()
            .args(args)
            .env("PASTOR_TASK", "t-1")
            .output()
            .unwrap()
    };
    assert_eq!(
        error_code(&as_agent(&["bundle", "apply", "here"])),
        "agent_refused"
    );
    assert!(
        as_agent(&["bundle", "apply", "here", "--dry-run"])
            .status
            .success()
    );
    assert_eq!(
        error_code(&env.run(&["bundle", "apply", "nowhere"])),
        "unknown_machine"
    );
}

/// A machine whose skills dir is a symlink is not written through; the
/// apply exits 1 with a row that says so.
#[test]
fn bundle_apply_refuses_a_symlinked_home_dir() {
    let env = bundle_env(FLOCK);
    let src = env.write_bundle("x\n");
    env.ok(&["bundle", "add", path(&src)]);
    let home = env.tmp.path().join("h");
    let dotfiles = env.tmp.path().join("dotfiles/skills");
    std::fs::create_dir_all(&dotfiles).unwrap();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::os::unix::fs::symlink(&dotfiles, home.join(".claude/skills")).unwrap();
    let out = env.run(&["bundle", "apply", "here"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains(
            "linked          ~/.claude/skills  team-rules: ~/.claude/skills is a symlink to"
        ),
        "{said}"
    );
    assert_eq!(std::fs::read_dir(&dotfiles).unwrap().count(), 0);
}

/// `bundle adopt` takes over a home `bundle apply` would not write
/// through: the dry run lists what it would replace and changes nothing,
/// the adopt backs up first, makes the linked skills dir a plain copy and
/// puts the bundle's skill in place of the user's, and is recorded as an
/// apply; a plain apply after has nothing to do. `--agent` takes one home,
/// and only an agent of the machine; an agent pastor started may only
/// dry-run it.
#[test]
fn bundle_adopt_takes_over_a_home() {
    let env = bundle_env(FLOCK);
    let src = env.write_bundle("Read the diff first.\n");
    env.ok(&["bundle", "add", path(&src)]);
    let home = env.tmp.path().join("h");
    let dotfiles = env.tmp.path().join("dotfiles/skills");
    std::fs::create_dir_all(dotfiles.join("reviewing-a-pr")).unwrap();
    std::fs::write(dotfiles.join("reviewing-a-pr/SKILL.md"), "Mine.\n").unwrap();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::os::unix::fs::symlink(&dotfiles, home.join(".claude/skills")).unwrap();

    let dry = env.json(&["bundle", "adopt", "here", "--dry-run", "--json"]);
    assert_eq!(dry["ok"], true, "{dry}");
    let replaced: Vec<(&str, &str)> = dry["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["action"] == "replaced")
        .map(|r| (r["path"].as_str().unwrap(), r["old"].as_str().unwrap()))
        .collect();
    assert_eq!(replaced.len(), 2, "{dry}");
    assert_eq!(replaced[0].0, "~/.claude/skills");
    assert!(replaced[0].1.starts_with("a symlink to "), "{dry}");
    assert_eq!(
        replaced[1],
        ("~/.claude/skills/reviewing-a-pr", "a directory")
    );
    assert!(
        home.join(".claude/skills")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!env.state.join("applied").exists());
    assert!(!env.state.join("bundles/applied.json").exists());

    let out = env.ok(&["bundle", "adopt", "here"]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.starts_with("here: 1 added, 2 replaced\n  backup: "),
        "{said}"
    );
    assert!(
        said.contains(
            "replaced        ~/.claude/skills/reviewing-a-pr  team-rules: was a directory; backup "
        ),
        "{said}"
    );
    let skills = home.join(".claude/skills");
    assert!(skills.symlink_metadata().unwrap().is_dir());
    assert_eq!(
        std::fs::read_to_string(skills.join("reviewing-a-pr/SKILL.md")).unwrap(),
        "Read the diff first.\n"
    );
    assert_eq!(
        std::fs::read_to_string(dotfiles.join("reviewing-a-pr/SKILL.md")).unwrap(),
        "Mine.\n",
        "the link's target is never written"
    );
    let backup = std::fs::read_dir(env.state.join("applied/backup"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("adopt-")
        })
        .expect("an adopt backup");
    assert_eq!(
        std::fs::read_to_string(backup.join(".claude/skills/reviewing-a-pr/SKILL.md")).unwrap(),
        "Mine.\n"
    );
    let recorded: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(env.state.join("bundles/applied.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(recorded["machines"]["here"]["ok"], true, "{recorded}");
    assert_eq!(
        String::from_utf8_lossy(&env.ok(&["bundle", "apply", "here"]).stdout),
        "here: 1 kept, nothing to change\n"
    );

    assert_eq!(
        error_code(&env.run(&["bundle", "adopt", "here", "--agent", "codex"])),
        "unknown_agent"
    );
    let one = env.json(&["bundle", "adopt", "here", "--agent", "claude", "--json"]);
    assert_eq!(one["ok"], true, "{one}");
    assert_eq!(
        error_code(&env.run(&["bundle", "adopt", "nowhere"])),
        "unknown_machine"
    );
    let as_agent = |args: &[&str]| {
        env.command()
            .args(args)
            .env("PASTOR_TASK", "t-1")
            .output()
            .unwrap()
    };
    assert_eq!(
        error_code(&as_agent(&["bundle", "adopt", "here"])),
        "agent_refused"
    );
    assert!(
        as_agent(&["bundle", "adopt", "here", "--dry-run"])
            .status
            .success()
    );
}

/// A fleet-wide apply leaves out a machine no bundle is meant for: it is
/// reported as skipped and nothing is recorded for it, so it can never be
/// held to an apply it did not need.
#[test]
fn a_fleet_wide_apply_leaves_out_a_machine_with_no_bundles() {
    let env = bundle_env(&format!(
        "{FLOCK}[[flock]]\nname = \"spare\"\nmachines = {{ idle = 1 }}\n[[machine]]\nname = \"idle\"\nlocal = true\n"
    ));
    let src = env.write_bundle("x\n");
    env.ok(&["bundle", "add", path(&src)]);
    let out = env.json(&["bundle", "apply", "--json"]);
    let idle = out
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["machine"] == "idle")
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(idle["ok"], true, "{out}");
    assert_eq!(idle["skipped"], "no bundle is meant for idle", "{out}");
    let recorded: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(env.state.join("bundles/applied.json")).unwrap(),
    )
    .unwrap();
    assert!(recorded["machines"]["here"].is_object(), "{recorded}");
    assert!(recorded["machines"]["idle"].is_null(), "{recorded}");
}
