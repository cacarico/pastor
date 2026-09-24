use std::os::unix::process::CommandExt;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use pastor::config::flock::{Flock, MachineConfig};
use pastor::config::job::{check_name, job_path, set_enabled};
use pastor::config::{PastorConfig, Paths, parse_duration};
use pastor::herdr::{ConnectorExt, Endpoint, shell_quote};
use pastor::ipc::{IpcRequest, IpcResponse, daemon_running, request};
use pastor::machine::{ChannelState, MachineStatus};
use pastor::scheduler::{JobRunReport, JobStatus, Scheduler};
use pastor::store::{Store, TaskFilter};
use pastor::task::{DispatchSpec, Task, TaskState, parse_task_id};

#[derive(Parser, Debug)]
#[command(
    name = "pastor",
    version,
    about = "run coding agents on machines you own"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the daemon: scheduler, machine channels, dispatch
    Serve,
    /// Create a one-off task and dispatch it
    Run(RunArgs),
    /// List tasks across the flock
    List(ListArgs),
    /// Inspect a task
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Manage the machines in the flock
    Machine {
        #[command(subcommand)]
        cmd: MachineCmd,
    },
    /// Attach to a task's agent terminal (ctrl+b q detaches)
    Attach { task: String },
    /// Open the full herdr UI on a machine
    Open { machine: String },
    /// Run one scheduler pass now and report what it did
    Tick(TickArgs),
    /// Re-read the job files now instead of at the next tick
    Reload,
    /// Manage jobs (files in ~/.config/pastor/jobs/)
    Job {
        #[command(subcommand)]
        cmd: JobCmd,
    },
    /// Print a shell completion script (fish, bash, zsh, ...) to stdout
    Completions { shell: clap_complete::Shell },
}

#[derive(Args, Debug)]
struct TickArgs {
    /// Run connectors and show what would be created; write nothing
    #[arg(long)]
    dry_run: bool,
    /// Only this job, and run it whether or not it is due
    #[arg(long)]
    job: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand, Debug)]
enum JobCmd {
    /// Every job file: schedule, enabled, last run, next run, last result
    List {
        #[arg(long)]
        json: bool,
    },
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
    /// Fire a job now, ignoring its schedule, the overlap rule and `enabled`
    Run {
        name: String,
    },
}

#[derive(Args, Debug)]
struct RunArgs {
    prompt: String,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    machine: Option<String>,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long)]
    worktree: bool,
    /// Branch for the worktree (needs --worktree; a plain workspace has no branch)
    #[arg(long, requires = "worktree")]
    branch: Option<String>,
    #[arg(long = "tag")]
    tags: Vec<String>,
    #[arg(long)]
    timeout: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ListArgs {
    /// Only tasks from this job (omit for one-off `run` tasks)
    #[arg(long)]
    job: Option<String>,
    /// Only tasks on this machine
    #[arg(long)]
    machine: Option<String>,
    /// Only blocked tasks, needing a human
    #[arg(long, group = "list_filter")]
    blocked: bool,
    /// Only done tasks
    #[arg(long, group = "list_filter")]
    done: bool,
    /// Include closed tasks
    #[arg(long, group = "list_filter")]
    all: bool,
    /// Print full task records as JSON instead of a table
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand, Debug)]
enum TaskCmd {
    Show {
        task: String,
        #[arg(long)]
        json: bool,
    },
    Read {
        task: String,
        #[arg(long, default_value_t = 40)]
        lines: u32,
    },
}

