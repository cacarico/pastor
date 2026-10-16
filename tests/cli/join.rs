//! `pastor join create|list|revoke` on the head, and `bridge --join <id>`,
//! the way a joining machine's one request reaches it.
use crate::helpers::*;

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// A user's own key in the head's authorized_keys, which no join may touch.
const FOREIGN: &str = "ssh-ed25519 AAAAFOREIGN me@laptop";

/// A head with no herdr machine: a `builders` flock, two pull machines
/// whose names a join may want, and authorized_keys and the sshd host key
/// in temp files.
struct Head {
    tmp: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
    keys: PathBuf,
    host_key: PathBuf,
    serve: std::process::Child,
}

impl Drop for Head {
    fn drop(&mut self) {
        let _ = self.serve.kill();
        let _ = self.serve.wait();
    }
}

impl Head {
    fn start(tick: &str) -> Head {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("c");
        let state = tmp.path().join("s");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("pastor.toml"),
            format!("tick = \"{tick}\"\nhead_address = \"me@head.example\"\n"),
        )
        .unwrap();
        std::fs::write(
            config.join("flock.toml"),
            "[[flock]]\nname = \"home\"\ndefault = true\n\n[[flock]]\nname = \"builders\"\n\n\
             [[machine]]\nname = \"taken\"\npull = true\n\n\
             [[machine]]\nname = \"taken-2\"\npull = true\n",
        )
        .unwrap();
        let keys = tmp.path().join("authorized_keys");
        std::fs::write(&keys, format!("{FOREIGN}\n")).unwrap();
        let host_key = tmp.path().join("ssh_host_ed25519_key.pub");
        std::fs::write(&host_key, "ssh-ed25519 AAAAHOSTKEY root@head\n").unwrap();
        let log = std::fs::File::create(tmp.path().join("serve.log")).unwrap();
        let mut serve = pastor();
        serve
            .args(["serve", "--foreground"])
            .stdout(Stdio::null())
            .stderr(log);
        let mut head = Head {
            config,
            state,
            keys,
            host_key,
            serve: Command::new("true").spawn().unwrap(),
            tmp,
        };
        head.env(&mut serve);
        head.serve = serve.spawn().unwrap();
        let deadline = Instant::now() + WAIT;
        loop {
            let out = head.bridge(&[], "{\"op\":\"ping\"}\n");
            if String::from_utf8_lossy(&out.stdout).contains("\"pong\"") {
                return head;
            }
            if head.serve.try_wait().unwrap().is_some() || Instant::now() >= deadline {
                panic!("head did not start:\n{}", head.log());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn env<'a>(&self, c: &'a mut Command) -> &'a mut Command {
        c.env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .env("PASTOR_AUTHORIZED_KEYS", &self.keys)
            .env("PASTOR_SSH_HOST_KEY", &self.host_key)
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.tmp.path().join("serve.log")).unwrap_or_default()
    }

    fn cmd(&self, args: &[&str]) -> std::process::Output {
        let mut c = pastor();
        self.env(&mut c).args(args).output().unwrap()
    }

    /// `pastor bridge <args>` with `input` on stdin, as sshd runs it.
    fn bridge(&self, args: &[&str], input: &str) -> std::process::Output {
        let mut c = pastor();
        let mut child = self
            .env(&mut c)
            .arg("bridge")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// `join create <args> --json`: the token's id and the token.
    fn create(&self, args: &[&str]) -> (String, String) {
        let mut all = vec!["join", "create", "--json"];
        all.extend_from_slice(args);
        let out = self.cmd(&all);
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stderr),
            self.log()
        );
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        (
            v["id"].as_str().unwrap().to_string(),
            v["token"].as_str().unwrap().to_string(),
        )
    }

    /// The joining machine's one request, through `bridge --join <id>`:
    /// the reply, parsed.
    fn join(&self, id: &str, name: &str) -> serde_json::Value {
        let line = serde_json::json!({
            "op": "join",
            "id": "0000000000000000",
            "name": name,
            "machine_key": format!("ssh-ed25519 AAAAMACHINE{} m", name.len()),
            "agent_key": "ssh-ed25519 AAAAAGENT a",
            "host_key": "ssh-ed25519 AAAAMACHINEHOST",
            "facts": {"os": "linux"},
        });
        let out = self.bridge(&["--join", id], &format!("{line}\n"));
        reply(&out)
    }

    fn keys_text(&self) -> String {
        std::fs::read_to_string(&self.keys).unwrap()
    }

    fn flock(&self) -> toml::Value {
        toml::from_str(&std::fs::read_to_string(self.config.join("flock.toml")).unwrap()).unwrap()
    }
}

