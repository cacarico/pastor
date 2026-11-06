//! `pastor fleet update <version>`: a pastor release onto every machine
//! the head reaches, the head last.
//!
//! It runs on the head, in the CLI: the head's own serve restarts on the way,
//! so it cannot be the head that drives it. First every machine is looked at
//! (its platform and the pastor it runs, over the same pinned ssh a push
//! bootstrap uses) and the head's artifact cache must hold the version for
//! each, or nothing changes. Then, one machine at a time, the binary is
//! copied and its sha256 checked on the machine (`push::copy_command`), and a
//! script there installs it into `~/.local/bin`, reruns `pastor setup` for
//! the units it finds, restarts pastor's serve when one runs, and checks the
//! new version answers (`install_command`). The first failure stops the
//! rollout and leaves that machine's old binary in place.
//!
//! herdr and the agents' panes are never touched: tasks run in herdr, which
//! keeps running, and their rows are in the head's store, so a serve that
//! restarts finds them where they were. A pull machine runs its tasks
//! through its own serve and the head does not reach it: it is skipped.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;

use crate::artifacts::Artifact;
use crate::bootstrap::push::{PushRunner, copy_command, ssh_args, tail};
use crate::cli::CliError;
use crate::config::Paths;
use crate::config::flock::Flock;
use crate::herdr::transport::shell_quote;

/// Where the new binary lands on a machine, checked, before it is installed.
const STAGE: &str = "\"$HOME\"/.cache/pastor-update";

/// Where it is installed: where `install.sh` and bootstrap put pastor.
const BIN: &str = "\"$HOME\"/.local/bin";

/// How a machine is reached for the update.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reach {
    /// The head's own host.
    Here,
    /// Over ssh, as this login.
    Ssh(String),
}

#[derive(Debug, Clone)]
struct Target {
    name: String,
    reach: Reach,
    /// Tasks with a pane on it now: they keep running, and the report says
    /// how many.
    live: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Updated,
    /// `--dry-run`: what an update would do.
    WouldUpdate,
    Skipped,
    Failed,
}

/// One machine's line of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    pub machine: String,
    /// The head's own host, updated last.
    pub head: bool,
    pub status: Status,
    /// The pastor it ran before, when it said.
    pub from: Option<String>,
    /// Its tasks with a pane, which keep running.
    pub live_tasks: usize,
    pub why: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub version: String,
    pub ok: bool,
    pub rows: Vec<Row>,
}

/// What an update needs besides the version.
pub struct Ctx<'a> {
    pub paths: &'a Paths,
    pub flock: &'a Flock,
    /// Live tasks by machine (`live_tasks`).
    pub live: HashMap<String, usize>,
    pub runner: &'a dyn PushRunner,
}

/// The machines to update, in order, and the ones skipped up front: each
/// ssh machine in flock.toml's order, then the head's own host (its `local`
/// machines are that host, so one row stands for them), unless `only` names
/// one machine.
fn targets(ctx: &Ctx, only: Option<&str>) -> anyhow::Result<(Vec<Target>, Vec<Row>)> {
    if let Some(name) = only
        && ctx.flock.get(name).is_none()
    {
        return Err(CliError::err(
            "unknown_machine",
            format!("no machine {name} in flock.toml"),
        ));
    }
    let live = |name: &str| ctx.live.get(name).copied().unwrap_or(0);
    let mut targets = Vec::new();
    let mut skipped = Vec::new();
    let mut head: Option<Target> = None;
    for m in &ctx.flock.machines {
        if only.is_some_and(|o| o != m.name) {
            continue;
        }
        let skip = |why: &str| Row {
            machine: m.name.clone(),
            head: false,
            status: Status::Skipped,
            from: None,
            live_tasks: live(&m.name),
            why: Some(why.to_string()),
        };
        if m.local {
            match &mut head {
                Some(h) => h.live += live(&m.name),
                None => {
                    head = Some(Target {
                        name: m.name.clone(),
                        reach: Reach::Here,
                        live: live(&m.name),
                    })
                }
            }
        } else if m.pull {
            skipped.push(skip(
                "a pull machine: the head does not reach it, and its own serve runs its tasks; update it there with install.sh",
            ));
        } else if let Some(login) = &m.ssh {
            targets.push(Target {
                name: m.name.clone(),
                reach: Reach::Ssh(login.clone()),
                live: live(&m.name),
            });
        } else {
            skipped.push(skip(
                "reached through a command, which an update does not use; update it there with install.sh",
            ));
        }
    }
    // The head is in every update but one that names another machine.
    if only.is_none() && head.is_none() {
        head = Some(Target {
            name: "head".into(),
            reach: Reach::Here,
            live: 0,
        });
    }
    targets.extend(head);
    Ok((targets, skipped))
}

