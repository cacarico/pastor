//! What `pastor bootstrap` prints: one JSON object with the mode, each
//! step's result, and the keys and versions the head records.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Outcome {
    /// No step failed.
    pub ok: bool,
    pub mode: Mode,
    pub isolation: Isolation,
    /// The steps run, in order; the last one is the failed one when `ok` is false.
    pub steps: Vec<StepResult>,
    /// The service user's public machine key line.
    pub machine_key: Option<String>,
    /// The agent user's public agent key line.
    pub agent_key: Option<String>,
    /// `/etc/ssh/ssh_host_ed25519_key.pub`, for the head's known_hosts.
    pub host_key: Option<String>,
    pub herdr_version: Option<String>,
    pub pastor_version: Option<String>,
    /// The user that holds the machine key and runs the shepherd
    /// (`--service-user`, else the login user). Absent from a report made
    /// before it was sent.
    #[serde(default)]
    pub service_user: Option<String>,
}

/// Root mode makes users and installs packages; user mode does neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Root,
    User,
}

/// Whether agents run as a user who cannot read the machine key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    Users,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StepResult {
    pub step: String,
    pub status: StepStatus,
    pub why: Option<String>,
    pub fix: Option<String>,
    /// What the machine lacks: packages, or herdr.
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Done,
    AlreadyThere,
    Failed,
    Skipped,
}

impl StepResult {
    fn new(status: StepStatus) -> StepResult {
        StepResult {
            step: String::new(),
            status,
            why: None,
            fix: None,
            missing: Vec::new(),
        }
    }

    pub fn done() -> StepResult {
        StepResult::new(StepStatus::Done)
    }

    pub fn already_there() -> StepResult {
        StepResult::new(StepStatus::AlreadyThere)
    }

    /// `Done` when `changed`, else `AlreadyThere`.
    pub fn changed(changed: bool) -> StepResult {
        if changed {
            StepResult::done()
        } else {
            StepResult::already_there()
        }
    }

    pub fn skipped(why: impl Into<String>) -> StepResult {
        StepResult {
            why: Some(why.into()),
            ..StepResult::new(StepStatus::Skipped)
        }
    }

    pub fn failed(why: impl Into<String>, fix: impl Into<String>) -> StepResult {
        StepResult {
            why: Some(why.into()),
            fix: Some(fix.into()),
            ..StepResult::new(StepStatus::Failed)
        }
    }

    pub fn with_missing(mut self, missing: Vec<String>) -> StepResult {
        self.missing = missing;
        self
    }
}