#[derive(Subcommand, Debug)]
enum MachineCmd {
    Add {
        name: String,
        ssh: Option<String>,
        #[arg(long)]
        local: bool,
        #[arg(long, num_args = 1.., allow_hyphen_values = true)]
        command: Option<Vec<String>>,
        #[arg(long, default_value = "default")]
        session: String,
        #[arg(long, default_value_t = 2)]
        max_agents: u32,
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Also save it in herdr's sidebar (runs `herdr machine add`)
        #[arg(long, conflicts_with_all = ["local", "command"])]
        herdr: bool,
    },
    Remove {
        name: String,
        /// Also remove herdr's saved machine with this label
        #[arg(long)]
        herdr: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Status {
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pastor=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let paths = match Paths::from_env() {
        Ok(p) => p,
        Err(err) => fail("config_error", &err.to_string()),
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async {
        match cli.command {
            Command::Serve => pastor::daemon::serve(paths).await,
            Command::Run(args) => run(&paths, args).await,
            Command::List(args) => list(&paths, args).await,
            Command::Task { cmd } => task(&paths, cmd).await,
            Command::Machine { cmd } => machine(&paths, cmd).await,
            Command::Attach { task } => attach(&paths, &task).await,
            Command::Open { machine } => open(&paths, &machine).await,
            Command::Tick(args) => tick(&paths, args).await,
            Command::Reload => reload(&paths).await,
            Command::Job { cmd } => job(&paths, cmd).await,
            Command::Completions { shell } => {
                let mut cmd = <Cli as clap::CommandFactory>::command();
                clap_complete::generate(shell, &mut cmd, "pastor", &mut std::io::stdout());
                Ok(())
            }
        }
    });
    if let Err(err) = result {
        fail("runtime_error", &format!("{err:#}"));
    }
}

fn fail(code: &str, message: &str) -> ! {
    eprintln!("{}", serde_json::json!({"code": code, "message": message}));
    std::process::exit(1)
}

async fn ask(paths: &Paths, req: IpcRequest) -> anyhow::Result<IpcResponse> {
    let socket = paths.socket_file();
    let resp = request(&socket, &req).await.map_err(|e| {
        anyhow::anyhow!("pastor serve is not running ({e}); start it with `pastor serve`")
    })?;
    if let IpcResponse::Error { code, message } = &resp {
        fail(code, message);
    }
    Ok(resp)
}

async fn run(paths: &Paths, a: RunArgs) -> anyhow::Result<()> {
    let config = PastorConfig::load(&paths.config_file())?;
    let timeout = a
        .timeout
        .as_deref()
        .map(parse_duration)
        .transpose()
        .map_err(|e| anyhow::anyhow!(e))?
        .unwrap_or(config.timeout_duration());
    let spec = DispatchSpec {
        agent: a.agent.unwrap_or(config.defaults.agent),
        agent_args: vec![],
        repo: a.repo,
        worktree: a.worktree,
        branch: a.branch,
        machine: a.machine,
        tags: a.tags,
        timeout_secs: timeout.as_secs(),
    };
    let IpcResponse::Task(t) = ask(
        paths,
        IpcRequest::Run {
            prompt: a.prompt,
            spec,
        },
    )
    .await?
    else {
        unreachable!()
    };
    print_task(&t, a.json);
    Ok(())
}

fn print_task(t: &Task, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(t).unwrap());
    } else {
        println!(
            "{}",
            pastor::cli::table(
                &pastor::cli::TASK_HEADER,
                &pastor::cli::task_rows(std::slice::from_ref(t))
            )
        );
    }
}

/// Turn a `machine status` probe into a table/JSON row's fields. Pure so the
/// classification (including the agent.list-failed case) is unit-testable
/// without a live herdr.
///
/// `agent_count` is `None` when `ping` itself failed (agent.list was never
/// called), `Some(Err(_))` when ping succeeded but agent.list failed, and
/// `Some(Ok(n))` for the normal case.
///
/// Connect/ping failure classification (server down / unreachable / error) is
/// unchanged; only the ping-succeeded, agent.list-failed case is new: it must
/// not report the row as reachable with zero agents.
type MachineStatusRow = (
    &'static str,
    Option<String>,
    Option<u32>,
    Option<usize>,
    Option<String>,
);

