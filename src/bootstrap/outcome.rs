//! What a bootstrap run reports: each step's result, and the keys and
//! versions the head records.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Outcome {
    /// No step failed.
    pub ok: bool,
    pub isolation: Isolation,
    /// The steps run, in order; the last one is the failed one when `ok` is false.
    pub steps: Vec<StepResult>,
    /// The machine user's public machine key line.
    pub machine_key: Option<String>,
    /// The public agent key line.
    pub agent_key: Option<String>,
    /// The machine's ssh host key, for the head's known_hosts.
    pub host_key: Option<String>,
    pub herdr_version: Option<String>,
    pub pastor_version: Option<String>,
    /// What `machine check` said once the steps were done, when it ran.
    #[serde(default)]
    pub check: Option<crate::machine_check::Report>,
}

/// Whether agents run as a user who cannot read the machine key. Only the
/// agent runs as the login user for now; a separate agent user is a value
/// for a later card to add.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    None,
}

impl Isolation {
    pub fn as_str(self) -> &'static str {
        match self {
            Isolation::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StepResult {
    pub step: String,
    pub status: StepStatus,
    pub why: Option<String>,
    pub fix: Option<String>,
    /// What the machine lacks: packages, or herdr.
    #[serde(default)]
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
    fn new(step: &str, status: StepStatus) -> StepResult {
        StepResult {
            step: step.to_string(),
            status,
            why: None,
            fix: None,
            missing: Vec::new(),
        }
    }

    pub fn done(step: &str) -> StepResult {
        StepResult::new(step, StepStatus::Done)
    }

    pub fn already_there(step: &str) -> StepResult {
        StepResult::new(step, StepStatus::AlreadyThere)
    }

    pub fn skipped(step: &str, why: impl Into<String>) -> StepResult {
        StepResult {
            why: Some(why.into()),
            ..StepResult::new(step, StepStatus::Skipped)
        }
    }

    pub fn failed(step: &str, why: impl Into<String>, fix: impl Into<String>) -> StepResult {
        StepResult {
            why: Some(why.into()),
            fix: Some(fix.into()),
            ..StepResult::new(step, StepStatus::Failed)
        }
    }
}

impl Outcome {
    /// The step that failed, with why and what to do: none when the run is
    /// `ok`. A failed run without a failed step says so.
    pub fn failure(&self) -> Option<(String, String)> {
        if self.ok {
            return None;
        }
        Some(
            match self.steps.iter().find(|s| s.status == StepStatus::Failed) {
                Some(s) => {
                    let why = s.why.clone().unwrap_or_default();
                    let why = match &s.fix {
                        Some(fix) if !fix.is_empty() => format!("{why}; fix: {fix}"),
                        _ => why,
                    };
                    (s.step.clone(), why)
                }
                None => ("bootstrap".to_string(), "no step says why it failed".into()),
            },
        )
    }
}
