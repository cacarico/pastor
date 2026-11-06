//! `pastor login`: a guided sign-in of an agent on a machine. The head opens
//! a workspace there (`login/<agent>`, with the agent's `env`, as the
//! machine's login user), types the interactive login its harness manifest
//! names (`[login] start`), and reads the pane for the URL and device code a
//! person needs (`read_url`, `read_code`). `pastor login list` shows each
//! pending login, and `login.pending` says when one shows or changes.
//!
//! The head then runs the agent's login check (`Agents::login_check_command`)
//! every few reads. When it passes, the pane is closed and the signed-out
//! mark cleared (`Fleet::clear_signed_out`, `by: login`), with
//! `agent.signed_in`. A pane closed before that, a login that takes longer
//! than `TIMEOUT` or a `pastor login cancel` ends it with `login.closed` and
//! leaves the mark.
//!
//! pastor reads the pane and runs the check, nothing more: it never reads,
//! copies or stores the credential the harness writes. A code a person
//! pastes back (`pastor login code`) is typed into the pane and kept
//! nowhere, events and logs included.
//!
//! The pending logins live in the head's memory: a head that restarts
//! forgets them, and their panes stay open until someone closes them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cli::CliError;
use crate::config::Agents;
use crate::config::flock::MachineConfig;
use crate::harness_manifest::{Harnesses, LoginStart};
use crate::herdr::client::ConnectorExt;
use crate::herdr::transport::Connector;
use crate::limit::AccountLimit;
use crate::sync::Recover;

/// The label of the workspace a login opens, before the agent's name.
pub const LABEL_PREFIX: &str = "login/";

/// How often the head reads a login's pane.
pub const POLL: Duration = Duration::from_secs(2);

/// The check runs once every this many reads: a check can reach the
/// harness's servers (agy lists its models), so not on every read.
pub const CHECK_EVERY: u32 = 5;

/// How long `pastor login` waits for the pane to show a URL or a code
/// before it answers without one.
pub const START_WAIT: Duration = Duration::from_secs(20);

/// How long a login may wait on a person before the head closes its pane.
/// Device codes run out in about 15 minutes.
pub const TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How much of the pane is read: enough for a URL wrapped over many lines.
const READ_LINES: u32 = 200;

/// The code of a request about a login that is not pending.
pub const NOT_PENDING: &str = "login_not_pending";

/// A login waiting on a person, as `pastor login list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingLogin {
    pub machine: String,
    pub agent: String,
    /// The herdr pane the login runs in.
    pub pane: String,
    /// Where to sign in, once the pane shows it.
    pub url: Option<String>,
    /// The device code to give at `url`, for a login that shows one.
    pub code: Option<String>,
    /// The pane waits for a code pasted back from the browser: `pastor
    /// login code` types it in.
    pub paste: bool,
    pub started: DateTime<Utc>,
}

/// What `pastor login` answers: the login it started, or none when the
/// agent was signed in already, with the marks that cleared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Started {
    pub pending: Option<PendingLogin>,
    pub cleared: Vec<AccountLimit>,
}

/// What a login of `agent` on a machine runs, worked out before anything
/// opens (`plan`).
#[derive(Debug, Clone)]
pub struct Plan {
    pub machine: String,
    pub agent: String,
    /// What its signed-out mark is kept under (`Agents::signed_out_key`).
    pub key: String,
    /// The login check, as one command for the machine's shell.
    pub check: String,
    /// The line typed into the pane (`Manifest::login_start_line`).
    pub line: String,
    pub start: LoginStart,
    /// The agent's `env`, `~` not yet expanded.
    pub env: BTreeMap<String, String>,
}

fn refuse(code: &str, message: String) -> CliError {
    CliError {
        code: code.into(),
        message,
    }
}

/// How to sign `agent` in on machine `m`, or why pastor cannot.
pub fn plan(
    agents: &Agents,
    harnesses: &Harnesses,
    m: &MachineConfig,
    agent: &str,
) -> Result<Plan, CliError> {
    let machine = &m.name;
    if m.pull {
        return Err(refuse(
            "pull_machine",
            format!("{machine} is a pull machine: the head never connects to it"),
        ));
    }
    let kind = agents.kind(agent);
    let manifest = harnesses.get(kind);
    let (Some(start), Some(line)) = (
        manifest.and_then(|h| h.login_start.clone()),
        manifest.and_then(|h| h.login_start_line()),
    ) else {
        return Err(refuse(
            "no_login_start",
            format!(
                "agent {agent} ({kind}) has no interactive login; its harness manifest needs [login] start"
            ),
        ));
    };
    let Some(check) = agents.login_check_command(agent, manifest) else {
        return Err(refuse(
            "no_login_check",
            format!("agent {agent} has no login_check; set one under [agents.{agent}]"),
        ));
    };
    Ok(Plan {
        machine: machine.clone(),
        agent: agent.to_string(),
        key: agents.signed_out_key(machine, agent),
        check,
        line,
        start,
        env: agents.env(agent),
    })
}

