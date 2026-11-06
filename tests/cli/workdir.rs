use crate::helpers::*;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const SET: &str = r##"{
    "id": "reviewer",
    "kind": "claude",
    "skills": [{"name": "reviewing-a-pr", "files": {"SKILL.md": "---\nname: reviewing-a-pr\n---\nRead the diff.\n"}}],
    "files": [{"path": ".claude/hooks/start.sh", "content": "#!/bin/sh\necho {workdir}\n", "mode": "0755", "render": true}]
}"##;

struct WorkdirEnv {
    tmp: tempfile::TempDir,
    dir: PathBuf,
}

fn workdir_env() -> WorkdirEnv {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap().join("h/src/app");
    std::fs::create_dir_all(&dir).unwrap();
    WorkdirEnv { tmp, dir }
}

impl WorkdirEnv {
    fn command(&self) -> Command {
        let mut c = pastor();
        c.env("PASTOR_CONFIG_DIR", self.tmp.path().join("c"))
            .env("PASTOR_STATE_DIR", self.tmp.path().join("s"))
            .env("PASTOR_DATA_DIR", self.tmp.path().join("d"))
            .env("HOME", self.tmp.path().join("h"))
            .env("XDG_CONFIG_HOME", self.tmp.path().join("x"))
            .env_remove("PASTOR_TASK");
        c
    }

    /// `pastor <args>` with `SET` on stdin.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
        self.run_set(args, env, SET)
    }

    /// `pastor <args>` with `set` on stdin.
    fn run_set(&self, args: &[&str], env: &[(&str, &str)], set: &str) -> std::process::Output {
        let mut c = self.command();
        c.args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        // A refused command may exit before it reads stdin.
        let _ = child.stdin.take().unwrap().write_all(set.as_bytes());
        child.wait_with_output().unwrap()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        serde_json::from_str(&ok(self.run(args, &[]))).unwrap()
    }

    fn dir(&self) -> &str {
        self.dir.to_str().unwrap()
    }
}