fn machine_status_row(
    ping: Result<pastor::herdr::Pong, pastor::herdr::CallError>,
    agent_count: Option<Result<usize, pastor::herdr::CallError>>,
) -> MachineStatusRow {
    match ping {
        Ok(p) => {
            let compatible = p.protocol >= pastor::MIN_HERDR_PROTOCOL;
            match agent_count {
                Some(Ok(n)) => (
                    if compatible {
                        "reachable"
                    } else {
                        "incompatible"
                    },
                    Some(p.version),
                    Some(p.protocol),
                    Some(n),
                    if compatible {
                        None
                    } else {
                        Some(format!(
                            "protocol {} < {}",
                            p.protocol,
                            pastor::MIN_HERDR_PROTOCOL
                        ))
                    },
                ),
                Some(Err(e)) => (
                    "error",
                    Some(p.version),
                    Some(p.protocol),
                    None,
                    Some(format!("agent.list: {e}")),
                ),
                // The caller always attempts agent.list once ping succeeds; treat a
                // missing count the same as an agent.list failure rather than
                // pretending the machine is reachable with zero agents.
                None => (
                    "error",
                    Some(p.version),
                    Some(p.protocol),
                    None,
                    Some("agent.list: not attempted".into()),
                ),
            }
        }
        Err(e) => {
            let message = e.to_string();
            (
                if message.contains("herdr.sock") {
                    "server down"
                } else if e.is_transport() {
                    "unreachable"
                } else {
                    "error"
                },
                None,
                None,
                None,
                Some(message),
            )
        }
    }
}

/// Every state `pastor list` shows without `--all`: everything except `Closed`,
/// matching the spec's CLI table ("hides closed by default"). `Failed` belongs
/// here too — a failed task needs a human same as a blocked one, and hiding it by
/// default was a plan defect, not a design choice.
fn default_list_states() -> Vec<TaskState> {
    vec![
        TaskState::Queued,
        TaskState::Starting,
        TaskState::Running,
        TaskState::Blocked,
        TaskState::Done,
        TaskState::Stale,
        TaskState::Failed,
    ]
}

async fn list(paths: &Paths, a: ListArgs) -> anyhow::Result<()> {
    let states = if a.blocked {
        Some(vec![TaskState::Blocked])
    } else if a.done {
        Some(vec![TaskState::Done])
    } else if a.all {
        None
    } else {
        Some(default_list_states())
    };
    let filter = TaskFilter {
        job: a.job,
        machine: a.machine,
        states,
    };
    let tasks = if daemon_running(&paths.socket_file()).await {
        let IpcResponse::Tasks(ts) = ask(paths, IpcRequest::List { filter }).await? else {
            unreachable!()
        };
        ts
    } else {
        eprintln!("pastor serve is not running; showing the last known state");
        paths.ensure()?;
        Store::open(&paths.db_file())?.list_tasks(&filter)?
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&tasks)?);
    } else if tasks.is_empty() {
        println!("no tasks");
    } else {
        println!(
            "{}",
            pastor::cli::table(&pastor::cli::TASK_HEADER, &pastor::cli::task_rows(&tasks))
        );
    }
    Ok(())
}

fn task_id(s: &str) -> i64 {
    parse_task_id(s)
        .unwrap_or_else(|| fail("usage_error", &format!("{s} is not a task id like t-12")))
}

async fn task(paths: &Paths, cmd: TaskCmd) -> anyhow::Result<()> {
    match cmd {
        TaskCmd::Show { task, json } => {
            let id = task_id(&task);
            let t = if daemon_running(&paths.socket_file()).await {
                let IpcResponse::Task(t) = ask(paths, IpcRequest::TaskShow { id }).await? else {
                    unreachable!()
                };
                t
            } else {
                paths.ensure()?;
                Store::open(&paths.db_file())?
                    .get_task(id)?
                    .unwrap_or_else(|| fail("task_not_found", &task))
            };
            print_task(&t, json);
        }
        TaskCmd::Read { task, lines } => {
            let IpcResponse::Text(text) = ask(
                paths,
                IpcRequest::TaskRead {
                    id: task_id(&task),
                    lines,
                },
            )
            .await?
            else {
                unreachable!()
            };
            print!("{text}");
        }
    }
    Ok(())
}