/// The one reply line a bridge printed.
fn reply(out: &std::process::Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_else(|| panic!("no reply; stderr: {}", String::from_utf8_lossy(&out.stderr)));
    serde_json::from_str(line).unwrap()
}

fn error_code(v: &serde_json::Value) -> &str {
    assert_eq!(v["kind"], "error", "{v}");
    v["data"]["code"].as_str().unwrap()
}

fn machine<'a>(flock: &'a toml::Value, name: &str) -> Option<&'a toml::Value> {
    flock["machine"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"].as_str() == Some(name))
}

fn has_join_line(keys: &str, id: &str) -> bool {
    keys.lines()
        .any(|l| l.ends_with(&format!(" pastor:join:{id}")))
}

#[test]
fn create_writes_a_locked_join_line_and_a_pjt_token() {
    let h = Head::start("1s");
    let (id, token) = h.create(&["--flock", "builders"]);
    assert_eq!(id.len(), 16);
    assert!(
        id.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    assert!(token.starts_with("pjt_"), "{token}");
    let keys = h.keys_text();
    let line = keys
        .lines()
        .find(|l| l.ends_with(&format!(" pastor:join:{id}")))
        .unwrap_or_else(|| panic!("no join line in {keys}"));
    assert!(line.starts_with("restrict,command=\""), "{line}");
    assert!(line.contains(&format!(" bridge --join {id}\",")), "{line}");
    assert!(keys.contains(FOREIGN));
    // The head keeps no copy of the private half.
    let leftovers: Vec<_> = walk(&h.state)
        .into_iter()
        .filter(|p| std::fs::read_to_string(p).is_ok_and(|t| t.contains("PRIVATE KEY")))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let list = h.cmd(&["join", "list", "--json"]);
    assert!(list.status.success());
    let rows: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(rows[0]["id"], id.as_str());
    assert_eq!(rows[0]["flock"], "builders");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn join_adds_pull_machine() {
    let h = Head::start("1s");
    let (id, _) = h.create(&["--flock", "builders", "--tag", "gpu", "--tag", "big"]);
    let r = h.join(&id, "gpu-3");
    assert_eq!(r["kind"], "joined", "{r}\n{}", h.log());
    assert_eq!(r["data"]["name"], "gpu-3");
    let flock = h.flock();
    let m = machine(&flock, "gpu-3").expect("gpu-3 in flock.toml");
    assert_eq!(m["pull"].as_bool(), Some(true));
    assert_eq!(m["flock"].as_str(), Some("builders"));
    let tags: Vec<&str> = m["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert_eq!(tags, ["gpu", "big"]);
    let events = std::fs::read_to_string(h.state.join("events.jsonl")).unwrap_or_default();
    let deadline = Instant::now() + WAIT;
    let mut events = events;
    while !events.contains("machine.joined") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        events = std::fs::read_to_string(h.state.join("events.jsonl")).unwrap_or_default();
    }
    assert!(events.contains("machine.joined"), "{events}");
}

#[test]
fn token_line_removed_after_use() {
    let h = Head::start("1s");
    let (id, _) = h.create(&[]);
    assert!(has_join_line(&h.keys_text(), &id));
    let r = h.join(&id, "gpu-3");
    assert_eq!(r["kind"], "joined", "{r}");
    let keys = h.keys_text();
    assert!(!has_join_line(&keys, &id), "{keys}");
    assert!(keys.contains(FOREIGN));
    let machine = keys
        .lines()
        .find(|l| l.ends_with(" pastor:machine:gpu-3"))
        .unwrap_or_else(|| panic!("no machine line in {keys}"));
    assert!(
        machine.contains(" bridge --shepherd --machine gpu-3\","),
        "{machine}"
    );
    let agent = keys
        .lines()
        .find(|l| l.ends_with(" pastor:agent:gpu-3"))
        .unwrap_or_else(|| panic!("no agent line in {keys}"));
    assert!(
        agent.contains(" bridge --agent --machine gpu-3\","),
        "{agent}"
    );
}

#[test]
fn second_join_refused() {
    let h = Head::start("1s");
    let (id, _) = h.create(&[]);
    assert_eq!(h.join(&id, "gpu-3")["kind"], "joined");
    assert_eq!(error_code(&h.join(&id, "gpu-4")), "join_token_used");
    assert!(machine(&h.flock(), "gpu-4").is_none());
}

#[test]
fn expired_swept() {
    let h = Head::start("1s");
    let (id, _) = h.create(&["--ttl", "1s"]);
    assert!(has_join_line(&h.keys_text(), &id));
    let deadline = Instant::now() + WAIT;
    while has_join_line(&h.keys_text(), &id) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!has_join_line(&h.keys_text(), &id), "{}", h.log());
    assert!(h.keys_text().contains(FOREIGN));
    let mut events = String::new();
    while !events.contains("machine.join_token_expired") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        events = std::fs::read_to_string(h.state.join("events.jsonl")).unwrap_or_default();
    }
    assert!(events.contains("machine.join_token_expired"), "{events}");
    let list = h.cmd(&["join", "list", "--json"]);
    let rows: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(rows, serde_json::json!([]));
}

