//! `pastor leave`: a clean teardown of this machine, so a `pastor machine
//! remove` on the head does not just orphan a service that keeps running
//! underneath it. Touches only what bootstrap could have left here: the
//! pastor and herdr user services of this login and, in root mode, of
//! `pastor-agent`, and the machine and agent keys.
//!
//! Every step is checked. What worked is printed, what failed is printed
//! with it, and one failure makes the command end in `leave_incomplete`.

use std::path::{Path, PathBuf};

use crate::bootstrap::AGENT_USER;
use crate::cli::CliError;
use crate::config::Paths;
use crate::ipc::{HeadPing, SHEPHERD_ROLE};
use crate::setup::{CmdOutput, Runner, Unit};

/// The service managers bootstrap writes units for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    /// systemd user units under `~/.config/systemd/user`.
    Linux,
    /// launchd user agents under `~/Library/LaunchAgents`.
    Macos,
}

impl Os {
    /// This build's, or `None` where bootstrap sets nothing up.
    pub fn current() -> Option<Os> {
        if cfg!(target_os = "macos") {
            Some(Os::Macos)
        } else if cfg!(target_os = "linux") {
            Some(Os::Linux)
        } else {
            None
        }
    }

    /// As bootstrap's facts name it, for `facts::passwd`.
    fn uname(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Macos => "darwin",
        }
    }

    /// Where `home`'s user keeps its units.
    fn unit_dir(self, home: &Path) -> PathBuf {
        match self {
            Os::Linux => home.join(".config/systemd/user"),
            Os::Macos => home.join("Library/LaunchAgents"),
        }
    }
}

/// What `leave` reads from this process's environment, spelled out so a
/// test can point it at a temp dir.
pub struct Here {
    pub os: Os,
    /// This login's unit dir: where `pastor setup` writes its units.
    pub unit_dir: PathBuf,
}

impl Here {
    pub fn from_env(os: Os) -> anyhow::Result<Here> {
        let unit_dir = match os {
            Os::Linux => crate::config::config_home()
                .ok_or_else(|| anyhow::anyhow!("no config dir"))?
                .join("systemd/user"),
            Os::Macos => dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("no home dir"))?
                .join("Library/LaunchAgents"),
        };
        Ok(Here { os, unit_dir })
    }
}

/// What `leave` did and what it could not do, in order.
#[derive(Debug, Default)]
pub struct Report {
    pub done: Vec<String>,
    pub failed: Vec<String>,
}

impl Report {
    /// Print both lists; `leave_incomplete` when anything failed.
    pub fn finish(self) -> anyhow::Result<()> {
        for line in &self.done {
            println!("{line}");
        }
        for line in &self.failed {
            println!("failed: {line}");
        }
        if self.failed.is_empty() {
            if self.done.is_empty() {
                println!("nothing to remove: no pastor or herdr service and no keys here");
            }
            return Ok(());
        }
        Err(CliError::err(
            "leave_incomplete",
            format!(
                "leave did not finish; {} step(s) failed: {}",
                self.failed.len(),
                self.failed.join("; ")
            ),
        ))
    }

    fn check(&mut self, out: std::io::Result<CmdOutput>, done: String, what: &str) -> bool {
        match out {
            Ok(o) if o.success => {
                self.done.push(done);
                true
            }
            out => {
                self.failed.push(format!("{what}: {}", why(out)));
                false
            }
        }
    }
}

/// Why a command failed, for a report line.
fn why(out: std::io::Result<CmdOutput>) -> String {
    match out {
        Ok(o) if o.stderr.trim().is_empty() => "it exited non-zero".to_string(),
        Ok(o) => o.stderr.trim().to_string(),
        Err(err) => err.to_string(),
    }
}

/// A head runs here for this login: leaving would take the fleet's own
/// head down with it. A headless serve (a shepherd) is what leave is for.
pub fn refuse_on_head(ping: &HeadPing) -> anyhow::Result<()> {
    match ping {
        HeadPing::Pong { role, .. } if role.as_deref() != Some(SHEPHERD_ROLE) => {
            Err(CliError::err(
                "leave_on_head",
                "pastor serve runs here as the head; leave is for a flock machine, \
                 so stop the head first (`pastor serve stop`) if this machine is \
                 really leaving",
            ))
        }
        _ => Ok(()),
    }
}

