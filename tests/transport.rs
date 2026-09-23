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