/// What a machine said it runs, and the cached binary that goes there.
struct Look {
    from: Option<String>,
    file: PathBuf,
    sum: String,
}

/// The remote command that reads a machine's platform and its pastor.
const LOOK: &str =
    "uname -sm; getconf LONG_BIT; \"$HOME\"/.local/bin/pastor --version 2>/dev/null || echo none";

/// Run `remote` on `t`: through ssh, or `sh -c` on the head's host.
fn on(
    ctx: &Ctx,
    t: &Target,
    remote: &str,
    stdin: Option<&std::path::Path>,
) -> std::io::Result<crate::setup::CmdOutput> {
    match &t.reach {
        Reach::Here => ctx
            .runner
            .run("sh", &["-c".to_string(), remote.to_string()], stdin),
        Reach::Ssh(login) => ctx
            .runner
            .run("ssh", &ssh_args(ctx.paths, login, remote), stdin),
    }
}

/// The machine's platform and pastor, and the cached release for it.
fn look(ctx: &Ctx, t: &Target, version: &str) -> Result<Look, String> {
    let out = on(ctx, t, LOOK, None).map_err(|e| format!("could not run ssh: {e}"))?;
    if !out.success {
        return Err(format!("could not reach it: {}", tail(&out.stderr)));
    }
    let mut lines = out.stdout.lines().map(str::trim).filter(|l| !l.is_empty());
    let mut uname = lines.next().unwrap_or_default().split_whitespace();
    let os = uname.next().unwrap_or_default().to_lowercase();
    let arch = match uname.next().unwrap_or_default() {
        "arm64" => "aarch64".to_string(),
        other => other.to_string(),
    };
    let bits = if lines.next() == Some("32") { 32 } else { 64 };
    let from = lines
        .next()
        .filter(|l| *l != "none")
        .and_then(|l| l.split_whitespace().last())
        .map(str::to_string);
    let facts = crate::provision::run::Facts {
        os,
        arch,
        bits,
        distro: None,
        like: Vec::new(),
    };
    let target = crate::artifacts::target_for(&facts, "pastor").map_err(|e| e.to_string())?;
    let artifact = Artifact::new("pastor", version, &target).map_err(|e| e.to_string())?;
    let (file, sum) = crate::artifacts::cached(ctx.paths, &artifact).ok_or_else(|| {
        format!("the head's artifact cache does not hold pastor {version} for {target}")
    })?;
    Ok(Look { from, file, sum })
}