/// Characters a URL holds, by RFC 3986.
fn url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)
}

/// The URL the pane shows lowest, which is the newest. A login that draws
/// its own box (agy) wraps a long URL over several lines at the box's
/// width, which herdr cannot unwrap: a URL that runs to the end of a line of
/// at least 40 columns goes on with the next line when that line is one run
/// of URL characters, for as long as the lines keep the first one's width.
pub fn read_url(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().map(str::trim_end).collect();
    let (at, start) = lines
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, l)| l.rfind("https://").map(|s| (i, s)))?;
    let first = lines[at];
    let piece: String = first[start..]
        .chars()
        .take_while(|c| url_char(*c))
        .collect();
    let mut url = piece.clone();
    let width = first.chars().count();
    let mut runs_on = start + piece.len() == first.len() && width >= 40;
    for line in &lines[at + 1..] {
        let next = line.trim();
        if !runs_on || next.is_empty() || !next.chars().all(url_char) {
            break;
        }
        url.push_str(next);
        runs_on = line.chars().count() == width;
    }
    (url.len() > "https://".len()).then_some(url)
}

/// The device code after `after`, lowest on the pane: the rest of its line
/// when that is one word, else the first word of the next line with text
/// (Codex says `Enter this one-time code (expires in 15 minutes)`, then the
/// code on its own line).
pub fn read_code(screen: &str, after: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let (at, start) = lines
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, l)| l.rfind(after).map(|s| (i, s + after.len())))?;
    let word = |s: &str| {
        s.split_whitespace()
            .next()
            .map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
            .filter(|w| !w.is_empty())
            .map(str::to_string)
    };
    let rest = lines[at][start..].trim_start_matches([':', ' ']);
    if rest.split_whitespace().count() == 1 {
        return word(rest);
    }
    lines[at + 1..]
        .iter()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| word(l))
}

/// What a person needs from `screen`, for a login run by `start`.
fn read_screen(start: &LoginStart, screen: &str) -> (Option<String>, Option<String>, bool) {
    let code = start
        .code_after
        .as_deref()
        .and_then(|after| read_code(screen, after));
    let paste = start.paste.as_deref().is_some_and(|p| screen.contains(p));
    (read_url(screen), code, paste)
}

struct Entry {
    login: PendingLogin,
    workspace: String,
}

/// The logins the head is watching, by machine and agent.
pub struct Logins {
    pending: Mutex<BTreeMap<(String, String), Entry>>,
    poll_ms: AtomicU64,
}

impl Default for Logins {
    fn default() -> Logins {
        Logins {
            pending: Mutex::default(),
            poll_ms: AtomicU64::new(POLL.as_millis() as u64),
        }
    }
}

impl Logins {
    /// How often a login's pane is read; tests make it short.
    pub fn set_poll(&self, every: Duration) {
        self.poll_ms
            .store(every.as_millis().max(1) as u64, Ordering::Relaxed);
    }

    fn poll(&self) -> Duration {
        Duration::from_millis(self.poll_ms.load(Ordering::Relaxed))
    }

    /// Every pending login, oldest first.
    pub fn list(&self) -> Vec<PendingLogin> {
        let mut all: Vec<PendingLogin> = self
            .pending
            .lock()
            .recover()
            .values()
            .map(|e| e.login.clone())
            .collect();
        all.sort_by_key(|l| l.started);
        all
    }

    pub fn get(&self, machine: &str, agent: &str) -> Option<PendingLogin> {
        self.pending
            .lock()
            .recover()
            .get(&(machine.to_string(), agent.to_string()))
            .map(|e| e.login.clone())
    }

    /// Forget the login of `agent` on `machine` if it still runs in `pane`;
    /// its workspace, when it did.
    fn take(&self, machine: &str, agent: &str, pane: &str) -> Option<String> {
        let mut pending = self.pending.lock().recover();
        let key = (machine.to_string(), agent.to_string());
        if pending.get(&key).is_some_and(|e| e.login.pane == pane) {
            return pending.remove(&key).map(|e| e.workspace);
        }
        None
    }

