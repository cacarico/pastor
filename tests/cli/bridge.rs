use crate::helpers::*;

/// `pastor bridge` carries each request line to the head's socket and each
/// reply back, in order, one connection per line, and leaves the bytes alone.
/// The fake head answers every connection with the line it got, numbered, so
/// the test sees both the order and that nothing was rewritten.
#[test]
fn bridge_passes_each_line_to_the_head_and_its_reply_back() {
    use std::io::{BufRead, BufReader, Write};
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&state).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(state.join("pastor.sock")).unwrap();
    std::thread::spawn(move || {
        for (n, stream) in listener.incoming().enumerate() {
            let mut stream = stream.unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let reply = serde_json::json!({"kind": "text", "data": format!("{n}:{line}")});
            writeln!(stream, "{reply}").unwrap();
        }
    });
    let requests = "{\"op\":\"ping\"}\n{\"op\":\"task_show\",\"id\":7,\"from_task\":\"t-3\"}\n";
    let mut child = pastor()
        .arg("bridge")
        .env("PASTOR_CONFIG_DIR", tmp.path().join("c"))
        .env("PASTOR_STATE_DIR", &state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(requests.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let replies: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        replies,
        vec![
            serde_json::json!({"kind": "text", "data": "0:{\"op\":\"ping\"}\n"}),
            serde_json::json!({"kind": "text", "data": "1:{\"op\":\"task_show\",\"id\":7,\"from_task\":\"t-3\"}\n"}),
        ],
        "{stdout}"
    );
    // The bridge is plumbing: it never reads or writes the config.
    assert!(!tmp.path().join("c").exists());
}

/// `--machine` needs `--agent` or `--shepherd`, and the two are mutually
/// exclusive: both are clap usage errors, caught before any socket is
/// touched.
#[test]
fn bridge_machine_needs_a_mode_and_the_modes_exclude_each_other() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("s");
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", tmp.path().join("c"))
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let out = run(&["bridge", "--machine", "pi-1"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--agent or --shepherd"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run(&["bridge", "--agent", "--shepherd", "--machine", "pi-1"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(!state.join("pastor.sock").exists());
}

/// `--fetch` is a shepherd's: without `--shepherd` it is a usage error, and
/// with it but no head the failure is JSON on stderr, never on stdout, where
/// the binary would have been.
#[test]
fn bridge_fetch_needs_shepherd_and_fails_on_stderr() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("s");
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", tmp.path().join("c"))
            .env("PASTOR_STATE_DIR", &state)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let fetch = ["--fetch", "herdr", "0.9.5", "linux-x86_64"];
    let out = run(&[&["bridge", "--agent", "--machine", "pi-1"][..], &fetch].concat());
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let out = run(&[&["bridge"][..], &fetch].concat());
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let out = run(&[&["bridge", "--shepherd", "--machine", "pi-1"][..], &fetch].concat());
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let v: serde_json::Value =
        serde_json::from_slice(&out.stderr).unwrap_or_else(|e| panic!("{e}: {out:?}"));
    assert_eq!(v["code"], "no_head", "{v}");
}

/// With no head on the socket the bridge answers one `no_head` line on
/// stdout, where the remote CLI reads, and exits non-zero. It never starts a
/// head.
#[test]
fn bridge_without_a_head_answers_no_head() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("s");
    let mut child = pastor()
        .arg("bridge")
        .env("PASTOR_CONFIG_DIR", tmp.path().join("c"))
        .env("PASTOR_STATE_DIR", &state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"op\":\"ping\"}\n{\"op\":\"ping\"}\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["code"], "no_head", "{stdout}");
    assert!(v["message"].is_string(), "{stdout}");
    assert!(!state.join("pastor.sock").exists());
}

