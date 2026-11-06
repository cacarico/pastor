//! The `pane-env` row: whether a task's env survives the shell an agent's
//! pane starts. The other rows run over ssh, but an agent runs in a herdr
//! pane, whose login shell reads the user's profile, and a profile that sets
//! `PATH` or a harness's config dir again undoes what pastor set for the
//! task without a word.
//!
//! For each agent that can land on the machine, a short-lived workspace is
//! opened the way task start opens one, with the env a task of it gets, and
//! a probe is typed into its shell. The probe writes `NAME=value` for each
//! variable pastor set and `PATH`, or only `set`/`unset` for a name that
//! looks secret, to a file under `~/.cache/pastor-check`; the check reads
//! the file over its own shell, compares, and removes it. The pane is closed
//! on every path, a timeout and a dropped check included.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::herdr::client::ConnectorExt;
use crate::herdr::transport::{Connector, posix_command};
use crate::provision::run::{Row, Shell, Status};

pub const PANE_ENV_ROW: &str = "pane-env";

/// How long the pane's shell gets to start and run the probe.
pub(super) const TIMEOUT: Duration = Duration::from_secs(20);

/// How often the check looks for the probe's file.
const POLL: Duration = Duration::from_millis(300);

/// The workspace's label, which says what it is to anyone who sees it.
const LABEL: &str = "check/pane-env";

/// What `PASTOR_TASK` is in the probe's pane: no task's name.
const PROBE_TASK: &str = "pane-env-check";

/// The env one probe pane gets: a task's of `agent`, or a task's with no
/// agent env of its own.
pub(super) struct Want {
    pub agent: Option<String>,
    pub env: BTreeMap<String, String>,
}

/// The env a task gets on the machine before its agent's own (`layers`, as
/// `EnvLayers::merged` gives them), with each agent's `env` over it, and
/// pastor's own marks last, as dispatch sets them.
pub(super) fn wants(
    layers: &BTreeMap<String, String>,
    agents: &[crate::provision::run::AgentEnv],
    head: Option<&str>,
) -> Vec<Want> {
    let finish = |mut env: BTreeMap<String, String>| {
        env.insert(crate::ipc::TASK_ENV.into(), PROBE_TASK.into());
        if let Some(head) = head {
            env.insert(crate::ipc::HEAD_ENV.into(), head.into());
        }
        env
    };
    if agents.is_empty() {
        return vec![Want {
            agent: None,
            env: finish(layers.clone()),
        }];
    }
    agents
        .iter()
        .map(|a| {
            let mut env = layers.clone();
            env.extend(a.env.iter().map(|(k, v)| (k.clone(), v.clone())));
            Want {
                agent: Some(a.name.clone()),
                env: finish(env),
            }
        })
        .collect()
}

/// One row for each of `wants`, one pane at a time.
pub(super) async fn rows(
    shell: &dyn Shell,
    conn: Arc<dyn Connector>,
    machine: &str,
    wants: Vec<Want>,
    timeout: Duration,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for want in wants {
        rows.push(row(shell, conn.clone(), machine, want, timeout).await);
    }
    rows
}

/// Closes the pane when dropped, unless `close` did already: a check
/// dropped half way (a client that gave up) leaves no pane behind.
struct Opened {
    conn: Arc<dyn Connector>,
    pane: Option<String>,
}

impl Opened {
    async fn close(mut self) {
        if let Some(pane) = self.pane.take()
            && let Err(err) = self.conn.pane_close(&pane).await
        {
            tracing::warn!(%pane, %err, "close the pane-env probe's pane");
        }
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        if let Some(pane) = self.pane.take()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            let conn = self.conn.clone();
            rt.spawn(async move {
                let _ = conn.pane_close(&pane).await;
            });
        }
    }
}

/// The file the probe writes: `"$HOME"/...`, for a shell to expand.
fn result_file(id: &str) -> String {
    format!("\"$HOME\"/.cache/pastor-check/pane-env-{id}")
}

/// What `name` is reported as: its value, or only whether it is set.
fn secret(name: &str) -> bool {
    crate::bundle::secrets::env_name_looks_secret(name)
}

/// The probe, a POSIX script with no single quote and no `\\` in it, so
/// `posix_command` wraps it for any login shell, fish too. It writes each
/// name's line, then moves the file into place, so a file there is whole.
pub(super) fn probe(names: &[&str], file: &str) -> String {
    let mut lines = String::new();
    for n in names {
        if secret(n) {
            lines.push_str(&format!(
                "if [ -n \"${{{n}+x}}\" ]; then echo \"{n} set\"; else echo \"{n} unset\"; fi; "
            ));
        } else {
            lines.push_str(&format!(
                "if [ -n \"${{{n}+x}}\" ]; then printf \"%s=%s\\n\" {n} \"${n}\"; else echo \"{n} unset\"; fi; "
            ));
        }
    }
    format!("f={file}; mkdir -p \"${{f%/*}}\"; {{ {lines}}} > \"$f.part\" && mv \"$f.part\" \"$f\"")
}