    /// Set what the pane shows on the login in `pane`. `None` when the
    /// login is gone or nothing changed; else the login as it now is.
    fn update(
        &self,
        login: &PendingLogin,
        (url, code, paste): (Option<String>, Option<String>, bool),
    ) -> Option<PendingLogin> {
        let mut pending = self.pending.lock().recover();
        let e = pending.get_mut(&(login.machine.clone(), login.agent.clone()))?;
        if e.login.pane != login.pane {
            return None;
        }
        let url = url.or_else(|| e.login.url.clone());
        let code = code.or_else(|| e.login.code.clone());
        if (&url, &code, paste) == (&e.login.url, &e.login.code, e.login.paste) {
            return None;
        }
        e.login.url = url;
        e.login.code = code;
        e.login.paste = paste;
        Some(e.login.clone())
    }
}

/// What the head does about a login: its machines' connectors, the mark
/// to clear and the events to send.
pub trait Head: Send + Sync {
    /// Clear the signed-out mark under `key`, with `agent.signed_in` (`by`).
    fn clear_signed_out(&self, key: &str, by: &str) -> anyhow::Result<Vec<AccountLimit>>;
    /// Send event `kind` about `machine`.
    fn emit(&self, kind: &str, machine: &str, detail: serde_json::Value);
}

/// Why a herdr call failed, as `pastor login` says it.
fn herdr_refusal(machine: &str, err: crate::herdr::client::CallError) -> CliError {
    match err.code() {
        Some(code) => refuse(code, format!("{machine}: {err}")),
        None => refuse("machine_unreachable", format!("{machine}: {err}")),
    }
}

/// Run `plan`'s check on the machine: `Ok(true)` when the agent is signed
/// in there.
async fn check(conn: &dyn Connector, plan: &Plan) -> Result<bool, CliError> {
    match conn.command_succeeds(&plan.check).await {
        Ok(Some(signed_in)) => Ok(signed_in),
        Ok(None) => Err(refuse(
            "check_unknown",
            format!(
                "could not run the login check of {} on {}",
                plan.agent, plan.machine
            ),
        )),
        Err(err) => Err(refuse(
            "machine_unreachable",
            format!("{}: {}", plan.machine, err.message),
        )),
    }
}

/// Start the login `plan` names through `conn`, and watch it until it ends.
/// A login of the same agent on the same machine already pending is
/// answered as it is. An agent signed in already gets no pane: its mark
/// clears, and the answer has no pending login.
pub async fn start(
    logins: &Arc<Logins>,
    head: Weak<dyn Head>,
    conn: Arc<dyn Connector>,
    plan: Plan,
) -> Result<Started, CliError> {
    if let Some(pending) = logins.get(&plan.machine, &plan.agent) {
        return Ok(Started {
            pending: Some(pending),
            cleared: vec![],
        });
    }
    if check(conn.as_ref(), &plan).await? {
        let cleared = match head.upgrade() {
            Some(h) => h
                .clear_signed_out(&plan.key, "login")
                .map_err(|e| refuse("store_error", e.to_string()))?,
            None => vec![],
        };
        return Ok(Started {
            pending: None,
            cleared,
        });
    }
    let mut env = plan.env.clone();
    for (key, value) in env.iter_mut() {
        if value == "~" || value.starts_with("~/") {
            *value = crate::dispatch::expand_home(
                conn.as_ref(),
                &format!("env {key}"),
                value,
                Some(&plan.machine),
            )
            .await
            .map_err(|e| refuse("machine_unreachable", e.to_string()))?;
        }
    }
    let label = format!("{LABEL_PREFIX}{}", plan.agent);
    let created = conn
        .workspace_create(None, &label, &env)
        .await
        .map_err(|e| herdr_refusal(&plan.machine, e))?;
    let pane = created.root_pane.pane_id.clone();
    let typed = async {
        conn.pane_send_text(&pane, &plan.line).await?;
        conn.pane_send_keys(&pane, &["Enter".to_string()]).await
    };
    if let Err(err) = typed.await {
        let _ = conn.pane_close(&pane).await;
        return Err(herdr_refusal(&plan.machine, err));
    }
    let login = PendingLogin {
        machine: plan.machine.clone(),
        agent: plan.agent.clone(),
        pane: pane.clone(),
        url: None,
        code: None,
        paste: false,
        started: Utc::now(),
    };
    tracing::info!(machine = %plan.machine, agent = %plan.agent, %pane, "login started");
    logins.pending.lock().recover().insert(
        (plan.machine.clone(), plan.agent.clone()),
        Entry {
            login: login.clone(),
            workspace: created.workspace.workspace_id.clone(),
        },
    );
    tokio::spawn(watch(logins.clone(), head, conn, plan, login.clone()));
    // Answer once the pane shows what a person needs, or after a while
    // with what it shows by then.
    let deadline = tokio::time::Instant::now() + START_WAIT;
    loop {
        let now = logins.get(&login.machine, &login.agent);
        let shown = now
            .as_ref()
            .is_none_or(|l| l.url.is_some() || l.code.is_some() || l.paste);
        if shown || tokio::time::Instant::now() >= deadline {
            return Ok(Started {
                pending: now,
                cleared: vec![],
            });
        }
        tokio::time::sleep(logins.poll().min(Duration::from_millis(200))).await;
    }
}