/// Entry point for `pastor leave`.
pub async fn cli(paths: &Paths, yes: bool) -> anyhow::Result<()> {
    refuse_on_head(&crate::ipc::ping_head(&paths.socket_file()).await)?;
    let Some(os) = Os::current() else {
        return Err(CliError::err(
            "leave_unsupported_platform",
            "leave knows systemd (Linux) and launchd (macOS) units only; \
             remove this machine's pastor and herdr services and its keys by hand",
        ));
    };
    let here = Here::from_env(os)?;
    confirm(yes)?;
    run(paths, &here, &crate::setup::SystemRunner).finish()
}

/// Shown and waited on unless `--yes`: like `pastor setup`'s own prompt, a
/// closed stdin cannot answer, so that fails outright instead of hanging.
fn confirm(yes: bool) -> anyhow::Result<()> {
    if yes {
        return Ok(());
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        anyhow::bail!("stdin is not a terminal, so leave cannot ask for confirmation; pass --yes");
    }
    // On stdout, like `pastor setup`'s prompt: a declined prompt ends in a
    // JSON error that must be the only thing on stderr.
    print!(
        "About to disable and remove this machine's pastor and herdr services \
         (this login's, and pastor-agent's if bootstrap made one), and delete \
         its machine and agent keys.\nContinue? Type 'yes' to proceed: "
    );
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if answer.trim() != "yes" {
        anyhow::bail!("aborted");
    }
    Ok(())
}

/// A user whose units leave removes.
struct Who {
    name: String,
    uid: String,
    /// `None` for this login: its commands run directly, its files are
    /// removed by this process. `Some` for pastor-agent: everything goes
    /// through bootstrap's clean-env `setsid` wrapper, as that user.
    other: Option<Other>,
    unit_dir: PathBuf,
}

struct Other {
    home: PathBuf,
    uid: u32,
    /// This process is root: `runuser` rather than `sudo -n`.
    root: bool,
}

impl Who {
    fn run(&self, r: &dyn Runner, program: &str, args: &[&str]) -> std::io::Result<CmdOutput> {
        let Some(o) = &self.other else {
            return r.run(program, args);
        };
        let (wrapper, full) =
            crate::bootstrap::other_user_argv(o.root, &self.name, &o.home, o.uid, program, args);
        let full: Vec<&str> = full.iter().map(String::as_str).collect();
        r.run(wrapper, &full)
    }

    /// Whether `path` is there. For another user, through `test -e` as that
    /// user, whose files this process may not see.
    fn exists(&self, r: &dyn Runner, path: &Path) -> bool {
        match &self.other {
            None => path.exists(),
            Some(_) => self
                .run(r, "test", &["-e", &path.to_string_lossy()])
                .is_ok_and(|o| o.success),
        }
    }