/// The script that installs the checked binary at `STAGE` on a machine.
/// It refuses a binary that does not say it is `version`, keeps the old one
/// as `pastor.old` and puts it back if any later step fails, reruns `pastor
/// setup` for the units the machine has (`--enable`, which writes a changed
/// unit and starts nothing), restarts pastor's serve only when it runs, and
/// waits for it to answer as `version`. herdr's unit is rewritten for its
/// next start; herdr itself is not restarted.
pub(crate) fn install_command(version: &str) -> String {
    let want = shell_quote(&format!("pastor {version}"));
    let json = shell_quote(&format!("\"version\":\"{version}\""));
    format!(
        r#"set -e
new={STAGE}/pastor; bin={BIN}; p="$bin/pastor"
got=$("$new" --version)
[ "$got" = {want} ] || {{ echo "the copied binary says $got" >&2; exit 4; }}
mkdir -p "$bin"
had=; if [ -f "$p" ]; then cp -p "$p" "$bin/pastor.old"; had=1; fi
served=
back() {{ st=$?; if [ "$st" -ne 0 ] && [ -n "$had" ]; then mv -f "$bin/pastor.old" "$p"; if [ -n "$served" ]; then restart || true; fi; fi; }}
trap back EXIT
cp "$new" "$bin/pastor.new"; chmod 755 "$bin/pastor.new"; mv -f "$bin/pastor.new" "$p"
if [ "$(uname -s)" = Darwin ]; then
  dom="gui/$(id -u)"; la="$HOME/Library/LaunchAgents"
  restart() {{ launchctl kickstart -k "$dom/pastor.serve"; }}
  if [ -f "$la/pastor.serve.plist" ] && launchctl print "$dom/pastor.serve" >/dev/null 2>&1; then
    "$p" setup launchd --yes --enable; served=1; restart
  fi
  if [ -f "$la/pastor.herdr.plist" ] && launchctl print "$dom/pastor.herdr" >/dev/null 2>&1; then
    "$p" setup launchd --herdr --yes --enable
  fi
else
  ud="$HOME/.config/systemd/user"
  restart() {{ systemctl --user restart pastor.service; }}
  if [ -f "$ud/pastor.service" ] && systemctl --user is-active --quiet pastor.service; then
    "$p" setup systemd --yes --enable; served=1; restart
  fi
  if [ -f "$ud/herdr.service" ]; then "$p" setup systemd --herdr --yes --enable; fi
fi
if [ -z "$served" ] && "$p" serve status >/dev/null 2>&1; then
  restart() {{ "$p" serve stop && "$p" serve; }}
  served=1; restart
fi
if [ -n "$served" ]; then
  i=0
  until "$p" serve status --json 2>/dev/null | grep -qF {json}; do
    i=$((i+1)); [ "$i" -lt 30 ] || {{ echo "pastor serve does not answer as {version}" >&2; exit 5; }}
    sleep 1
  done
fi
rm -f "$new" "$bin/pastor.old"
"$p" --version"#
    )
}

/// Copy and install on one machine. `Err` says why it stopped; the
/// machine's old binary is where it was.
fn update_one(ctx: &Ctx, t: &Target, look: &Look, version: &str) -> Result<(), String> {
    let out = on(
        ctx,
        t,
        &copy_command(STAGE, "pastor", &look.sum),
        Some(&look.file),
    )
    .map_err(|e| format!("could not run ssh: {e}"))?;
    // The machine prints the sum it found before it compares.
    let got = out.stdout.split_whitespace().next().unwrap_or_default();
    if !got.is_empty() && got != look.sum {
        return Err(format!(
            "pastor arrived with sha256 {got}, not {}; the old binary is in place",
            look.sum
        ));
    }
    if !out.success {
        return Err(format!(
            "could not copy pastor there: {}",
            tail(&out.stderr)
        ));
    }
    let out = on(ctx, t, &install_command(version), None)
        .map_err(|e| format!("could not run ssh: {e}"))?;
    let said = out.stdout.lines().last().unwrap_or_default().trim();
    if !out.success || said != format!("pastor {version}") {
        return Err(format!(
            "the install did not finish, so the old binary is back: {}",
            tail(&out.stderr)
        ));
    }
    Ok(())
}