async fn machine(paths: &Paths, cmd: MachineCmd) -> anyhow::Result<()> {
    let path = paths.flock_file();
    match cmd {
        MachineCmd::Add {
            name,
            ssh,
            local,
            command,
            session,
            max_agents,
            tags,
            herdr,
        } => {
            let mut f = Flock::load(&path)?;
            let m = MachineConfig {
                name: name.clone(),
                local,
                ssh,
                command,
                session,
                max_agents,
                tags,
            };
            f.add(m).map_err(|e| anyhow::anyhow!(e))?;
            f.save(&path)?;
            println!("added {name} to {}", path.display());
            let target = f.get(&name).and_then(|m| m.ssh.clone());
            match (herdr, target) {
                // The flock file is already written: a herdr failure below is
                // reported, not rolled back, so the two lists never diverge
                // silently in the other direction either.
                (true, Some(target)) => {
                    herdr_cmd(&[
                        "machine",
                        "add",
                        &target,
                        "--label",
                        &name,
                        "--remote-session",
                        &f.get(&name).map(|m| m.session.clone()).unwrap_or_default(),
                    ]);
                    println!("saved in herdr's sidebar as {name}");
                }
                (false, Some(target)) => println!(
                    "to see it in your laptop's herdr sidebar: herdr machine add {target} --label {name} (or pass --herdr)"
                ),
                _ => {}
            }
            println!(
                "{}",
                machine_edit_hint(daemon_running(&paths.socket_file()).await)
            );
        }
        MachineCmd::Remove { name, herdr } => {
            let mut f = Flock::load(&path)?;
            let target = f.get(&name).and_then(|m| m.ssh.clone());
            anyhow::ensure!(f.remove(&name), "machine {name} not found");
            f.save(&path)?;
            println!(
                "removed {name}; {}",
                machine_edit_hint(daemon_running(&paths.socket_file()).await)
            );
            if herdr {
                // herdr removes by profile id; the label is all pastor knows.
                let list = herdr_cmd(&["machine", "list"]);
                match saved_machine_id(&list, &name) {
                    Some(id) => {
                        herdr_cmd(&["machine", "remove", &id]);
                        println!("removed {name} from herdr's sidebar");
                    }
                    None => {
                        eprintln!(
                            "herdr has no saved machine labelled {name}; nothing to remove there"
                        );
                        // The same host is often saved under another label (added
                        // by hand, or from an earlier flock name). Point at it
                        // rather than remove it: one host can legitimately be
                        // saved several times, for different sessions.
                        let same_host = target
                            .as_deref()
                            .map(|t| saved_machines_at(&list, t))
                            .unwrap_or_default();
                        if !same_host.is_empty() {
                            eprintln!("herdr does have this host saved under another label:");
                            for (id, label) in same_host {
                                eprintln!("  {label}: herdr machine remove {id}");
                            }
                        }
                    }
                }
            }
        }
        MachineCmd::List { json } => {
            let statuses: Vec<MachineStatus> = if daemon_running(&paths.socket_file()).await {
                let IpcResponse::Machines(ms) = ask(paths, IpcRequest::FlockList).await? else {
                    unreachable!()
                };
                ms
            } else {
                eprintln!(
                    "no head is running (start one with `pastor serve`); showing the flock file only, nothing is connected"
                );
                Flock::load(&path)?
                    .machines
                    .iter()
                    .map(|m| MachineStatus {
                        name: m.name.clone(),
                        endpoint: Endpoint::from_machine(m, paths).describe(),
                        channel: ChannelState::Connecting,
                        herdr_version: None,
                        protocol: None,
                        error: Some("no head running; start pastor serve".into()),
                        live: 0,
                        max_agents: m.max_agents,
                        tags: m.tags.clone(),
                    })
                    .collect()
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&statuses)?);
            } else {
                println!(
                    "{}",
                    pastor::cli::table(
                        &pastor::cli::MACHINE_HEADER,
                        &pastor::cli::machine_rows(&statuses)
                    )
                );
            }
        }
        MachineCmd::Status { name, json } => {
            let f = Flock::load(&path)?;
            // A typo would otherwise filter every row out and print an empty
            // table with exit 0; name it the way run, attach and open do.
            if let Some(n) = &name
                && f.get(n).is_none()
            {
                fail(
                    "unknown_machine",
                    &format!("machine {n} is not in the flock"),
                );
            }
            let mut rows = Vec::new();
            for m in f
                .machines
                .iter()
                .filter(|m| name.as_ref().is_none_or(|n| &m.name == n))
            {
                let ep = Endpoint::from_machine(m, paths);
                // Two ordinary calls, each on its own connection, exactly as the
                // daemon makes them: herdr answers one request per connection.
                let ping = ep.ping().await;
                let agent_count = match &ping {
                    Ok(_) => Some(ep.agent_list().await.map(|a| a.len())),
                    Err(_) => None,
                };
                let (status, version, protocol, agents, error) =
                    machine_status_row(ping, agent_count);
                rows.push(serde_json::json!({"name": m.name, "endpoint": ep.describe(), "status": status, "herdr_version": version, "protocol": protocol, "agents": agents, "error": error}));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                let table_rows: Vec<Vec<String>> = rows
                    .iter()
                    .map(|r| {
                        vec![
                            r["name"].as_str().unwrap_or("").into(),
                            r["status"].as_str().unwrap_or("").into(),
                            r["herdr_version"].as_str().unwrap_or("-").into(),
                            r["agents"]
                                .as_u64()
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "-".into()),
                            r["error"].as_str().unwrap_or("").into(),
                        ]
                    })
                    .collect();
                println!(
                    "{}",
                    pastor::cli::table(
                        &["NAME", "STATUS", "HERDR", "AGENTS", "ERROR"],
                        &table_rows
                    )
                );
            }
        }
    }
    Ok(())
}

