//! `pastor status`: the fleet on one screen, built from the replies `machine
//! list`, `task list`, `job list` and `limit list` already get, so it needs
//! nothing new of the head. Without a head it stops with `not_running`,
//! unless `--probe` asks for the machines probed and the rest read from the
//! files and store here.
use chrono::{DateTime, Utc};
use clap::Args;
use serde::Serialize;

use crate::cli::{CliError, MachineRow, ask, unexpected};
use crate::ipc::{Client, IpcRequest, IpcResponse};
use crate::limit::AccountLimit;
use crate::scheduler::JobStatus;
use crate::store::TaskFilter;
use crate::task::{LIVE_STATES, Task, TaskState};

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// With no head running, probe each machine over ssh and read tasks, jobs and limits from the files and store here, instead of failing
    #[arg(long)]
    pub probe: bool,
    /// Print as a JSON object
    #[arg(long)]
    pub json: bool,
}

/// The head as `status` found it. `at` is the ssh destination of a head
/// elsewhere, else `here`; `version` is what the head said to a ping, `None`
/// when none runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HeadSummary {
    pub running: bool,
    pub at: String,
    pub version: Option<String>,
}

/// A machine that cannot take work, with the channel that says why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DownMachine {
    pub name: String,
    pub channel: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MachineSummary {
    pub total: usize,
    pub up: usize,
    pub down: Vec<DownMachine>,
}

/// A job that needs a look: its last run failed, or its file does not load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobProblem {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobSummary {
    pub total: usize,
    pub enabled: usize,
    pub failing: Vec<JobProblem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub head: HeadSummary,
    pub machines: MachineSummary,
    /// The count of live tasks in each live state, every one of
    /// `LIVE_STATES` present so a script can read any of them.
    pub tasks: std::collections::BTreeMap<String, usize>,
    pub jobs: JobSummary,
    /// `None` from a head that predates usage limits.
    pub limits: Option<Vec<AccountLimit>>,
}

/// Channels that take work: a head's live ones that accept dispatch, and a
/// probe that reached the machine's herdr.
fn is_up(channel: &str) -> bool {
    matches!(channel, "connected" | "polling" | "probed")
}

pub fn machines(rows: &[MachineRow]) -> MachineSummary {
    let down: Vec<DownMachine> = rows
        .iter()
        .filter(|m| !is_up(&m.channel))
        .map(|m| DownMachine {
            name: m.name.clone(),
            channel: m.channel.clone(),
        })
        .collect();
    MachineSummary {
        total: rows.len(),
        up: rows.len() - down.len(),
        down,
    }
}

pub fn tasks(tasks: &[Task]) -> std::collections::BTreeMap<String, usize> {
    LIVE_STATES
        .iter()
        .map(|s| {
            let n = tasks.iter().filter(|t| t.state == *s).count();
            (s.to_string(), n)
        })
        .collect()
}

/// A disabled job is left out of `failing`, as `watch` leaves it out: it
/// will not run again until someone turns it on.
pub fn jobs(jobs: &[JobStatus]) -> JobSummary {
    let failing = jobs
        .iter()
        .filter(|j| j.enabled)
        .filter_map(|j| {
            let reason = match &j.error {
                Some(e) => Some(format!("invalid: {e}")),
                None => crate::watch::failing(j),
            }?;
            Some(JobProblem {
                name: j.name.clone(),
                reason,
            })
        })
        .collect();
    JobSummary {
        total: jobs.len(),
        enabled: jobs.iter().filter(|j| j.enabled).count(),
        failing,
    }
}

