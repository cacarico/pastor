//! The `inbox` row of `pastor machine check`: whether a Claude session on a
//! machine takes a message through its inbox socket, which is how pastor
//! hands it a task's prompt. Nothing else says so before a task relies on
//! it; the typed fallback hides a socket that never worked.
//!
//! For each Claude agent a task can get on the machine, a short-lived
//! session is started as a task starts one (a workspace with the agent's
//! env, `agent.start` with a session id of its own) and sent one message
//! with the machine's `pastor inbox send`. A session needs a moment to
//! open its socket, so a send that finds none is tried again until the
//! deadline. Pass is the socket taking the message; every other answer
//! fails with the reason. The session's pane is closed on every path, the
//! timeout's too. The message asks for nothing, but a session that takes it
//! may start a turn before the pane closes.
//!
//! The row is skipped for an agent whose `delivery` is `type`, one that is
//! signed out (its login row says so), and a machine that cannot send
//! through the socket (a `command` machine, or no pastor there that has
//! `inbox send`), and one where no pane can be opened (herdr is not
//! reachable, which is not what this row is about).

use std::time::Duration;

use crate::config::{Agents, Delivery};
use crate::herdr::client::ConnectorExt;
use crate::herdr::transport::Connector;
use crate::provision::run::{AgentEnv, Row, Status};

/// The id of the row.
pub const INBOX_ROW: &str = "inbox";

/// What the session is sent.
const TEXT: &str =
    "pastor machine check: this is a test of the inbox socket. Nothing is asked of you.";

/// The name of the agent the check starts; herdr names agents it is given.
const AGENT_NAME: &str = "pastor-inbox-check";

/// How long a session has to take the message, and how often the send is
/// tried again while it has no socket.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub total: Duration,
    pub retry: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            total: Duration::from_secs(45),
            retry: Duration::from_secs(2),
        }
    }
}

/// One `inbox` row for each Claude agent in `agents` that is not skipped
/// before a session starts. `rows` are the rows the check has so far, whose
/// failed login rows keep their agents out.
pub async fn rows(
    conn: &dyn Connector,
    machine: &str,
    config: &Agents,
    agents: &[AgentEnv],
    rows: &[Row],
    timing: Timing,
) -> Vec<Row> {
    let mut out = Vec::new();
    for agent in agents.iter().filter(|a| a.kind == "claude") {
        let row = |status, first_line: &str, reason: Option<String>| Row {
            id: INBOX_ROW.into(),
            agent: Some(agent.name.clone()),
            status,
            first_line: first_line.into(),
            reason,
        };
        if config.delivery(&agent.name) != Delivery::Socket {
            out.push(row(
                Status::Skipped,
                "",
                Some(format!(
                    "agent {} delivers prompts by typing (delivery = \"type\")",
                    agent.name
                )),
            ));
            continue;
        }
        if rows.iter().any(|r| {
            r.id == "claude-login"
                && r.agent.as_deref() == Some(&agent.name)
                && r.status != Status::Pass
        }) {
            out.push(row(
                Status::Skipped,
                "",
                Some("claude is not signed in; see its login row".into()),
            ));
            continue;
        }
        out.push(match one(conn, machine, config, agent, timing).await {
            Verdict::Delivered => row(Status::Pass, "a test message reached the session", None),
            Verdict::Cannot => row(
                Status::Skipped,
                "",
                Some(
                    "this machine cannot send through the socket: a command machine, or no pastor there with `inbox send`"
                        .into(),
                ),
            ),
            Verdict::NoPane(why) => row(Status::Skipped, "", Some(why)),
            Verdict::Failed(why) => row(Status::Fail, "the session did not take a test message", Some(why)),
        });
    }
    out
}

enum Verdict {
    Delivered,
    Cannot,
    /// No pane to start a session in; the machine's herdr is another row's
    /// to report.
    NoPane(String),
    Failed(String),
}

/// Start a session for `agent`, send it the message and close it.
async fn one(
    conn: &dyn Connector,
    machine: &str,
    config: &Agents,
    agent: &AgentEnv,
    timing: Timing,
) -> Verdict {
    let Some(session) = crate::task::new_session_id() else {
        return Verdict::Failed("no random bytes for a session id".into());
    };
    let mut env = agent.env.clone();
    for (key, value) in env.iter_mut() {
        if value == "~" || value.starts_with("~/") {
            match crate::dispatch::expand_home(conn, &format!("env {key}"), value, Some(machine))
                .await
            {
                Ok(expanded) => *value = expanded,
                Err(err) => return Verdict::Failed(err.to_string()),
            }
        }
    }
    let label = format!("check/inbox-{}", agent.name);
    let created = match conn.workspace_create(None, &label, &env).await {
        Ok(created) => created,
        Err(err) => {
            return Verdict::NoPane(format!(
                "no session to test: could not open a pane on {machine}: {err}"
            ));
        }
    };
    let pane = created.root_pane.pane_id.clone();
    let run = async {
        let args = ["--session-id".to_string(), session.clone()];
        conn.agent_start(AGENT_NAME, &agent.kind, &pane, &args)
            .await
            .map_err(|err| format!("start {} in a pane: {err}", agent.kind))?;
        send_until(
            conn,
            crate::usage::config_dir(config, &agent.name),
            &session,
            timing.retry,
        )
        .await
    };
    let verdict = match tokio::time::timeout(timing.total, run).await {
        Ok(Ok(Some(()))) => Verdict::Delivered,
        Ok(Ok(None)) => Verdict::Cannot,
        Ok(Err(why)) => Verdict::Failed(why),
        Err(_) => Verdict::Failed(format!(
            "timed out after {}s: the session opened no inbox socket (this Claude may have none), or never answered",
            timing.total.as_secs()
        )),
    };
    // On every path: delivered, refused, failed to start, timed out.
    if let Err(err) = conn.pane_close(&pane).await {
        tracing::warn!(%machine, %pane, %err, "close the inbox check's pane");
    }
    verdict
}

