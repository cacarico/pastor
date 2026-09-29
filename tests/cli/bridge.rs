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