/// Read the pane and run the check until the login ends one way or another.
async fn watch(
    logins: Arc<Logins>,
    head: Weak<dyn Head>,
    conn: Arc<dyn Connector>,
    plan: Plan,
    login: PendingLogin,
) {
    let (machine, agent, pane) = (&login.machine, &login.agent, &login.pane);
    let Some(workspace) = logins
        .pending
        .lock()
        .recover()
        .get(&(machine.clone(), agent.clone()))
        .map(|e| e.workspace.clone())
    else {
        return;
    };
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut picked = false;
    let mut reads = 0u32;
    loop {
        tokio::time::sleep(logins.poll()).await;
        if logins.get(machine, agent).is_none_or(|l| l.pane != *pane) {
            // Cancelled: whoever took it closed the pane and said so.
            return;
        }
        let Some(h) = head.upgrade() else {
            return;
        };
        let open = match conn.pane_list(&workspace).await {
            Ok(panes) => Some(panes.iter().any(|p| p.pane_id == *pane)),
            Err(e) if e.code() == Some("workspace_not_found") => Some(false),
            Err(_) => None,
        };
        if open == Some(true)
            && let Ok(screen) = conn.agent_read(pane, READ_LINES).await
        {
            if !picked
                && let Some((text, keys)) = &plan.start.pick
                && screen.contains(text.as_str())
            {
                picked = conn.pane_send_keys(pane, keys).await.is_ok();
            }
            if let Some(now) = logins.update(&login, read_screen(&plan.start, &screen)) {
                h.emit(
                    "login.pending",
                    machine,
                    serde_json::to_value(&now).unwrap_or_default(),
                );
            }
        }
        reads += 1;
        let gone = open == Some(false);
        if (gone || reads.is_multiple_of(CHECK_EVERY))
            && matches!(check(conn.as_ref(), &plan).await, Ok(true))
        {
            if logins.take(machine, agent, pane).is_none() {
                return;
            }
            if !gone {
                let _ = conn.pane_close(pane).await;
            }
            tracing::info!(%machine, %agent, "signed in through pastor login");
            match h.clear_signed_out(&plan.key, "login") {
                // No mark to clear: say it all the same, it signed in.
                Ok(cleared) if cleared.is_empty() => h.emit(
                    "agent.signed_in",
                    machine,
                    serde_json::json!({"account": plan.key, "model": null,
                        "agent": agent, "by": "login"}),
                ),
                Ok(_) => {}
                Err(err) => tracing::error!(%machine, %agent, %err, "clear a signed-out mark"),
            }
            return;
        }
        let why = if gone {
            "the pane closed"
        } else if tokio::time::Instant::now() >= deadline {
            let _ = conn.pane_close(pane).await;
            "timed out"
        } else {
            continue;
        };
        if logins.take(machine, agent, pane).is_some() {
            closed(h.as_ref(), &login, why);
        }
        return;
    }
}

/// `login.closed`: the login ended with the agent still signed out.
fn closed(head: &dyn Head, login: &PendingLogin, why: &str) {
    tracing::info!(machine = %login.machine, agent = %login.agent, why, "login ended unsigned");
    head.emit(
        "login.closed",
        &login.machine,
        serde_json::json!({"agent": login.agent, "pane": login.pane, "why": why}),
    );
}

/// Stop the login of `agent` on `machine`: close its pane and forget it,
/// with `login.closed` (`why: cancelled`).
pub async fn cancel(
    logins: &Logins,
    head: &dyn Head,
    conn: &dyn Connector,
    machine: &str,
    agent: &str,
) -> Result<PendingLogin, CliError> {
    let login = pending_or_refuse(logins, machine, agent)?;
    logins.take(machine, agent, &login.pane);
    let _ = conn.pane_close(&login.pane).await;
    closed(head, &login, "cancelled");
    Ok(login)
}