/// A request line with invalid UTF-8 must reach the head unread, byte for
/// byte, exactly like every other line: the bridge does not get to decide
/// that it is malformed. The fake head here answers exactly as the real
/// daemon does (`read_request` in src/daemon.rs) when a line fails
/// `String::from_utf8`, so this pins the bridge to relaying bytes rather
/// than decoding them into its own `runtime_error` first.
#[test]
fn bridge_relays_invalid_utf8_bytes_to_the_head() {
    use std::io::{Read, Write};
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&state).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(state.join("pastor.sock")).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut buf = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                match stream.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => {
                        let b = byte[0];
                        buf.push(b);
                        if b == b'\n' {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let reply = if std::str::from_utf8(&buf).is_ok() {
                serde_json::json!({"kind": "text", "data": "valid utf-8, unexpectedly"})
            } else {
                serde_json::json!({
                    "kind": "error",
                    "data": {"code": "invalid_request", "message": "a request must be UTF-8"},
                })
            };
            writeln!(stream, "{reply}").unwrap();
        }
    });
    // Lone continuation byte 0x80: not valid UTF-8 on its own.
    let mut request = b"{\"op\":\"ping\",\"bad\":\"\x80\"}".to_vec();
    request.push(b'\n');
    assert!(
        std::str::from_utf8(&request).is_err(),
        "fixture must be invalid UTF-8"
    );
    let mut child = pastor()
        .arg("bridge")
        .env("PASTOR_CONFIG_DIR", tmp.path().join("c"))
        .env("PASTOR_STATE_DIR", &state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&request).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "bridge should relay the head's reply, not fail on its own: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["kind"], "error", "{stdout}");
    assert_eq!(v["data"]["code"], "invalid_request", "{stdout}");
}

/// A head on `<state>/pastor.sock` that answers the shepherd bridge's
/// generation check, and every other request with where
/// `<state>/artifacts/herdr/0.9.5/linux-x86_64/herdr` is, holding `body`,
/// and records each request line.
fn artifact_head(
    state: &std::path::Path,
    body: &[u8],
) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
    use sha2::{Digest, Sha256};
    use std::io::{BufRead, BufReader, Write};
    let dir = state.join("artifacts/herdr/0.9.5/linux-x86_64");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("herdr");
    std::fs::write(&path, body).unwrap();
    let sum: String = Sha256::digest(body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let reply = serde_json::json!({"kind": "artifact", "data": {
        "path": path, "sha256": sum, "size": body.len(),
    }});
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    let listener = std::os::unix::net::UnixListener::bind(state.join("pastor.sock")).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let check = line.contains("\"shepherd_check\"");
            log.lock().unwrap().push(line);
            if check {
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({"kind": "text", "data": "current"})
                )
                .unwrap();
            } else {
                writeln!(stream, "{reply}").unwrap();
            }
        }
    });
    seen
}

/// A pull machine's key forces `bridge --shepherd --machine X`, and sshd
/// hands a forced command none of the client's argv, so a fetch comes as
/// the first request line: the bridge checks the key's generation, then
/// streams the artifact.
#[test]
fn shepherd_forced_command_runs_a_fetch_from_its_first_line() {
    use std::io::Write;
    let line = "{\"op\":\"artifact_fetch\",\"name\":\"herdr\",\"version\":\"0.9.5\",\"target\":\"linux-x86_64\"}\n";
    for original in [
        None,
        Some("pastor bridge"),
        Some("sh -c '/usr/local/bin/pastor bridge'"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("s");
        let body: Vec<u8> = (0..70_000u32).map(|i| (i % 253) as u8).collect();
        let seen = artifact_head(&state, &body);
        let mut cmd = pastor();
        cmd.args(["bridge", "--shepherd", "--machine", "pi-1", "--gen", "3"])
            .env("PASTOR_CONFIG_DIR", tmp.path().join("c"))
            .env("PASTOR_STATE_DIR", &state)
            .env_remove("SSH_ORIGINAL_COMMAND")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(o) = original {
            cmd.env("SSH_ORIGINAL_COMMAND", o);
        }
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("\n{line}").as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{original:?}: {out:?}");
        assert_eq!(out.stdout, body, "{original:?}");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{original:?}: {seen:?}");
        let check: serde_json::Value = serde_json::from_str(&seen[0]).unwrap();
        assert_eq!(check["op"], "shepherd_check", "{original:?}");
        assert_eq!(check["machine"], "pi-1", "{original:?}");
        assert_eq!(check["generation"], 3, "{original:?}");
        let req: serde_json::Value = serde_json::from_str(&seen[1]).unwrap();
        assert_eq!(req["op"], "artifact_locate", "{original:?}");
        assert_eq!(req["version"], "0.9.5", "{original:?}");
    }
    // A fetch line with a part that is not a plain name never reaches the
    // head.
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("s");
    let seen = artifact_head(&state, b"bin");
    let mut child = pastor()
        .args(["bridge", "--shepherd", "--machine", "pi-1"])
        .env("PASTOR_CONFIG_DIR", tmp.path().join("c"))
        .env("PASTOR_STATE_DIR", &state)
        .env_remove("SSH_ORIGINAL_COMMAND")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"{\"op\":\"artifact_fetch\",\"name\":\"herdr\",\"version\":\"..\",\"target\":\"x\"}\n",
        )
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(v["code"], "invalid_artifact", "{v}");
    assert!(seen.lock().unwrap().is_empty());
}