fn actions(v: &serde_json::Value) -> Vec<(String, String)> {
    v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["path"].as_str().unwrap().to_string(),
                r["action"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// `workdir place --local` writes a set from stdin, a second run keeps it,
/// and `workdir remove --local` takes it out again.
#[test]
fn place_and_remove_a_set_in_a_working_directory() {
    let env = workdir_env();
    let dir = env.dir();
    let dry = env.json(&[
        "workdir",
        "place",
        "--local",
        dir,
        "--set",
        "-",
        "--dry-run",
        "--json",
    ]);
    assert_eq!(dry["dry_run"], true);
    assert!(!env.dir.join(".claude").exists());
    let placed = env.json(&["workdir", "place", "--local", dir, "--set", "-", "--json"]);
    assert_eq!(placed["target"], "workdir");
    assert_eq!(placed["dir"], dir);
    assert_eq!(placed["id"], "reviewer");
    assert_eq!(placed["ok"], true);
    assert_eq!(
        actions(&placed),
        vec![
            (
                ".claude/skills/reviewing-a-pr/SKILL.md".to_string(),
                "added".to_string()
            ),
            (".claude/hooks/start.sh".to_string(), "added".to_string()),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(env.dir.join(".claude/hooks/start.sh")).unwrap(),
        format!("#!/bin/sh\necho {dir}\n")
    );
    let text = ok(env.run(&["workdir", "place", "--local", dir, "--set", "-"], &[]));
    assert_eq!(text, format!("{dir} reviewer: 2 kept, nothing to change\n"));
    let removed = env.json(&[
        "workdir", "remove", "--local", dir, "--id", "reviewer", "--json",
    ]);
    assert!(
        actions(&removed).iter().all(|(_, a)| a == "removed"),
        "{removed}"
    );
    assert!(!env.dir.join(".claude").exists());
}

/// A problem row exits 1, with the answer still printed.
#[test]
fn a_conflict_exits_one() {
    let env = workdir_env();
    std::fs::create_dir_all(env.dir.join(".claude/hooks")).unwrap();
    std::fs::write(env.dir.join(".claude/hooks/start.sh"), "the repo's\n").unwrap();
    let out = env.run(
        &[
            "workdir",
            "place",
            "--local",
            env.dir(),
            "--set",
            "-",
            "--json",
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert!(
        actions(&v).contains(&(".claude/hooks/start.sh".to_string(), "conflict".to_string())),
        "{v}"
    );
}

/// An agent pastor started may ask what a set would change, never place
/// one; without `--local`, a place names its machine as well as the
/// directory.
#[test]
fn an_agent_may_only_dry_run_and_a_head_place_names_its_machine() {
    let env = workdir_env();
    let dir = env.dir();
    let as_agent = [("PASTOR_TASK", "t-1")];
    let out = env.run(
        &["workdir", "place", "--local", dir, "--set", "-"],
        &as_agent,
    );
    assert_eq!(error_code(&out), "agent_refused");
    let out = env.run(&["workdir", "remove", "--local", dir], &as_agent);
    assert_eq!(error_code(&out), "agent_refused");
    ok(env.run(
        &[
            "workdir",
            "place",
            "--local",
            dir,
            "--set",
            "-",
            "--dry-run",
        ],
        &as_agent,
    ));
    assert!(!env.dir.join(".claude").exists());
    let out = env.run(&["workdir", "place", dir, "--set", "-"], &[]);
    assert_eq!(error_code(&out), "workdir_invalid");
    let out = env.run(
        &["workdir", "place", "--local", "m", dir, "--set", "-"],
        &[],
    );
    assert_eq!(error_code(&out), "workdir_invalid");
}

/// Through a running head, onto a local machine: `place` writes the set
/// with the pastor there, `list` shows the head's record, `check` is ok
/// while nothing would change, and `remove` takes the set and the record
/// out. Each real place and remove is an event.
#[test]
fn place_check_list_and_remove_through_the_head() {
    let env = workdir_env();
    let config = env.tmp.path().join("c");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[machine]]\nname = \"here\"\nlocal = true\n",
    )
    .unwrap();
    std::fs::write(
        config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n",
    )
    .unwrap();
    let log = env.tmp.path().join("serve.log");
    let mut serve = env.command();
    serve
        .args(["serve", "--foreground"])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap());
    let _served = Served {
        child: serve.spawn().unwrap(),
        log,
    };
    let deadline = std::time::Instant::now() + WAIT;
    while !env
        .run(&["workdir", "list", "--json"], &[])
        .status
        .success()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the head never came up"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let dir = env.dir();
    let dry = env.json(&[
        "workdir",
        "place",
        "here",
        dir,
        "--set",
        "-",
        "--dry-run",
        "--json",
    ]);
    assert_eq!(dry["machine"], "here");
    assert_eq!(dry["dry_run"], true);
    assert!(!env.dir.join(".claude").exists());
    assert_eq!(
        env.json(&["workdir", "list", "--json"]),
        serde_json::json!([])
    );

    let placed = env.json(&["workdir", "place", "here", dir, "--set", "-", "--json"]);
    assert_eq!(placed["ok"], true, "{placed}");
    assert_eq!(placed["machine"], "here");
    assert_eq!(
        std::fs::read_to_string(env.dir.join(".claude/hooks/start.sh")).unwrap(),
        format!("#!/bin/sh\necho {dir}\n"),
        "the machine's own vars, filled there"
    );
    let listed = env.json(&["workdir", "list", "here", "--json"]);
    assert_eq!(listed[0]["dir"], dir);
    assert_eq!(listed[0]["id"], "reviewer");
    assert_eq!(listed[0]["result"], "ok");
    let text = ok(env.run(&["workdir", "list"], &[]));
    assert!(text.starts_with("MACHINE"), "{text}");

    let checked = env.json(&["workdir", "check", "here", dir, "--json"]);
    assert_eq!(checked[0]["ok"], true, "{checked}");
    std::fs::remove_file(env.dir.join(".claude/hooks/start.sh")).unwrap();
    let out = env.run(&["workdir", "check", "here", dir, "--json"], &[]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a missing file is not ok: {out:?}"
    );

    let removed = env.json(&["workdir", "remove", "here", dir, "--json"]);
    assert_eq!(removed["ok"], true, "{removed}");
    assert!(!env.dir.join(".claude/skills").exists());
    assert_eq!(
        env.json(&["workdir", "list", "--json"]),
        serde_json::json!([])
    );
    let out = env.run(&["workdir", "check", "here", dir], &[]);
    assert_eq!(error_code(&out), "workdir_not_found");

    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let out = env.run(&["events", "--json"], &[]);
        let kinds: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|e| e["type"].as_str().map(str::to_string))
            .filter(|k| k.starts_with("workdir."))
            .collect();
        if kinds.len() >= 2 {
            assert_eq!(kinds, ["workdir.placed", "workdir.removed"]);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "events so far: {kinds:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// `workdir place --bundle` places a stored workdir bundle through the
/// head: the set it gives the kind, with the flock's vars. A kind it has
/// nothing for places nothing and says so; a stored bundle has no
/// `--local` place, and needs `--kind`.
#[test]
fn place_a_stored_bundle_through_the_head() {
    let env = workdir_env();
    let config = env.tmp.path().join("c");
    std::fs::create_dir_all(&config).unwrap();
    let src = env.tmp.path().join("src/repo-notes");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("checker.md"), "Tickets at {ticket_url}.\n").unwrap();
    std::fs::write(
        src.join("pastor-bundle.toml"),
        "id = \"repo-notes\"\nversion = \"1\"\ntarget = \"workdir\"\n\
         [vars]\nticket_url = {}\n\
         [[file]]\nsrc = \"checker.md\"\npath = \".claude/agents/checker.md\"\nkinds = [\"claude\"]\nrender = true\n",
    )
    .unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[flock]]\nname = \"ops\"\ndefault = true\nmachines = { here = 1 }\n\
         workdir_bundles = [\"repo-notes\"]\nbundle_vars = { ticket_url = \"https://t.example.com\" }\n\
         [[machine]]\nname = \"here\"\nlocal = true\n",
    )
    .unwrap();
    std::fs::write(
        config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n",
    )
    .unwrap();
    ok(env.run(&["bundle", "add", src.to_str().unwrap()], &[]));
    let log = env.tmp.path().join("serve.log");
    let mut serve = env.command();
    serve
        .args(["serve", "--foreground"])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap());
    let _served = Served {
        child: serve.spawn().unwrap(),
        log,
    };
    let deadline = std::time::Instant::now() + WAIT;
    while !env
        .run(&["workdir", "list", "--json"], &[])
        .status
        .success()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the head never came up"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let dir = env.dir();
    let placed = env.json(&[
        "workdir",
        "place",
        "here",
        dir,
        "--bundle",
        "repo-notes",
        "--kind",
        "claude",
        "--json",
    ]);
    assert_eq!(placed["ok"], true, "{placed}");
    assert_eq!(
        std::fs::read_to_string(env.dir.join(".claude/agents/checker.md")).unwrap(),
        "Tickets at https://t.example.com.\n"
    );
    let listed = env.json(&["workdir", "list", "here", "--json"]);
    assert_eq!(listed[0]["id"], "repo-notes");

    let none = env.json(&[
        "workdir",
        "place",
        "here",
        dir,
        "--bundle",
        "repo-notes",
        "--kind",
        "agy",
        "--json",
    ]);
    assert_eq!(none, serde_json::json!([]));
    let text = ok(env.run(
        &[
            "workdir",
            "place",
            "here",
            dir,
            "--bundle",
            "repo-notes",
            "--kind",
            "agy",
        ],
        &[],
    ));
    assert!(text.contains("nothing for agy agents"), "{text}");
    let out = env.run(
        &[
            "workdir",
            "place",
            "here",
            dir,
            "--bundle",
            "repo-notes",
            "--kind",
            "codex",
        ],
        &[],
    );
    assert_eq!(error_code(&out), "workdir_unsupported");

    let out = env.run(
        &[
            "workdir",
            "place",
            "--local",
            dir,
            "--bundle",
            "repo-notes",
            "--kind",
            "claude",
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let out = env.run(
        &["workdir", "place", "here", dir, "--bundle", "repo-notes"],
        &[],
    );
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

/// In a repo checkout the placed files are listed in `info/exclude` and
/// `git status` shows none of them; the row says so, and `remove` takes the
/// lines out again.
#[test]
fn placed_files_are_kept_out_of_git() {
    let e = workdir_env();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&e.dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    git(&["init", "-q"]);
    let v = e.json(&[
        "workdir",
        "place",
        "--local",
        e.dir(),
        "--set",
        "-",
        "--json",
    ]);
    assert_eq!(v["ok"], true, "{v}");
    let row = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["path"].as_str().unwrap().ends_with("info/exclude"))
        .unwrap_or_else(|| panic!("{v}"));
    assert_eq!(row["action"], "updated");
    let exclude = std::fs::read_to_string(e.dir.join(".git/info/exclude")).unwrap();
    assert!(exclude.contains("# pastor:workdir\n"), "{exclude}");
    assert!(exclude.contains("/.claude/hooks/start.sh\n"), "{exclude}");
    assert_eq!(git(&["status", "--porcelain", "-uall"]), "");

    ok(e.run(&["workdir", "remove", "--local", e.dir()], &[]));
    let exclude = std::fs::read_to_string(e.dir.join(".git/info/exclude")).unwrap();
    assert!(!exclude.contains("pastor:workdir"), "{exclude}");
}

/// A set's settings and instructions are placed, a key per row, and a
/// second set that wants a key the first holds is refused, naming both.
#[test]
fn settings_and_instructions_and_a_clash() {
    let env = workdir_env();
    let dir = env.dir();
    let set = |name: &str, text: &str| {
        let path = env.tmp.path().join(name);
        std::fs::write(&path, text).unwrap();
        path.display().to_string()
    };
    let first = set(
        "first.json",
        r#"{"id": "reviewer", "kind": "claude",
            "settings": {"env": {"WHERE": "{workdir}"}},
            "instructions": "Review the diff."}"#,
    );
    let text = ok(env.run(&["workdir", "place", "--local", dir, "--set", &first], &[]));
    assert_eq!(
        text,
        format!(
            "{dir} reviewer: 2 added\n  added .claude/settings.local.json env.WHERE\n  added CLAUDE.local.md\n"
        )
    );
    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(env.dir.join(".claude/settings.local.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(settings, serde_json::json!({"env": {"WHERE": dir}}));
    let second = set(
        "second.json",
        r#"{"id": "notes", "kind": "claude", "settings": {"env": {"WHERE": "elsewhere"}}}"#,
    );
    let out = env.run(&["workdir", "place", "--local", dir, "--set", &second], &[]);
    assert_eq!(error_code(&out), "workdir_conflict");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("reviewer") && said.contains("notes"),
        "{said}"
    );
    let refused = set(
        "owned.json",
        r#"{"id": "x", "kind": "claude", "settings": {"permissions": {"allow": ["Bash"]}}}"#,
    );
    let out = env.run(
        &["workdir", "place", "--local", dir, "--set", &refused],
        &[],
    );
    assert_eq!(error_code(&out), "workdir_permissions_owned");
}

const PROFILED: &str = r#"{"id": "reviewer", "kind": "claude", "profile": "review"}"#;

/// A set that names a profile has it rendered from pastor.toml (the
/// built-ins need none): its rules go in the settings file, and the answer
/// carries what the launch adds. A profile nobody has is refused, with
/// nothing written.
#[test]
fn a_profile_in_a_set_is_rendered_and_its_launch_args_answered() {
    let e = workdir_env();
    let dir = e.dir();
    let args = ["workdir", "place", "--local", dir, "--set", "-", "--json"];
    let v: serde_json::Value = serde_json::from_str(&ok(e.run_set(&args, &[], PROFILED))).unwrap();
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["profile"]["name"], "review");
    assert!(v["profile"]["flags"].is_array(), "{v}");
    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(e.dir.join(".claude/settings.local.json")).unwrap(),
    )
    .unwrap();
    assert!(settings["permissions"]["allow"].is_array(), "{settings}");
    assert_eq!(settings["permissions"]["defaultMode"], "dontAsk");
    let text = ok(e.run_set(
        &["workdir", "place", "--local", dir, "--set", "-"],
        &[],
        PROFILED,
    ));
    assert!(
        text.contains("profile review: start the agent with"),
        "{text}"
    );

    let e = workdir_env();
    let bad = PROFILED.replace("review", "no-such-profile");
    let out = e.run_set(
        &["workdir", "place", "--local", e.dir(), "--set", "-"],
        &[],
        &bad,
    );
    assert_eq!(error_code(&out), "unknown_profile");
    assert!(!e.dir.join(".claude").exists());
}