/// Type `code`, pasted back from the browser, into the pane of the login
/// of `agent` on `machine`, then Enter. The code is kept nowhere.
pub async fn paste(
    logins: &Logins,
    conn: &dyn Connector,
    machine: &str,
    agent: &str,
    code: &str,
) -> Result<PendingLogin, CliError> {
    let login = pending_or_refuse(logins, machine, agent)?;
    let code = code.trim();
    if code.is_empty() || code.contains(['\n', '\r']) {
        return Err(refuse(
            "usage_error",
            "the code is one line, and not empty".into(),
        ));
    }
    conn.pane_send_text(&login.pane, code)
        .await
        .map_err(|e| herdr_refusal(machine, e))?;
    conn.pane_send_keys(&login.pane, &["Enter".to_string()])
        .await
        .map_err(|e| herdr_refusal(machine, e))?;
    Ok(login)
}

fn pending_or_refuse(
    logins: &Logins,
    machine: &str,
    agent: &str,
) -> Result<PendingLogin, CliError> {
    logins.get(machine, agent).ok_or_else(|| {
        refuse(
            NOT_PENDING,
            format!("no login of {agent} is pending on {machine}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude's `claude auth login`, as herdr reads it: the URL on one
    /// line, and the paste prompt under it.
    const CLAUDE: &str = "$ sh -c '...'\nOpening browser to sign in…\nIf the browser didn't open, visit: https://claude.ai/oauth/authorize?code=true&client_id=abc&state=xyz\nPaste code here if prompted > \n";

    /// agy draws a URL in a box 78 wide, so it reaches herdr wrapped: a
    /// space, then 77 columns a line, the last padded.
    fn agy(url: &str) -> String {
        let boxed: String = url
            .as_bytes()
            .chunks(77)
            .map(|c| format!(" {:<77}\n", std::str::from_utf8(c).unwrap()))
            .collect();
        format!(
            " Signing in...Your browser should open automatically. If not:\n\n{boxed}\n  (1-20 of 29 lines)\n"
        )
    }

    /// Codex's device login: the URL, then the code under its label.
    const CODEX: &str = "Follow these steps to sign in with ChatGPT using device code authorization:\n1. Open this link in your browser and sign in to your account\n   https://auth.openai.com/codex/device\n2. Enter this one-time code (expires in 15 minutes)\n   ABCD-12345\nContinue only if you started this login in Codex.\n";

    #[test]
    fn a_url_is_read_off_its_line() {
        assert_eq!(
            read_url(CLAUDE).as_deref(),
            Some("https://claude.ai/oauth/authorize?code=true&client_id=abc&state=xyz")
        );
        assert_eq!(
            read_url(CODEX).as_deref(),
            Some("https://auth.openai.com/codex/device")
        );
        assert_eq!(read_url("no link here\n"), None);
        assert_eq!(read_url("https://\n"), None);
    }

    /// A URL a box wrapped is joined back, and stops at the box's last
    /// line: a short line, a blank one or one with spaces in it.
    #[test]
    fn a_wrapped_url_is_joined_back() {
        let url = "https://accounts.google.com/o/oauth2/auth?access_type=offline&client_id=123456060591-abcdefgh.apps.googleusercontent.com&code_challenge=J7HfxPySUlNiOkDOQHEvmAASQBsOfQHb4Aqm5QjjgAk&redirect_uri=https%3A%2F%2Fantigravity.google%2Foauth-callback&state=zA";
        assert_eq!(read_url(&agy(url)).as_deref(), Some(url));
        // One that ends exactly at the box's edge stops at the blank line.
        let edge = &url[..77 * 2];
        assert_eq!(read_url(&agy(edge)).as_deref(), Some(edge));
        // A short URL never takes the word under it.
        assert_eq!(
            read_url("visit https://x.example/a\nword\n").as_deref(),
            Some("https://x.example/a")
        );
    }

    #[test]
    fn a_code_is_read_after_its_label() {
        assert_eq!(
            read_code(CODEX, "Enter this one-time code").as_deref(),
            Some("ABCD-12345")
        );
        assert_eq!(
            read_code("Your code: WXYZ-9876\n", "Your code").as_deref(),
            Some("WXYZ-9876")
        );
        assert_eq!(read_code(CLAUDE, "Enter this one-time code"), None);
    }

    #[test]
    fn the_screen_says_whether_a_code_is_wanted_back() {
        let claude = Harnesses::builtin().get("claude").unwrap();
        let start = claude.login_start.clone().unwrap();
        let (url, code, paste) = read_screen(&start, CLAUDE);
        assert!(url.is_some());
        assert_eq!(code, None);
        assert!(paste);
        let (_, _, paste) = read_screen(&start, "Opening browser to sign in…\n");
        assert!(!paste);
    }
}
