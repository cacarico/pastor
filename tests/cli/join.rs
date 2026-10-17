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
        // Keys of its own per name: a key already authorized is refused.
        let hex: String = name.bytes().map(|b| format!("{b:02x}")).collect();
        self.join_keys(
            id,
            name,
            &format!("ssh-ed25519 AAAAMACHINE{hex} m"),
            &format!("ssh-ed25519 AAAAAGENT{hex} a"),
        )
    }

    /// `join` with the machine and agent keys given.
    fn join_keys(
        &self,
        id: &str,
        name: &str,
        machine_key: &str,
        agent_key: &str,
    ) -> serde_json::Value {
        let line = serde_json::json!({
            "op": "join",
            "id": "0000000000000000",
            "name": name,
            "machine_key": machine_key,
            "agent_key": agent_key,
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
        machine.contains(" bridge --shepherd --machine gpu-3 --gen 1\","),
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
    assert_eq!(error_code(&h.join(&id, "a b")), "machine_name_invalid");
}

/// A shepherd bridge as sshd runs it, with its stdin and stdout kept open:
/// one connection that sends a line, reads its reply, and sends more later.
struct Live {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl Live {
    fn start(h: &Head, args: &[&str]) -> Live {
        let mut c = pastor();
        let mut child = h
            .env(&mut c)
            .arg("bridge")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        Live {
            child,
            stdin,
            stdout,
        }
    }

    /// One request and its reply, parsed; `Null` once the bridge has hung up.
    fn ask(&mut self, line: &str) -> serde_json::Value {
        use std::io::BufRead;
        let _ = writeln!(self.stdin, "{line}");
        let _ = self.stdin.flush();
        let mut reply = String::new();
        self.stdout.read_line(&mut reply).unwrap();
        serde_json::from_str(reply.trim()).unwrap_or(serde_json::Value::Null)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// OpenSSH fixes a connection's forced command when it authenticates, so a
/// shepherd connected before a `--replace` keeps the old `--gen`: the head
/// refuses its next request `machine_revoked` and the bridge hangs up,
/// while a bridge under the new generation is served. A removed machine's
/// bridge is refused the same way.
#[test]
fn a_replace_cuts_off_a_shepherd_connection_that_already_authenticated() {
    let h = Head::start("1h");
    let (id, _) = h.create(&[]);
    assert_eq!(h.join(&id, "gpu-3")["kind"], "joined");
    let machine_line = |h: &Head| {
        h.keys_text()
            .lines()
            .find(|l| l.ends_with(" pastor:machine:gpu-3"))
            .map(str::to_string)
            .unwrap()
    };
    assert!(
        machine_line(&h).contains("--gen 1\""),
        "{}",
        machine_line(&h)
    );
    let mut old = Live::start(&h, &["--shepherd", "--machine", "gpu-3", "--gen", "1"]);
    let ping = "{\"op\":\"ping\"}";
    assert_eq!(old.ask(ping)["kind"], "pong");
    // A gen-less line, as one made by hand, no longer speaks for a joined
    // machine.
    let out = h.bridge(&["--shepherd", "--machine", "gpu-3"], &format!("{ping}\n"));
    assert_eq!(error_code(&reply(&out)), "machine_revoked");

    let (id, _) = h.create(&["--replace", "gpu-3"]);
    // Keys of its own: the old ones are still on the old lines.
    let r = h.join_keys(
        &id,
        "gpu-3",
        "ssh-ed25519 AAAANEWM m",
        "ssh-ed25519 AAAANEWA a",
    );
    assert_eq!(r["kind"], "joined", "{r}");
    assert!(
        machine_line(&h).contains("--gen 2\""),
        "{}",
        machine_line(&h)
    );
    // The same live connection, past the replace.
    assert_eq!(error_code(&old.ask(ping)), "machine_revoked");
    // And nothing more is read on it.
    assert_eq!(old.ask(ping), serde_json::Value::Null);
    let mut new = Live::start(&h, &["--shepherd", "--machine", "gpu-3", "--gen", "2"]);
    assert_eq!(new.ask(ping)["kind"], "pong");

    let out = h.cmd(&["machine", "remove", "gpu-3"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(error_code(&new.ask(ping)), "machine_revoked");
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

#[test]
fn a_name_that_reads_as_an_option_is_refused() {
    let h = Head::start("1h");
    let (id, _) = h.create(&[]);
    assert_eq!(error_code(&h.join(&id, "-oops")), "machine_name_invalid");
    assert!(machine(&h.flock(), "-oops").is_none());
    assert_eq!(h.join(&id, "gpu-3")["kind"], "joined");
}

/// A name with a shell metacharacter would reach the shell sshd runs the
/// forced command with: refused before the token is used or anything is
/// written, so the token still joins a good name after.
#[test]
fn a_name_with_a_shell_metacharacter_is_refused_before_the_token_is_used() {
    let h = Head::start("1h");
    let (id, _) = h.create(&[]);
    let keys = h.keys_text();
    let flock = h.flock();
    for bad in [
        "a;id", "a`id`", "$(id)", "a$(id)", "a|b", "a'b", "a\\b", ".hidden", "_x",
    ] {
        assert_eq!(
            error_code(&h.join(&id, bad)),
            "machine_name_invalid",
            "{bad}"
        );
        assert_eq!(h.keys_text(), keys, "{bad}");
        assert_eq!(h.flock(), flock, "{bad}");
    }
    let list = h.cmd(&["join", "list", "--json"]);
    let rows: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert!(rows[0]["used_at"].is_null(), "{rows}");
    assert_eq!(h.join(&id, "gpu-3.lan_2")["kind"], "joined");
}

#[test]
fn keys_of_an_unknown_type_or_already_authorized_are_refused() {
    let h = Head::start("1h");
    let (id, _) = h.create(&[]);
    let good = "ssh-ed25519 AAAAGOODMACHINE m";
    let agent = "ssh-ed25519 AAAAGOODAGENT a";
    for (machine_key, agent_key, code) in [
        ("ssh-dss AAAADSS m", agent, "invalid_key"),
        (good, "x-nope AAAAX a", "invalid_key"),
        // The user's own key, already in authorized_keys.
        (FOREIGN, agent, "key_in_use"),
        (good, FOREIGN, "key_in_use"),
        // One key for both: sshd would only ever match the first line.
        (good, good, "invalid_key"),
    ] {
        let r = h.join_keys(&id, "gpu-3", machine_key, agent_key);
        assert_eq!(error_code(&r), code, "{machine_key} / {agent_key}: {r}");
        assert!(machine(&h.flock(), "gpu-3").is_none());
    }
    assert!(!h.keys_text().contains("pastor:machine:gpu-3"));
    // Refusals leave the token good.
    let r = h.join_keys(&id, "gpu-3", "ecdsa-sha2-nistp256 AAAAEC m", agent);
    assert_eq!(r["kind"], "joined", "{r}");
}

#[test]
fn two_redeems_racing_add_one_machine() {
    let h = Head::start("1h");
    let (id, _) = h.create(&[]);
    let gate = std::sync::Barrier::new(2);
    let (a, b) = std::thread::scope(|s| {
        let a = s.spawn(|| {
            gate.wait();
            h.join(&id, "gpu-3")
        });
        let b = s.spawn(|| {
            gate.wait();
            h.join(&id, "gpu-4")
        });
        (a.join().unwrap(), b.join().unwrap())
    });
    let joined: Vec<_> = [&a, &b]
        .into_iter()
        .filter(|r| r["kind"] == "joined")
        .collect();
    assert_eq!(joined.len(), 1, "{a}\n{b}");
    let lost = if a["kind"] == "joined" { &b } else { &a };
    assert_eq!(error_code(lost), "join_token_used", "{lost}");
    let flock = h.flock();
    let added = ["gpu-3", "gpu-4"]
        .iter()
        .filter(|n| machine(&flock, n).is_some())
        .count();
    assert_eq!(added, 1);
    assert!(!has_join_line(&h.keys_text(), &id));
}

/// A release of one fake pastor for x86_64 Linux in `dir`, as install.sh
/// fetches it: the tarball and SHA256SUMS. The fake prints `pastor 1.0.0`
/// for `--version`, else each argument as `[arg]`, then what it was given
/// in PASTOR_JOIN_TOKEN and, with `--token-stdin`, on stdin, upper-cased
/// (so a test can tell a token echoed by the terminal from the one the
/// fake got).
fn fake_release(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let release = dir.join("release");
    let name = "pastor-1.0.0-x86_64-unknown-linux-musl";
    std::fs::create_dir_all(release.join(name)).unwrap();
    let fake = release.join(name).join("pastor");
    std::fs::write(
        &fake,
        "#!/bin/sh\n\
         if [ \"$1\" = --version ]; then echo 'pastor 1.0.0'; exit 0; fi\n\
         stdin=no\n\
         for a in \"$@\"; do printf '[%s]\\n' \"$a\"; [ \"$a\" = --token-stdin ] && stdin=yes; done\n\
         printf 'env=[%s]\\n' \"${PASTOR_JOIN_TOKEN-unset}\"\n\
         if [ $stdin = yes ]; then printf 'stdin=[%s]\\n' \"$(tr a-z A-Z)\"; fi\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let tarball = format!("{name}.tar.gz");
    let ok = std::process::Command::new("tar")
        .arg("-czf")
        .arg(release.join(&tarball))
        .arg("-C")
        .arg(&release)
        .arg(name)
        .status()
        .unwrap();
    assert!(ok.success());
    let sums = std::process::Command::new("sha256sum")
        .arg(&tarball)
        .current_dir(&release)
        .output()
        .unwrap();
    std::fs::write(release.join("SHA256SUMS"), &sums.stdout).unwrap();
    release
}

/// `program` with install.sh's environment: the fake release in `dir`,
/// installed under `dir`/bin, and no gh login (the attestation check is
/// not these tests').
fn install_sh_env(program: &str, dir: &Path, release: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.current_dir(dir)
        .env("PASTOR_VERSION", "1.0.0")
        .env(
            "PASTOR_DOWNLOAD_URL",
            format!("file://{}", release.display()),
        )
        .env("PASTOR_INSTALL_DIR", dir.join("bin"))
        .env("GH_CONFIG_DIR", dir.join("gh"))
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("PASTOR_JOIN_TOKEN");
    cmd
}

fn install_sh() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh")
}

/// The lines the fake pastor printed: its arguments, env and stdin.
fn fake_said(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| l.starts_with('[') || l.starts_with("env=") || l.starts_with("stdin="))
        .map(str::to_string)
        .collect()
}

/// `install.sh join …` installs pastor, then runs its `join` with the rest
/// of the arguments exactly as given: a space stays inside its argument,
/// and nothing is evaluated by the shell on the way.
#[test]
fn install_sh_join_passes_its_args_through() {
    if !cfg!(target_os = "linux") || std::env::consts::ARCH != "x86_64" {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let release = fake_release(tmp.path());
    let out = install_sh_env("sh", tmp.path(), &release)
        .arg(install_sh())
        .args(["join", "--name", "a b", "$(touch pwned)", "--token-stdin"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        fake_said(&stdout),
        [
            "[join]",
            "[--name]",
            "[a b]",
            "[$(touch pwned)]",
            "[--token-stdin]",
            "env=[unset]",
            "stdin=[]",
        ],
        "{stdout}"
    );
    assert!(!tmp.path().join("pwned").exists());
    assert!(!stdout.contains("next:"), "{stdout}");

    // No token and no terminal to ask on (a new session, so no /dev/tty):
    // `pastor join` gets the arguments as they are and says what is
    // missing itself.
    let out = install_sh_env("setsid", tmp.path(), &release)
        .args(["-w", "sh"])
        .arg(install_sh())
        .args(["join", "--name", "gpu-3"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        fake_said(&stdout),
        ["[join]", "[--name]", "[gpu-3]", "env=[unset]"],
        "{stdout}"
    );
}

/// `curl … | sudo sh -s -- join` with no token anywhere asks for it on the
/// terminal with echo off, and hands it to `pastor join --token-stdin` on a
/// pipe: never in an argument or the environment, where `ps` and
/// /proc/<pid>/environ show it. `script` gives the run a real terminal.
#[test]
fn install_sh_join_asks_for_the_token_on_the_terminal() {
    use std::io::{Read, Write};
    if !cfg!(target_os = "linux") || std::env::consts::ARCH != "x86_64" {
        return;
    }
    if std::process::Command::new("script")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("no util-linux script here to give install.sh a terminal; skipped");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let release = fake_release(tmp.path());
    let token = "pjt_secret-token";
    let mut child = install_sh_env("script", tmp.path(), &release)
        .args(["-qec"])
        .arg(format!("sh {} join --name gpu-3", install_sh().display()))
        .arg("/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 {
                break;
            }
            let _ = tx.send(buf[..n].to_vec());
        }
    });
    let mut seen = Vec::new();
    let deadline = Instant::now() + WAIT;
    // Typed only once the prompt shows, so echo is already off.
    while !String::from_utf8_lossy(&seen).contains("join token: ") {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(chunk) => seen.extend(chunk),
            Err(_) => {
                let _ = child.kill();
                panic!(
                    "no prompt for the token: {}",
                    String::from_utf8_lossy(&seen)
                );
            }
        }
    }
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(format!("{token}\n").as_bytes()).unwrap();
    stdin.flush().unwrap();
    while let Ok(chunk) = rx.recv_timeout(WAIT) {
        seen.extend(chunk);
    }
    let status = child.wait().unwrap();
    drop(stdin);
    let stdout = String::from_utf8_lossy(&seen);
    assert!(status.success(), "{status:?}: {stdout}");
    assert_eq!(
        fake_said(&stdout),
        [
            "[join]",
            "[--token-stdin]",
            "[--name]",
            "[gpu-3]",
            "env=[unset]",
            &format!("stdin=[{}]", token.to_uppercase()),
        ],
        "{stdout}"
    );
    assert!(
        !stdout.contains(token),
        "the terminal echoed the token: {stdout}"
    );
}

/// `pastor join` takes PASTOR_JOIN_TOKEN out of its environment before it
/// runs anything, so no child (the bootstrap's commands, ssh) has it in
/// /proc/<pid>/environ; and a join killed while its ssh runs removes the
/// token's key dir on the way out.
#[test]
fn join_hides_the_token_env_and_cleans_up_when_killed() {
    use std::os::unix::fs::PermissionsExt;
    if !Path::new("/etc/ssh/ssh_host_ed25519_key.pub").exists() {
        eprintln!("no sshd host key here for the join to send; skipped");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (config, state, data, bin) = (
        tmp.path().join("c"),
        tmp.path().join("s"),
        tmp.path().join("d"),
        tmp.path().join("bin"),
    );
    std::fs::create_dir_all(&bin).unwrap();
    let exec = |name: &str, text: String| {
        let p = bin.join(name);
        std::fs::write(&p, text).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    let real_uname = String::from_utf8(
        std::process::Command::new("sh")
            .args(["-c", "command -v uname"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let t = tmp.path().display();
    exec(
        "uname",
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"${{PASTOR_JOIN_TOKEN-unset}}\" >> {t}/uname.env\nexec {} \"$@\"\n",
            real_uname.trim()
        ),
    );
    exec(
        "ssh",
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"${{PASTOR_JOIN_TOKEN-unset}}\" > {t}/ssh.env\necho $$ > {t}/ssh.pid\nexec sleep 60\n"
        ),
    );
    let token = pastor::join::Token {
        head: "me@head.invalid".into(),
        head_host_key: "head.invalid ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHEAD".into(),
        key: "-----BEGIN OPENSSH PRIVATE KEY-----\nSECRET\n-----END OPENSSH PRIVATE KEY-----\n"
            .into(),
    }
    .encode();
    let mut path = std::ffi::OsString::from(&bin);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    let mut child = pastor()
        .args(["join", "--name", "gpu-3", "--skip-bootstrap"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .env("PASTOR_DATA_DIR", &data)
        .env("PATH", &path)
        .env("PASTOR_JOIN_TOKEN", &token)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid_file = tmp.path().join("ssh.pid");
    let deadline = Instant::now() + WAIT;
    while !pid_file.exists() {
        if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the join never ran ssh: {:?}", child.wait_with_output());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(200));
    let key_dirs = || -> Vec<String> {
        std::fs::read_dir(&state)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("join-"))
            .collect()
    };
    assert_eq!(key_dirs().len(), 1, "the key dir while ssh runs");
    let ok = std::process::Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(ok.success());
    let deadline = Instant::now() + WAIT;
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the join did not end on SIGTERM");
        std::thread::sleep(Duration::from_millis(50));
    }
    let ssh_pid = std::fs::read_to_string(&pid_file).unwrap();
    let _ = std::process::Command::new("kill")
        .arg(ssh_pid.trim())
        .status();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(key_dirs().is_empty(), "{:?}", key_dirs());
    let ssh_env = std::fs::read_to_string(tmp.path().join("ssh.env")).unwrap();
    assert_eq!(ssh_env, "unset\n");
    let uname_env = std::fs::read_to_string(tmp.path().join("uname.env")).unwrap();
    assert!(!uname_env.is_empty());
    assert!(uname_env.lines().all(|l| l == "unset"), "{uname_env}");
}