/// A profile written in pastor.toml is the one a set gets, and `--machine`
/// settles it as for a task on that machine of the flock: a profile the
/// machine's own does not allow, and a machine nobody has, are refused with
/// nothing written.
#[test]
fn a_set_profile_comes_from_pastor_toml_and_the_machine_of_the_flock() {
    let e = workdir_env();
    let config = e.tmp.path().join("c");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("pastor.toml"),
        "[profiles.ci]\nextends = \"review\"\nallow = [\"mcp__board__post\"]\n",
    )
    .unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[machine]]\nname = \"here\"\nlocal = true\nprofile = \"ci\"\n",
    )
    .unwrap();
    let dir = e.dir();
    let place = |set: &str, machine: Option<&str>| {
        let mut args = vec!["workdir", "place", "--local", dir, "--set", "-", "--json"];
        if let Some(machine) = machine {
            args.extend(["--machine", machine]);
        }
        e.run_set(&args, &[], set)
    };
    let refused = |out: std::process::Output, code: &str| {
        assert_eq!(error_code(&out), code);
        assert!(!e.dir.join(".claude").exists());
    };

    let unrestricted = PROFILED.replace("review", "unrestricted");
    refused(place(&unrestricted, Some("here")), "profile_not_allowed");
    refused(place(PROFILED, Some("nope")), "unknown_machine");

    let ci = PROFILED.replace("review", "ci");
    let v: serde_json::Value = serde_json::from_str(&ok(place(&ci, Some("here")))).unwrap();
    assert_eq!(v["profile"]["name"], "ci", "{v}");
    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(e.dir.join(".claude/settings.local.json")).unwrap(),
    )
    .unwrap();
    assert!(
        settings["permissions"]["allow"]
            .as_array()
            .unwrap()
            .contains(&"mcp__board__post".into()),
        "{settings}"
    );
}

