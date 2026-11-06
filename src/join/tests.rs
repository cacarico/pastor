use std::path::Path;

use super::*;
use crate::setup::CmdOutput;

fn code(e: &anyhow::Error) -> String {
    e.downcast_ref::<CliError>()
        .map(|e| e.code.clone())
        .unwrap_or_else(|| format!("not a CliError: {e:#}"))
}

fn sample() -> Token {
    Token {
        head: "me@head.example".into(),
        head_host_key: "head.example ssh-ed25519 AAAAHOST".into(),
        // Put together here so that no line of this file looks like a private
        // key to a secret scanner.
        key: [
            "-----BEGIN OPENSSH ",
            "PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA+/=\n-----END OPENSSH ",
            "PRIVATE KEY-----\n",
        ]
        .concat(),
    }
}

#[test]
fn token_round_trips() {
    let t = sample();
    let s = t.encode();
    assert!(s.starts_with("pjt_"), "{s}");
    // One copy-pasteable word: base64url, no padding.
    assert!(
        s["pjt_".len()..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "{s}"
    );
    assert_eq!(Token::decode(&s).unwrap(), t);
    // Pasted with the newline a terminal adds.
    assert_eq!(Token::decode(&format!("{s}\n")).unwrap(), t);
    // Every length of input survives, padding or none.
    for n in 0..8 {
        let bytes: Vec<u8> = (0..n).map(|i| i * 37 + 1).collect();
        assert_eq!(base64url_decode(&base64url_encode(&bytes)), Some(bytes));
    }
    // Its Debug form keeps the private key out of logs.
    assert!(!format!("{t:?}").contains("PRIVATE"));
}

#[test]
fn decode_rejects_garbage() {
    let enc = |text: &str| format!("pjt_{}", base64url_encode(text.as_bytes()));
    for bad in [
        String::new(),
        "pjt_".into(),
        "nope".into(),
        format!("xyz_{}", &sample().encode()[4..]),
        "pjt_!!!!".into(),
        "pjt_QUJD=".into(),
        "pjt_A".into(),
        format!("pjt_{}", base64url_encode(&[0xff, 0xfe, 0x00])),
        enc("only a head"),
        enc("head\nhost key"),
        enc("\nhost ssh-ed25519 K\nkey"),
        enc("head\n \nkey"),
        enc("-oProxyCommand=x\nhost ssh-ed25519 K\nkey"),
        enc("me@head extra\nhost ssh-ed25519 K\nkey"),
    ] {
        let err = Token::decode(&bad).unwrap_err();
        assert_eq!(code(&err), INVALID, "{bad:?}");
    }
}

#[test]
fn known_hosts_names_a_port_off_22() {
    assert_eq!(known_hosts_name("me@head"), "head");
    assert_eq!(known_hosts_name("head"), "head");
    assert_eq!(known_hosts_name("ssh://me@head:2222"), "[head]:2222");
    assert_eq!(known_hosts_name("ssh://me@head:22"), "head");
}

/// An `ssh-keygen` that writes a pair where `-f` says.
pub(crate) struct FakeKeygen;

impl Runner for FakeKeygen {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput> {
        assert_eq!(program, "ssh-keygen");
        let file = args[args.iter().position(|a| *a == "-f").unwrap() + 1];
        let name = Path::new(file).file_name().unwrap().to_string_lossy();
        let blob: String = name.chars().filter(char::is_ascii_alphanumeric).collect();
        std::fs::write(file, "-----BEGIN OPENSSH PRIVATE KEY-----\nsecret\n")?;
        std::fs::write(
            format!("{file}.pub"),
            format!("ssh-ed25519 AAAA{blob} comment\n"),
        )?;
        Ok(CmdOutput {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

pub(crate) fn head_files(dir: &Path) -> HeadFiles {
    let host_key = dir.join("ssh_host_ed25519_key.pub");
    std::fs::write(&host_key, "ssh-ed25519 AAAAHOSTKEY root@head\n").unwrap();
    HeadFiles {
        authorized_keys: dir.join("authorized_keys"),
        host_key,
        pastor: PathBuf::from("/usr/local/bin/pastor"),
        work: dir.join("work"),
    }
}

fn ask(ttl: Duration) -> Ask {
    Ask {
        flock: None,
        tags: Vec::new(),
        replace: None,
        ttl,
    }
}

#[test]
fn create_writes_the_line_and_keeps_no_private_key() {
    let dir = tempfile::tempdir().unwrap();
    let files = head_files(dir.path());
    let store = Store::open(&dir.path().join("db")).unwrap();
    let now = Utc::now();
    let made = create_with(
        &FakeKeygen,
        &files,
        &store,
        "me@head",
        &ask(Duration::from_secs(3600)),
        now,
    )
    .unwrap();
    let id = &made.row.id;
    assert!(is_join_id(id), "{id}");
    assert_eq!(made.token.head, "me@head");
    assert_eq!(made.token.head_host_key, "head ssh-ed25519 AAAAHOSTKEY");
    assert!(made.token.key.contains("PRIVATE KEY"));
    let text = std::fs::read_to_string(&files.authorized_keys).unwrap();
    let expiry = (now + chrono::Duration::hours(1)).format("%Y%m%d%H%M%SZ");
    assert!(
        text.contains(&format!("expiry-time=\"{expiry}\""))
            && text.contains(&format!(
                "command=\"/usr/local/bin/pastor bridge --join {id}\""
            ))
            && text.trim_end().ends_with(&format!("pastor:join:{id}")),
        "{text}"
    );
    // The pair is gone from disk; the store has the id and no key.
    assert_eq!(std::fs::read_dir(&files.work).unwrap().count(), 0);
    let row = store.join_token(id).unwrap().unwrap();
    assert_eq!(row, made.row);
}

#[test]
fn expired_swept() {
    let dir = tempfile::tempdir().unwrap();
    let files = head_files(dir.path());
    let store = Store::open(&dir.path().join("db")).unwrap();
    let now = Utc::now();
    let short = create_with(
        &FakeKeygen,
        &files,
        &store,
        "me@head",
        &ask(Duration::from_secs(60)),
        now,
    )
    .unwrap()
    .row
    .id;
    let long = create_with(
        &FakeKeygen,
        &files,
        &store,
        "me@head",
        &ask(Duration::from_secs(3600)),
        now,
    )
    .unwrap()
    .row
    .id;
    let (tx, mut rx) = tokio::sync::broadcast::channel(8);
    // Not yet: both stay.
    assert_eq!(sweep(&store, &files.authorized_keys, &tx, now), 0);
    let later = now + chrono::Duration::minutes(5);
    assert_eq!(sweep(&store, &files.authorized_keys, &tx, later), 1);
    let text = std::fs::read_to_string(&files.authorized_keys).unwrap();
    assert!(!text.contains(&format!("pastor:join:{short}")), "{text}");
    assert!(text.contains(&format!("pastor:join:{long}")), "{text}");
    assert!(store.join_token(&short).unwrap().is_none());
    assert!(store.join_token(&long).unwrap().is_some());
    let ev = rx.try_recv().unwrap();
    assert_eq!(ev.kind, "machine.join_token_expired");
    assert_eq!(ev.detail.unwrap()["id"], short.as_str());
    // Swept once only.
    assert_eq!(sweep(&store, &files.authorized_keys, &tx, later), 0);
}

#[test]
fn spend_names_why_a_token_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let files = head_files(dir.path());
    let store = Store::open(&dir.path().join("db")).unwrap();
    let now = Utc::now();
    let id = create_with(
        &FakeKeygen,
        &files,
        &store,
        "me@head",
        &ask(Duration::from_secs(60)),
        now,
    )
    .unwrap()
    .row
    .id;
    let late = now + chrono::Duration::minutes(2);
    assert_eq!(code(&spend(&store, &id, late).unwrap_err()), EXPIRED);
    assert_eq!(spend(&store, &id, now).unwrap().id, id);
    assert_eq!(code(&spend(&store, &id, now).unwrap_err()), USED);
    for unknown in ["0123456789abcdef", "pjt_abc", "../../etc", ""] {
        assert_eq!(code(&spend(&store, unknown, now).unwrap_err()), INVALID);
    }
}

/// A head on a socket in `dir` that answers every line with `Text("ok")`
/// and keeps the lines it got.
fn fake_head(dir: &Path) -> (PathBuf, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let socket = dir.join("pastor.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = got.clone();
    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let (read, mut write) = conn.into_split();
                let mut lines = tokio::io::BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    seen.lock().unwrap().push(line);
                    let mut reply = serde_json::to_vec(&IpcResponse::Text("ok".into())).unwrap();
                    reply.push(b'\n');
                    let _ = write.write_all(&reply).await;
                }
            });
        }
    });
    (socket, got)
}