#[test]
fn expired_join_refused() {
    // No pass sweeps it first: the head's tick is an hour.
    let h = Head::start("1h");
    let (id, _) = h.create(&["--ttl", "1s"]);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(error_code(&h.join(&id, "gpu-3")), "join_token_expired");
    assert!(machine(&h.flock(), "gpu-3").is_none());
}

#[test]
fn name_taken_refused() {
    let h = Head::start("1s");
    let (id, _) = h.create(&[]);
    assert_eq!(error_code(&h.join(&id, "taken")), "machine_name_taken");
    let flock = h.flock();
    let m = machine(&flock, "taken").unwrap();
    assert!(m.get("tags").is_none(), "{m}");
    assert!(!h.keys_text().contains("pastor:machine:taken"));
    // A name no marker or forced command can carry is refused as such.
    assert_eq!(error_code(&h.join(&id, "a b")), "invalid_machine");
}

#[test]
fn replace_allows_same_name_only() {
    let h = Head::start("1s");
    let (id, _) = h.create(&["--replace", "taken", "--tag", "new"]);
    assert_eq!(error_code(&h.join(&id, "taken-2")), "machine_name_taken");
    let r = h.join(&id, "taken");
    assert_eq!(r["kind"], "joined", "{r}");
    let flock = h.flock();
    let m = machine(&flock, "taken").unwrap();
    assert_eq!(m["pull"].as_bool(), Some(true));
    assert_eq!(m["tags"][0].as_str(), Some("new"));
    assert!(machine(&flock, "taken-2").is_some());
    let n = flock["machine"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["name"].as_str() == Some("taken"))
        .count();
    assert_eq!(n, 1);
}

#[test]
fn bridge_join_refuses_other_requests() {
    let h = Head::start("1s");
    let (id, _) = h.create(&[]);
    let out = h.bridge(&["--join", &id], "{\"op\":\"ping\"}\n");
    assert_eq!(error_code(&reply(&out)), "join_token_invalid");
    let out = h.bridge(&["--join", &id], "not json\n");
    assert_eq!(error_code(&reply(&out)), "join_token_invalid");
    // Exactly one line: a join after a refused first line never reaches the head.
    let join = serde_json::json!({
        "op": "join", "id": id, "name": "gpu-3", "machine_key": "ssh-ed25519 AAAAM",
        "agent_key": "ssh-ed25519 AAAAA", "host_key": "ssh-ed25519 AAAAH", "facts": {},
    });
    let out = h.bridge(&["--join", &id], &format!("{{\"op\":\"ping\"}}\n{join}\n"));
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 1);
    assert!(machine(&h.flock(), "gpu-3").is_none());
    // An id that is not a join id gets nothing either.
    let out = h.bridge(&["--join", "../x"], &format!("{join}\n"));
    assert_eq!(error_code(&reply(&out)), "join_token_invalid");
    // The token still works: nothing above used it.
    assert_eq!(h.join(&id, "gpu-3")["kind"], "joined");
}

#[test]
fn revoke_removes_the_line_and_the_token() {
    let h = Head::start("1h");
    let (id, _) = h.create(&[]);
    let out = h.cmd(&["join", "revoke", &id]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!has_join_line(&h.keys_text(), &id));
    assert!(h.keys_text().contains(FOREIGN));
    let out = h.cmd(&["join", "revoke", &id]);
    assert_eq!(out.status.code(), Some(1));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["code"], "join_token_invalid");
    assert_eq!(error_code(&h.join(&id, "gpu-3")), "join_token_invalid");
}

#[test]
fn create_with_an_unknown_flock_is_refused() {
    let h = Head::start("1h");
    let out = h.cmd(&["join", "create", "--flock", "nope"]);
    assert_eq!(out.status.code(), Some(1));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["code"], "unknown_flock", "{err}");
    assert_eq!(h.keys_text(), format!("{FOREIGN}\n"));
}