/// What to do after editing the flock. The daemon reads `flock.toml` only at
/// start (hot reload is a later plan), so a running one must be restarted;
/// with none running, "restart" reads as "already picked up" and misleads.
fn machine_edit_hint(daemon_up: bool) -> &'static str {
    if daemon_up {
        "restart pastor serve to pick it up (the flock does not reload while it runs)"
    } else {
        "start pastor serve to use it"
    }
}

/// Run the local `herdr` CLI and return its stdout. Any failure (not on PATH,
/// non-zero exit) is a runtime error carrying herdr's stderr, so the user sees
/// herdr's own words rather than a generic exit status.
fn herdr_cmd(args: &[&str]) -> String {
    let out = match std::process::Command::new("herdr").args(args).output() {
        Ok(out) => out,
        Err(err) => fail("herdr_error", &format!("cannot run herdr: {err}")),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            out.status.to_string()
        } else {
            stderr
        };
        fail(
            "herdr_error",
            &format!("herdr {}: {detail}", args.join(" ")),
        );
    }
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The profile id of the saved herdr machine labelled `label`, from the output
/// of `herdr machine list` (`id<TAB>label<TAB>target<TAB>session<TAB>enabled`
/// per line). Only the label column is matched: an id or a target that happens
/// to equal the name is not the same machine.
fn saved_machine_id(list: &str, label: &str) -> Option<String> {
    list.lines().find_map(|line| {
        let mut cols = line.split('\t');
        let id = cols.next()?;
        (cols.next()? == label).then(|| id.to_string())
    })
}

/// Every saved herdr machine whose target column equals `target`, as
/// `(id, label)`, from the same `herdr machine list` output.
fn saved_machines_at(list: &str, target: &str) -> Vec<(String, String)> {
    list.lines()
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let id = cols.next()?;
            let label = cols.next()?;
            (cols.next()? == target).then(|| (id.to_string(), label.to_string()))
        })
        .collect()
}

/// The command run on the remote host by `ssh -t target <this>`. `session` and
/// `agent` both come from data an attacker could shape (a flock session name,
/// a task's agent name derived from user input via the daemon) and are quoted
/// with `shell_quote` so the remote shell can't be made to run anything else.
fn attach_remote_command(session: &str, agent: &str) -> String {
    format!(
        "herdr --session {} agent attach {}",
        shell_quote(session),
        shell_quote(agent)
    )
}

async fn attach(paths: &Paths, task: &str) -> anyhow::Result<()> {
    let id = task_id(task);
    paths.ensure()?;
    let t = Store::open(&paths.db_file())?
        .get_task(id)?
        .unwrap_or_else(|| fail("task_not_found", task));
    let (Some(machine), Some(agent)) = (t.machine.clone(), t.agent_name.clone()) else {
        fail("no_agent", &format!("{} has no agent yet", t.display_id()))
    };
    if !t.state.occupies_pane() {
        fail(
            "no_agent",
            &format!("{} is {}; nothing to attach to", t.display_id(), t.state),
        );
    }
    let f = Flock::load(&paths.flock_file())?;
    let m = f
        .get(&machine)
        .unwrap_or_else(|| fail("unknown_machine", &machine));
    let err = if let Some(target) = &m.ssh {
        std::process::Command::new("ssh")
            .arg("-t")
            .arg(target)
            .arg(attach_remote_command(&m.session, &agent))
            .exec()
    } else if m.local {
        std::process::Command::new("herdr")
            .args(["--session", &m.session, "agent", "attach", &agent])
            .exec()
    } else {
        fail(
            "no_terminal",
            "command machines have no terminal to attach to",
        )
    };
    Err(anyhow::anyhow!("exec failed: {err}"))
}