fn join_line(id: &str, name: &str) -> String {
    let req = IpcRequest::Join {
        id: id.into(),
        name: name.into(),
        machine_key: "ssh-ed25519 AAAAM".into(),
        agent_key: "ssh-ed25519 AAAAA".into(),
        host_key: "ssh-ed25519 AAAAH".into(),
    };
    crate::ipc::request_line(&req, None).unwrap()
}

fn reply_code(out: &[u8]) -> Option<String> {
    let first = out.split(|b| *b == b'\n').next().unwrap();
    match serde_json::from_slice::<IpcResponse>(first).unwrap() {
        IpcResponse::Error { code, .. } => Some(code),
        _ => None,
    }
}

#[tokio::test]
async fn bridge_join_refuses_other_requests() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, got) = fake_head(dir.path());
    let id = "0123456789abcdef";
    for other in [
        crate::ipc::request_line(&IpcRequest::Ping, None).unwrap(),
        crate::ipc::request_line(&IpcRequest::JoinList, None).unwrap(),
        crate::ipc::request_line(&IpcRequest::JoinRevoke { id: id.into() }, None).unwrap(),
        "not json".into(),
    ] {
        let mut out = Vec::new();
        bridge(&socket, id, format!("{other}\n").as_bytes(), &mut out)
            .await
            .unwrap();
        assert_eq!(reply_code(&out).as_deref(), Some(INVALID), "{other}");
    }
    assert!(got.lock().unwrap().is_empty(), "{:?}", got.lock().unwrap());

    // One join goes through, with the id the forced command names, and a
    // second line is never read.
    let input = format!(
        "\n{}\n{}\n",
        join_line("ffffffffffffffff", "pi-3"),
        join_line("ffffffffffffffff", "pi-4")
    );
    let mut out = Vec::new();
    bridge(&socket, id, input.as_bytes(), &mut out)
        .await
        .unwrap();
    assert_eq!(reply_code(&out), None, "{}", String::from_utf8_lossy(&out));
    assert_eq!(out.iter().filter(|b| **b == b'\n').count(), 1);
    let got = got.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "{got:?}");
    let (req, _) = crate::ipc::parse_request_line(&got[0]).unwrap();
    let IpcRequest::Join { id: sent, name, .. } = req else {
        panic!("{req:?}")
    };
    assert_eq!((sent.as_str(), name.as_str()), (id, "pi-3"));

    // A forced command whose id is not one pastor makes passes nothing on.
    let mut out = Vec::new();
    bridge(
        &socket,
        "x;rm",
        format!("{}\n", join_line(id, "pi-3")).as_bytes(),
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(reply_code(&out).as_deref(), Some(INVALID));
}
