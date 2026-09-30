use crate::helpers::*;

#[test]
fn edit_saves_a_valid_edit_of_each_file() {
    let o = offline();
    let job = o.config.join("jobs/nightly.toml");
    let edited = NIGHTLY.replace("1h", "2h");
    let out = o.edit(&o.editor(&[&edited]), &["job", "edit", "nightly"], "");
    let text = ok(out);
    assert!(text.contains("saved"), "{text}");
    assert_eq!(o.seen(0), NIGHTLY, "the editor starts from the file");
    assert_eq!(std::fs::read_to_string(&job).unwrap(), edited);
    // The temp copy is gone once the edit is saved; the lock file stays.
    let mut tmp_left: Vec<_> = std::fs::read_dir(o.config.join("jobs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    tmp_left.sort();
    assert_eq!(tmp_left, [".nightly.toml.lock", "nightly.toml"]);

    // flock.toml and pastor.toml need not exist yet.
    let flock = "[[flock]]\nname = \"work\"\ndefault = true\n\n[[machine]]\nname = \"pi-1\"\nlocal = true\n";
    let o2 = offline();
    ok(o2.edit(&o2.editor(&[flock]), &["flock", "edit"], ""));
    assert_eq!(o2.seen(0), "");
    assert_eq!(
        std::fs::read_to_string(o2.config.join("flock.toml")).unwrap(),
        flock
    );
    let config = "tick = \"5s\"\n";
    let o3 = offline();
    ok(o3.edit(&o3.editor(&[config]), &["config", "edit"], ""));
    assert_eq!(
        std::fs::read_to_string(o3.config.join("pastor.toml")).unwrap(),
        config
    );

    // $VISUAL wins over $EDITOR; the editor is a shell word list, as git
    // reads it.
    let o4 = offline();
    let visual = o4.editor(&["tick = \"7s\"\n"]);
    let out = pastor()
        .args(["config", "edit"])
        .env("PASTOR_CONFIG_DIR", &o4.config)
        .env("PASTOR_STATE_DIR", &o4.state)
        .env("VISUAL", format!("sh {visual}"))
        .env("EDITOR", "false")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    ok(out);
    assert_eq!(
        std::fs::read_to_string(o4.config.join("pastor.toml")).unwrap(),
        "tick = \"7s\"\n"
    );
}

#[test]
fn edit_of_a_symlinked_job_writes_its_target() {
    let o = offline();
    let real = o.tmp.path().join("dotfiles/nightly.toml");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    let link = o.config.join("jobs/nightly.toml");
    std::fs::rename(&link, &real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let edited = NIGHTLY.replace("1h", "3h");
    ok(o.edit(&o.editor(&[&edited]), &["job", "edit", "nightly"], ""));
    assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
    assert_eq!(std::fs::read_to_string(&real).unwrap(), edited);
}

#[test]
fn edit_that_is_invalid_reopens_until_fixed() {
    let o = offline();
    let job = o.config.join("jobs/nightly.toml");
    let broken = NIGHTLY.replace("1h", "soon");
    let fixed = NIGHTLY.replace("1h", "4h");
    let out = o.edit(
        &o.editor(&[&broken, &fixed]),
        &["job", "edit", "nightly"],
        "y\n",
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    ok(out);
    assert_eq!(o.runs(), 2);
    assert!(stderr.contains("soon"), "the error is shown: {stderr}");
    // The reopened copy is the broken edit, with the error on top as comments.
    let second = o.seen(1);
    assert!(second.starts_with("# pastor:"), "{second}");
    assert!(second.ends_with(&broken), "{second}");
    assert_eq!(std::fs::read_to_string(&job).unwrap(), fixed);

    // pastor.toml is checked as the head loads it: a zero tick is refused.
    let o2 = offline();
    let out = o2.edit(
        &o2.editor(&["tick = \"0s\"\n", "tick = \"2s\"\n"]),
        &["config", "edit"],
        "\n",
    );
    ok(out);
    assert_eq!(
        std::fs::read_to_string(o2.config.join("pastor.toml")).unwrap(),
        "tick = \"2s\"\n"
    );
}

#[test]
fn edit_that_is_invalid_and_not_reopened_leaves_the_file_alone() {
    let o = offline();
    let job = o.config.join("jobs/nightly.toml");
    let broken = NIGHTLY.replace("clock", "no-such-connector");
    let out = o.edit(&o.editor(&[&broken]), &["job", "edit", "nightly"], "n\n");
    let (code, message) = last_error(&out);
    assert_eq!(code, "invalid_edit");
    assert_eq!(std::fs::read_to_string(&job).unwrap(), NIGHTLY);
    // The message names the kept copy, which holds the edit.
    let kept = message
        .split_whitespace()
        .find(|w| w.contains("pastor-edit"))
        .unwrap_or_else(|| panic!("{message}"))
        .trim_end_matches([',', ';', '.']);
    assert_eq!(std::fs::read_to_string(kept).unwrap(), broken);
    std::fs::remove_file(kept).unwrap();

    // No answer at all (stdin closed) is a no too, never a loop.
    let o2 = offline();
    let out = o2.edit(
        &o2.editor(&["[[machine]]\nname = \"a\"\n"]),
        &["flock", "edit"],
        "",
    );
    let (code, message) = last_error(&out);
    assert_eq!(code, "invalid_edit");
    assert_eq!(o2.runs(), 1);
    assert!(!o2.config.join("flock.toml").exists());
    if let Some(kept) = message
        .split_whitespace()
        .find(|w| w.contains("pastor-edit"))
    {
        let _ = std::fs::remove_file(kept);
    }
}

#[test]
fn edit_aborted_or_unchanged_writes_nothing() {
    let o = offline();
    let job = o.config.join("jobs/nightly.toml");
    let out = o.edit("false", &["job", "edit", "nightly"], "");
    assert_eq!(error_code(&out), "editor_failed");
    assert_eq!(std::fs::read_to_string(&job).unwrap(), NIGHTLY);
    let text = ok(o.edit("true", &["job", "edit", "nightly"], ""));
    assert!(text.contains("no changes"), "{text}");
    assert_eq!(std::fs::read_to_string(&job).unwrap(), NIGHTLY);
    let out = o.edit("true", &["job", "edit", "ghost"], "");
    assert_eq!(error_code(&out), "job_not_found");
    let out = o.edit("true", &["job", "edit", "../pastor"], "");
    assert_eq!(error_code(&out), "job_not_found");
    // A file changed behind the editor is not overwritten.
    let script = format!(
        "#!/bin/sh\necho '# changed elsewhere' >> {}\necho '# mine' >> \"$1\"\n",
        job.display()
    );
    let ed = o.tmp.path().join("race.sh");
    std::fs::write(&ed, script).unwrap();
    let out = o.edit(
        &format!("sh {}", ed.display()),
        &["job", "edit", "nightly"],
        "",
    );
    assert_eq!(error_code(&out), "edit_conflict");
    let now = std::fs::read_to_string(&job).unwrap();
    assert!(
        now.ends_with("# changed elsewhere\n") && !now.contains("# mine"),
        "{now}"
    );
}

#[test]
fn edit_reloads_a_running_head() {
    let env = start_with_jobs(&[("nightly", NIGHTLY)]);
    let edited = format!("enabled = false\n{NIGHTLY}");
    let ed = env.config.join("ed.sh");
    std::fs::write(&ed, format!("#!/bin/sh\ncat > \"$1\" <<'X'\n{edited}X\n")).unwrap();
    let out = pastor()
        .args(["job", "edit", "nightly"])
        .env("PASTOR_CONFIG_DIR", &env.config)
        .env("PASTOR_STATE_DIR", &env.state)
        .env_remove("VISUAL")
        .env("EDITOR", format!("sh {}", ed.display()))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = ok(out);
    assert!(text.contains("picked it up"), "{text}");
    let jobs: Vec<serde_json::Value> =
        serde_json::from_str(&ok(env.cmd(&["job", "list", "--json"]))).unwrap();
    assert_eq!(jobs[0]["enabled"], false);
}

/// Copilot 4109347330: only a reopened copy carries pastor's error block, so
/// a leading `# pastor:` comment of the user's own survives the first pass.
#[test]
fn edit_keeps_a_leading_pastor_comment_of_the_users_own() {
    let o = offline();
    let job = o.config.join("jobs/nightly.toml");
    let edited = format!("# pastor: keep this\n{NIGHTLY}");
    ok(o.edit(&o.editor(&[&edited]), &["job", "edit", "nightly"], ""));
    assert_eq!(std::fs::read_to_string(&job).unwrap(), edited);
}

/// Copilot 4109415065: saving through a symlink must not chmod the
/// directory the link points into; only a missing parent is created, 0700.
#[test]
fn edit_leaves_an_existing_parent_dir_mode_alone() {
    use std::os::unix::fs::PermissionsExt;
    let o = offline();
    let dotfiles = o.tmp.path().join("dotfiles");
    std::fs::create_dir_all(&dotfiles).unwrap();
    std::fs::set_permissions(&dotfiles, std::fs::Permissions::from_mode(0o755)).unwrap();
    let real = dotfiles.join("nightly.toml");
    let link = o.config.join("jobs/nightly.toml");
    std::fs::rename(&link, &real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let edited = NIGHTLY.replace("1h", "5h");
    ok(o.edit(&o.editor(&[&edited]), &["job", "edit", "nightly"], ""));
    assert_eq!(std::fs::read_to_string(&real).unwrap(), edited);
    let mode = std::fs::metadata(&dotfiles).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);

    let o2 = offline();
    std::fs::remove_dir_all(&o2.config).unwrap();
    ok(o2.edit(&o2.editor(&["tick = \"5s\"\n"]), &["config", "edit"], ""));
    let mode = std::fs::metadata(&o2.config).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
}

/// Copilot 4109415047: the file is written through a fresh temp file made
/// with exclusive creation, so a symlink planted at a predictable temp name
/// cannot redirect the write, or the chmod after it, to another file.
#[test]
fn edit_never_writes_through_a_planted_temp_symlink() {
    use std::os::unix::fs::PermissionsExt;
    let o = offline();
    let job = o.config.join("jobs/nightly.toml");
    let victim = o.tmp.path().join("victim");
    std::fs::write(&victim, "untouched\n").unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
    let planted = o.config.join("jobs/nightly.toml.tmp");
    std::os::unix::fs::symlink(&victim, &planted).unwrap();
    let edited = NIGHTLY.replace("1h", "6h");
    ok(o.edit(&o.editor(&[&edited]), &["job", "edit", "nightly"], ""));
    assert_eq!(std::fs::read_to_string(&job).unwrap(), edited);
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched\n");
    let mode = std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644);
    // No temp file of pastor's is left beside the job.
    let mut left: Vec<_> = std::fs::read_dir(o.config.join("jobs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| !n.ends_with(".lock"))
        .collect();
    left.sort();
    assert_eq!(left, ["nightly.toml", "nightly.toml.tmp"]);
}

/// Copilot 4109347317: the conflict check and the rename happen together
/// under an advisory lock on a file beside the target, so a pastor edit
/// that saves while another holds the lock waits, then checks the file as
/// it is after the other one's write.
#[test]
fn edit_saves_under_a_lock_and_rechecks_after_waiting() {
    use std::os::unix::io::AsRawFd;
    let hold = |o: &Offline| {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(o.config.join("jobs/.nightly.toml.lock"))
            .unwrap();
        assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) }, 0);
        f
    };
    let spawn = |o: &Offline, edited: &str| {
        pastor()
            .args(["job", "edit", "nightly"])
            .env("PASTOR_CONFIG_DIR", &o.config)
            .env("PASTOR_STATE_DIR", &o.state)
            .env_remove("VISUAL")
            .env("EDITOR", o.editor(&[edited]))
            .env("TMPDIR", o.tmp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let wait_for_editor = |o: &Offline| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while o.runs() == 0 {
            assert!(std::time::Instant::now() < deadline, "editor never ran");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    };

    // Held, then released untouched: the edit waits, then saves.
    let o = offline();
    let job = o.config.join("jobs/nightly.toml");
    let edited = NIGHTLY.replace("1h", "7h");
    let lock = hold(&o);
    let mut child = spawn(&o, &edited);
    wait_for_editor(&o);
    assert!(child.try_wait().unwrap().is_none(), "the edit did not wait");
    assert_eq!(std::fs::read_to_string(&job).unwrap(), NIGHTLY);
    drop(lock);
    ok(child.wait_with_output().unwrap());
    assert_eq!(std::fs::read_to_string(&job).unwrap(), edited);

    // Changed while the edit waited: the check sees it and refuses.
    let o = offline();
    let job = o.config.join("jobs/nightly.toml");
    let lock = hold(&o);
    let child = spawn(&o, &NIGHTLY.replace("1h", "8h"));
    wait_for_editor(&o);
    let theirs = NIGHTLY.replace("1h", "9h");
    std::fs::write(&job, &theirs).unwrap();
    drop(lock);
    let out = child.wait_with_output().unwrap();
    assert_eq!(last_error(&out).0, "edit_conflict");
    assert_eq!(std::fs::read_to_string(&job).unwrap(), theirs);
}

#[test]
fn edit_with_a_head_changes_the_head_s_files() {
    let env = start_with_jobs(&[("nightly", NIGHTLY)]);
    let l = laptop(&env);
    let head_job = env.config.join("jobs/nightly.toml");

    let edited = format!("enabled = false\n{NIGHTLY}");
    let text = ok(l.edit(&l.editor(&[&edited]), &["job", "edit", "nightly"], ""));
    assert!(text.contains("saved"), "{text}");
    assert!(text.contains("picked it up"), "{text}");
    assert_eq!(l.seen(0), NIGHTLY, "the editor starts from the head's file");
    assert_eq!(std::fs::read_to_string(&head_job).unwrap(), edited);
    assert_eq!(
        std::fs::read_to_string(l.config.join("jobs/nightly.toml")).unwrap(),
        NIGHTLY,
        "the local copy is not touched"
    );
    let jobs: Vec<serde_json::Value> =
        serde_json::from_str(&ok(env.cmd(&["job", "list", "--json"]))).unwrap();
    assert_eq!(jobs[0]["enabled"], false);

    // flock.toml and pastor.toml: the head's, which the laptop does not have.
    let head_flock = std::fs::read_to_string(env.config.join("flock.toml")).unwrap();
    let l2 = laptop(&env);
    let flock = format!("{head_flock}# edited\n");
    ok(l2.edit(&l2.editor(&[&flock]), &["flock", "edit"], ""));
    assert_eq!(l2.seen(0), head_flock);
    assert_eq!(
        std::fs::read_to_string(env.config.join("flock.toml")).unwrap(),
        flock
    );
    assert!(!l2.config.join("flock.toml").exists());
    let head_config = std::fs::read_to_string(env.config.join("pastor.toml")).unwrap();
    let l3 = laptop(&env);
    let config = format!("{head_config}# edited\n");
    ok(l3.edit(&l3.editor(&[&config]), &["config", "edit"], ""));
    assert_eq!(
        std::fs::read_to_string(env.config.join("pastor.toml")).unwrap(),
        config
    );
    assert!(!l3.config.join("pastor.toml").exists());

    // An unchanged file sends nothing and says so.
    let text = ok(l3.edit("true", &["config", "edit"], ""));
    assert!(text.contains("no changes"), "{text}");
    // A job only the laptop has is not the head's.
    std::fs::write(l3.config.join("jobs/mine.toml"), NIGHTLY).unwrap();
    assert_eq!(
        error_code(&l3.edit("true", &["job", "edit", "mine"], "")),
        "job_not_found"
    );
}

#[test]
fn edit_with_a_head_refuses_an_invalid_or_stale_edit() {
    let env = start_with_jobs(&[("nightly", NIGHTLY)]);
    let head_job = env.config.join("jobs/nightly.toml");

    // Invalid, not reopened: the head's error is shown, its file is untouched.
    let l = laptop(&env);
    let broken = NIGHTLY.replace("1h", "soon");
    let out = l.edit(&l.editor(&[&broken]), &["job", "edit", "nightly"], "n\n");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(last_error(&out).0, "invalid_edit");
    assert!(stderr.contains("soon"), "{stderr}");
    assert_eq!(std::fs::read_to_string(&head_job).unwrap(), NIGHTLY);

    // Invalid, then fixed in the reopened editor.
    let l = laptop(&env);
    let fixed = NIGHTLY.replace("1h", "4h");
    ok(l.edit(
        &l.editor(&[&broken, &fixed]),
        &["job", "edit", "nightly"],
        "y\n",
    ));
    assert_eq!(l.runs(), 2);
    assert!(l.seen(1).starts_with("# pastor:"), "{}", l.seen(1));
    assert_eq!(std::fs::read_to_string(&head_job).unwrap(), fixed);

    // Changed on the head while the editor was open: a stale hash.
    let l = laptop(&env);
    let ed = l.tmp.path().join("race.sh");
    std::fs::write(
        &ed,
        format!(
            "#!/bin/sh\necho '# changed elsewhere' >> {}\necho '# mine' >> \"$1\"\n",
            head_job.display()
        ),
    )
    .unwrap();
    let out = l.edit(
        &format!("sh {}", ed.display()),
        &["job", "edit", "nightly"],
        "",
    );
    assert_eq!(last_error(&out).0, "edit_conflict");
    let now = std::fs::read_to_string(&head_job).unwrap();
    assert!(
        now.ends_with("# changed elsewhere\n") && !now.contains("# mine"),
        "{now}"
    );
}

/// The reverse: `pastor serve` takes the same lock while it starts, so it
/// does not load flock.toml or listen while an offline edit holds it.
#[test]
fn serve_does_not_listen_while_an_offline_edit_holds_the_fleet_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let socket = tmp.path().join("herdr.sock");
    let herdr = Command::new(env!("CARGO_BIN_EXE_fake-herdr"))
        .arg("--listen")
        .arg(&socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::fs::write(
        config.join("flock.toml"),
        format!(
            "[[machine]]\nname = \"fake\"\ncommand = [\"{}\", \"--connect\", \"{}\"]\nmax_agents = 2\n",
            env!("CARGO_BIN_EXE_fake-herdr"),
            socket.display()
        ),
    )
    .unwrap();
    let paths = pastor::config::Paths::new(&config, &state);
    let held = pastor::fleet_edit::lock_fleet(&paths, Duration::from_secs(5)).unwrap();
    let serve = pastor()
        .args(["serve", "--foreground"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut env = ServeEnv { serve, herdr };
    std::thread::sleep(Duration::from_millis(1500));
    assert!(env.serve.try_wait().unwrap().is_none(), "serve exited");
    assert!(
        !state.join("pastor.sock").exists(),
        "serve listened under the lock"
    );
    drop(held);
    let deadline = Instant::now() + WAIT;
    while !state.join("pastor.sock").exists() {
        assert!(Instant::now() < deadline, "serve never listened");
        assert!(env.serve.try_wait().unwrap().is_none(), "serve exited");
        std::thread::sleep(Duration::from_millis(100));
    }
}