/// Update the fleet to `version`, or with `dry_run` say what would be done
/// and change nothing.
pub fn update(
    ctx: &Ctx,
    version: &str,
    only: Option<&str>,
    dry_run: bool,
) -> anyhow::Result<Report> {
    Artifact::new("pastor", version, "x")
        .map_err(|e| CliError::err("invalid_args", format!("{version:?}: {e}")))?;
    let (targets, mut rows) = targets(ctx, only)?;
    let row = |t: &Target, status, from: Option<String>, why: Option<String>| Row {
        machine: t.name.clone(),
        head: t.reach == Reach::Here,
        status,
        from,
        live_tasks: t.live,
        why,
    };
    // Every machine looked at before any changes.
    let looks: Vec<Result<Look, String>> = targets.iter().map(|t| look(ctx, t, version)).collect();
    if looks.iter().any(Result::is_err) {
        let first = targets[looks.iter().position(Result::is_err).unwrap()]
            .name
            .clone();
        for (t, l) in targets.iter().zip(looks) {
            rows.push(match l {
                Err(why) => row(t, Status::Failed, None, Some(why)),
                Ok(l) => row(
                    t,
                    Status::Skipped,
                    l.from,
                    Some(format!("nothing was changed: {first} could not be updated")),
                ),
            });
        }
        return Ok(Report {
            version: version.into(),
            ok: false,
            rows,
        });
    }
    let mut stopped: Option<String> = None;
    for (t, l) in targets.iter().zip(looks.into_iter().map(Result::unwrap)) {
        let r = if let Some(at) = &stopped {
            row(
                t,
                Status::Skipped,
                l.from,
                Some(format!("the rollout stopped at {at}")),
            )
        } else if l.from.as_deref() == Some(version) {
            row(
                t,
                Status::Skipped,
                l.from,
                Some(format!("it runs pastor {version} already")),
            )
        } else if dry_run {
            row(t, Status::WouldUpdate, l.from, None)
        } else {
            match update_one(ctx, t, &l, version) {
                Ok(()) => row(t, Status::Updated, l.from, None),
                Err(why) => {
                    stopped = Some(t.name.clone());
                    row(t, Status::Failed, l.from, Some(why))
                }
            }
        };
        rows.push(r);
    }
    Ok(Report {
        version: version.into(),
        ok: rows.iter().all(|r| r.status != Status::Failed),
        rows,
    })
}

/// Tasks with a pane, by machine, from the head's store.
pub fn live_tasks(paths: &Paths) -> anyhow::Result<HashMap<String, usize>> {
    let store = crate::store::Store::open(&paths.db_file())?;
    let tasks = store.list_tasks(&crate::store::TaskFilter {
        states: Some(crate::task::PANE_OWNING_STATES.to_vec()),
        ..Default::default()
    })?;
    let mut live = HashMap::new();
    for t in tasks {
        if let Some(m) = t.machine {
            *live.entry(m).or_insert(0) += 1;
        }
    }
    Ok(live)
}

#[derive(clap::Subcommand, Debug)]
pub enum FleetCmd {
    /// Put pastor <version> from the head's artifact cache on every machine the head reaches, the head last
    Update(UpdateArgs),
}

#[derive(clap::Args, Debug)]
pub struct UpdateArgs {
    /// The pastor version, as the artifact cache holds it (like 0.10.0)
    pub version: String,
    /// Only this machine, as flock.toml names it
    #[arg(long)]
    pub machine: Option<String>,
    /// Look at every machine and say what would be updated; change nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Print as a JSON object: version, ok, and per machine its status, the version it ran, its live tasks and why
    #[arg(long)]
    pub json: bool,
}

/// `pastor fleet ...`, on the head.
pub fn cli(paths: &Paths, cmd: FleetCmd) -> anyhow::Result<()> {
    let FleetCmd::Update(args) = cmd;
    let flock = Flock::load(&paths.flock_file())?;
    let ctx = Ctx {
        paths,
        flock: &flock,
        live: live_tasks(paths)?,
        runner: &crate::bootstrap::push::SystemPushRunner,
    };
    let report = update(&ctx, &args.version, args.machine.as_deref(), args.dry_run)?;
    if args.json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        print!("{}", human(&report));
    }
    if report.ok {
        Ok(())
    } else {
        Err(CliError::err(
            "update_failed",
            "the update stopped; the machines after the failed one were not changed",
        ))
    }
}

fn human(r: &Report) -> String {
    let rows: Vec<Vec<String>> = r
        .rows
        .iter()
        .map(|row| {
            let status = match row.status {
                Status::Updated => "updated",
                Status::WouldUpdate => "would update",
                Status::Skipped => "skipped",
                Status::Failed => "failed",
            };
            vec![
                if row.head {
                    format!("{} (head)", row.machine)
                } else {
                    row.machine.clone()
                },
                status.to_string(),
                row.from.clone().unwrap_or_else(|| "-".into()),
                row.live_tasks.to_string(),
                row.why.clone().unwrap_or_default(),
            ]
        })
        .collect();
    crate::cli::table(&["MACHINE", "STATUS", "FROM", "LIVE", "WHY"], &rows)
}

#[cfg(test)]
mod tests;