/// Anything else in SSH_ORIGINAL_COMMAND is refused with
/// `not_allowed_for_shepherd`, exit 1, before the head is asked, and the
/// line bridge never runs. Empty, unset or `bridge` alone runs the line
/// bridge as before.
#[test]
fn shepherd_forced_command_refuses_any_other_original_command() {
    use std::io::Write;
    for original in [
        // A fetch as argv: it goes as a request line now.
        "bridge --fetch herdr 0.9.5 linux-x86_64",
        "pastor bridge --fetch herdr 0.9.5 linux-x86_64",
        "sh -c '/usr/local/bin/pastor bridge --fetch herdr 0.9.5 linux-x86_64'",
        "bridge --fetch herdr 0.9.5 linux-x86_64 extra",
        "pastor pastor bridge --fetch herdr 0.9.5 linux-x86_64",
        "bridge --agent --machine pi-1",
        "bridge --shepherd --machine pi-2",
        "bridge --machine pi-2 --fetch herdr 0.9.5",
        "bridge --join",
        "bridge --fetch herdr 0.9.5",
        "bridge --fetch herdr ../.. linux-x86_64",
        "bridge --fetch herdr 0.9.5 linux-x86_64;id",
        "bridge --fetch herdr $(id) linux-x86_64",
        "bridge --fetch herdr 0.9.5 `id`",
        "bridge --fetch herdr 0.9.5 a|b",
        "machine list",
        "task run --prompt hi",
        "sh -c id",
        "sh -c 'pastor bridge; id'",
        "sh -c 'pastor bridge' && id",
        "pastor pastor bridge",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("s");
        let seen = artifact_head(&state, b"bin");
        let mut child = pastor()
            .args(["bridge", "--shepherd", "--machine", "pi-1"])
            .env("PASTOR_CONFIG_DIR", tmp.path().join("c"))
            .env("PASTOR_STATE_DIR", &state)
            .env("SSH_ORIGINAL_COMMAND", original)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // A line the line bridge would have carried to the head.
        let _ = child
            .stdin
            .take()
            .unwrap()
            .write_all(b"{\"op\":\"pull_trust\",\"machine\":\"pi-1\"}\n");
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{original}: {out:?}");
        assert!(out.stdout.is_empty(), "{original}: {out:?}");
        let v: serde_json::Value = serde_json::from_slice(&out.stderr)
            .unwrap_or_else(|e| panic!("{original}: {e}: {out:?}"));
        assert_eq!(v["code"], "not_allowed_for_shepherd", "{original}: {v}");
        assert!(seen.lock().unwrap().is_empty(), "{original}: head asked");
    }
    // The shepherd's own ssh sends the line bridge as `sh -c '<pastor>
    // bridge'` (`ssh::Ssh::args`), so that runs the line bridge too.
    for original in [
        None,
        Some(""),
        Some("   "),
        Some("sh -c 'pastor bridge'"),
        Some("pastor bridge"),
        Some("bridge"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("s");
        let seen = artifact_head(&state, b"bin");
        let mut cmd = pastor();
        cmd.args(["bridge", "--shepherd", "--machine", "pi-1"])
            .env("PASTOR_CONFIG_DIR", tmp.path().join("c"))
            .env("PASTOR_STATE_DIR", &state)
            .env_remove("SSH_ORIGINAL_COMMAND")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(o) = original {
            cmd.env("SSH_ORIGINAL_COMMAND", o);
        }
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"{\"op\":\"pull_trust\",\"machine\":\"pi-1\"}\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{original:?}: {out:?}");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{original:?}: line bridge ran");
        assert!(seen[0].contains("shepherd_check"), "{original:?}: {seen:?}");
        assert!(seen[1].contains("pull_trust"), "{original:?}: {seen:?}");
    }
}