/// The text `status` prints: one labelled line per part.
pub fn text(s: &Status, now: DateTime<Utc>) -> String {
    let head = match (&s.head.running, &s.head.version) {
        (true, Some(v)) => format!("{} ({v})", s.head.at),
        (true, None) => s.head.at.clone(),
        (false, _) => "not running; probed the machines directly".into(),
    };
    let m = &s.machines;
    let mut machines = format!("{} up", m.up);
    if !m.down.is_empty() {
        let names: Vec<String> = m
            .down
            .iter()
            .map(|d| format!("{}: {}", d.name, d.channel))
            .collect();
        machines.push_str(&format!(", {} down ({})", m.down.len(), names.join(", ")));
    }
    if m.total == 0 {
        machines = "none".into();
    }
    // Order of attention: what is stuck before what waits.
    let order = [
        TaskState::Blocked,
        TaskState::Running,
        TaskState::Starting,
        TaskState::Queued,
        TaskState::Waiting,
        TaskState::Paused,
    ];
    let counts: Vec<String> = order
        .iter()
        .filter_map(|st| {
            let n = *s.tasks.get(st.as_str())?;
            (n > 0).then(|| format!("{n} {st}"))
        })
        .collect();
    let tasks = if counts.is_empty() {
        "none live".to_string()
    } else {
        counts.join(", ")
    };
    let j = &s.jobs;
    let jobs = if j.total == 0 {
        "none".to_string()
    } else {
        let mut line = format!("{} enabled of {}", j.enabled, j.total);
        if !j.failing.is_empty() {
            let names: Vec<&str> = j.failing.iter().map(|p| p.name.as_str()).collect();
            line.push_str(&format!(
                ", {} failing ({})",
                j.failing.len(),
                names.join(", ")
            ));
        }
        line
    };
    let limits = match &s.limits {
        None => "- (the head predates usage limits)".to_string(),
        Some(l) if l.is_empty() => "none".to_string(),
        Some(l) => l
            .iter()
            .map(|l| {
                format!(
                    "{} until {}",
                    l.name(),
                    crate::limit::local_time(l.retry_at, now)
                )
            })
            .collect::<Vec<_>>()
            .join(", "),
    };
    [
        ("head", head),
        ("machines", machines),
        ("tasks", tasks),
        ("jobs", jobs),
        ("limits", limits),
    ]
    .iter()
    .map(|(k, v)| format!("{k:<9} {v}"))
    .collect::<Vec<_>>()
    .join("\n")
}

pub fn print(s: &Status, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(s)?);
    } else {
        println!("{}", text(s, Utc::now()));
    }
    Ok(())
}

/// The refusal when no head runs and `--probe` was not given.
pub fn not_running() -> anyhow::Error {
    CliError::err(
        "not_running",
        "pastor serve is not running here; start it with `pastor serve`, or run `pastor status --probe` to probe the machines directly",
    )
}

