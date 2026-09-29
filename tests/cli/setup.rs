use crate::helpers::*;

/// The default action is part of the command's own help, not only the README.
#[test]
fn setup_systemd_help_names_the_default_action() {
    let out = pastor()
        .args(["setup", "systemd", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("With no action flag") && help.contains("enable --now"),
        "{help}"
    );
}

/// `setup launchd` offers the same action flags as `setup systemd`.
#[test]
fn setup_launchd_help_names_the_default_action_and_flags() {
    let out = pastor()
        .args(["setup", "launchd", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("With no action flag"), "{help}");
    for flag in ["--herdr", "--enable", "--start", "--now", "--stop", "--yes"] {
        assert!(help.contains(flag), "{flag}: {help}");
    }
}

/// launchd exists only on macOS; elsewhere the command says so and points at
/// systemd, as a JSON error, before writing anything.
#[cfg(not(target_os = "macos"))]
#[test]
fn setup_launchd_off_macos_points_at_systemd() {
    let tmp = tempfile::tempdir().unwrap();
    let out = pastor()
        .env("PASTOR_CONFIG_DIR", tmp.path().join("c"))
        .env("PASTOR_STATE_DIR", tmp.path().join("s"))
        .env("HOME", tmp.path())
        .args(["setup", "launchd", "--yes"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("setup systemd"), "{err}");
    assert!(!tmp.path().join("Library").exists());
}

/// `--yes`/`-y` installs with no prompt, so setup runs from a script, a task
/// or `ssh host pastor setup systemd --yes`. Stdin here is not a terminal.
#[test]
fn setup_systemd_yes_installs_without_a_prompt() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let xdg = tmp.path().join("xdg");
    let data = tmp.path().join("data");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let calls = tmp.path().join("systemctl.log");
    std::fs::write(
        bin.join("systemctl"),
        format!("#!/bin/sh\necho \"$@\" >> {}\n", calls.display()),
    )
    .unwrap();
    std::fs::write(
        bin.join("loginctl"),
        "#!/bin/sh\nif [ \"$1\" = show-user ]; then echo Linger=yes; fi\n",
    )
    .unwrap();
    for f in ["systemctl", "loginctl"] {
        std::fs::set_permissions(bin.join(f), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    for flag in ["--yes", "-y"] {
        let _ = std::fs::remove_file(&calls);
        let out = pastor()
            .args(["setup", "systemd", flag])
            .env("PATH", &path)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("XDG_DATA_HOME", &data)
            .env_remove("PASTOR_DATA_DIR")
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "{flag}\nstdout:\n{}\nstderr:\n{stderr}",
            String::from_utf8_lossy(&out.stdout),
        );
        assert!(!stderr.contains("Continue?"), "{flag} prompted: {stderr}");
        assert!(
            xdg.join("systemd/user/pastor.service").exists(),
            "{flag} should install the unit"
        );
        // A head started at login must use the dirs this run used, including
        // a data dir that came from XDG_DATA_HOME rather than an override.
        let unit = std::fs::read_to_string(xdg.join("systemd/user/pastor.service")).unwrap();
        for (var, dir) in [
            ("PASTOR_CONFIG_DIR", config.clone()),
            ("PASTOR_STATE_DIR", state.clone()),
            ("PASTOR_DATA_DIR", data.join("pastor")),
        ] {
            assert!(
                unit.contains(&format!("{var}={}", dir.display())),
                "{flag} unit lacks {var}:\n{unit}"
            );
        }
        let calls = std::fs::read_to_string(&calls).unwrap();
        assert!(calls.contains("--user daemon-reload"), "{calls}");
        assert!(
            calls.contains("--user enable --now pastor.service"),
            "{calls}"
        );
    }
}

/// Without `--yes` and without a terminal, setup fails at once, telling the
/// caller to pass --yes, instead of waiting on a stdin nobody will answer;
/// and it fails before touching anything.
#[test]
fn setup_systemd_without_a_terminal_fails_fast_and_names_yes() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let xdg = tmp.path().join("xdg");
    let mut child = pastor()
        .args(["setup", "systemd"])
        .env("XDG_CONFIG_HOME", &xdg)
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Hold stdin open: a read_line would block here until the deadline.
    // This guard starts no daemon, so it keeps a short deadline of its own:
    // a regression that blocks on stdin should fail fast, not after WAIT.
    let stdin = child.stdin.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("setup systemd waited on a stdin that is not a terminal");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--yes"), "{stderr}");
    assert!(
        !xdg.join("systemd/user/pastor.service").exists(),
        "a refused setup must not write the unit"
    );
    assert!(
        !config.exists() && !state.exists(),
        "a refused setup must stop before permission hardening mutates paths"
    );
}