/// An agy set may name a profile, which must be one its machine's own
/// covers (agy's rules are per machine): another is `agy_profile_mismatch`
/// with nothing written, and one that is covered writes nothing
/// permission-shaped and answers the flags the launch adds.
#[test]
fn an_agy_set_profile_must_fit_the_machines_own() {
    let e = workdir_env();
    let config = e.tmp.path().join("c");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[machine]]\nname = \"dev\"\nssh = \"user@dev\"\nprofile = \"develop\"\n",
    )
    .unwrap();
    let dir = e.dir();
    let place = |profile: &str, machine: &str| {
        let set = format!(r#"{{"id": "reviewer", "kind": "agy", "profile": "{profile}"}}"#);
        e.run_set(
            &[
                "workdir",
                "place",
                "--local",
                dir,
                "--set",
                "-",
                "--machine",
                machine,
                "--json",
            ],
            &[],
            &set,
        )
    };
    let out = place("review", "dev");
    assert_eq!(error_code(&out), "agy_profile_mismatch");
    assert!(!e.dir.join(".agents").exists());

    let v: serde_json::Value = serde_json::from_str(&ok(place("develop", "dev"))).unwrap();
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["profile"]["name"], "develop");
    assert!(
        v["profile"]["flags"]
            .as_array()
            .unwrap()
            .contains(&"--dangerously-skip-permissions".into()),
        "{v}"
    );
    assert!(!e.dir.join(".claude").exists());
    assert!(!e.dir.join(".agents").exists());
}

