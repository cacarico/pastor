//! Opt-in integration test against a real, running herdr server.
//!
//! Every other test in this crate talks to `FakeHerdr`, an in-process stand-in that
//! speaks pastor's understanding of the herdr socket protocol but has never had to
//! prove that understanding against the real thing. This test is the one place that
//! does: it connects to a `herdr` session a developer started by hand, pings it,
//! checks the protocol version, creates a workspace and lists agents.
//!
//! It is `#[ignore]`d because herdr 0.9+ is not installed in every environment this
//! crate is built and tested in (this machine included), and because it depends on
//! a herdr session already running outside the test. To run it:
//!
//!   1. Start a herdr session to point it at, e.g. `herdr --session pastor-test`.
//!   2. `PASTOR_REAL_HERDR_SESSION=pastor-test cargo test --test real_herdr -- --ignored`
//!
//! With the env var unset the test skips itself with a clear message on stderr
//! instead of failing, so the command above is still safe to run without herdr
//! installed or a session up — the case on this machine today.
//!
//! pastor's herdr client has no workspace- or pane-close call (see
//! `src/herdr/client.rs`), and pastor itself never closes panes on its own by
//! design (see `docs/superpowers/specs/2026-09-23-pastor-design.md`, "Dispatch and
//! tasks"), so the workspace this test creates is left in place, exactly as a real
//! dispatch would leave it.

use pastor::MIN_HERDR_PROTOCOL;
use pastor::herdr::{ConnectorExt, Endpoint};

#[tokio::test]
#[ignore = "needs a real herdr server; set PASTOR_REAL_HERDR_SESSION and run with --ignored"]
async fn talks_to_a_real_herdr_session() {
    let Ok(session) = std::env::var("PASTOR_REAL_HERDR_SESSION") else {
        eprintln!(
            "skipping: set PASTOR_REAL_HERDR_SESSION to a herdr session name \
             (started with `herdr --session <name>`) to run this test"
        );
        return;
    };

    // Every call below opens its own connection, because that is all a real herdr
    // connection carries: one request, one reply, close. Two calls in a row are
    // the point of this test — the defect this shape fixes looked exactly like a
    // ping that worked followed by a second request that got EOF.
    let endpoint = Endpoint::Local { session };

    let pong = endpoint.ping().await.expect("ping the real herdr");
    assert!(
        pong.protocol >= MIN_HERDR_PROTOCOL,
        "herdr protocol {} is older than the {} pastor requires; update herdr",
        pong.protocol,
        MIN_HERDR_PROTOCOL
    );

    // The second request, on a second connection: before the one-request-per-
    // connection fix this is where a real herdr returned EOF.
    endpoint
        .agent_list()
        .await
        .expect("list agents on a second connection");

    let label = format!("pastor-real-herdr-test-{}", std::process::id());
    let created = endpoint
        .workspace_create(None, &label)
        .await
        .expect("create a workspace on the real herdr");
    assert!(!created.workspace.workspace_id.is_empty());
    assert!(!created.root_pane.pane_id.is_empty());

    // Round-trips agent.list once more, now with the workspace just created. No
    // agent was started in it, so this only proves the call itself works against
    // a real server, not any particular agent state.
    endpoint
        .agent_list()
        .await
        .expect("list agents after creating a workspace");
}