async fn open(paths: &Paths, machine: &str) -> anyhow::Result<()> {
    let f = Flock::load(&paths.flock_file())?;
    let m = f
        .get(machine)
        .unwrap_or_else(|| fail("unknown_machine", machine));
    let err = if let Some(target) = &m.ssh {
        std::process::Command::new("herdr")
            .args(["--remote", target, "--session", &m.session])
            .exec()
    } else if m.local {
        std::process::Command::new("herdr")
            .args(["--session", &m.session])
            .exec()
    } else {
        fail("no_terminal", "command machines have no UI to open")
    };
    Err(anyhow::anyhow!("exec failed: {err}"))
}

fn print_runs(runs: &[JobRunReport], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(runs)?);
    } else if runs.is_empty() {
        println!("no jobs");
    } else {
        println!(
            "{}",
            pastor::cli::table(&pastor::cli::RUN_HEADER, &pastor::cli::run_rows(runs))
        );
    }
    Ok(())
}

fn print_jobs(jobs: &[JobStatus], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(jobs)?);
    } else if jobs.is_empty() {
        println!("no jobs");
    } else {
        println!(
            "{}",
            pastor::cli::table(&pastor::cli::JOB_HEADER, &pastor::cli::job_rows(jobs))
        );
    }
    Ok(())
}

/// Offline scheduler over the same store, for `tick` and `job list` when
/// `pastor serve` is down. Tasks it queues wait for the daemon.
fn standalone(paths: &Paths) -> anyhow::Result<Scheduler> {
    let config = PastorConfig::load(&paths.config_file())?;
    paths.ensure()?;
    let store = Arc::new(Store::open(&paths.db_file())?);
    Ok(Scheduler::standalone(paths.clone(), &config, store))
}

async fn tick(paths: &Paths, a: TickArgs) -> anyhow::Result<()> {
    let runs = if daemon_running(&paths.socket_file()).await {
        let IpcResponse::Runs(runs) = ask(
            paths,
            IpcRequest::Tick {
                job: a.job,
                dry_run: a.dry_run,
            },
        )
        .await?
        else {
            unreachable!()
        };
        runs
    } else {
        eprintln!(
            "pastor serve is not running; running the pass here (new tasks stay queued until it starts)"
        );
        let mut s = standalone(paths)?;
        s.tick_now(a.job.as_deref(), a.dry_run, chrono::Utc::now())
            .await
    };
    print_runs(&runs, a.json)
}

async fn reload(paths: &Paths) -> anyhow::Result<()> {
    let IpcResponse::Jobs(jobs) = ask(paths, IpcRequest::Reload).await? else {
        unreachable!()
    };
    print_jobs(&jobs, false)
}

async fn job(paths: &Paths, cmd: JobCmd) -> anyhow::Result<()> {
    match cmd {
        JobCmd::List { json } => {
            let jobs = if daemon_running(&paths.socket_file()).await {
                let IpcResponse::Jobs(jobs) = ask(paths, IpcRequest::JobList).await? else {
                    unreachable!()
                };
                jobs
            } else {
                eprintln!(
                    "pastor serve is not running; showing the job files and the last known state"
                );
                let mut s = standalone(paths)?;
                s.reload();
                s.statuses(chrono::Utc::now())
            };
            print_jobs(&jobs, json)?;
        }
        JobCmd::Enable { name } => toggle(paths, &name, true).await?,
        JobCmd::Disable { name } => toggle(paths, &name, false).await?,
        JobCmd::Run { name } => {
            let IpcResponse::Text(msg) = ask(paths, IpcRequest::JobRun { name }).await? else {
                unreachable!()
            };
            println!("{msg}");
        }
    }
    Ok(())
}