/// `pastor <args>` on a machine whose head is elsewhere (`client`), with
/// `set` on stdin and its own home.
fn on_client(c: &Client, home: &std::path::Path, args: &[&str], set: &str) -> std::process::Output {
    let mut child = pastor()
        .args(args)
        .env("PASTOR_CONFIG_DIR", &c.config)
        .env("PASTOR_STATE_DIR", &c.state)
        .env("PASTOR_DATA_DIR", &c.data)
        .env("PATH", &c.path)
        .env("HOME", home)
        .env_remove("PASTOR_HEAD")
        .env_remove("PASTOR_TASK")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(set.as_bytes());
    child.wait_with_output().unwrap()
}

/// The reports waiting on a machine whose head has not taken them.
fn waiting_reports(c: &Client) -> usize {
    std::fs::read_dir(c.state.join("workdir-reports"))
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                .count()
        })
        .unwrap_or(0)
}

fn listed(c: &Client) -> Vec<(String, String, String)> {
    let out = c.cmd(&["workdir", "list", "--json"]);
    let rows: serde_json::Value = serde_json::from_str(&ok(out)).unwrap();
    rows.as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["machine"].as_str().unwrap().to_string(),
                r["id"].as_str().unwrap().to_string(),
                r["result"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// A machine the head does not push to (the test head's `fake` is a command
/// machine) tells the head what `place --local` and `remove --local` did,
/// so `workdir list` shows it.
#[test]
fn a_local_place_and_remove_are_reported_to_the_head() {
    let head = start();
    let c = client(Some(&head));
    ok(c.head_set("head-up", &[]));
    let home = c.tmp.path().join("home");
    let dir = home.join("src/app");
    std::fs::create_dir_all(&dir).unwrap();
    let (home_s, dir_s) = (home.to_str().unwrap(), dir.to_str().unwrap());
    let args = |verb: &'static str| {
        let mut a = vec!["workdir", verb, "--local", dir_s, "--machine", "fake"];
        if verb == "place" {
            a.extend(["--set", "-"]);
        }
        a
    };
    let out = on_client(&c, &home, &args("place"), SET);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(
        listed(&c),
        [("fake".to_string(), "reviewer".to_string(), "ok".to_string())]
    );

    // A dry run reports nothing and changes nothing.
    let mut dry = args("remove");
    dry.push("--dry-run");
    assert!(on_client(&c, &home, &dry, "").status.success());
    assert_eq!(listed(&c).len(), 1);

    let out = on_client(&c, &home, &args("remove"), "");
    assert!(out.status.success(), "{out:?}");
    assert!(listed(&c).is_empty());
    let _ = home_s;
}

/// A head that does not answer does not stop a place: a set with no profile
/// is placed and its report waits, to go with the next one once the head
/// answers. A set with a profile needs the head's rules and fails before it
/// writes anything.
#[test]
fn a_report_waits_for_a_silent_head_and_a_profile_set_needs_it() {
    let head = start();
    let c = client(Some(&head));
    ok(c.head_set("unreachable", &["--force"]));
    let home = c.tmp.path().join("home");
    let dir = home.join("src/app");
    std::fs::create_dir_all(&dir).unwrap();
    let place = |set: &str| {
        on_client(
            &c,
            &home,
            &[
                "workdir",
                "place",
                "--local",
                dir.to_str().unwrap(),
                "--machine",
                "fake",
                "--set",
                "-",
            ],
            set,
        )
    };

    let out = place(PROFILED);
    assert_eq!(error_code(&out), "head_unreachable");
    assert!(!dir.join(".claude").exists(), "nothing written");

    let out = place(SET);
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("did not take the report"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.join(".claude/skills/reviewing-a-pr/SKILL.md").exists());
    assert_eq!(waiting_reports(&c), 1);

    // The head answers again: the next run sends both.
    ok(c.head_set("head-up", &[]));
    let second = r#"{"id": "second", "kind": "claude",
        "files": [{"path": ".claude/agents/second.md", "content": "x"}]}"#;
    let out = place(second);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    let mut got = listed(&c);
    got.sort();
    assert_eq!(
        got,
        [
            ("fake".to_string(), "reviewer".to_string(), "ok".to_string()),
            ("fake".to_string(), "second".to_string(), "ok".to_string()),
        ]
    );
    assert_eq!(waiting_reports(&c), 0);
}

