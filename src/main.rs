use std::os::unix::process::CommandExt;

use clap::{Args, Parser, Subcommand};
use pastor::config::flock::{Flock, MachineConfig};
use pastor::config::{PastorConfig, Paths, parse_duration};
use pastor::herdr::{Connector, Endpoint, shell_quote};
use pastor::ipc::{IpcRequest, IpcResponse, daemon_running, request};
use pastor::machine::{ChannelState, MachineStatus};
use pastor::store::{Store, TaskFilter};
use pastor::task::{DispatchSpec, Task, TaskState, parse_task_id};

#[derive(Parser)]
#[command(
    name = "pastor",
    version,
    about = "run coding agents on machines you own"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
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
    /// Manage machines
    Flock {
        #[command(subcommand)]
        cmd: FlockCmd,
    },
    /// Attach to a task's agent terminal (ctrl+b q detaches)
    Attach { task: String },
    /// Open the full herdr UI on a machine
    Open { machine: String },
}

#[derive(Args)]
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
    #[arg(long)]
    branch: Option<String>,
    #[arg(long = "tag")]
    tags: Vec<String>,
    #[arg(long)]
    timeout: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ListArgs {
    #[arg(long)]
    job: Option<String>,
    #[arg(long)]
    machine: Option<String>,
    #[arg(long)]
    blocked: bool,
    #[arg(long)]
    done: bool,
    /// Include closed and failed tasks
    #[arg(long)]
    all: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
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

#[derive(Subcommand)]
enum FlockCmd {
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
    },
    Remove {
        name: String,
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
            Command::Flock { cmd } => flock(&paths, cmd).await,
            Command::Attach { task } => attach(&paths, &task).await,
            Command::Open { machine } => open(&paths, &machine).await,
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

async fn list(paths: &Paths, a: ListArgs) -> anyhow::Result<()> {
    let states = if a.blocked {
        Some(vec![TaskState::Blocked])
    } else if a.done {
        Some(vec![TaskState::Done])
    } else if a.all {
        None
    } else {
        Some(vec![
            TaskState::Queued,
            TaskState::Starting,
            TaskState::Running,
            TaskState::Blocked,
            TaskState::Done,
            TaskState::Stale,
        ])
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

async fn flock(paths: &Paths, cmd: FlockCmd) -> anyhow::Result<()> {
    let path = paths.flock_file();
    match cmd {
        FlockCmd::Add {
            name,
            ssh,
            local,
            command,
            session,
            max_agents,
            tags,
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
            if let Some(target) = f.get(&name).and_then(|m| m.ssh.clone()) {
                println!(
                    "to see it in your laptop's herdr sidebar: herdr machine add {target} --label {name}"
                );
            }
            println!("restart pastor serve to pick it up");
        }
        FlockCmd::Remove { name } => {
            let mut f = Flock::load(&path)?;
            anyhow::ensure!(f.remove(&name), "machine {name} not found");
            f.save(&path)?;
            println!("removed {name}; restart pastor serve to apply");
        }
        FlockCmd::List { json } => {
            let statuses: Vec<MachineStatus> = if daemon_running(&paths.socket_file()).await {
                let IpcResponse::Machines(ms) = ask(paths, IpcRequest::FlockList).await? else {
                    unreachable!()
                };
                ms
            } else {
                eprintln!("pastor serve is not running; showing the flock file only");
                Flock::load(&path)?
                    .machines
                    .iter()
                    .map(|m| MachineStatus {
                        name: m.name.clone(),
                        endpoint: Endpoint::from_machine(m).describe(),
                        channel: ChannelState::Connecting,
                        herdr_version: None,
                        protocol: None,
                        error: Some("daemon down".into()),
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
        FlockCmd::Status { name, json } => {
            let f = Flock::load(&path)?;
            let mut rows = Vec::new();
            for m in f
                .machines
                .iter()
                .filter(|m| name.as_ref().is_none_or(|n| &m.name == n))
            {
                let ep = Endpoint::from_machine(m);
                let (status, version, protocol, agents, error) = match ep.connect().await {
                    Ok(mut c) => match c.ping().await {
                        Ok(p) => {
                            let compatible = p.protocol >= pastor::MIN_HERDR_PROTOCOL;
                            let n = c.agent_list().await.map(|a| a.len()).unwrap_or(0);
                            (
                                if compatible {
                                    "reachable"
                                } else {
                                    "incompatible"
                                },
                                Some(p.version),
                                Some(p.protocol),
                                n,
                                if compatible {
                                    None
                                } else {
                                    Some(format!(
                                        "protocol {} < {}",
                                        p.protocol,
                                        pastor::MIN_HERDR_PROTOCOL
                                    ))
                                },
                            )
                        }
                        Err(e) => ("error", None, None, 0, Some(e.to_string())),
                    },
                    Err(e) => (
                        if e.message.contains("herdr.sock") {
                            "server down"
                        } else {
                            "unreachable"
                        },
                        None,
                        None,
                        0,
                        Some(e.message),
                    ),
                };
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
                            r["agents"].to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
