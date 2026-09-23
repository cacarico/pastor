//! Speaks the herdr socket protocol. Used by the transport tests and for running
//! pastor end to end without a real herdr.
//!
//! herdr serves one request per connection, so a fake that only ever speaks on
//! stdin/stdout cannot back a pastor machine any more: each request would spawn a
//! fresh process with an empty workspace list. It comes in the same two pieces the
//! real thing does:
//!
//!   fake-herdr --listen <socket>   the server, holding all the state
//!   fake-herdr --connect <socket>  a bridge, stdio <-> that socket, one per request
//!                                  (herdr's `remote-api-bridge`)
//!   fake-herdr                     one connection on stdin/stdout, and no shared
//!                                  state: enough for a single request
//!
//! So a flock machine is `command = ["fake-herdr", "--connect", "<socket>"]` with a
//! `--listen` process running beside it.
//!
//! Env, for the `--listen` server (the only mode that outlives a request):
//!   FAKE_HERDR_AUTO_DONE_MS=<n>  after `agent.prompt` flips an agent to
//!                                `working`, flip it back to `idle` n ms later
//!                                with `completion_seq` incremented.
//!   FAKE_HERDR_READY_MS=<n>      a started agent reports `unknown` and refuses
//!                                prompts for n ms, the way herdr does while a
//!                                managed agent is still launching.
use std::path::PathBuf;

use pastor::herdr::{AgentStatus, fake::FakeHerdr};
use tokio::io::AsyncWriteExt;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => stdio().await,
        ["--listen", path] => listen(PathBuf::from(path)).await,
        ["--connect", path] => bridge(PathBuf::from(path)).await,
        _ => {
            eprintln!(
                "usage: fake-herdr [--listen <socket> | --connect <socket>]\n\
                 with no arguments: serve one connection on stdin/stdout"
            );
            std::process::exit(2);
        }
    }
}

/// One connection on stdin/stdout: one request, one reply, exit. No auto-done
/// watcher — this process is gone before it could ever fire.
async fn stdio() {
    let fake = FakeHerdr::new();
    ready_after(&fake);
    fake.serve(Box::new(tokio::io::stdin()), Box::new(tokio::io::stdout()))
        .await;
}

/// The server: every accepted connection carries one request, as herdr's does.
async fn listen(path: PathBuf) {
    let fake = FakeHerdr::new();
    ready_after(&fake);
    auto_done(&fake);
    // A leftover socket from a previous run would make bind fail with EADDRINUSE.
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)
        .unwrap_or_else(|e| panic!("bind {}: {e}", path.display()));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let fake = fake.clone();
        tokio::spawn(async move {
            let (r, w) = stream.into_split();
            fake.serve(Box::new(r), Box::new(w)).await;
        });
    }
}

/// The bridge: copy stdin to the server and the server's replies to stdout, then
/// exit when the server closes the connection (which it does after one reply).
async fn bridge(path: PathBuf) {
    let stream = match tokio::net::UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("connect {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    let (mut from_server, mut to_server) = stream.into_split();
    let up = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut to_server).await;
    });
    let mut stdout = tokio::io::stdout();
    let _ = tokio::io::copy(&mut from_server, &mut stdout).await;
    let _ = stdout.flush().await;
    up.abort();
}

/// Makes started agents spend `FAKE_HERDR_READY_MS` launching, so a dispatch
/// has to wait for readiness the way it does against a real herdr.
fn ready_after(fake: &FakeHerdr) {
    if let Some(ms) = std::env::var("FAKE_HERDR_READY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        fake.set_ready_after(std::time::Duration::from_millis(ms));
    }
}

/// Watches for agents that have just been prompted and lets them finish on their
/// own after `FAKE_HERDR_AUTO_DONE_MS`, so an end-to-end run reaches `done`.
fn auto_done(fake: &FakeHerdr) {
    let Some(ms) = std::env::var("FAKE_HERDR_AUTO_DONE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    else {
        return;
    };
    let watcher = fake.clone();
    tokio::spawn(async move {
        let mut seen = std::collections::HashSet::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            for a in watcher.agents() {
                if a.agent_status == AgentStatus::Working
                    && seen.insert((a.pane_id.clone(), a.state_change_seq))
                {
                    let w = watcher.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                        w.set_status(
                            &a.pane_id,
                            AgentStatus::Idle,
                            Some(a.completion_seq.unwrap_or(0) + 1),
                        );
                    });
                }
            }
        }
    });
}