/// What one probe found against what pastor set: the names the shell
/// changed, and those it dropped.
pub(super) fn compare(env: &BTreeMap<String, String>, found: &str) -> (Vec<String>, Vec<String>) {
    let mut changed = Vec::new();
    let mut dropped = Vec::new();
    for (name, want) in env {
        let line = found.lines().find(|l| {
            l.strip_prefix(name.as_str())
                .is_some_and(|rest| rest.starts_with('=') || rest.starts_with(' '))
        });
        match line.map(|l| &l[name.len()..]) {
            None | Some(" unset") => dropped.push(name.clone()),
            Some(" set") => {}
            Some(rest) => {
                if rest.strip_prefix('=') != Some(want.as_str()) {
                    changed.push(name.clone());
                }
            }
        }
    }
    (changed, dropped)
}

fn error(agent: Option<String>, why: String) -> Row {
    Row {
        id: PANE_ENV_ROW.into(),
        agent,
        status: Status::Error,
        first_line: String::new(),
        reason: Some(why),
    }
}

async fn row(
    shell: &dyn Shell,
    conn: Arc<dyn Connector>,
    machine: &str,
    want: Want,
    timeout: Duration,
) -> Row {
    let Want { agent, mut env } = want;
    for (key, value) in env.iter_mut() {
        if value == "~" || value.starts_with("~/") {
            match crate::dispatch::expand_home(
                conn.as_ref(),
                &format!("env {key}"),
                value,
                Some(machine),
            )
            .await
            {
                Ok(v) => *value = v,
                Err(e) => return error(agent, e.to_string()),
            }
        }
    }
    let id = format!(
        "{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let file = result_file(&id);
    let mut names: Vec<&str> = env.keys().map(String::as_str).collect();
    if !env.contains_key("PATH") {
        names.push("PATH");
    }
    let created = match conn.workspace_create(None, LABEL, &env).await {
        Ok(c) => c,
        Err(e) => return error(agent, format!("could not open a herdr pane: {e}")),
    };
    let opened = Opened {
        conn: conn.clone(),
        pane: Some(created.root_pane.pane_id.clone()),
    };
    let pane = created.root_pane.pane_id;
    let probe = posix_command(&probe(&names, &file));
    let ran = tokio::time::timeout(timeout, async {
        conn.pane_send_text(&pane, &probe)
            .await
            .map_err(|e| e.to_string())?;
        conn.pane_send_keys(&pane, &["Enter".to_string()])
            .await
            .map_err(|e| e.to_string())?;
        loop {
            if let Ok(out) = shell
                .run(format!("cat {file} 2>/dev/null"), vec![], POLL * 10)
                .await
                && out.code == Some(0)
                && !out.stdout.is_empty()
            {
                return Ok::<String, String>(String::from_utf8_lossy(&out.stdout).into_owned());
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await;
    opened.close().await;
    let _ = shell
        .run(format!("rm -f {file} {file}.part"), vec![], POLL * 10)
        .await;
    let found = match ran {
        Err(_) => {
            return error(
                agent,
                format!(
                    "the pane's shell did not run the probe within {}s; is its profile waiting for input?",
                    timeout.as_secs()
                ),
            );
        }
        Ok(Err(why)) => return error(agent, format!("could not type into the pane: {why}")),
        Ok(Ok(found)) => found,
    };
    let (changed, dropped) = compare(&env, &found);
    if changed.is_empty() && dropped.is_empty() {
        return Row {
            id: PANE_ENV_ROW.into(),
            agent,
            status: Status::Pass,
            first_line: format!(
                "a pane's shell keeps the {} variables pastor sets for a task",
                env.len()
            ),
            reason: None,
        };
    }
    let mut said = Vec::new();
    if !changed.is_empty() {
        said.push(format!("changes {}", changed.join(", ")));
    }
    if !dropped.is_empty() {
        said.push(format!("drops {}", dropped.join(", ")));
    }
    Row {
        id: PANE_ENV_ROW.into(),
        agent,
        status: Status::Warn,
        first_line: String::new(),
        reason: Some(format!(
            "the login shell of a pane {}, which pastor set for a task; look for them in the shell's profile on {machine}",
            said.join(" and ")
        )),
    }
}

#[cfg(test)]
mod tests;
