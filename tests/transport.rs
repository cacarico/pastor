//! The `command` transport against the `fake-herdr` binary: a process per
//! connection, one request per connection.
use pastor::herdr::transport::*;
use pastor::herdr::{CallError, ConnectorExt, HerdrError};

fn fake_herdr() -> Endpoint {
    Endpoint::Command {
        argv: vec![env!("CARGO_BIN_EXE_fake-herdr").to_string()],
    }
}

#[tokio::test]
async fn command_transport_talks_to_fake_herdr() {
    let ep = fake_herdr();
    assert_eq!(ep.ping().await.unwrap().protocol, 22);
    // A second request gets a second process, and works just the same: nothing
    // is held between requests.
    assert!(ep.agent_list().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_connection_carries_one_request() {
    // Straight at `Connection`: one call consumes it, so a second request needs
    // a second connect — and here, a second bridge process.
    let ep = fake_herdr();
    let conn = connect(&ep).await.unwrap();
    conn.call("ping", serde_json::json!({})).await.unwrap();

    let conn = connect(&ep).await.unwrap();
    conn.call("agent.list", serde_json::json!({}))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_bridge_that_dies_names_the_command_and_its_stderr() {
    let ep = Endpoint::Command {
        argv: vec![
            "sh".into(),
            "-c".into(),
            "echo herdr: command not found >&2; exit 127".into(),
        ],
    };
    let err = ep.ping().await.unwrap_err();
    let message = err.to_string();
    assert!(err.is_transport(), "{message}");
    assert!(message.contains("command not found"), "{message}");
    assert!(message.contains("127"), "{message}");
    let CallError::Herdr(HerdrError::Transport(_)) = err else {
        panic!("expected a transport error, got {err:?}");
    };
}

/// The events subscription lives as long as the machine does, and ssh keeps
/// talking on stderr the whole time (host key notices, keepalive complaints).
/// If nothing drains that pipe the child blocks on its next write and the event
/// stream silently stops, so the bridge's stderr is drained for its whole life.
#[tokio::test]
async fn a_long_lived_connection_keeps_its_bridge_stderr_drained() {
    let ep = Endpoint::Command {
        argv: vec![
            "sh".into(),
            "-c".into(),
            // Ack the subscription, write far more to stderr than a pipe buffer
            // holds (64 KiB on Linux), then send an event and stay alive.
            "read line; \
             printf '{\"id\":\"p1\",\"result\":{\"type\":\"subscription_started\"}}\\n'; \
             head -c 200000 /dev/zero | tr '\\0' 'x' >&2; \
             printf '{\"event\":\"pane_closed\",\"data\":{\"type\":\"pane_closed\",\"pane_id\":\"w1:p1\",\"workspace_id\":\"w1\"}}\\n'; \
             sleep 30"
                .into(),
        ],
    };
    let mut stream = ep
        .subscribe(vec![pastor::herdr::subscription_lifecycle("pane.closed")])
        .await
        .expect("subscribe");
    let ev = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
        .await
        .expect("the event arrived: a blocked stderr write would swallow it")
        .expect("event");
    assert!(ev.is_pane_closed(), "{ev:?}");
}

/// An error reply is herdr answering, not the bridge dying: it keeps its code,
/// and the machine stays healthy. Before this, `diagnose` rewrote every error
/// that came over a bridge into a transport error, which is how a live dispatch
/// turned `agent_not_ready` into `machine.lost`.
#[tokio::test]
async fn an_api_error_over_the_bridge_keeps_its_code() {
    let ep = fake_herdr();
    let err = ep
        .agent_start("t-1", "claude", "w9:p9", &[])
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("pane_not_found"), "{err}");
    assert!(!err.is_transport(), "{err}");
}

/// Same for the error replies that end an event stream.
#[tokio::test]
async fn an_events_lost_error_over_the_bridge_keeps_its_code() {
    let ep = Endpoint::Command {
        argv: vec![
            "sh".into(),
            "-c".into(),
            "read line; \
             printf '{\"id\":\"p1\",\"result\":{\"type\":\"subscription_started\"}}\\n'; \
             printf '{\"id\":\"p1\",\"error\":{\"code\":\"events_lost\",\"message\":\"behind\"}}\\n'"
                .into(),
        ],
    };
    let mut stream = ep
        .subscribe(vec![pastor::herdr::subscription_lifecycle("pane.closed")])
        .await
        .expect("subscribe");
    let err = stream.next().await.unwrap_err();
    assert_eq!(err.code(), Some("events_lost"), "{err}");
}