/// A head that takes forever to answer does not hold a launcher up: the set
/// is placed, the run returns within a few seconds, and the report waits.
#[test]
fn a_head_that_does_not_answer_does_not_hold_up_a_local_place() {
    use std::os::unix::fs::PermissionsExt;
    let head = start();
    let c = client(Some(&head));
    ok(c.head_set("head-up", &[]));
    // The head's address now leads to an ssh that never answers.
    let toml = c.config.join("client.toml");
    let text = std::fs::read_to_string(&toml).unwrap();
    std::fs::write(&toml, text.replace("head-up", "slow-head")).unwrap();
    let bin = c.tmp.path().join("bin");
    std::fs::rename(bin.join("ssh"), bin.join("ssh.real")).unwrap();
    std::fs::write(
        bin.join("ssh"),
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = slow-head ] && exec sleep 120; done\nexec {} \"$@\"\n",
            bin.join("ssh.real").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(bin.join("ssh"), std::fs::Permissions::from_mode(0o755)).unwrap();

    let home = c.tmp.path().join("home");
    let dir = home.join("src/app");
    std::fs::create_dir_all(&dir).unwrap();
    let started = std::time::Instant::now();
    let out = on_client(
        &c,
        &home,
        &[
            "workdir",
            "place",
            "--local",
            dir.to_str().unwrap(),
            "--machine",
            "fake",
            "--set",
            "-",
        ],
        SET,
    );
    let took = started.elapsed();
    assert!(out.status.success(), "{out:?}");
    assert!(took < std::time::Duration::from_secs(30), "{took:?}");
    assert!(dir.join(".claude/skills/reviewing-a-pr/SKILL.md").exists());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("did not take the report"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(waiting_reports(&c), 1);
}
