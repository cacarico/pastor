use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
    Success { id: String, result: Value },
    Error { id: String, error: ErrorBody },
}

impl Response {
    pub fn id(&self) -> &str {
        match self {
            Response::Success { id, .. } | Response::Error { id, .. } => id,
        }
    }
}

/// Lifecycle events (`pane_closed`) and subscription events (`pane.agent_status_changed`)
/// share this envelope; only the `event` spelling differs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event: String,
    pub data: Value,
}

impl Event {
    pub fn pane_id(&self) -> Option<&str> {
        self.data.get("pane_id").and_then(Value::as_str)
    }
    pub fn agent_status(&self) -> Option<AgentStatus> {
        serde_json::from_value(self.data.get("agent_status")?.clone()).ok()
    }
    pub fn is_pane_closed(&self) -> bool {
        self.event == "pane_closed" || self.event == "pane.closed"
    }
    pub fn is_pane_exited(&self) -> bool {
        self.event == "pane_exited" || self.event == "pane.exited"
    }
    pub fn is_agent_status(&self) -> bool {
        self.event == "pane.agent_status_changed" || self.event == "pane_agent_status_changed"
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Incoming {
    Response(Response),
    Event(Event),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub completion_seq: Option<u64>,
    #[serde(default)]
    pub state_change_seq: u64,
    #[serde(default)]
    pub interactive_ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneRef {
    pub pane_id: String,
    pub workspace_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRef {
    pub workspace_id: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pong {
    pub version: String,
    pub protocol: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Created {
    pub workspace: WorkspaceRef,
    pub root_pane: PaneRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResult {
    pub agent: AgentInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentList {
    pub agents: Vec<AgentInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadBody {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneRead {
    pub read: ReadBody,
}

pub fn subscription_agent_status(pane_id: &str) -> Value {
    serde_json::json!({"type": "pane.agent_status_changed", "pane_id": pane_id})
}

pub fn subscription_lifecycle(kind: &str) -> Value {
    serde_json::json!({"type": kind})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_success_and_error_responses() {
        let ok: Incoming = serde_json::from_str(
            r#"{"id":"r1","result":{"type":"pong","version":"0.9.1","protocol":22}}"#,
        )
        .unwrap();
        match ok {
            Incoming::Response(Response::Success { id, result }) => {
                assert_eq!(id, "r1");
                let pong: Pong = serde_json::from_value(result).unwrap();
                assert_eq!(pong.protocol, 22);
            }
            other => panic!("{other:?}"),
        }
        let err: Incoming = serde_json::from_str(
            r#"{"id":"r2","error":{"code":"agent_blocked","message":"blocked"}}"#,
        )
        .unwrap();
        match err {
            Incoming::Response(Response::Error { error, .. }) => {
                assert_eq!(error.code, "agent_blocked")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_lifecycle_and_subscription_events() {
        let closed: Incoming = serde_json::from_str(
            r#"{"event":"pane_closed","data":{"type":"pane_closed","pane_id":"w1:p1","workspace_id":"w1"}}"#,
        )
        .unwrap();
        let Incoming::Event(e) = closed else { panic!() };
        assert!(e.is_pane_closed());
        assert_eq!(e.pane_id(), Some("w1:p1"));

        let status: Incoming = serde_json::from_str(
            r#"{"event":"pane.agent_status_changed","data":{"pane_id":"w1:p2","workspace_id":"w1","agent_status":"blocked","agent":"claude"}}"#,
        )
        .unwrap();
        let Incoming::Event(e) = status else { panic!() };
        assert!(e.is_agent_status());
        assert_eq!(e.agent_status(), Some(AgentStatus::Blocked));
    }

    #[test]
    fn parses_agent_list_with_missing_optionals() {
        let v = serde_json::json!({"type":"agent_list","agents":[{"terminal_id":"t","agent_status":"idle","workspace_id":"w1","tab_id":"w1:t1","pane_id":"w1:p1","focused":false,"revision":3}]});
        let list: AgentList = serde_json::from_value(v).unwrap();
        assert_eq!(list.agents[0].completion_seq, None);
        assert_eq!(list.agents[0].state_change_seq, 0);
        assert_eq!(list.agents[0].agent_status, AgentStatus::Idle);
    }

    #[test]
    fn unknown_agent_status_value_falls_back_to_unknown() {
        let status: AgentStatus = serde_json::from_value(serde_json::json!("something_new"))
            .expect("an unrecognised status must not fail to parse");
        assert_eq!(status, AgentStatus::Unknown);
    }

    #[test]
    fn request_serialises_with_method_and_params() {
        let r = Request {
            id: "x".into(),
            method: "agent.list".into(),
            params: serde_json::json!({}),
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"id":"x","method":"agent.list","params":{}}"#
        );
    }
}
