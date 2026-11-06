use crate::helpers::*;

const SESSION: &str = "2a191324-a7b9-431f-85b4-9a1773eeea24";

/// Runs `pastor inbox send` for SESSION with `text` on stdin.
fn send(config: &std::path::Path, text: &str, extra: &[&str]) -> std::process::Output {
    use std::io::Write;
    let mut child = pastor()
        .args(["inbox", "send", "--session", SESSION, "--mode", "prompting"])
        .args(extra)
        .env("CLAUDE_CONFIG_DIR", config)
        .env("PASTOR_CONFIG_DIR", config.join("pastor"))
        .env("PASTOR_STATE_DIR", config.join("state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// A message to a live session is one frame on its socket, and the one JSON
/// line says it was accepted, since the session sent no receipt.
#[test]
fn inbox_send_writes_one_frame_and_reports_accepted() {
    use std::io::Read;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("claude");
    std::fs::create_dir_all(config.join("sessions")).unwrap();
    let socket = tmp.path().join("4242.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::fs::write(
        config.join("sessions/4242.json"),
        serde_json::json!({"sessionId": SESSION, "messagingSocketPath": socket}).to_string(),
    )
    .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut data = String::new();
            stream.unwrap().read_to_string(&mut data).unwrap();
            // The liveness check connects and writes nothing.
            if !data.is_empty() {
                tx.send(data).unwrap();
            }
        }
    });

    let out = send(&config, "hello there\n", &["--wait", "0"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["result"], "accepted", "{result}");
    let data = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert_eq!(data.lines().count(), 1, "{data}");
    let frame: serde_json::Value = serde_json::from_str(&data).unwrap();
    assert_eq!(frame["msg_id"], result["msg_id"]);
    assert_eq!(frame["priority"], "next");
    assert!(
        frame["message"]["content"]
            .as_str()
            .unwrap()
            .contains("\nhello there\n\n</cross-session-message>"),
        "{frame}"
    );
    // Plumbing: pastor's own config and state stay untouched.
    assert!(!config.join("pastor").exists());
    assert!(!config.join("state").exists());
}

#[test]
fn inbox_send_without_a_session_exits_1_with_no_session() {
    let tmp = tempfile::tempdir().unwrap();
    let out = send(tmp.path(), "hello", &[]);
    assert_eq!(out.status.code(), Some(1));
    let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["result"], "no-session", "{result}");
    assert_eq!(result["msg_id"], serde_json::Value::Null);
}

#[test]
fn inbox_send_with_no_text_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let out = send(tmp.path(), " \n", &[]);
    assert_eq!(out.status.code(), Some(1));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["code"], "empty_message", "{err}");
}

#[test]
fn inbox_send_takes_only_a_session_id() {
    let out = pastor()
        .args(["inbox", "send", "--session", "latest", "--mode", "bypass"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}