/// Send until the session takes the message (`Some`), until the machine
/// says it cannot send (`None`), or until the session refuses it (`Err`). A
/// session that has no socket yet is tried again.
async fn send_until(
    conn: &dyn Connector,
    config_dir: Option<&str>,
    session: &str,
    retry: Duration,
) -> Result<Option<()>, String> {
    loop {
        let sent = conn
            .inbox_send(config_dir, session, crate::inbox::Mode::Prompting, TEXT)
            .await
            .map_err(|err| format!("send through the inbox socket: {err}"))?;
        let Some(out) = sent else {
            return Ok(None);
        };
        match out.result {
            "accepted" => return Ok(Some(())),
            "no-session" => tokio::time::sleep(retry).await,
            other => {
                return Err(match out.detail {
                    Some(detail) => format!("the session answered {other}: {detail}"),
                    None => format!("the session answered {other}"),
                });
            }
        }
    }
}

#[cfg(all(test, feature = "fake-herdr"))]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;

    fn claude() -> Vec<AgentEnv> {
        vec![AgentEnv {
            name: "claude".into(),
            kind: "claude".into(),
            env: Default::default(),
        }]
    }

    fn quick() -> Timing {
        Timing {
            total: Duration::from_millis(300),
            retry: Duration::from_millis(20),
        }
    }

    async fn row_for(fake: &FakeHerdr, agents: &[AgentEnv], seen: &[Row], t: Timing) -> Vec<Row> {
        rows(fake, "pi-3", &Agents::default(), agents, seen, t).await
    }

    /// A session that takes the message passes, was started the way a task
    /// starts one (its own session id, the agent's kind), is sent in the
    /// mode it launched under, and its pane is gone.
    #[tokio::test]
    async fn delivered_passes_and_closes_the_session() {
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        let got = row_for(&fake, &claude(), &[], quick()).await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].id, INBOX_ROW);
        assert_eq!(got[0].agent.as_deref(), Some("claude"));
        assert_eq!(got[0].status, Status::Pass, "{got:?}");
        let sends = fake.inbox_sends();
        assert_eq!(sends.len(), 1);
        assert!(crate::task::is_session_id(&sends[0].session));
        assert_eq!(sends[0].mode, crate::inbox::Mode::Prompting);
        assert!(sends[0].text.contains("test of the inbox socket"));
        assert!(fake.workspaces().is_empty(), "{:?}", fake.workspaces());
    }

    /// A session that refuses messages from other sessions fails the row
    /// with what it said, once, and is closed.
    #[tokio::test]
    async fn refused_fails_with_the_reason_and_closes() {
        for result in ["refused", "held", "dropped", "denied", "expired"] {
            let fake = FakeHerdr::new();
            fake.set_inbox_result(Some(result));
            let got = row_for(&fake, &claude(), &[], quick()).await;
            assert_eq!(got[0].status, Status::Fail, "{result}: {got:?}");
            let why = got[0].reason.as_deref().unwrap();
            assert!(why.contains(result) && why.contains("fake"), "{why}");
            assert_eq!(fake.inbox_sends().len(), 1, "{result}: not tried again");
            assert!(fake.workspaces().is_empty(), "{result}");
        }
    }

    /// A session that never opens its socket is tried again until the
    /// deadline, then fails as timed out, and its pane is closed.
    #[tokio::test]
    async fn timed_out_fails_and_closes_the_session() {
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("no-session"));
        let got = row_for(&fake, &claude(), &[], quick()).await;
        assert_eq!(got[0].status, Status::Fail, "{got:?}");
        assert!(
            got[0].reason.as_deref().unwrap().contains("timed out"),
            "{got:?}"
        );
        assert!(fake.inbox_sends().len() > 2, "tried again");
        assert!(fake.workspaces().is_empty(), "closed on the timeout");
    }

    /// A machine that cannot send through the socket, an agent that types
    /// and a signed-out claude are skipped, and a kind that is not claude
    /// has no row. Only the first finds out by starting a session, which it
    /// closes again.
    #[tokio::test]
    async fn skipped_starts_no_session() {
        // The machine cannot send (the fake's default answer).
        let fake = FakeHerdr::new();
        let got = row_for(&fake, &claude(), &[], quick()).await;
        assert_eq!(got[0].status, Status::Skipped, "{got:?}");
        assert!(fake.workspaces().is_empty(), "still closed");

        // Signed out.
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        let login = Row {
            id: "claude-login".into(),
            agent: Some("claude".into()),
            status: Status::Fail,
            first_line: String::new(),
            reason: None,
        };
        let got = row_for(&fake, &claude(), &[login], quick()).await;
        assert_eq!(got[0].status, Status::Skipped, "{got:?}");
        assert!(got[0].reason.as_deref().unwrap().contains("signed in"));
        assert!(fake.inbox_sends().is_empty());

        // delivery = "type".
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        let agents: Agents =
            toml::from_str("[claude]\nkind = \"claude\"\ndelivery = \"type\"\n").expect("agents");
        let got = rows(&fake, "pi-3", &agents, &claude(), &[], quick()).await;
        assert_eq!(got[0].status, Status::Skipped, "{got:?}");
        assert!(fake.inbox_sends().is_empty());

        // Not claude: no row at all.
        let codex = vec![AgentEnv {
            name: "codex".into(),
            kind: "codex".into(),
            env: Default::default(),
        }];
        assert!(row_for(&fake, &codex, &[], quick()).await.is_empty());
    }
}