    /// `rm -f -- paths` as this user; for this login, `remove_file`.
    fn remove(&self, r: &dyn Runner, paths: &[PathBuf]) -> std::io::Result<CmdOutput> {
        if self.other.is_some() {
            let shown: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into()).collect();
            let mut args = vec!["-f", "--"];
            args.extend(shown.iter().map(String::as_str));
            return self.run(r, "rm", &args);
        }
        for p in paths {
            match std::fs::remove_file(p) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(std::io::Error::new(
                        err.kind(),
                        format!("{}: {err}", p.display()),
                    ));
                }
            }
        }
        Ok(CmdOutput {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

/// The teardown itself: this login, then pastor-agent if there is one.
pub fn run(paths: &Paths, here: &Here, r: &dyn Runner) -> Report {
    let mut report = Report::default();
    let uid = crate::bootstrap::facts::stdout(r, "id", &["-u"]).unwrap_or_default();
    let login = crate::bootstrap::facts::stdout(r, "id", &["-un"]).unwrap_or_default();
    let me = Who {
        name: if login.is_empty() {
            "this login".into()
        } else {
            login.clone()
        },
        uid: uid.clone(),
        other: None,
        unit_dir: here.unit_dir.clone(),
    };
    remove_units(r, here.os, &me, &mut report);
    remove_own_keys(paths, &mut report);

    if login == AGENT_USER {
        return report;
    }
    let Some((agent_uid, home)) = crate::bootstrap::facts::passwd(r, here.os.uname(), AGENT_USER)
    else {
        // User mode: bootstrap made no pastor-agent.
        return report;
    };
    let agent = Who {
        name: AGENT_USER.into(),
        uid: agent_uid.to_string(),
        other: Some(Other {
            home: home.clone(),
            uid: agent_uid,
            root: uid == "0",
        }),
        unit_dir: here.os.unit_dir(&home),
    };
    // Its own pastor says where its state (and so its key) is; this also
    // proves commands reach it, so a `test -e` that fails below means the
    // file is not there, not that sudo said no.
    let state = match agent.run(r, "pastor", &["bootstrap", "--print-state-dir"]) {
        Ok(o) if o.success && o.stdout.trim().starts_with('/') => PathBuf::from(o.stdout.trim()),
        Ok(o) if o.success => {
            report.failed.push(format!(
                "{AGENT_USER}: its pastor printed no state dir ({:?}); remove its pastor and \
                 herdr units and its agent_key and agent_key.pub by hand",
                o.stdout.trim()
            ));
            return report;
        }
        out => {
            report.failed.push(format!(
                "{AGENT_USER}: could not ask its pastor for its state dir \
                 (`pastor bootstrap --print-state-dir` as {AGENT_USER}): {}; remove its pastor \
                 and herdr units and its agent_key and agent_key.pub by hand",
                why(out)
            ));
            return report;
        }
    };
    remove_units(r, here.os, &agent, &mut report);
    let keys = [state.join("agent_key"), state.join("agent_key.pub")];
    if keys.iter().any(|k| agent.exists(r, k)) {
        let out = agent.remove(r, &keys);
        report.check(
            out,
            format!(
                "{AGENT_USER}: deleted its agent key ({})",
                keys[0].display()
            ),
            &format!(
                "{AGENT_USER}: could not delete its agent key {}",
                keys[0].display()
            ),
        );
    }
    report
}

/// Stop, disable and remove `who`'s pastor and herdr units, then reload its
/// manager if any went.
fn remove_units(r: &dyn Runner, os: Os, who: &Who, report: &mut Report) {
    let mut removed = false;
    for unit in [Unit::Pastor, Unit::Herdr] {
        let file = match os {
            Os::Linux => unit.file_name(),
            Os::Macos => unit.plist_name(),
        };
        let path = who.unit_dir.join(file);
        if !who.exists(r, &path) {
            continue;
        }
        match os {
            Os::Linux => {
                let out = who.run(r, "systemctl", &["--user", "disable", "--now", file]);
                report.check(
                    out,
                    format!("{}: stopped and disabled {file}", who.name),
                    &format!("{}: could not stop and disable {file}", who.name),
                );
            }
            Os::Macos => {
                let target = format!("gui/{}/{}", who.uid, unit.label());
                match who.run(r, "launchctl", &["bootout", &target]) {
                    Ok(o) if o.success => {
                        report
                            .done
                            .push(format!("{}: unloaded {}", who.name, unit.label()))
                    }
                    // Not loaded (stopped with `setup launchd --stop`): nothing to unload.
                    Ok(o)
                        if o.stderr.contains("No such process")
                            || o.stderr.contains("Could not find specified service") => {}
                    out => report.failed.push(format!(
                        "{}: could not unload {} (`launchctl bootout {target}`): {}",
                        who.name,
                        unit.label(),
                        why(out)
                    )),
                }
            }
        }
        let out = who.remove(r, std::slice::from_ref(&path));
        removed |= report.check(
            out,
            format!("{}: removed {}", who.name, path.display()),
            &format!("{}: could not remove {}", who.name, path.display()),
        );
    }
    if removed && os == Os::Linux {
        let out = who.run(r, "systemctl", &["--user", "daemon-reload"]);
        report.check(
            out,
            format!("{}: reloaded its systemd user manager", who.name),
            &format!("{}: could not reload its systemd user manager", who.name),
        );
    }
}

/// This login's machine and agent keys, each with its `.pub`.
fn remove_own_keys(paths: &Paths, report: &mut Report) {
    for (label, key) in [
        ("machine key", paths.machine_key()),
        ("agent key", paths.agent_key()),
    ] {
        let public = PathBuf::from(format!("{}.pub", key.display()));
        for p in [&key, &public] {
            match std::fs::remove_file(p) {
                Ok(()) => report
                    .done
                    .push(format!("deleted the {label} ({})", p.display())),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => report.failed.push(format!(
                    "could not delete the {label} {}: {err}",
                    p.display()
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests;