async fn toggle(paths: &Paths, name: &str, enabled: bool) -> anyhow::Result<()> {
    // Validate before joining: `job_path` just formats and joins, so an
    // unchecked name like "../pastor" would resolve outside the jobs
    // directory instead of failing not-found.
    if let Err(e) = check_name(name) {
        fail("job_not_found", &e);
    }
    let path = job_path(&paths.jobs_dir(), name);
    if !path.exists() {
        fail("job_not_found", &format!("no job file {}", path.display()));
    }
    set_enabled(&path, enabled)?;
    let verb = if enabled { "enabled" } else { "disabled" };
    if daemon_running(&paths.socket_file()).await {
        ask(paths, IpcRequest::Reload).await?;
        println!("{verb} {name}");
    } else {
        println!("{verb} {name}; applies when pastor serve starts");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_list_states_hides_only_closed() {
        let states = default_list_states();
        assert!(!states.contains(&TaskState::Closed), "{states:?}");
        assert!(
            states.contains(&TaskState::Failed),
            "a failed task needs a human just as much as a blocked one: {states:?}"
        );
        for s in [
            TaskState::Queued,
            TaskState::Starting,
            TaskState::Running,
            TaskState::Blocked,
            TaskState::Done,
            TaskState::Stale,
        ] {
            assert!(states.contains(&s), "{s} missing from {states:?}");
        }
    }

    fn pong() -> pastor::herdr::Pong {
        pastor::herdr::Pong {
            version: "0.9.1".into(),
            protocol: pastor::MIN_HERDR_PROTOCOL,
        }
    }

    fn agent_list_error() -> pastor::herdr::CallError {
        pastor::herdr::CallError::from(pastor::herdr::HerdrError::Api {
            code: "internal_error".into(),
            message: "boom".into(),
        })
    }

    #[test]
    fn agent_list_failure_is_surfaced_not_hidden_as_zero_agents() {
        let (status, version, protocol, agents, error) =
            machine_status_row(Ok(pong()), Some(Err(agent_list_error())));
        assert_eq!(status, "error");
        assert_eq!(version, Some("0.9.1".into()));
        assert_eq!(protocol, Some(pastor::MIN_HERDR_PROTOCOL));
        assert_eq!(agents, None, "agent count must show as absent, not zero");
        let error = error.expect("the agent.list error must be surfaced");
        assert!(error.contains("boom"), "{error}");
    }

    #[test]
    fn agent_list_success_still_reports_reachable() {
        let (status, _, _, agents, error) = machine_status_row(Ok(pong()), Some(Ok(3)));
        assert_eq!(status, "reachable");
        assert_eq!(agents, Some(3));
        assert_eq!(error, None);
    }

    #[test]
    fn ping_failure_classification_is_unchanged() {
        let err = pastor::herdr::CallError::from(pastor::herdr::HerdrError::Closed);
        let (status, version, protocol, agents, error) = machine_status_row(Err(err), None);
        assert_eq!(status, "unreachable");
        assert_eq!(version, None);
        assert_eq!(protocol, None);
        assert_eq!(agents, None);
        assert!(error.is_some());
    }

    #[test]
    fn list_blocked_done_all_are_mutually_exclusive() {
        fn err(args: &[&str]) -> clap::Error {
            match Cli::try_parse_from(args) {
                Ok(_) => panic!("{args:?}: expected a usage error"),
                Err(e) => e,
            }
        }

        let e = err(&["pastor", "list", "--blocked", "--done"]);
        assert_eq!(e.kind(), clap::error::ErrorKind::ArgumentConflict);
        assert_eq!(e.exit_code(), 2);

        assert_eq!(
            err(&["pastor", "list", "--blocked", "--all"]).kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
        assert_eq!(
            err(&["pastor", "list", "--done", "--all"]).kind(),
            clap::error::ErrorKind::ArgumentConflict
        );

        // Each flag alone, and none of them, must still parse.
        for args in [
            vec!["pastor", "list"],
            vec!["pastor", "list", "--blocked"],
            vec!["pastor", "list", "--done"],
            vec!["pastor", "list", "--all"],
        ] {
            if let Err(e) = Cli::try_parse_from(&args) {
                panic!("{args:?}: {e}");
            }
        }
    }

    /// A flock edit is only picked up by a daemon start. With no daemon
    /// running there is nothing to restart, and saying so misleads: the
    /// user reads "restart" as "it is already known". Say start or restart
    /// depending on what is actually running.
    #[test]
    fn machine_edit_hint_matches_daemon_state() {
        assert_eq!(
            machine_edit_hint(true),
            "restart pastor serve to pick it up (the flock does not reload while it runs)"
        );
        assert_eq!(machine_edit_hint(false), "start pastor serve to use it");
    }

    #[test]
    fn herdr_flag_needs_an_ssh_machine() {
        fn err(args: &[&str]) -> clap::Error {
            match Cli::try_parse_from(args) {
                Ok(_) => panic!("{args:?}: expected a usage error"),
                Err(e) => e,
            }
        }
        assert_eq!(
            err(&["pastor", "machine", "add", "x", "--local", "--herdr"]).kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
        assert_eq!(
            err(&[
                "pastor",
                "machine",
                "add",
                "x",
                "--herdr",
                "--command",
                "fake"
            ])
            .kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
        Cli::try_parse_from(["pastor", "machine", "add", "x", "user@h", "--herdr"]).unwrap();
        Cli::try_parse_from(["pastor", "machine", "remove", "x", "--herdr"]).unwrap();
    }

    /// `herdr machine remove` wants the profile id; pastor knows the label.
    /// `herdr machine list` prints `id<TAB>label<TAB>target<TAB>session<TAB>enabled`.
    #[test]
    fn saved_machine_id_matches_the_label_column() {
        let list = "id-1\tother\tx@y\tdefault\tenabled\nid-2\tpi-3\tfleet@pi-3\tdefault\tenabled\n";
        assert_eq!(saved_machine_id(list, "pi-3").as_deref(), Some("id-2"));
        assert_eq!(
            saved_machine_id(list, "fleet@pi-3"),
            None,
            "a target is not a label"
        );
        assert_eq!(
            saved_machine_id(list, "id-1"),
            None,
            "an id is not a label either"
        );
        assert_eq!(saved_machine_id("", "pi-3"), None);
        assert_eq!(saved_machine_id("No saved SSH machines.\n", "pi-3"), None);
    }

    /// When no label matches, the hint lists every saved machine that points at
    /// the removed machine's SSH target, so the user can see what herdr calls it.
    #[test]
    fn saved_machines_at_target_lists_id_and_label() {
        let list = "id-1\tother\tx@y\tdefault\tenabled\nid-2\tpi-3\tfleet@pi-3\tdefault\tenabled\nid-3\tpi-3-alt\tfleet@pi-3\twork\tenabled\n";
        assert_eq!(
            saved_machines_at(list, "fleet@pi-3"),
            vec![
                ("id-2".to_string(), "pi-3".to_string()),
                ("id-3".to_string(), "pi-3-alt".to_string())
            ]
        );
        assert!(
            saved_machines_at(list, "pi-3").is_empty(),
            "a label is not a target"
        );
        assert!(saved_machines_at("No saved SSH machines.\n", "x@y").is_empty());
    }

    #[test]
    fn job_and_tick_commands_parse() {
        for args in [
            vec!["pastor", "tick"],
            vec!["pastor", "tick", "--dry-run", "--job", "a", "--json"],
            vec!["pastor", "reload"],
            vec!["pastor", "job", "list", "--json"],
            vec!["pastor", "job", "enable", "a"],
            vec!["pastor", "job", "disable", "a"],
            vec!["pastor", "job", "run", "a"],
        ] {
            if let Err(e) = Cli::try_parse_from(&args) {
                panic!("{args:?}: {e}");
            }
        }
        assert_eq!(
            Cli::try_parse_from(["pastor", "job", "enable"])
                .unwrap_err()
                .exit_code(),
            2
        );
    }

    #[test]
    fn attach_remote_command_quotes_session_and_agent() {
        assert_eq!(
            attach_remote_command("my session", "t-1"),
            "herdr --session 'my session' agent attach t-1"
        );
        assert_eq!(
            attach_remote_command("o'brien's box", "t-2"),
            "herdr --session 'o'\\''brien'\\''s box' agent attach t-2"
        );
        assert_eq!(
            attach_remote_command("default", "t-3"),
            "herdr --session default agent attach t-3"
        );
        // An empty session name must still quote to `''`, not to nothing, or the
        // remote shell would see `agent` where it expects the session argument.
        assert_eq!(
            attach_remote_command("", "t-1"),
            "herdr --session '' agent attach t-1"
        );
    }
}