/// Everything `status` shows, asked of a running head.
pub async fn from_head(client: &Client) -> anyhow::Result<Status> {
    let version = match ask(client, IpcRequest::Ping).await? {
        IpcResponse::Pong { version, .. } => version,
        other => return Err(unexpected(other)),
    };
    let rows: Vec<MachineRow> = match ask(client, IpcRequest::FlockList).await? {
        IpcResponse::Machines(ms) => ms.iter().map(MachineRow::from).collect(),
        other => return Err(unexpected(other)),
    };
    let filter = TaskFilter {
        job: None,
        flock: None,
        machine: None,
        states: Some(LIVE_STATES.to_vec()),
    };
    let live = match ask(client, IpcRequest::List { filter }).await? {
        IpcResponse::Tasks(ts) => ts,
        other => return Err(unexpected(other)),
    };
    let job_list = match ask(client, IpcRequest::JobList).await? {
        IpcResponse::Jobs(js) => js,
        other => return Err(unexpected(other)),
    };
    // An older head has no limits to tell; the rest still stands.
    let limits = match ask(client, IpcRequest::LimitList).await {
        Ok(IpcResponse::Limits(l)) => Some(l),
        Ok(other) => return Err(unexpected(other)),
        Err(e) if e.code == "head_too_old" => None,
        Err(e) => return Err(e.into()),
    };
    Ok(Status {
        head: HeadSummary {
            running: true,
            at: client
                .head
                .as_ref()
                .map(|h| h.ssh.clone())
                .unwrap_or_else(|| "here".into()),
            version: Some(version),
        },
        machines: machines(&rows),
        tasks: tasks(&live),
        jobs: jobs(&job_list),
        limits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, channel: &str) -> MachineRow {
        MachineRow {
            name: name.into(),
            host: format!("user@{name}"),
            endpoint: String::new(),
            flock: "default".into(),
            flocks: Vec::new(),
            channel: channel.into(),
            herdr_version: None,
            pastor_version: None,
            protocol: None,
            error: None,
            live: None,
            max_agents: 1,
            job_slots: 0,
            burst: 0,
            tags: Vec::new(),
            orphans: Vec::new(),
            now: Vec::new(),
            profile: None,
            description: None,
        }
    }

    fn job(name: &str, enabled: bool, result: Option<&str>, error: Option<&str>) -> JobStatus {
        JobStatus {
            name: name.into(),
            schedule: Some("1h".into()),
            enabled,
            connector: None,
            error: error.map(str::to_string),
            last_run_at: None,
            last_result: result.map(str::to_string),
            next_due: None,
            running: false,
            flock: None,
            description: None,
        }
    }

    fn status() -> Status {
        Status {
            head: HeadSummary {
                running: true,
                at: "user@pi-1".into(),
                version: Some("0.9.0".into()),
            },
            machines: machines(&[]),
            tasks: tasks(&[]),
            jobs: jobs(&[]),
            limits: Some(Vec::new()),
        }
    }

    #[test]
    fn machines_count_only_dispatchable_channels_as_up() {
        let m = machines(&[
            row("pi-1", "connected"),
            row("pi-2", "polling"),
            row("pi-3", "reconnecting"),
            row("pi-4", "probed"),
            row("pi-5", "unreachable"),
        ]);
        assert_eq!(m.total, 5);
        assert_eq!(m.up, 3);
        let down: Vec<&str> = m.down.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(down, ["pi-3", "pi-5"]);
    }

    #[test]
    fn jobs_flag_failed_and_invalid_but_not_disabled() {
        let j = jobs(&[
            job("ok", true, Some("ok: 2 tasks"), None),
            job("bad", true, Some("failed: exit 1"), None),
            job("broken", true, None, Some("unknown key")),
            job("off", false, Some("failed: exit 1"), None),
        ]);
        assert_eq!((j.total, j.enabled), (4, 3));
        let failing: Vec<(&str, &str)> = j
            .failing
            .iter()
            .map(|p| (p.name.as_str(), p.reason.as_str()))
            .collect();
        assert_eq!(
            failing,
            [
                ("bad", "failed: exit 1"),
                ("broken", "invalid: unknown key")
            ]
        );
    }

    #[test]
    fn tasks_list_every_live_state() {
        let t = tasks(&[]);
        assert_eq!(t.len(), LIVE_STATES.len());
        assert!(t.values().all(|n| *n == 0));
    }

    #[test]
    fn text_puts_each_part_on_a_labelled_line() {
        let mut s = status();
        s.machines = machines(&[row("pi-1", "connected"), row("pi-3", "reconnecting")]);
        s.tasks.insert("running".into(), 2);
        s.tasks.insert("blocked".into(), 1);
        s.tasks.insert("queued".into(), 3);
        s.jobs = jobs(&[
            job("a", true, None, None),
            job("nightly", true, Some("failed: exit 1"), None),
        ]);
        assert_eq!(
            text(&s, Utc::now()),
            "head      user@pi-1 (0.9.0)\n\
             machines  1 up, 1 down (pi-3: reconnecting)\n\
             tasks     1 blocked, 2 running, 3 queued\n\
             jobs      2 enabled of 2, 1 failing (nightly)\n\
             limits    none"
        );
    }

    #[test]
    fn text_says_none_for_an_empty_fleet() {
        let t = text(&status(), Utc::now());
        assert!(t.contains("machines  none"), "{t}");
        assert!(t.contains("tasks     none live"), "{t}");
        assert!(t.contains("jobs      none"), "{t}");
    }

    #[test]
    fn text_names_a_probe_and_an_old_head() {
        let mut s = status();
        s.head = HeadSummary {
            running: false,
            at: "here".into(),
            version: None,
        };
        s.limits = None;
        let t = text(&s, Utc::now());
        assert!(t.starts_with("head      not running; probed"), "{t}");
        assert!(t.contains("limits    - (the head predates"), "{t}");
    }

    #[test]
    fn json_keeps_every_part() {
        let v = serde_json::to_value(status()).unwrap();
        for key in ["head", "machines", "tasks", "jobs", "limits"] {
            assert!(v.get(key).is_some(), "{key} missing: {v}");
        }
        assert_eq!(v["head"]["running"], true);
        assert_eq!(v["tasks"]["queued"], 0);
    }
}
