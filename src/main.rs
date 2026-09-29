use std::io::Read;
use std::os::unix::process::CommandExt;
use std::sync::Arc;

use clap::{ArgGroup, Args, Parser, Subcommand};
use pastor::cli::{CliError, ask, print_task, unexpected};
use pastor::config::flock::{DEFAULT_FLOCK, EditError, Flock, FlockDoc, MachineConfig};
use pastor::config::job::set_enabled;
use pastor::config::{AgentChoice, PastorConfig, Paths, parse_duration};
use pastor::edit::{ConfigFile, Outcome};
use pastor::fleet_edit;
use pastor::herdr::{Connector, ConnectorExt, Endpoint, shell_quote};
use pastor::ipc::{Head, HeadPing, IpcRequest, IpcResponse, request};
use pastor::scheduler::{JobRunReport, JobStatus, Scheduler};
use pastor::store::{Store, TaskFilter};
use pastor::task::{
    DispatchSpec, LIVE_STATES, Place, Task, TaskRole, TaskState, bad_task_id, parse_task_id,
};

/// The agent skill, built into the binary so an agent on any machine with
/// pastor installed can read the guide that matches this exact CLI.
const SKILL: &str = include_str!("../skills/pastor/SKILL.md");

/// Agents read `--help` first; this sends them to the skill once.
const AGENT_FOOTER: &str = "\
Are you an AI agent? `pastor --skill` prints a guide to driving pastor.
Skip it if a pastor skill is already in your context.";

#[derive(Parser, Debug)]
#[command(
    name = "pastor",
    version,
    about = "run coding agents on machines you own",
    arg_required_else_help = true,
    after_help = AGENT_FOOTER
)]
struct Cli {
    /// Print the agent skill (SKILL.md) for this version and exit
    #[arg(long, exclusive = true)]
    skill: bool,
    /// Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)
    #[arg(long, global = true, value_name = "DEST")]
    head: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the daemon in the background: scheduler, machine channels, dispatch. With a head set on another machine, run headless: only this machine's jobs and hooks
    ///
    /// A bare `pastor serve` starts the head in the background, logging to
    /// serve.log in the state dir, and returns once it answers; --foreground
    /// keeps it in this terminal. One started by systemd or launchd stays in
    /// the foreground either way.
    Serve(ServeArgs),
    /// Manage tasks
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// The queued tasks in the order they will start, and why each waits; move one
    Queue(pastor::queue_cli::QueueArgs),
    /// Manage the machines and which flock each is in
    Machine {
        #[command(subcommand)]
        cmd: MachineCmd,
    },
    /// Manage the flocks: named groups of machines that tasks and jobs target
    Flock {
        #[command(subcommand)]
        cmd: FlockCmd,
    },
    /// Run one scheduler pass now and report what it did
    Tick(TickArgs),
    /// Manage jobs (files in ~/.config/pastor/jobs/). With a head elsewhere, a job whose file is here is this machine's; the rest are the head's
    Job {
        #[command(subcommand)]
        cmd: JobCmd,
    },
    /// Manage orchestrators (files in ~/.config/pastor/orchestrators/): the head runs a pre script on a schedule and starts an agent with the orchestrator role for what it prints, or keeps one agent running through set hours
    Orchestrator {
        #[command(subcommand)]
        cmd: pastor::orchestrator_cli::OrchestratorCmd,
    },
    /// pastor.toml: the head's settings and the task defaults
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Print a shell completion script (fish, bash, zsh, ...) to stdout
    Completions {
        /// The shell to write the script for
        shell: clap_complete::Shell,
    },
    /// Show the events log (task, job and machine events)
    Events(pastor::events::EventsArgs),
    /// Print one line per change to act on: tasks, failing jobs, the head, connector lines
    Watch(pastor::watch::WatchArgs),
    /// Install pastor or herdr as a user service (systemd, or launchd on macOS)
    Setup {
        #[command(subcommand)]
        cmd: pastor::setup::SetupCmd,
    },
    /// Install, link, list and try out connectors
    Connector {
        #[command(subcommand)]
        cmd: pastor::connector::cli::ConnectorCmd,
    },
    /// Permission profiles: named tool allow and deny lists, built in and from pastor.toml
    Profile {
        #[command(subcommand)]
        cmd: pastor::profile_cli::ProfileCmd,
    },
    /// The repos whose folder-trust prompt pastor answers on each machine
    Trust {
        #[command(subcommand)]
        cmd: pastor::trust_cli::TrustCmd,
    },
    /// Pass request lines from stdin to this machine's head; for a remote CLI over ssh
    Bridge(BridgeArgs),
    /// Which head this CLI uses: this machine's, or one on another machine over ssh
    Head {
        #[command(subcommand)]
        cmd: pastor::head::HeadCmd,
    },
}

#[derive(Args, Debug)]
struct ServeArgs {
    /// Run in this terminal until a signal, logging to stderr; what a service runs
    #[arg(short, long)]
    foreground: bool,
    #[command(subcommand)]
    cmd: Option<pastor::serve_cli::ServeCmd>,
}

#[derive(Args, Debug)]
struct BridgeArgs {
    /// Pass on only what an agent on --machine may ask: its machine's tasks
    #[arg(long, requires = "machine")]
    agent: bool,
    /// The machine whose agents this bridge serves, as flock.toml names it
    #[arg(long, requires = "agent")]
    machine: Option<String>,
}

#[derive(Args, Debug)]
struct TickArgs {
    /// Run connectors and show what would be created; write nothing
    #[arg(long)]
    dry_run: bool,
    /// Only this job, and run it whether or not it is due
    #[arg(long)]
    job: Option<String>,
    /// Print as a JSON array, one entry per job run
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand, Debug)]
enum JobCmd {
    /// Every job file: schedule, enabled, last run, next run, last result. With a head elsewhere, the head's jobs, then this machine's
    List {
        /// Add a DESCRIPTION column, cut to the terminal's width
        #[arg(short, long)]
        wide: bool,
        /// Print as a JSON array; with a head elsewhere, each job says `where` it lives (head or shepherd)
        #[arg(long)]
        json: bool,
    },
    /// Enable a job file
    Enable {
        /// The job: its file name without .toml
        name: String,
    },
    /// Disable a job file
    Disable {
        /// The job: its file name without .toml
        name: String,
    },
    /// Fire a job now, ignoring its schedule and `enabled`; it starts once a run already going has finished
    Run {
        /// The job: its file name without .toml
        name: String,
    },
    /// Re-read the job files, flock.toml and pastor.toml now instead of at the next tick
    Reload,
    /// Open a job file in $VISUAL or $EDITOR; save it only once it is valid
    Edit {
        /// The job: its file name without .toml
        name: String,
    },
    /// One job in full: schedule, connector, dispatch, last runs, recent tasks
    Describe {
        /// The job: its file name without .toml
        name: String,
        /// Print as a JSON object
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Open pastor.toml in $VISUAL or $EDITOR; save it only once it is valid
    Edit {
        /// This machine's pastor.toml, even with a head on another machine
        #[arg(long)]
        local: bool,
    },
}

#[derive(Args, Debug)]
struct RunArgs {
    /// What the agent is asked to do (or --prompt-file)
    #[arg(required_unless_present = "prompt_file")]
    prompt: Option<String>,
    /// Read the prompt from this file on this machine ('-' for stdin); it
    /// spares long prompts the shell's quoting
    #[arg(long, value_name = "PATH", conflicts_with = "prompt")]
    prompt_file: Option<String>,
    /// The repo the agent works in: a path on the machine that runs the
    /// task, not on this one
    #[arg(long, value_name = "PATH")]
    repo: Option<String>,
    /// Only this flock's machines take the task (default: the flock of
    /// --machine, else the default flock)
    #[arg(long)]
    flock: Option<String>,
    /// Run it on this machine (a name from flock.toml) instead of any free one
    #[arg(long)]
    machine: Option<String>,
    /// The agent command to start, like claude or codex (default: the
    /// machine's, else its flock's, else `[defaults]`, else claude)
    #[arg(long)]
    agent: Option<String>,
    /// One argument for the agent; repeat it, in order, for more. Replaces
    /// the flock's and `[defaults]` agent_args. The next word is always the
    /// value, dashes and all.
    #[arg(long = "agent-arg", value_name = "ARG", allow_hyphen_values = true)]
    agent_args: Vec<String>,
    /// Run this model, a name from `[models]` in pastor.toml; its args go
    /// before the agent's (default: the machine's, else its flock's, else
    /// `[defaults] model`, else none)
    #[arg(long, value_name = "NAME")]
    model: Option<String>,
    /// Queue at this level: low, normal, high or critical; dispatch takes
    /// higher levels first (default: the pinned machine's, else its
    /// flock's, else `[defaults] priority`, else normal)
    #[arg(long, value_name = "LEVEL")]
    priority: Option<String>,
    /// A critical task only: on a full machine, pause the newest low Claude
    /// task there (its session resumes when a slot frees) and take its slot
    #[arg(long)]
    preempt: bool,
    /// Run under this permission profile, built in or from `[profiles]` in
    /// pastor.toml: a Claude agent gets its allow and deny lists and never
    /// asks (default: the machine's, else its flock's, else `[defaults]
    /// profile`, else none)
    #[arg(long, value_name = "NAME")]
    profile: Option<String>,
    /// A git worktree per task, branched from --repo (so it needs --repo)
    #[arg(long, requires = "repo")]
    worktree: bool,
    /// Branch for the worktree (needs --worktree; a plain workspace has no branch)
    #[arg(long, requires = "worktree")]
    branch: Option<String>,
    /// Only a machine with this tag takes the task; repeat for more, and it needs them all
    #[arg(long = "tag")]
    tags: Vec<String>,
    /// Mark the task stale once it has run this long (30m, 2h; default: `[defaults]` timeout)
    #[arg(long)]
    timeout: Option<String>,
    /// Label template of the workspace pastor makes for the task, with
    /// {{ task.id }}, {{ flock }}, {{ machine }}, {{ job }} and
    /// {{ item.key }} (default: the flock's `label`, else `[defaults]
    /// label`, else {{ flock }}/{{ task.id }}). The agent stays t-N
    #[arg(long, value_name = "TEMPLATE", value_parser = parse_label)]
    label: Option<String>,
    /// Where the agent's pane goes: repo (under the repo it works on), own
    /// (its own workspace), pastor (the `pastor` workspace) or
    /// pane:<workspace> (default: `[defaults] place`, else repo)
    #[arg(long, value_name = "PLACE")]
    place: Option<Place>,
    /// What the agent may change through the head: agent (read, and end
    /// its own task) or orchestrator (also run, retry, send to and close
    /// tasks and enable and disable jobs). Only a person may start an
    /// orchestrator, never a task
    #[arg(long, value_enum, value_name = "ROLE", default_value_t = TaskRole::Agent)]
    role: TaskRole,
    /// Ask the agent for a summary when it finishes (ask), also fail the
    /// task if it stops without one (require), or neither (off) (default:
    /// the flock's `summary`, else `[defaults] summary`, else ask)
    #[arg(long, value_enum, value_name = "MODE")]
    summary: Option<pastor::task::SummaryMode>,
    /// One line on what the task is about, for `task list --wide` and
    /// `describe` (default: the prompt's first line)
    #[arg(long, value_name = "TEXT")]
    description: Option<String>,
    /// Print as a JSON object
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ListArgs {
    /// Only tasks from this job (omit for one-off `run` tasks)
    #[arg(long)]
    job: Option<String>,
    /// Only tasks of this flock
    #[arg(long)]
    flock: Option<String>,
    /// Only tasks on this machine
    #[arg(long)]
    machine: Option<String>,
    /// Only blocked tasks, needing a human
    #[arg(long, group = "list_filter")]
    blocked: bool,
    /// Only done tasks
    #[arg(long, group = "list_filter")]
    done: bool,
    /// Every task, finished ones too (done, failed, stale, closed)
    #[arg(long, group = "list_filter")]
    all: bool,
    /// Add a DESCRIPTION column, cut to the terminal's width
    #[arg(short, long)]
    wide: bool,
    /// Print as a JSON array of full task records
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand, Debug)]
enum TaskCmd {
    /// Create a one-off task and dispatch it
    Run(Box<RunArgs>),
    /// List live tasks across the flock; --all adds finished ones
    List(ListArgs),
    /// One task in full: state, machine, agent, prompt, error, summary
    Describe {
        /// A task, like t-12 or 12
        task: String,
        /// Show every round's summary, not only the last
        #[arg(long)]
        all_summaries: bool,
        /// Print as a JSON object
        #[arg(long)]
        json: bool,
    },
    /// Read recent output from a task's pane
    Read {
        /// A task, like t-12 or 12
        task: String,
        /// How many lines from the bottom of the pane
        #[arg(long, default_value_t = 40)]
        lines: u32,
    },
    /// Attach to a task's agent terminal (ctrl+b q detaches); a closed Claude
    /// task's session reopens in a new pane
    Attach {
        /// A task, like t-12 or 12
        task: String,
    },
    /// Re-dispatch a failed or stale task as a new task (retry_of points back)
    Retry(pastor::task_cli::RetryArgs),
    /// Put a queued task at another level: low, normal, high or critical
    Priority(pastor::task_cli::PriorityArgs),
    /// Close tasks' panes (and with --remove-worktree their worktrees), or orphaned agents
    Close(pastor::task_cli::CloseArgs),
    /// Delete old finished tasks; their items stay seen
    Prune(pastor::task_cli::PruneArgs),
    /// Type text or press keys in a live task's agent, to answer what it is waiting on
    Send(pastor::task_cli::SendArgs),
    /// Mark a task done, its pane to close after close_done_after; an agent may end its own
    Done(pastor::task_cli::DoneArgs),
}

#[derive(Subcommand, Debug)]
enum MachineCmd {
    /// Add a machine to flock.toml, reached over ssh, locally or by a command
    #[command(group(ArgGroup::new("reach").args(["ssh", "local", "command"])))]
    Add {
        /// The name pastor calls it by, in tasks, jobs and agent names
        name: String,
        /// How ssh reaches it, like user@pi-1 (or an ssh config Host)
        ssh: Option<String>,
        /// This machine itself, through herdr's local socket; no ssh
        #[arg(long)]
        local: bool,
        /// Developer option: the bridge command as one string, split on
        /// whitespace (`--command "fake-herdr --connect /tmp/h.sock"`).
        /// Words containing spaces go in flock.toml by hand.
        #[arg(long, value_name = "COMMAND")]
        command: Option<String>,
        /// The herdr session on the machine that agents run in
        #[arg(long, default_value = "default")]
        session: String,
        /// How many tasks it runs at once
        #[arg(long, default_value_t = 2)]
        max_agents: u32,
        /// Extra slots only tasks from jobs take, on top of --max-agents; 0 for none
        #[arg(long, default_value_t = 1)]
        job_slots: u32,
        /// How many past --max-agents a critical task may start; 0 for none
        #[arg(long, default_value_t = 1)]
        burst: u32,
        /// A label a task's --tag can ask for; repeat for more
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// The flock it joins (default: the default flock)
        #[arg(long)]
        flock: Option<String>,
        /// One line on what the machine is for, for `machine list --wide`
        #[arg(long, value_name = "TEXT")]
        description: Option<String>,
        /// Also save it in herdr's sidebar (runs `herdr machine add`)
        #[arg(long, conflicts_with_all = ["local", "command"])]
        herdr: bool,
    },
    /// Remove a machine from flock.toml; tasks already on it keep their rows
    Remove {
        /// The machine, as flock.toml names it
        name: String,
        /// Also remove herdr's saved machine with this label
        #[arg(long)]
        herdr: bool,
    },
    /// Take a machine out of every flock and put it in this one; tasks already on it stay there
    Move {
        /// The machine, as flock.toml names it
        name: String,
        /// The flock it moves to
        flock: String,
    },
    /// A line about the head, then each machine: host, flock, channel, herdr, agents
    List {
        /// Only the machines of this flock
        #[arg(long)]
        flock: Option<String>,
        /// Add a DESCRIPTION column, cut to the terminal's width
        #[arg(short, long)]
        wide: bool,
        /// Print as a JSON object, {head, machines}: the head's row, then the machines
        #[arg(long)]
        json: bool,
    },
    /// One machine in full: host, flock, channel, versions, agents, recent errors
    Describe {
        /// The machine, as flock.toml names it
        name: String,
        /// Print as a JSON object
        #[arg(long)]
        json: bool,
    },
    /// Open the full herdr UI on a machine
    Open {
        /// The machine, as flock.toml names it
        name: String,
    },
    /// Print the authorized_keys line that lets a machine's agents reach this head
    AuthorizedKey {
        /// The machine, as flock.toml names it
        name: String,
        /// The public key of the machine's user: a .pub file, or - for stdin
        #[arg(long, value_name = "FILE")]
        key: String,
    },
}

#[derive(Subcommand, Debug)]
enum FlockDefaultCmd {
    /// Print the default flock
    Show,
    /// Make another flock the default; machines stay in their flocks
    Set {
        /// The flock that becomes the default
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum FlockCmd {
    /// Every flock: default or not, its machines, live agents, queued tasks
    List {
        /// Add a DESCRIPTION column, cut to the terminal's width
        #[arg(short, long)]
        wide: bool,
        /// Print as a JSON array
        #[arg(long)]
        json: bool,
    },
    /// Declare a flock; with --default, new tasks and jobs go to it
    Add {
        /// The new flock's name
        name: String,
        /// Machines that join it, each with its max_agents
        machines: Vec<String>,
        /// Make it the default flock too
        #[arg(long)]
        default: bool,
        /// One line on what the flock is for, for `flock list --wide`
        #[arg(long, value_name = "TEXT")]
        description: Option<String>,
    },
    /// Put a machine in a flock, or change its number there; it stays in its other flocks
    Join {
        /// The flock
        flock: String,
        /// The machine, as flock.toml names it
        machine: String,
        /// At most this many of the flock's tasks on the machine (default: its max_agents, or its number there already)
        #[arg(long, value_name = "N")]
        max: Option<u32>,
    },
    /// Take a machine out of a flock; out of its last one it is in the default flock
    Leave {
        /// The flock
        flock: String,
        /// The machine, as flock.toml names it
        machine: String,
    },
    /// Remove a flock; refused while it has machines or queued tasks, or is the default
    Remove {
        /// The flock
        name: String,
    },
    /// The flock that new tasks and jobs go to
    Default {
        #[command(subcommand)]
        cmd: FlockDefaultCmd,
    },
    /// Open flock.toml in $VISUAL or $EDITOR; save it only once it is valid
    Edit,
    /// One flock in full: default or not, its agent, machines, live tasks
    Describe {
        /// The flock
        name: String,
        /// Print as a JSON object
        #[arg(long)]
        json: bool,
    },
}

fn main() {
    // `pastor __complete <shell> -- <words>` is what the completion scripts
    // ask at TAB time. It is not in the clap tree, which would put it in the
    // very scripts it serves, and it runs before the legacy migration, the
    // fleet guard and the runtime: it only reads, and quietly.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("__complete") {
        complete(&args[2..]);
    }
    let cli = Cli::parse();
    let serve_log = serve_log(&cli);
    init_tracing(serve_log.as_deref());
    // `exclusive` does not cover subcommands, and `--head` being global rules
    // out `args_conflicts_with_subcommands`, so `--skill task` is refused here.
    if cli.skill && cli.command.is_some() {
        <Cli as clap::CommandFactory>::command()
            .error(
                clap::error::ErrorKind::ArgumentConflict,
                "--skill cannot be used with a subcommand",
            )
            .exit();
    }
    if cli.skill {
        print!("{SKILL}");
        return;
    }
    if let Some(Command::Serve(args)) = &cli.command
        && args.foreground
        && args.cmd.is_some()
    {
        <Cli as clap::CommandFactory>::command()
            .error(
                clap::error::ErrorKind::ArgumentConflict,
                "--foreground starts a head; it cannot be used with `serve stop` or `serve status`",
            )
            .exit();
    }
    let head_flag = cli.head.clone();
    // `arg_required_else_help` means clap has already printed help for a bare
    // `pastor`; the only other way here without a command is `--skill`.
    let Some(command) = cli.command else {
        unreachable!("clap requires a command or --skill")
    };
    let paths = match Paths::from_env() {
        Ok(p) => p,
        Err(err) => fail("config_error", &err.to_string()),
    };
    // Plumbing, before the legacy migration and the fleet guard: it reads no
    // config, and the head checks each request it carries.
    if let Command::Bridge(args) = &command {
        bridge(&paths, args);
    }
    if let Some(legacy) = pastor::config::legacy_macos_dir() {
        match pastor::config::migrate_legacy_dir(&legacy, &paths) {
            Ok(Some(note)) => eprintln!("{note}"),
            Ok(None) => {}
            Err(err) => fail("config_error", &format!("{err:#}")),
        }
    }
    let remote = match pastor::head::load(&pastor::head::client_file(&paths)) {
        Ok(file) => pastor::head::resolve(
            &paths,
            cli.head.as_deref(),
            std::env::var(pastor::head::HEAD_ENV).ok().as_deref(),
            file,
        ),
        Err(err) => match err.downcast_ref::<CliError>() {
            Some(e) => fail(&e.code, &e.message),
            None => fail("config_error", &format!("{err:#}")),
        },
    };
    if let Some(r) = &remote {
        match remote_route(&command) {
            // With a head elsewhere, serve runs headless (`shepherd`).
            RemoteRoute::Head | RemoteRoute::Here | RemoteRoute::Serve => {}
            RemoteRoute::Unsupported(why) => fail(
                "remote_head_unsupported",
                &format!(
                    "`pastor {}` runs on the head only: {why}; run it on {}",
                    command_path(),
                    r.ssh
                ),
            ),
        }
    }
    pastor::ipc::set_remote_head(remote.clone());
    pastor::ipc::set_caller_task(pastor::ipc::task_from_env());
    pastor::ipc::set_caller_orchestrator(pastor::ipc::orchestrator_from_env());
    if let Some(task) = pastor::ipc::caller_task() {
        if makes_orchestrator(&command) {
            fail(
                "role_refused",
                &format!(
                    "{task} is a task, and only a person may run a task with --role orchestrator"
                ),
            );
        }
        if !head_decides(&paths, &command, remote.is_some())
            && changes_fleet(&command)
            && !ends_own_task(&command, &task)
            && !orchestrator_may(&command)
            && !agents_change_fleet(&paths)
        {
            fail(
                "agent_refused",
                &pastor::daemon::refusal(&task, local_caller_role(&paths, &task)),
            );
        }
    } else if let Some(o) = pastor::ipc::caller().orchestrator {
        // An orchestrator's pre or post script: the role's table, as the
        // head applies it (`Daemon::handle_as`), for what the CLI does
        // without it.
        if makes_orchestrator(&command) {
            fail(
                "role_refused",
                &format!(
                    "orchestrator {o}'s script is not a person, and only a person may run a task with --role orchestrator"
                ),
            );
        }
        if changes_fleet(&command) && !orchestrator_may(&command) && !agents_change_fleet(&paths) {
            fail("agent_refused", &pastor::daemon::script_refusal(&o));
        }
    }
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async {
        // Commands that never talk to the head do not read `head`, and with a
        // remote head, commands that stay here on purpose do not ask it.
        let head_use = match remote_route(&command) {
            RemoteRoute::Here if remote.is_some() && !reads_head_files(&command) => None,
            // Attach and open go to the machine directly, but ask a remote
            // head for the task and its files, so it must answer.
            RemoteRoute::Here if remote.is_some() => Some(false),
            // What a local head leaves to the files here (`flock default
            // show`, `profile`) is fetched from a remote head, so it must
            // answer.
            RemoteRoute::Head if remote.is_some() => Some(head_use(&command).unwrap_or(false)),
            // Here `events` reads the log file, head or no head; a remote
            // head is asked for it, so it must be up and new enough.
            _ if remote.is_some() && matches!(command, Command::Events(_)) => Some(false),
            _ => head_use(&command),
        };
        let head = match head_use {
            // What each request needs of the head is checked as it goes
            // (`IpcRequest::min_protocol`); the probe checks what the files
            // here need of it.
            Some(flocky) => {
                probe_head(
                    &paths,
                    // A remote head's flock.toml is not here to read.
                    flocky || remote.is_some() || flocks_declared(&paths),
                    // flock.toml uses a field an older head's `FlockEntry`
                    // (`deny_unknown_fields`) does not know; its reload
                    // fails and it silently keeps the old flocks, so refuse
                    // rather than let the CLI dispatch, list or reload on
                    // that stale view.
                    if remote.is_none() {
                        flock_file_need(&paths)
                    } else {
                        None
                    },
                )
                .await?
            }
            None => Head::Absent,
        };
        match command {
            Command::Serve(args) => serve(paths, args, remote.clone(), head_flag, serve_log).await,
            Command::Task { cmd } => task(&paths, cmd, head).await,
            Command::Machine { cmd } => machine(&paths, cmd, head).await,
            Command::Flock { cmd } => flock(&paths, cmd, head).await,
            Command::Tick(args) => tick(&paths, args, head).await,
            Command::Job { cmd } => job(&paths, cmd, head).await,
            Command::Orchestrator { cmd } => pastor::orchestrator_cli::run(&paths, cmd, head).await,
            Command::Config {
                cmd: ConfigCmd::Edit { local },
            } => config_edit(&paths, local, head).await,
            Command::Completions { shell } => {
                let mut cmd = completion_tree();
                clap_complete::generate(shell, &mut cmd, "pastor", &mut std::io::stdout());
                match shell {
                    clap_complete::Shell::Fish => print!("{}", pastor::complete::fish_hook(&cmd)),
                    clap_complete::Shell::Bash => print!("{}", pastor::complete::BASH_HOOK),
                    _ => {}
                }
                Ok(())
            }
            Command::Events(args) => pastor::events::cli(&paths, args).await,
            Command::Watch(args) => pastor::watch::cli(&paths, args).await,
            Command::Setup { cmd } => pastor::setup::cli(&paths, cmd),
            Command::Connector { cmd } => pastor::connector::cli::run(&paths, cmd, head).await,
            Command::Profile { cmd } => pastor::profile_cli::run(&head_config(&paths).await?, cmd),
            Command::Trust { cmd } => pastor::trust_cli::run(&paths, cmd, head).await,
            Command::Queue(args) => pastor::queue_cli::run(&paths, args, head).await,
            Command::Bridge(_) => unreachable!("handled before the runtime"),
            Command::Head { cmd } => pastor::head::run(&paths, cmd, remote.as_ref()).await,
        }
    });
    if let Err(err) = result {
        if let Some(e) = err.downcast_ref::<pastor::cli::CliError>() {
            fail(&e.code, &e.message);
        }
        fail("runtime_error", &format!("{err:#}"));
    }
}

/// The log a background `pastor serve --foreground` was started with
/// (`serve_cli::LOG_ENV`), taken out of the environment before any thread
/// starts, so the connectors and hooks it runs do not inherit it.
fn serve_log(cli: &Cli) -> Option<std::path::PathBuf> {
    let log = std::env::var_os(pastor::serve_cli::LOG_ENV)?;
    // SAFETY: no other thread runs yet: the tokio runtime starts later.
    unsafe { std::env::remove_var(pastor::serve_cli::LOG_ENV) };
    match &cli.command {
        Some(Command::Serve(ServeArgs {
            foreground: true,
            cmd: None,
        })) => Some(log.into()),
        _ => None,
    }
}

/// Tracing to stderr, or for a background `pastor serve` to its rotated log,
/// without colour.
fn init_tracing(serve_log: Option<&std::path::Path>) {
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "pastor=info".into())
    };
    if let Some(path) = serve_log {
        use pastor::serve_cli::{LOG_KEEP, LOG_MAX_BYTES, RotatingLog};
        match RotatingLog::open(path.to_path_buf(), LOG_MAX_BYTES, LOG_KEEP, true) {
            Ok(log) => {
                tracing_subscriber::fmt()
                    .with_env_filter(filter())
                    .with_ansi(false)
                    .with_writer(log)
                    .init();
                return;
            }
            Err(err) => fail("config_error", &format!("open {}: {err}", path.display())),
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(filter())
        .with_writer(std::io::stderr)
        .init();
}

/// `pastor serve`, `serve stop` and `serve status`. A bare `serve` goes to
/// the background unless a service manager started it.
async fn serve(
    paths: Paths,
    args: ServeArgs,
    remote: Option<pastor::head::RemoteHead>,
    head_flag: Option<String>,
    log: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    use pastor::serve_cli::{self, ServeCmd};
    match args.cmd {
        Some(ServeCmd::Stop) => return serve_cli::stop(&paths).await,
        Some(ServeCmd::Status { json }) => return serve_cli::status(&paths, json).await,
        None => {}
    }
    let service = serve_cli::service_manager();
    if !args.foreground && service.is_none() {
        return serve_cli::start_background(&paths, head_flag.as_deref()).await;
    }
    serve_cli::record_start(&paths, service, log.as_deref()).await?;
    match remote {
        Some(r) => pastor::shepherd::serve(paths, r).await,
        None => pastor::daemon::serve(paths).await,
    }
}

/// `pastor __complete <shell> -- <words>`: the names for the last of
/// `words`, and exit 0; exit 1 with nothing printed when that word takes no
/// name (or the call is malformed), so the script falls back to its static
/// completions. fish gets a description after a tab.
fn complete(args: &[String]) -> ! {
    use pastor::complete;
    let (Some(shell), Some("--")) = (args.first(), args.get(1).map(String::as_str)) else {
        std::process::exit(1)
    };
    let Some(kind) = complete::slot(&completion_tree(), &args[2..]) else {
        std::process::exit(1)
    };
    if let Ok(paths) = Paths::from_env() {
        let names = complete::names(&paths, kind);
        print!("{}", complete::render(&names, shell == "fish"));
    }
    std::process::exit(0)
}

/// The command tree `pastor completions` describes: the real one, since there
/// are no hidden subcommands or aliases (`__complete` is not in it).
fn completion_tree() -> clap::Command {
    <Cli as clap::CommandFactory>::command().version(env!("CARGO_PKG_VERSION"))
}

/// `pastor bridge`. Its client reads replies on stdout, so a failure is
/// written there too, as the one line where the reply would have been.
fn bridge(paths: &Paths, args: &BridgeArgs) -> ! {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let socket = paths.socket_file();
    let input = tokio::io::BufReader::new(tokio::io::stdin());
    let result = rt.block_on(async {
        match (args.agent, &args.machine) {
            (true, Some(machine)) => {
                pastor::bridge::run_agent(&socket, machine, input, tokio::io::stdout()).await
            }
            _ => pastor::bridge::run(&socket, input, tokio::io::stdout()).await,
        }
    });
    let Err(err) = result else {
        std::process::exit(0)
    };
    let (code, message) = match err.downcast_ref::<CliError>() {
        Some(e) => (e.code.clone(), e.message.clone()),
        None => ("runtime_error".to_string(), format!("{err:#}")),
    };
    println!("{}", serde_json::json!({"code": code, "message": message}));
    std::process::exit(1)
}

fn fail(code: &str, message: &str) -> ! {
    eprintln!("{}", serde_json::json!({"code": code, "message": message}));
    std::process::exit(1)
}

/// The one ping a command sends the head, before it does anything, and what
/// the command then acts on throughout. A head that is not running is
/// `Head::Absent` and the command works without it. One that is listening
/// but does not answer is a hard error, never taken for no head: `tick`
/// would start a second scheduler next to it, and an edit or a prune would
/// go offline behind it. With `flocks` in play (`head_use`) a head from
/// before flocks is refused too: it ignores the `flock` field of a request
/// (serde skips unknown fields) and reads flock.toml as one flock, so it
/// would dispatch, list or reload across every flock.
///
/// `need` is the protocol flock.toml here needs of the head, with what an
/// older head would do instead (`flock_file_need`); such a head is refused
/// too. What each request needs is checked as it is sent
/// (`IpcRequest::min_protocol`), against the pong kept here.
async fn probe_head(
    paths: &Paths,
    flocks: bool,
    need: Option<(u32, &str)>,
) -> anyhow::Result<Head> {
    let socket = paths.socket_file();
    let ping = match pastor::ipc::remote_head() {
        // A remote head is never absent: the command must not fall back to
        // this machine's files, so no answer stops it.
        Some(remote) => {
            let line = pastor::ipc::request_line(&IpcRequest::Ping, None)?;
            match remote.request(&line, pastor::head::PING_TIMEOUT).await {
                Ok(IpcResponse::Pong {
                    version,
                    protocol,
                    role,
                }) => HeadPing::Pong {
                    version,
                    protocol,
                    role,
                },
                Ok(other) => {
                    return Err(CliError::err(
                        "head_unresponsive",
                        format!("the head on {} answered ping with {other:?}", remote.ssh),
                    ));
                }
                Err(err) => return Err(pastor::head::failure(&err)),
            }
        }
        None => pastor::ipc::ping_head(&socket).await,
    };
    if let HeadPing::Pong {
        version, protocol, ..
    } = &ping
    {
        pastor::ipc::head_gate(paths).remember(version, *protocol);
    }
    match ping {
        HeadPing::NotRunning => Ok(Head::Absent),
        HeadPing::Pong {
            role: Some(role), ..
        } if role == pastor::ipc::SHEPHERD_ROLE => Err(pastor::cli::CliError::err(
            "shepherd_running",
            match pastor::ipc::remote_head() {
                Some(r) => format!(
                    "{} runs a headless pastor serve, not a head; point `pastor head set` at the head",
                    r.ssh
                ),
                None => format!(
                    "a headless pastor serve holds {} and there is no head here; set one with `pastor head set`, or stop it",
                    socket.display()
                ),
            },
        )),
        HeadPing::Unresponsive => Err(pastor::cli::CliError::err(
            "head_unresponsive",
            format!(
                "pastor serve is listening on {} but not answering; nothing was done, run it again once it answers",
                socket.display()
            ),
        )),
        HeadPing::Pong {
            version, protocol, ..
        } if let Some((needed, why)) = need
            && protocol < needed =>
        {
            Err(pastor::cli::CliError::err(
                "head_too_old",
                format!("the running pastor serve ({version}) {why}; restart it"),
            ))
        }
        HeadPing::Pong { protocol, .. } if !flocks || protocol >= pastor::ipc::FLOCK_PROTOCOL => {
            Ok(Head::Live)
        }
        HeadPing::Pong { version, .. } => Err(pastor::cli::CliError::err(
            "head_too_old",
            format!(
                "the running pastor serve ({version}) predates flocks and would act on every flock; restart it, or stop it to work without a head"
            ),
        )),
    }
}

/// Whether `command` talks to or reloads the head, and if so whether it
/// brings flocks into play on its own: it takes `--flock`, or it edits
/// flock.toml. `None` for a command that never talks to the head.
fn head_use(command: &Command) -> Option<bool> {
    use pastor::connector::cli::ConnectorCmd;
    match command {
        Command::Task { cmd } => match cmd {
            TaskCmd::Run(a) => Some(a.flock.is_some()),
            TaskCmd::List(a) => Some(a.flock.is_some()),
            // Attach goes straight to the machine over ssh and herdr; a busy
            // or old head must not stand between the user and a pane.
            TaskCmd::Attach { .. } => None,
            _ => Some(false),
        },
        Command::Machine { cmd } => match cmd {
            MachineCmd::List { flock, .. } => Some(flock.is_some()),
            MachineCmd::Describe { .. } => Some(false),
            // Open execs herdr on the machine; the head has no part in it.
            MachineCmd::Open { .. } => None,
            // Reads flock.toml and prints a line; it edits no file.
            MachineCmd::AuthorizedKey { .. } => None,
            _ => Some(true),
        },
        Command::Flock { cmd } => match cmd {
            // Reads flock.toml and nothing else: no head to ask.
            FlockCmd::Default {
                cmd: FlockDefaultCmd::Show,
            } => None,
            FlockCmd::List { .. } | FlockCmd::Describe { .. } => Some(false),
            _ => Some(true),
        },
        Command::Tick(_) | Command::Job { .. } | Command::Config { .. } => Some(false),
        Command::Orchestrator { .. } => Some(false),
        Command::Trust { .. } => Some(false),
        Command::Queue(a) => Some(a.flock.is_some()),
        Command::Connector { cmd } => {
            (!matches!(cmd, ConnectorCmd::List { .. } | ConnectorCmd::Try { .. })).then_some(false)
        }
        _ => None,
    }
}

/// How `command` runs while a remote head is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteRoute {
    /// Through the head, over ssh: every file it needs is the head's.
    Head,
    /// Here, without the head, on purpose: connectors are this machine's,
    /// and attach goes to the machine directly.
    Here,
    /// `pastor serve`: headless, running this machine's jobs and hooks for
    /// the head.
    Serve,
    /// Only on the head's own machine, for the reason given: run here it
    /// would act on this machine's files instead of the head's.
    Unsupported(&'static str),
}

fn remote_route(command: &Command) -> RemoteRoute {
    match command {
        Command::Task {
            cmd: TaskCmd::Attach { .. },
        } => RemoteRoute::Here,
        // The line names the pastor binary on the machine it is printed
        // on, and goes in that machine's authorized_keys.
        Command::Machine {
            cmd: MachineCmd::AuthorizedKey { .. },
        } => RemoteRoute::Unsupported(
            "the line it prints names the head's pastor and goes in the head's authorized_keys",
        ),
        // Open, like attach, goes to the machine directly; the head only
        // says how to reach it.
        Command::Machine {
            cmd: MachineCmd::Open { .. },
        } => RemoteRoute::Here,
        Command::Config {
            cmd: ConfigCmd::Edit { local: true },
        } => RemoteRoute::Here,
        // Watch asks the head for its events, tasks and jobs; its connectors
        // are this machine's.
        Command::Task { .. }
        | Command::Watch(_)
        | Command::Queue(_)
        | Command::Machine { .. }
        | Command::Flock { .. }
        | Command::Trust { .. }
        | Command::Profile { .. }
        | Command::Config { .. }
        | Command::Events(_)
        | Command::Tick(_)
        // The head's jobs through it, this machine's own here (`job`).
        | Command::Job { .. }
        // Orchestrators run only on the head.
        | Command::Orchestrator { .. } => RemoteRoute::Head,
        Command::Completions { .. }
        | Command::Setup { .. }
        | Command::Head { .. }
        | Command::Bridge(_)
        | Command::Connector { .. } => RemoteRoute::Here,
        // Stop and status act on this machine's serve, head or headless.
        Command::Serve(ServeArgs { cmd: Some(_), .. }) => RemoteRoute::Here,
        Command::Serve(_) => RemoteRoute::Serve,
    }
}

/// Whether a remote head, not this machine, judges whether the task
/// calling may run `command`: the head refuses what its own
/// `agents_change_fleet` and the task's role forbid, and this machine's
/// pastor.toml and store have no say in it. A job whose file is here is
/// not the head's: `job()` runs and edits it here, so the guard here
/// applies.
fn head_decides(paths: &Paths, command: &Command, remote: bool) -> bool {
    remote && remote_route(command) == RemoteRoute::Head && !names_local_job(paths, command)
}

/// Whether `command` is a `job` command naming a job whose file is on this
/// machine (`is_local_job`), which `job()` hands to `local_job` when the
/// head is elsewhere.
fn names_local_job(paths: &Paths, command: &Command) -> bool {
    matches!(command, Command::Job { cmd }
        if job_name(cmd).is_some_and(|name| is_local_job(paths, name)))
}

/// Whether `command`, sent to a remote head, fetches one of its files
/// (`head_file`) or its task where a local head's CLI would read them
/// here: it needs `FILE_PROTOCOL`. Attach and open stay here, but still
/// ask the head.
fn reads_head_files(command: &Command) -> bool {
    matches!(
        command,
        Command::Profile { .. }
            | Command::Task {
                cmd: TaskCmd::Attach { .. }
            }
            | Command::Machine {
                cmd: MachineCmd::Open { .. }
            }
            | Command::Flock {
                cmd: FlockCmd::List { .. }
                    | FlockCmd::Default {
                        cmd: FlockDefaultCmd::Show
                    }
            }
    )
}

/// The subcommand words the command line named (`machine add`), for a
/// message about it.
fn command_path() -> String {
    let Ok(m) = <Cli as clap::CommandFactory>::command().try_get_matches() else {
        return "?".into();
    };
    let mut words = Vec::new();
    let mut cur = &m;
    while let Some((name, sub)) = cur.subcommand() {
        words.push(name.to_string());
        cur = sub;
    }
    words.join(" ")
}

/// Whether `command` changes the fleet: the CLI's side of
/// `IpcRequest::changes_fleet`, for the edits it makes without the head
/// (machines, flocks, jobs, connectors, an offline tick). A tick, dry or not,
/// and a job reload apply pastor.toml and flock.toml, so they count. An agent
/// pastor started (`ipc::TASK_ENV`) is refused these up front, head or no
/// head.
fn changes_fleet(command: &Command) -> bool {
    use pastor::connector::cli::ConnectorCmd;
    match command {
        Command::Task { cmd } => matches!(
            cmd,
            TaskCmd::Run(_)
                | TaskCmd::Retry(_)
                | TaskCmd::Priority(_)
                | TaskCmd::Close(_)
                | TaskCmd::Prune(_)
                | TaskCmd::Send(_)
                | TaskCmd::Done(_)
                // herdr's agent terminal types into any task's pane.
                | TaskCmd::Attach { .. }
        ),
        // Reading the fleet is fine: list and describe change nothing. Open
        // counts: herdr's full UI drives every pane and agent on the machine.
        Command::Machine { cmd } => {
            !matches!(cmd, MachineCmd::List { .. } | MachineCmd::Describe { .. })
        }
        Command::Flock { cmd } => !matches!(
            cmd,
            FlockCmd::List { .. }
                | FlockCmd::Describe { .. }
                | FlockCmd::Default {
                    cmd: FlockDefaultCmd::Show
                }
        ),
        Command::Tick(_) => true,
        Command::Job { cmd } => !matches!(cmd, JobCmd::List { .. } | JobCmd::Describe { .. }),
        Command::Orchestrator { cmd } => pastor::orchestrator_cli::changes_fleet(cmd),
        // pastor.toml holds agents_change_fleet itself.
        Command::Config { .. } => true,
        // Install, link, uninstall and unlink edit the catalog and reload the
        // head's jobs, restarting its stream connectors.
        Command::Connector { cmd } => !matches!(
            cmd,
            ConnectorCmd::List { .. } | ConnectorCmd::Describe { .. } | ConnectorCmd::Try { .. }
        ),
        // A head started from an agent's pane schedules and dispatches with
        // no request to refuse; setup installs one that starts on login.
        // Status only reads.
        Command::Serve(ServeArgs {
            cmd: Some(pastor::serve_cli::ServeCmd::Status { .. }),
            ..
        }) => false,
        Command::Serve(_) | Command::Setup { .. } => true,
        Command::Trust { cmd } => pastor::trust_cli::changes_fleet(cmd),
        Command::Queue(a) => pastor::queue_cli::changes_fleet(a),
        _ => false,
    }
}

/// Whether `command` is `task run --role orchestrator`, which no task may
/// send.
fn makes_orchestrator(command: &Command) -> bool {
    matches!(command, Command::Task { cmd: TaskCmd::Run(a) } if a.role == TaskRole::Orchestrator)
}

/// Whether `command` is one an orchestrator task may make
/// (`IpcRequest::orchestrator_may`). The CLI does not know the caller's
/// role, so from a task's pane it leaves these to the head, which does:
/// `task run|retry|send|close` only ever go through it, and `job
/// enable|disable` is refused for a task caller in `toggle` (no head) and
/// in `local_toggle` (a head elsewhere, but the job is this machine's own,
/// which never reaches it either).
fn orchestrator_may(command: &Command) -> bool {
    match command {
        Command::Task { cmd } => {
            matches!(
                cmd,
                TaskCmd::Run(_) | TaskCmd::Retry(_) | TaskCmd::Send(_) | TaskCmd::Close(_)
            )
        }
        Command::Job { cmd } => matches!(cmd, JobCmd::Enable { .. } | JobCmd::Disable { .. }),
        Command::Orchestrator { cmd } => pastor::orchestrator_cli::orchestrator_may(cmd),
        _ => false,
    }
}

/// Whether `command` is `task done` for `task`, the task the caller runs
/// in: the one change an agent may make without `agents_change_fleet`.
fn ends_own_task(command: &Command, task: &str) -> bool {
    matches!(command, Command::Task { cmd: TaskCmd::Done(a) } if a.ends(task))
}

/// `agents_change_fleet` in pastor.toml. A file that does not load counts
/// as off: the refusal is the safe side.
fn agents_change_fleet(paths: &Paths) -> bool {
    PastorConfig::load(&paths.config_file()).is_ok_and(|c| c.agents_change_fleet)
}

/// The role of `task`, read straight from the database for this local guard,
/// which runs before the head is ever contacted. One the store does not know,
/// or cannot read, is a plain agent: the refusal is the safe side, same as
/// `Daemon::caller_role`.
fn local_caller_role(paths: &Paths, task: &str) -> TaskRole {
    parse_task_id(task)
        .and_then(|id| {
            Store::open_read_only(&paths.db_file())
                .ok()?
                .get_task(id)
                .ok()?
        })
        .map_or(TaskRole::Agent, |t| t.role)
}

/// `--label`, checked before anything is sent (`task::check_label`).
fn parse_label(s: &str) -> Result<String, String> {
    pastor::task::check_label(s).map(|()| s.to_string())
}

/// Whether flock.toml declares named flocks, so a command with no `--flock`
/// means the default flock rather than every machine. A flock.toml that does
/// not load counts as declaring them.
fn flocks_declared(paths: &Paths) -> bool {
    Flock::load(&paths.flock_file()).map_or(true, |f| !f.flocks.is_empty())
}

/// The protocol a head needs to read this machine's flock.toml without
/// dropping part of it on reload, and why, when the file uses a field older
/// heads' `FlockEntry` does not know: per-flock `machines`
/// (`MULTI_FLOCK_PROTOCOL`), a flock's `timeout` or `place`
/// (`FLOCK_TIMEOUT_PLACE_PROTOCOL`), a share and a max
/// (`FLOCK_SHARE_PROTOCOL`). The newest one the file uses wins.
fn flock_file_need(paths: &Paths) -> Option<(u32, &'static str)> {
    if flock_share_declared(paths) {
        Some((
            pastor::ipc::FLOCK_SHARE_PROTOCOL,
            "predates a flock's share and max on a machine, and would silently drop flock.toml's `machines` on reload, keeping its old membership",
        ))
    } else if flock_timeout_or_place_declared(paths) {
        Some((
            pastor::ipc::FLOCK_TIMEOUT_PLACE_PROTOCOL,
            "predates per-flock timeout and place, and would silently drop flock.toml's `timeout`/`place` on reload, keeping its old settings",
        ))
    } else if multi_flock_declared(paths) {
        Some((
            pastor::ipc::MULTI_FLOCK_PROTOCOL,
            "predates per-flock machine limits, and would silently drop flock.toml's `machines` on reload, keeping its old membership",
        ))
    } else {
        None
    }
}

/// Whether flock.toml gives any flock a share and a max on a machine
/// (`machines = { desk = { share = 2, max = 4 } }`), the schema an older
/// head's `FlockEntry` does not know (see `FLOCK_SHARE_PROTOCOL`). A
/// flock.toml that does not load counts as not declaring it.
fn flock_share_declared(paths: &Paths) -> bool {
    Flock::load(&paths.flock_file()).is_ok_and(|f| {
        f.flocks.iter().any(|e| {
            e.machines
                .values()
                .any(|n| matches!(n, pastor::config::flock::FlockNumber::Split(_)))
        })
    })
}

/// Whether flock.toml puts any machine in a flock with a number
/// (`[[flock]] machines = { desk = 2 }`), the schema an older head's
/// `FlockEntry` does not know (see `probe_head`'s `need` above). A
/// flock.toml that does not load counts as not declaring it: a head that
/// cannot read the file either is refused for other reasons first.
fn multi_flock_declared(paths: &Paths) -> bool {
    Flock::load(&paths.flock_file()).is_ok_and(|f| f.flocks.iter().any(|e| !e.machines.is_empty()))
}

/// Whether flock.toml gives any flock its own `timeout` or `place`, the
/// schema an older head's `FlockEntry` does not know (see
/// `FLOCK_TIMEOUT_PLACE_PROTOCOL`). A flock.toml that does not load counts
/// as not declaring it: a head that cannot read the file either is refused
/// for other reasons first.
fn flock_timeout_or_place_declared(paths: &Paths) -> bool {
    Flock::load(&paths.flock_file()).is_ok_and(|f| {
        f.flocks
            .iter()
            .any(|e| e.timeout.is_some() || e.place.is_some())
    })
}

/// The prompt of `pastor task run`: the positional one as given, or the
/// contents of `--prompt-file` (`-` is stdin) without the newlines an editor
/// or `echo` leaves at the end.
fn run_prompt(a: &RunArgs) -> anyhow::Result<String> {
    let Some(path) = a.prompt_file.as_deref() else {
        return Ok(a.prompt.clone().expect("clap requires a prompt or a file"));
    };
    let unreadable = |e: &dyn std::fmt::Display| {
        CliError::err(
            "prompt_file_unreadable",
            format!("cannot read the prompt from {path}: {e}"),
        )
    };
    let mut text = String::new();
    if path == "-" {
        std::io::stdin().read_to_string(&mut text)
    } else {
        std::fs::File::open(path).and_then(|mut f| f.read_to_string(&mut text))
    }
    .map_err(|e| unreadable(&e))?;
    let text = text.trim_end_matches(['\n', '\r']);
    if text.trim().is_empty() {
        return Err(CliError::err(
            "prompt_file_empty",
            format!("the prompt file {path} is empty"),
        ));
    }
    Ok(text.to_string())
}

async fn run(paths: &Paths, a: RunArgs) -> anyhow::Result<()> {
    if let Some(m) = &a.model {
        pastor::config::check_model_name(m).map_err(|e| {
            CliError::err(
                "unknown_model",
                format!("{e}; --model takes a name from [models] in pastor.toml, not agent args"),
            )
        })?;
    }
    let priority = a
        .priority
        .as_deref()
        .map(pastor::task_cli::parse_priority)
        .transpose()?;
    if let Some(p) = &a.profile {
        pastor::config::check_profile_name(p).map_err(|e| {
            CliError::err(
                pastor::config::profile::UNKNOWN_PROFILE,
                format!("{e}; --profile takes a profile name, see `pastor profile list`"),
            )
        })?;
    }
    let prompt = run_prompt(&a)?;
    // With a remote head, pastor.toml is the head's and not here: the built-in
    // defaults fill the spec, and the head resolves the agent again with its own.
    let config = if pastor::ipc::remote_head().is_some() {
        PastorConfig::default()
    } else {
        PastorConfig::load(&paths.config_file())?
    };
    let spec = run_spec(&a, &config)?;
    let resp = ask(
        paths,
        IpcRequest::Run {
            agent: Some(agent_choice(&a)),
            prompt,
            spec,
            flock: a.flock,
            priority,
            role: a.role,
            description: pastor::config::clean_description(a.description.as_deref()),
            preempt: a.preempt,
            summary: a.summary,
        },
    )
    .await?;
    let IpcResponse::Task(t) = resp else {
        return Err(unexpected(resp));
    };
    print_task(&t, a.json)
}

/// What `pastor task run`'s flags say about the agent, its timeout and its
/// place; the head fills in the rest from the task's flock and `[defaults]`.
fn agent_choice(a: &RunArgs) -> AgentChoice {
    AgentChoice {
        agent: a.agent.clone(),
        agent_args: (!a.agent_args.is_empty()).then(|| a.agent_args.clone()),
        model: a.model.clone(),
        profile: a.profile.clone(),
        // Checked by `run_spec`, which refuses a bad one.
        timeout_secs: a
            .timeout
            .as_deref()
            .and_then(|t| parse_duration(t).ok())
            .map(|d| d.as_secs()),
        place: a.place.clone(),
        ..Default::default()
    }
}

/// What `pastor task run`'s flags ask for, with pastor.toml's `[defaults]` filling
/// in what they leave out. The agent in it is only what a head from before
/// flock agents would run: a current head resolves it again, with the flock.
fn run_spec(a: &RunArgs, config: &PastorConfig) -> anyhow::Result<DispatchSpec> {
    let timeout = a
        .timeout
        .as_deref()
        .map(parse_duration)
        .transpose()
        .map_err(|e| anyhow::anyhow!(e))?
        .unwrap_or(config.timeout_duration());
    let pick = config.defaults.resolve_agent(&agent_choice(a), None);
    Ok(DispatchSpec {
        agent: pick.agent,
        agent_args: pick.agent_args,
        allow: pick.allow,
        deny: pick.deny,
        repo: a.repo.clone(),
        worktree: a.worktree,
        branch: a.branch.clone(),
        machine: a.machine.clone(),
        tags: a.tags.clone(),
        timeout_secs: timeout.as_secs(),
        checkout: None,
        reopen: None,
        agent_source: None,
        place: a
            .place
            .clone()
            .unwrap_or_else(|| config.defaults.place.clone()),
        session_id: None,
        // Only the ask: the head fills in the flock's or `[defaults]`.
        label: pastor::task::WorkspaceLabel {
            template: a.label.clone(),
            ..Default::default()
        },
        summary: Default::default(),
    })
}

/// Turn a direct probe (what `machine list` does with no head running) into a
/// row's channel, herdr version, protocol, agent count and error. Pure so the
/// classification is unit-testable without a live herdr.
///
/// `agent_count` is `None` when `ping` itself failed (agent.list was never
/// called), `Some(Err(_))` when ping succeeded but agent.list failed, and
/// `Some(Ok(n))` for the normal case. A machine that answered the ping is
/// `probed`; anything wrong past that (old protocol, agent.list failing) goes
/// in the error, and a failed agent.list never reads as zero agents.
///
/// A failed connect/ping is classified the same three ways the removed
/// `machine_status_row` used: `server down` when the connect error's own
/// message names `herdr.sock` (a `Local` endpoint with nothing listening),
/// `unreachable` for any other transport failure, and `error` otherwise. The
/// message is kept in the error field regardless.
type ProbeFields = (
    &'static str,
    Option<String>,
    Option<u32>,
    Option<usize>,
    Option<String>,
);

/// `--command` as argv: one value split on whitespace. It used to be
/// `num_args = 1..`, which swallowed every option after it.
fn command_argv(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

fn probe_fields(
    ping: Result<pastor::herdr::Pong, pastor::herdr::CallError>,
    agent_count: Option<Result<usize, pastor::herdr::CallError>>,
) -> ProbeFields {
    match ping {
        Ok(p) => {
            let old_protocol = (p.protocol < pastor::MIN_HERDR_PROTOCOL)
                .then(|| format!("protocol {} < {}", p.protocol, pastor::MIN_HERDR_PROTOCOL));
            let (agents, error) = match agent_count {
                Some(Ok(n)) => (Some(n), old_protocol),
                Some(Err(e)) => (None, Some(format!("agent.list: {e}"))),
                // The caller always attempts agent.list once ping succeeds.
                None => (None, Some("agent.list: not attempted".into())),
            };
            ("probed", Some(p.version), Some(p.protocol), agents, error)
        }
        Err(e) => {
            let message = e.to_string();
            let channel = if message.contains("herdr.sock") {
                "server down"
            } else if e.is_transport() {
                "unreachable"
            } else {
                "error"
            };
            (channel, None, None, None, Some(message))
        }
    }
}

/// One machine's row from a probe made here, without the head. Two ordinary
/// calls, each on its own connection, exactly as the head makes them: herdr
/// answers one request per connection. Orphans are agents no open task owns;
/// the rows live in the store here even with no head running.
/// The own profile of `m`, which decides whether a task may ask for
/// `unrestricted` there, from this machine's pastor.toml and flock.toml, as
/// the head would settle it (`Defaults::own_profile`).
fn own_profile(config: &PastorConfig, f: &Flock, m: &MachineConfig) -> Option<String> {
    config
        .defaults
        .own_profile(Some(m), f.entry(f.primary_flock(m)))
}

async fn probe_machine(
    m: &MachineConfig,
    f: &Flock,
    config: &PastorConfig,
    paths: &Paths,
    store: &Store,
) -> anyhow::Result<pastor::cli::MachineRow> {
    let flock = f.primary_flock(m);
    let on_machine = store.tasks_on_machine(&m.name)?;
    let flocks = f
        .flocks_of(m)
        .into_iter()
        .map(|(name, number)| {
            let live = on_machine
                .iter()
                .filter(|t| t.flock.as_deref().unwrap_or(f.default_flock()) == name)
                .count();
            pastor::machine::FlockSeat::new(name, number, live)
        })
        .collect();
    // Nothing connects to a pull machine, and with no head running nobody
    // has heard its claims: its row says so, with the tasks the store puts
    // there, rather than probing whatever herdr runs here.
    if m.pull {
        return Ok(pastor::cli::MachineRow {
            name: m.name.clone(),
            host: "pull".into(),
            endpoint: pastor::machine::PULL_ENDPOINT.into(),
            flock: flock.to_string(),
            flocks,
            channel: "not probed".into(),
            herdr_version: None,
            pastor_version: None,
            protocol: None,
            error: None,
            live: Some(on_machine.len()),
            max_agents: m.max_agents,
            job_slots: m.job_slots,
            burst: m.burst,
            tags: m.tags.clone(),
            orphans: vec![],
            profile: own_profile(config, f, m),
            description: pastor::config::clean_description(m.description.as_deref()),
        });
    }
    let ep = Endpoint::from_machine(m, paths);
    let ping = ep.ping().await;
    let agents = match &ping {
        Ok(_) => Some(ep.agent_list().await),
        Err(_) => None,
    };
    let orphans: Vec<String> = match &agents {
        Some(Ok(list)) => pastor::machine::orphan_agents(list, &on_machine)
            .into_iter()
            .map(|(name, _)| name)
            .collect(),
        _ => vec![],
    };
    let agent_count = agents.map(|r| r.map(|a| a.len()));
    // Only a machine that answered is worth the extra ssh. A failure here
    // leaves the version unknown and the probe's verdict alone, as it does
    // on the head.
    let pastor_version = match &ping {
        Ok(_) => ep.pastor_version().await.unwrap_or(None),
        Err(_) => None,
    };
    let (channel, herdr_version, protocol, live, error) = probe_fields(ping, agent_count);
    Ok(pastor::cli::MachineRow {
        name: m.name.clone(),
        host: ep.host(),
        endpoint: ep.describe(),
        flock: flock.to_string(),
        flocks,
        channel: channel.into(),
        herdr_version,
        pastor_version,
        protocol,
        error,
        live,
        max_agents: m.max_agents,
        job_slots: m.job_slots,
        burst: m.burst,
        tags: m.tags.clone(),
        orphans,
        profile: own_profile(config, f, m),
        description: pastor::config::clean_description(m.description.as_deref()),
    })
}

/// This machine's hostname (`config::hostname`).
fn hostname() -> String {
    pastor::config::hostname()
}

/// The head's row: this machine's hostname and the herdr it has, if any.
fn head_row() -> pastor::cli::HeadRow {
    let hostname = hostname();
    let herdr_version = std::process::Command::new("herdr")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| pastor::cli::herdr_version_from(&String::from_utf8_lossy(&o.stdout)));
    pastor::cli::HeadRow::new(hostname, herdr_version)
}

/// A remote head's row: its ssh destination for the host and the version
/// it answers ping with. Its herdr is not asked, so it reads `-`.
async fn remote_head_row(paths: &Paths, ssh: &str) -> anyhow::Result<pastor::cli::HeadRow> {
    let resp = ask(paths, IpcRequest::Ping).await?;
    let IpcResponse::Pong { version, .. } = resp else {
        return Err(unexpected(resp));
    };
    let mut row = pastor::cli::HeadRow::new(ssh.to_string(), None);
    row.pastor_version = version;
    Ok(row)
}

/// `machine list`: the head's view when it runs; otherwise a probe of each
/// machine in flock.toml. The note that says so is printed only once
/// everything worked, since a failure must leave exactly one JSON value on
/// stderr.
async fn machine_list(
    paths: &Paths,
    flock: Option<&str>,
    wide: bool,
    json: bool,
    head: Head,
) -> anyhow::Result<()> {
    // A head that is up but busy never gets here (`probe_head`), so the
    // machines are probed directly only when nothing is listening.
    let (rows, note) = match head {
        Head::Live => {
            let resp = ask(paths, IpcRequest::FlockList).await?;
            let IpcResponse::Machines(ms) = resp else {
                return Err(unexpected(resp));
            };
            let rows: Vec<pastor::cli::MachineRow> =
                ms.iter().map(pastor::cli::MachineRow::from).collect();
            (rows, None)
        }
        Head::Absent => {
            let f = Flock::load(&paths.flock_file())?;
            let config = PastorConfig::load(&paths.config_file())?;
            // Connects without the head, so the state dir the ssh master sockets
            // live under may not exist yet, and must be private.
            paths.ensure()?;
            let store = Store::open(&paths.db_file())?;
            let mut rows = Vec::new();
            // Only the machines asked for: a probe is an ssh round trip each.
            for m in f
                .machines
                .iter()
                .filter(|m| flock.is_none_or(|n| f.in_flock(m, n)))
            {
                rows.push(probe_machine(m, &f, &config, paths, &store).await?);
            }
            (
                rows,
                Some("pastor serve is not running; probed the machines directly"),
            )
        }
    };
    let mut rows: Vec<pastor::cli::MachineRow> = rows
        .into_iter()
        .filter(|m| flock.is_none_or(|n| m.in_flock(n)))
        .collect();
    pastor::cli::head_machine_first(&mut rows);
    let head = match pastor::ipc::remote_head() {
        Some(remote) => remote_head_row(paths, &remote.ssh).await?,
        None => head_row(),
    };
    if let Some(note) = note {
        eprintln!("{note}");
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&pastor::cli::machine_list_json(&head, &rows))?
        );
    } else {
        // Without a head the notice on stderr stands in for the line.
        if note.is_none() {
            println!("{}\n", pastor::cli::head_line(&head, &rows));
        }
        let descriptions: Vec<Option<String>> =
            rows.iter().map(|m| m.description.clone()).collect();
        println!(
            "{}",
            pastor::cli::list_table(
                &pastor::cli::MACHINE_HEADER,
                &pastor::cli::machine_rows(&rows),
                wide,
                &descriptions,
            )
        );
    }
    Ok(())
}

/// The states `pastor task list` selects, `None` meaning all of them. `--blocked`
/// and `--done` are single-state views, `--all` is everything, and no flag is
/// the live tasks. `--job` and `--machine` narrow whichever set this picks.
fn list_states(a: &ListArgs) -> Option<Vec<TaskState>> {
    if a.blocked {
        Some(vec![TaskState::Blocked])
    } else if a.done {
        Some(vec![TaskState::Done])
    } else if a.all {
        None
    } else {
        Some(LIVE_STATES.to_vec())
    }
}

/// What to say on stderr when the list comes back empty: only the default
/// view hides anything a user might be looking for.
fn list_empty_hint(a: &ListArgs) -> Option<&'static str> {
    (list_states(a).as_deref() == Some(&LIVE_STATES[..]))
        .then_some("no live tasks; pastor task list --all shows finished ones")
}

/// Whether `pastor task list` prints orphan lines. An orphan has no row, so
/// no state and no job: a view narrowed to a state (`--blocked`, `--done`)
/// or a job cannot select one. `--machine` still applies, to the orphans too.
fn list_shows_orphans(a: &ListArgs) -> bool {
    !a.blocked && !a.done && a.job.is_none()
}

async fn list(paths: &Paths, a: ListArgs, head: Head) -> anyhow::Result<()> {
    let states = list_states(&a);
    let hint = list_empty_hint(&a);
    let show_orphans = list_shows_orphans(&a);
    let filter = TaskFilter {
        job: a.job,
        machine: a.machine.clone(),
        states,
        flock: a.flock.clone(),
    };
    let tasks = if head.is_live() {
        let resp = ask(paths, IpcRequest::List { filter }).await?;
        let IpcResponse::Tasks(ts) = resp else {
            return Err(unexpected(resp));
        };
        ts
    } else {
        eprintln!("pastor serve is not running; showing the last known state");
        open_store(paths)?.list_tasks(&filter)?
    };
    if tasks.is_empty()
        && let Some(hint) = hint
    {
        eprintln!("{hint}");
    }
    if a.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&tasks.iter().map(Task::to_json).collect::<Vec<_>>())?
        );
    } else if tasks.is_empty() {
        if hint.is_none() {
            println!("no tasks");
        }
    } else {
        // Orchestrators first, in a table of their own; `--json` keeps one
        // array, with `role`.
        let (orchestrators, agents): (Vec<Task>, Vec<Task>) = tasks
            .iter()
            .cloned()
            .partition(|t| t.role == TaskRole::Orchestrator);
        // Titled only when there are orchestrators, so a plain list reads
        // as it always has.
        let titled = !orchestrators.is_empty();
        let mut tables = Vec::new();
        for (title, group) in [("orchestrators", &orchestrators), ("tasks", &agents)] {
            if group.is_empty() {
                continue;
            }
            let table = task_table(paths, group, a.wide).await?;
            tables.push(if titled {
                format!("{title}:\n{table}")
            } else {
                table
            });
        }
        println!("{}", tables.join("\n\n"));
    }
    // Orphans have no row to list, so they get a line each under the table.
    // Only a running head knows them (its last reconcile); `--json` stays a
    // plain task array, and `machine list --json` carries them instead.
    if head.is_live() && !a.json && show_orphans {
        let resp = ask(paths, IpcRequest::FlockList).await?;
        let IpcResponse::Machines(ms) = resp else {
            return Err(unexpected(resp));
        };
        for line in pastor::cli::orphan_lines(&ms, a.machine.as_deref(), a.flock.as_deref()) {
            println!("{line}");
        }
    }
    Ok(())
}

/// `task list`'s table of `tasks`, with `--wide`'s RESULT and DESCRIPTION.
async fn task_table(paths: &Paths, tasks: &[Task], wide: bool) -> anyhow::Result<String> {
    let mut rows = pastor::cli::task_rows(tasks);
    // The flock file is the truth for "removed" whether or not a head
    // runs (a running head re-reads it every tick). `load_existing`
    // rather than `load`: a flock.toml that is momentarily missing (an
    // editor's delete-and-rename, or a race with `machine add|remove`
    // rewriting it) must mark nothing, not everything (Copilot
    // 4103271200, 4103271289, 4103271156).
    // A remote head's flock.toml is not here: its machines are what it
    // reports.
    if pastor::ipc::remote_head().is_some() {
        let resp = ask(paths, IpcRequest::FlockList).await?;
        let IpcResponse::Machines(ms) = resp else {
            return Err(unexpected(resp));
        };
        pastor::cli::mark_removed(&mut rows, tasks, |m| ms.iter().any(|s| s.name == m));
    } else if let Ok(flock) = Flock::load_existing(&paths.flock_file()) {
        pastor::cli::mark_removed(&mut rows, tasks, |m| flock.get(m).is_some());
    }
    let descriptions: Vec<Option<String>> =
        tasks.iter().map(|t| Some(t.description_text())).collect();
    // `--wide` adds how each task's last round ended, before DESCRIPTION.
    let mut header = pastor::cli::TASK_HEADER.to_vec();
    if wide {
        header.push("RESULT");
        for (row, t) in rows.iter_mut().zip(tasks) {
            row.push(pastor::cli::task_result(t));
        }
    }
    Ok(pastor::cli::list_table(&header, &rows, wide, &descriptions))
}

/// The store, for a CLI path that reads it without the head. Rows from
/// before flocks join the default flock first, as the head does at start, so
/// `--flock` finds them. A flock.toml that does not load leaves them for the
/// head; the read itself does not depend on it.
fn open_store(paths: &Paths) -> anyhow::Result<Store> {
    paths.ensure()?;
    let store = Store::open(&paths.db_file())?;
    if let Ok(flock) = Flock::load(&paths.flock_file()) {
        store.adopt_default_flock(flock.default_flock())?;
    }
    Ok(store)
}

fn task_id(s: &str) -> i64 {
    parse_task_id(s).unwrap_or_else(|| fail("usage_error", &bad_task_id(s)))
}

async fn task(paths: &Paths, cmd: TaskCmd, head: Head) -> anyhow::Result<()> {
    match cmd {
        TaskCmd::Run(args) => run(paths, *args).await?,
        TaskCmd::List(args) => list(paths, args, head).await?,
        TaskCmd::Describe {
            task,
            all_summaries,
            json,
        } => {
            let id = task_id(&task);
            let (t, all) = if head.is_live() {
                let resp = ask(paths, IpcRequest::TaskShow { id }).await?;
                let IpcResponse::Task(t) = resp else {
                    return Err(unexpected(resp));
                };
                let all = if all_summaries {
                    let resp = ask(paths, IpcRequest::TaskSummaries { id }).await?;
                    let IpcResponse::Summaries(all) = resp else {
                        return Err(unexpected(resp));
                    };
                    Some(all)
                } else {
                    None
                };
                (t, all)
            } else {
                let store = open_store(paths)?;
                let t = store
                    .get_task(id)?
                    .unwrap_or_else(|| fail("task_not_found", &task));
                let all = if all_summaries {
                    Some(store.summaries(id)?)
                } else {
                    None
                };
                (t, all)
            };
            match (json, all) {
                (true, Some(all)) => {
                    let mut v = t.to_json();
                    v["summaries"] = serde_json::to_value(&all)?;
                    println!("{}", serde_json::to_string_pretty(&v)?);
                }
                (true, None) => print_task(&t, true)?,
                (false, Some(all)) => println!("{}", pastor::cli::task_detail_with(&t, &all)),
                (false, None) => println!("{}", pastor::cli::task_detail(&t)),
            }
        }
        TaskCmd::Read { task, lines } => {
            let resp = ask(
                paths,
                IpcRequest::TaskRead {
                    id: task_id(&task),
                    lines,
                },
            )
            .await?;
            let IpcResponse::Text(text) = resp else {
                return Err(unexpected(resp));
            };
            print!("{}", pastor::cli::printable(&text));
        }
        TaskCmd::Attach { task } => attach(paths, &task).await?,
        TaskCmd::Retry(a) => pastor::task_cli::retry(paths, a).await?,
        TaskCmd::Priority(a) => pastor::task_cli::priority(paths, a).await?,
        TaskCmd::Close(a) => pastor::task_cli::close(paths, a).await?,
        TaskCmd::Prune(a) => pastor::task_cli::prune(paths, a, head).await?,
        TaskCmd::Send(a) => pastor::task_cli::send(paths, a).await?,
        TaskCmd::Done(a) => pastor::task_cli::done(paths, a).await?,
    }
    Ok(())
}

async fn machine(paths: &Paths, cmd: MachineCmd, head: Head) -> anyhow::Result<()> {
    let path = paths.flock_file();
    match cmd {
        MachineCmd::Add {
            name,
            ssh,
            local,
            command,
            session,
            max_agents,
            job_slots,
            burst,
            tags,
            flock,
            description,
            herdr,
        } => {
            let m = MachineConfig {
                pull: false,
                description: pastor::config::clean_description(description.as_deref()),
                name: name.clone(),
                local,
                ssh,
                command: command.as_deref().map(command_argv),
                session,
                max_agents,
                job_slots,
                burst,
                tags,
                flock,
                agent: None,
                agent_args: None,
                model: None,
                priority: None,
                agents: Default::default(),
                profile: None,
            };
            // With a head the reload line comes with its answer, before the
            // herdr lines; without one it follows them.
            let reload = if head.is_live() {
                println!(
                    "{}",
                    ask_text(paths, IpcRequest::MachineAdd { machine: m.clone() }).await?
                );
                None
            } else {
                println!(
                    "{}",
                    fleet_edit::add_machine(&path, &m).map_err(edit_error)?
                );
                Some(reload_running_head(paths, head).await)
            };
            match (herdr, m.ssh.as_deref()) {
                // The flock file is already written: a herdr failure below is
                // reported, not rolled back, so the two lists never diverge
                // silently in the other direction either.
                (true, Some(target)) => {
                    herdr_cmd(&[
                        "machine",
                        "add",
                        target,
                        "--label",
                        &name,
                        "--remote-session",
                        &m.session,
                    ]);
                    println!("saved in herdr's sidebar as {name}");
                }
                (false, Some(target)) => println!(
                    "to see it in your laptop's herdr sidebar: herdr machine add {target} --label {name} (or pass --herdr)"
                ),
                _ => {}
            }
            if let Some(reload) = reload {
                println!("{reload}");
            }
        }
        MachineCmd::Remove { name, herdr } => {
            // Only for the herdr hint below, read before the machine goes,
            // from the head's file; a file that does not load gives no hint.
            let target = if herdr {
                head_flock(paths)
                    .await
                    .ok()
                    .and_then(|f| f.get(&name).and_then(|m| m.ssh.clone()))
            } else {
                None
            };
            if head.is_live() {
                println!(
                    "{}",
                    ask_text(paths, IpcRequest::MachineRemove { name: name.clone() }).await?
                );
            } else {
                let _lock = offline_fleet_lock(paths)?;
                let done = fleet_edit::remove_machine(&path, &name).map_err(edit_error)?;
                println!("{done}; {}", reload_running_head(paths, head).await);
            }
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
        MachineCmd::Move { name, flock } => {
            if head.is_live() {
                println!(
                    "{}",
                    ask_text(paths, IpcRequest::MachineMove { name, flock }).await?
                );
            } else {
                let done = fleet_edit::move_machine(&path, &name, &flock).map_err(edit_error)?;
                println!("{done}; {}", reload_running_head(paths, head).await);
            }
        }
        MachineCmd::List { flock, wide, json } => {
            machine_list(paths, flock.as_deref(), wide, json, head).await?
        }
        MachineCmd::Describe { name, json } => machine_describe(paths, &name, json, head).await?,
        MachineCmd::Open { name } => open(paths, &name).await?,
        MachineCmd::AuthorizedKey { name, key } => authorized_key(&path, &name, &key)?,
    }
    Ok(())
}

/// A refused flock.toml edit, with its stable code; any other error as it
/// is.
fn edit_error(err: impl Into<anyhow::Error>) -> anyhow::Error {
    let err = err.into();
    match err.downcast_ref::<EditError>() {
        Some(e) => pastor::cli::CliError::err(e.code(), e),
        None => err,
    }
}

/// The fleet lock for an edit made with no head, held until dropped: from
/// the edit's store check to its save of flock.toml, so a head starting
/// meanwhile waits and then reads the edited file. A head that started
/// listening before the lock came free could already have taken a task the
/// check would not see, so the edit stops with `head_started` instead.
fn offline_fleet_lock(paths: &Paths) -> anyhow::Result<std::fs::File> {
    let lock = fleet_edit::lock_fleet(paths, fleet_edit::FLEET_LOCK_WAIT)?;
    if std::os::unix::net::UnixStream::connect(paths.socket_file()).is_ok() {
        return Err(pastor::cli::CliError::err(
            "head_started",
            "pastor serve started while this edit waited for it; run the command again to make the edit through the head",
        ));
    }
    Ok(lock)
}

/// `ask`, for a request the head answers with `Text`.
async fn ask_text(paths: &Paths, req: IpcRequest) -> anyhow::Result<String> {
    let resp = ask(paths, req).await?;
    let IpcResponse::Text(text) = resp else {
        return Err(unexpected(resp));
    };
    Ok(text)
}

async fn flock(paths: &Paths, cmd: FlockCmd, head: Head) -> anyhow::Result<()> {
    let path = paths.flock_file();
    let edit = |f: &dyn Fn(&mut FlockDoc) -> Result<(), EditError>| -> anyhow::Result<()> {
        let mut doc = FlockDoc::open(&path)?;
        f(&mut doc).map_err(edit_error)?;
        doc.save(&path)
    };
    let done = match cmd {
        FlockCmd::List { wide, json } => return flock_list(paths, wide, json, head).await,
        FlockCmd::Describe { name, json } => {
            return flock_describe(paths, &name, json, head).await;
        }
        FlockCmd::Edit => return edit_file(paths, ConfigFile::Flock, head).await,
        FlockCmd::Add {
            name,
            machines,
            default,
            description,
        } => {
            if head.is_live() {
                println!(
                    "{}",
                    ask_text(
                        paths,
                        IpcRequest::FlockAdd {
                            name,
                            default,
                            description,
                            machines,
                        }
                    )
                    .await?
                );
                return Ok(());
            }
            let _lock = offline_fleet_lock(paths)?;
            let queued = || -> anyhow::Result<Vec<String>> {
                Ok(open_store(paths)?
                    .list_tasks(&TaskFilter {
                        states: Some(vec![TaskState::Queued]),
                        flock: Some(DEFAULT_FLOCK.into()),
                        ..Default::default()
                    })?
                    .iter()
                    .map(|t| t.display_id())
                    .collect())
            };
            fleet_edit::add_flock(
                &path,
                &name,
                default,
                description.as_deref(),
                &machines,
                queued,
            )
            .map_err(edit_error)?
        }
        FlockCmd::Join {
            flock,
            machine,
            max,
        } => {
            if head.is_live() {
                println!(
                    "{}",
                    ask_text(
                        paths,
                        IpcRequest::FlockJoin {
                            flock,
                            machine,
                            max
                        }
                    )
                    .await?
                );
                return Ok(());
            }
            let _lock = offline_fleet_lock(paths)?;
            fleet_edit::join_flock(&path, &flock, &machine, max).map_err(edit_error)?
        }
        FlockCmd::Leave { flock, machine } => {
            if head.is_live() {
                println!(
                    "{}",
                    ask_text(paths, IpcRequest::FlockLeave { flock, machine }).await?
                );
                return Ok(());
            }
            let _lock = offline_fleet_lock(paths)?;
            let queued = || -> anyhow::Result<Vec<Task>> {
                open_store(paths)?.list_tasks(&TaskFilter {
                    states: Some(vec![TaskState::Queued, TaskState::Paused]),
                    ..Default::default()
                })
            };
            fleet_edit::leave_flock(&path, &flock, &machine, queued).map_err(edit_error)?
        }
        FlockCmd::Remove { name } => {
            // With a head, the head checks and edits under the lock `task
            // run` takes, so no task can be queued in the flock in between.
            if head.is_live() {
                let resp = ask(paths, IpcRequest::FlockRemove { name }).await?;
                let IpcResponse::Text(done) = resp else {
                    return Err(unexpected(resp));
                };
                println!("{done}");
                return Ok(());
            }
            // With no head, the fleet lock covers the store check and the
            // file edit: a head starting meanwhile waits for the save and
            // reads the file without the flock, so it never queues a task in
            // it, and one that already listens stops the edit.
            let _lock = offline_fleet_lock(paths)?;
            let queued: Vec<String> = open_store(paths)?
                .list_tasks(&TaskFilter {
                    states: Some(vec![TaskState::Queued]),
                    flock: Some(name.clone()),
                    ..Default::default()
                })?
                .iter()
                .map(|t| t.display_id())
                .collect();
            edit(&|d| d.remove_flock(&name, &queued))?;
            format!("removed flock {name}")
        }
        FlockCmd::Default {
            cmd: FlockDefaultCmd::Show,
        } => {
            println!("{}", head_flock(paths).await?.default_flock());
            return Ok(());
        }
        FlockCmd::Default {
            cmd: FlockDefaultCmd::Set { name },
        } => {
            if head.is_live() {
                println!(
                    "{}",
                    ask_text(paths, IpcRequest::FlockSetDefault { name }).await?
                );
                return Ok(());
            }
            fleet_edit::set_default(&path, &name).map_err(edit_error)?
        }
    };
    println!("{done}; {}", reload_running_head(paths, head).await);
    Ok(())
}

/// `flock list`: the flock file, the queued tasks, and with a head running
/// the live agents on each flock's machines.
async fn flock_list(paths: &Paths, wide: bool, json: bool, head: Head) -> anyhow::Result<()> {
    let f = head_flock(paths).await?;
    let live = if head.is_live() {
        let resp = ask(paths, IpcRequest::FlockList).await?;
        let IpcResponse::Machines(ms) = resp else {
            return Err(unexpected(resp));
        };
        Some(ms)
    } else {
        eprintln!("pastor serve is not running; agents are unknown");
        None
    };
    let queued = queued_tasks(paths, head).await?;
    let rows = pastor::cli::flock_list(&f, live.as_deref(), &queued);
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        let descriptions: Vec<Option<String>> =
            rows.iter().map(|f| f.description.clone()).collect();
        println!(
            "{}",
            pastor::cli::list_table(
                &pastor::cli::FLOCK_HEADER,
                &pastor::cli::flock_rows(&rows),
                wide,
                &descriptions,
            )
        );
    }
    Ok(())
}

/// The head's copy of `file` when the head is on another machine, fetched
/// with `FileGet`, with the path it has there; `None` with the head here,
/// whose files are this machine's.
async fn head_file(
    paths: &Paths,
    file: ConfigFile,
) -> anyhow::Result<Option<(std::path::PathBuf, String)>> {
    if pastor::ipc::remote_head().is_none() {
        return Ok(None);
    }
    let resp = ask(
        paths,
        IpcRequest::FileGet {
            file: file.to_string(),
        },
    )
    .await?;
    let IpcResponse::File(got) = resp else {
        return Err(unexpected(resp));
    };
    Ok(Some((got.path.into(), got.text)))
}

/// flock.toml as the head reads it: the remote head's, else this machine's.
async fn head_flock(paths: &Paths) -> anyhow::Result<Flock> {
    match head_file(paths, ConfigFile::Flock).await? {
        Some((path, text)) => Flock::parse(&path, &text),
        None => Flock::load(&paths.flock_file()),
    }
}

/// pastor.toml as the head reads it: the remote head's, else this
/// machine's.
async fn head_config(paths: &Paths) -> anyhow::Result<PastorConfig> {
    match head_file(paths, ConfigFile::Config).await? {
        Some((path, text)) => PastorConfig::parse(&path, &text),
        None => PastorConfig::load(&paths.config_file()),
    }
}

/// `m` as this machine reaches it. The head's own machine (`local`) is,
/// seen from a CLI with a remote head, the head's ssh destination.
fn reach_from_here(m: &MachineConfig) -> MachineConfig {
    let mut m = m.clone();
    if m.local
        && let Some(r) = pastor::ipc::remote_head()
    {
        m.local = false;
        m.ssh = Some(r.ssh.clone());
    }
    m
}

/// The queued tasks, from the head when one runs, else from the store.
async fn queued_tasks(paths: &Paths, head: Head) -> anyhow::Result<Vec<Task>> {
    let filter = TaskFilter {
        states: Some(vec![TaskState::Queued]),
        ..Default::default()
    };
    tasks_matching(paths, head, filter).await
}

/// After `machine add|remove` rewrote flock.toml, a running head re-reads it
/// now (the `pastor job reload` path), so the edit needs no restart. `head`
/// is what the command's one probe found (`probe_head`); a head that did not
/// answer it stopped the command before the edit.
async fn reload_running_head(paths: &Paths, head: Head) -> &'static str {
    match head {
        Head::Absent => "start pastor serve to use it",
        Head::Live => match request(&paths.socket_file(), &IpcRequest::Reload).await {
            Ok(IpcResponse::Jobs(_)) => "the running pastor serve picked it up",
            _ => "pastor serve did not take the reload; run `pastor job reload`",
        },
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

/// The code `task attach` refuses a paused task with.
const TASK_PAUSED: &str = "task_paused";

/// Why `task attach` refuses `t`: a paused task's session resumes on its
/// own when a slot frees, and a second `claude --resume` of it in a pane of
/// attach's would put two agents on one conversation. `None` for any
/// other state.
fn paused_attach_refusal(t: &Task) -> Option<String> {
    (t.state == TaskState::Paused).then(|| {
        let by = t
            .pause
            .paused_for
            .map(|id| format!(" for {}", Task::agent_name_for(id)))
            .unwrap_or_default();
        format!(
            "{} is paused{by}; its session resumes in a new pane on {} when a slot there frees. Close it (`pastor task close {}`) to give it up",
            t.display_id(),
            t.machine.as_deref().unwrap_or("its machine"),
            t.display_id()
        )
    })
}

async fn attach(paths: &Paths, task: &str) -> anyhow::Result<()> {
    let id = task_id(task);
    // A remote head's task, its flock.toml and its pastor.toml; the pane
    // itself is reached from here.
    let t = if pastor::ipc::remote_head().is_some() {
        let resp = ask(paths, IpcRequest::TaskShow { id }).await?;
        let IpcResponse::Task(t) = resp else {
            return Err(unexpected(resp));
        };
        t
    } else {
        open_store(paths)?
            .get_task(id)?
            .unwrap_or_else(|| fail("task_not_found", task))
    };
    let (Some(machine), Some(mut agent)) = (t.machine.clone(), t.agent_name.clone()) else {
        fail("no_agent", &format!("{} has no agent yet", t.display_id()))
    };
    if let Some(why) = paused_attach_refusal(&t) {
        fail(TASK_PAUSED, &why);
    }
    let f = head_flock(paths).await?;
    let m = reach_from_here(
        f.get(&machine)
            .unwrap_or_else(|| fail("unknown_machine", &machine)),
    );
    let m = &m;
    // Its pane is gone: a Claude task's own session opens again in a new
    // pane, and the task stays as it is.
    if !t.state.occupies_pane() {
        let agents = head_config(paths).await?.agents;
        if let Some(why) = pastor::reopen::why_not(&t, &agents) {
            fail(
                "no_agent",
                &format!(
                    "{} is {}; nothing to attach to: {why}",
                    t.display_id(),
                    t.state
                ),
            );
        }
        if m.ssh.is_none() && !m.local {
            fail(
                "no_terminal",
                "command machines have no terminal to attach to",
            );
        }
        let ep = Endpoint::from_machine(m, paths);
        agent = pastor::reopen::reopen(&ep, &t, &agents)
            .await
            .unwrap_or_else(|e| fail(e.code, &e.message));
        if agent != Task::agent_name_for(t.id) {
            eprintln!(
                "{} is {}; its session is open again in {agent} on {machine}",
                t.display_id(),
                t.state
            );
        }
    }
    let err = if let Some(target) = &m.ssh {
        std::process::Command::new("ssh")
            .args(["-t", "--", target])
            .arg(pastor::herdr::posix_command(&attach_remote_command(
                &m.session, &agent,
            )))
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

/// `pastor machine authorized-key`: prints the line, and never edits
/// `authorized_keys` or any other file.
fn authorized_key(flock: &std::path::Path, machine: &str, key: &str) -> anyhow::Result<()> {
    if Flock::load(flock)?.get(machine).is_none() {
        fail("unknown_machine", machine);
    }
    let key = if key == "-" {
        std::io::read_to_string(std::io::stdin())?
    } else {
        std::fs::read_to_string(key).map_err(|e| {
            pastor::cli::CliError::err("invalid_key", format!("cannot read {key}: {e}"))
        })?
    };
    let pastor = std::env::current_exe()?;
    println!(
        "{}",
        pastor::bridge::authorized_key_line(&pastor, machine, &key)?
    );
    Ok(())
}

async fn open(paths: &Paths, machine: &str) -> anyhow::Result<()> {
    let f = head_flock(paths).await?;
    let m = reach_from_here(
        f.get(machine)
            .unwrap_or_else(|| fail("unknown_machine", machine)),
    );
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

fn print_jobs(jobs: &[JobStatus], wide: bool, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(jobs)?);
    } else if jobs.is_empty() {
        println!("no jobs");
    } else {
        let descriptions: Vec<Option<String>> =
            jobs.iter().map(|j| j.description.clone()).collect();
        println!(
            "{}",
            pastor::cli::list_table(
                &pastor::cli::JOB_HEADER,
                &pastor::cli::job_rows(jobs),
                wide,
                &descriptions,
            )
        );
    }
    Ok(())
}

/// Offline scheduler over the same store, for `tick` and `job list` when
/// `pastor serve` is down. Tasks it queues wait for the daemon.
fn standalone(paths: &Paths) -> anyhow::Result<Scheduler> {
    let config = PastorConfig::load(&paths.config_file())?;
    let store = Arc::new(open_store(paths)?);
    Ok(Scheduler::standalone(paths.clone(), &config, store)?.with_connectors())
}

async fn tick(paths: &Paths, a: TickArgs, head: Head) -> anyhow::Result<()> {
    let runs = if head.is_live() {
        let resp = ask(
            paths,
            IpcRequest::Tick {
                job: a.job,
                dry_run: a.dry_run,
            },
        )
        .await?;
        let IpcResponse::Runs(runs) = resp else {
            return Err(unexpected(resp));
        };
        runs
    } else {
        let mut s = standalone(paths)?;
        eprintln!(
            "pastor serve is not running; running the pass here (new tasks stay queued until it starts)"
        );
        s.tick_now(a.job.as_deref(), a.dry_run, chrono::Utc::now())
            .await
    };
    print_runs(&runs, a.json)
}

async fn reload(paths: &Paths) -> anyhow::Result<()> {
    let resp = ask(paths, IpcRequest::Reload).await?;
    let IpcResponse::Jobs(jobs) = resp else {
        return Err(unexpected(resp));
    };
    print_jobs(&jobs, false, false)
}

async fn job(paths: &Paths, cmd: JobCmd, head: Head) -> anyhow::Result<()> {
    // With a head elsewhere, this machine's own jobs run in its headless
    // serve: the list shows both, and a job whose file is here is driven here.
    if let Some(remote) = pastor::ipc::remote_head() {
        if let JobCmd::List { json, .. } = cmd {
            return shepherd_job_list(paths, &remote.ssh, json).await;
        }
        if job_name(&cmd).is_some_and(|name| is_local_job(paths, name)) {
            return local_job(paths, cmd).await;
        }
    }
    match cmd {
        JobCmd::List { wide, json } => {
            let jobs = if head.is_live() {
                let resp = ask(paths, IpcRequest::JobList).await?;
                let IpcResponse::Jobs(jobs) = resp else {
                    return Err(unexpected(resp));
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
            print_jobs(&jobs, wide, json)?;
        }
        JobCmd::Enable { name } => toggle(paths, &name, true, head).await?,
        JobCmd::Disable { name } => toggle(paths, &name, false, head).await?,
        JobCmd::Run { name } => {
            let resp = ask(paths, IpcRequest::JobRun { name }).await?;
            let IpcResponse::Text(msg) = resp else {
                return Err(unexpected(resp));
            };
            println!("{msg}");
        }
        JobCmd::Reload => reload(paths).await?,
        JobCmd::Edit { name } => edit_file(paths, ConfigFile::Job(name), head).await?,
        JobCmd::Describe { name, json } => job_describe(paths, &name, json, head).await?,
    }
    Ok(())
}

/// The job `cmd` names, if it names one.
fn job_name(cmd: &JobCmd) -> Option<&str> {
    match cmd {
        JobCmd::Enable { name }
        | JobCmd::Disable { name }
        | JobCmd::Run { name }
        | JobCmd::Edit { name }
        | JobCmd::Describe { name, .. } => Some(name),
        JobCmd::List { .. } | JobCmd::Reload => None,
    }
}

/// Whether `name` is this machine's job, with a head elsewhere: its file is
/// here. Job names are shared by the head and its shepherds, so a name on
/// both is not expected; the file here wins, being the one this machine's
/// serve runs.
fn is_local_job(paths: &Paths, name: &str) -> bool {
    ConfigFile::Job(name.to_string()).path(paths).is_ok()
}

/// `job list` with a head elsewhere: the head's jobs, then this machine's.
async fn shepherd_job_list(paths: &Paths, head: &str, json: bool) -> anyhow::Result<()> {
    let resp = ask(paths, IpcRequest::JobList).await?;
    let IpcResponse::Jobs(head_jobs) = resp else {
        return Err(unexpected(resp));
    };
    let here = local_jobs(paths).await?;
    if json {
        let jobs = pastor::cli::placed_jobs(&head_jobs, &here);
        println!("{}", serde_json::to_string_pretty(&jobs)?);
    } else {
        let text = pastor::cli::job_sections(head, &head_jobs, &hostname(), &here);
        println!("{text}");
    }
    Ok(())
}

/// This machine's jobs as its headless serve reports them, or with none
/// running, the job files and the last state it saved.
async fn local_jobs(paths: &Paths) -> anyhow::Result<Vec<JobStatus>> {
    if local_serve(paths).await? {
        let resp = ask_here(paths, IpcRequest::JobList).await?;
        let IpcResponse::Jobs(jobs) = resp else {
            return Err(unexpected(resp));
        };
        return Ok(jobs);
    }
    eprintln!(
        "this machine's pastor serve is not running; showing its job files and the last known state"
    );
    let config = PastorConfig::load(&paths.config_file())?;
    paths.ensure()?;
    let store = Arc::new(Store::open(&paths.shepherd_db_file())?);
    let mut s = Scheduler::standalone_headless(paths.clone(), &config, store).with_connectors();
    s.reload();
    Ok(s.statuses(chrono::Utc::now()))
}

/// Whether this machine's own `pastor serve` answers on its socket. One
/// that listens but does not answer stops the command, as a head does.
async fn local_serve(paths: &Paths) -> anyhow::Result<bool> {
    let socket = paths.socket_file();
    match pastor::ipc::ping_head(&socket).await {
        HeadPing::NotRunning => Ok(false),
        HeadPing::Pong {
            role: Some(role), ..
        } if role == pastor::ipc::SHEPHERD_ROLE => Ok(true),
        HeadPing::Pong { .. } => Err(CliError::err(
            "shepherd_unexpected",
            format!(
                "{} is a normal pastor head, not this machine's shepherd serve; this machine's own jobs cannot be read",
                socket.display()
            ),
        )),
        HeadPing::Unresponsive => Err(CliError::err(
            "shepherd_unresponsive",
            format!(
                "this machine's pastor serve is listening on {} but not answering; nothing was done, run it again once it answers",
                socket.display()
            ),
        )),
    }
}

/// `ask` of this machine's own serve, never the head: for its own jobs
/// while a head is set elsewhere.
async fn ask_here(paths: &Paths, req: IpcRequest) -> anyhow::Result<IpcResponse> {
    Ok(pastor::cli::reply(
        request(&paths.socket_file(), &req).await,
    )?)
}

/// A job whose file is here, with a head elsewhere: this machine's serve
/// runs it, so its file and state are here and only its tasks are the
/// head's.
async fn local_job(paths: &Paths, cmd: JobCmd) -> anyhow::Result<()> {
    match cmd {
        JobCmd::Run { name } => {
            if !local_serve(paths).await? {
                return Err(CliError::err(
                    "shepherd_not_running",
                    "this machine's pastor serve is not running; start it to run a job here",
                ));
            }
            let resp = ask_here(paths, IpcRequest::JobRun { name }).await?;
            let IpcResponse::Text(msg) = resp else {
                return Err(unexpected(resp));
            };
            println!("{msg}");
        }
        JobCmd::Enable { name } => local_toggle(paths, &name, true).await?,
        JobCmd::Disable { name } => local_toggle(paths, &name, false).await?,
        JobCmd::Edit { name } => {
            let file = ConfigFile::Job(name);
            let path = file.path(paths)?;
            let check = file.checker(paths)?;
            let editor = pastor::edit::editor();
            match pastor::edit::edit_here(&path, &editor, &check, &mut |_| ask_reopen()).await? {
                Outcome::Unchanged => println!("no changes to {}", path.display()),
                Outcome::Saved => {
                    println!("saved {}; {}", path.display(), reload_here(paths).await?)
                }
            }
        }
        JobCmd::Describe { name, json } => {
            let statuses = local_jobs(paths).await?;
            let store = Store::open(&paths.shepherd_db_file())?;
            let mut d = pastor::describe::job(paths, &name, statuses, &store)?;
            let filter = TaskFilter {
                job: Some(name),
                ..Default::default()
            };
            let resp = ask(paths, IpcRequest::List { filter }).await?;
            let IpcResponse::Tasks(tasks) = resp else {
                return Err(unexpected(resp));
            };
            d.tasks = tasks.into_iter().take(pastor::describe::RECENT).collect();
            print_description(&d, json, pastor::describe::job_text)?;
        }
        JobCmd::List { .. } | JobCmd::Reload => unreachable!("names no job"),
    }
    Ok(())
}

/// `toggle`'s own guard: `job enable|disable` reaches this machine's serve
/// with no head to ask about the caller's role (the shepherd keeps no task
/// store, `shepherd.rs`), so a task caller is refused unless
/// `agents_change_fleet` is on, orchestrator or not.
async fn local_toggle(paths: &Paths, name: &str, enabled: bool) -> anyhow::Result<()> {
    if let Some(task) = pastor::ipc::caller_task()
        && !agents_change_fleet(paths)
    {
        return Err(CliError::err(
            "agent_refused",
            format!(
                "{task} is an agent pastor started; this machine's own jobs have no head to check its role, so it may not {} a job",
                if enabled { "enable" } else { "disable" }
            ),
        ));
    }
    set_enabled(&ConfigFile::Job(name.to_string()).path(paths)?, enabled)?;
    let verb = if enabled { "enabled" } else { "disabled" };
    println!("{verb} {name}; {}", reload_here(paths).await?);
    Ok(())
}

/// After a job file here changed: this machine's serve re-reads it now, or
/// when it starts.
async fn reload_here(paths: &Paths) -> anyhow::Result<&'static str> {
    Ok(if local_serve(paths).await? {
        ask_here(paths, IpcRequest::Reload).await?;
        "this machine's pastor serve picked it up"
    } else {
        "applies when this machine's pastor serve starts"
    })
}

/// Edit `file` in the user's editor, checked the way the head loads it.
/// With a head the file is the head's: fetched with `FileGet`, edited here,
/// sent back with `FilePut`, which checks it, writes it and reloads the
/// head; an invalid edit comes back with the head's error and reopens the
/// editor. With no head the file is the local one, through the same
/// `edit::put`.
async fn edit_file(paths: &Paths, file: ConfigFile, head: Head) -> anyhow::Result<()> {
    let editor = pastor::edit::editor();
    if !head.is_live() {
        let path = file.path(paths)?;
        let check = file.checker(paths)?;
        match pastor::edit::edit_here(&path, &editor, &check, &mut |_| ask_reopen()).await? {
            Outcome::Unchanged => println!("no changes to {}", path.display()),
            Outcome::Saved => println!("saved {}; start pastor serve to use it", path.display()),
        }
        return Ok(());
    }
    let resp = ask(
        paths,
        IpcRequest::FileGet {
            file: file.to_string(),
        },
    )
    .await?;
    let IpcResponse::File(got) = resp else {
        return Err(unexpected(resp));
    };
    let label = std::path::PathBuf::from(&got.path);
    let mut saved = String::new();
    let put = async |text: &str| {
        let req = IpcRequest::FilePut {
            file: file.to_string(),
            text: text.to_string(),
            base_hash: got.hash.clone(),
        };
        let resp = ask(paths, req).await?;
        let IpcResponse::Text(msg) = resp else {
            return Err(unexpected(resp));
        };
        saved = msg;
        Ok(())
    };
    match pastor::edit::edit(&label, &got.text, &editor, put, &mut |_| ask_reopen()).await? {
        Outcome::Unchanged => println!("no changes to {}", label.display()),
        Outcome::Saved => println!("{saved}"),
    }
    Ok(())
}

/// Whether to reopen an invalid edit: yes unless the answer on stdin says
/// no. No answer at all (stdin closed) is a no, so a script never loops.
fn ask_reopen() -> bool {
    eprint!("reopen the editor to fix it? [Y/n] ");
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    // A terminal echoes the answer and its newline; anything else does not.
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!();
    }
    match read {
        Ok(0) | Err(_) => false,
        Ok(_) => {
            let a = line.trim().to_ascii_lowercase();
            a.is_empty() || a == "y" || a == "yes"
        }
    }
}

/// `config edit`: the head's pastor.toml. With `--local` and a head on
/// another machine, this machine's, which its headless serve reads; with the
/// head here the two are the same file.
async fn config_edit(paths: &Paths, local: bool, head: Head) -> anyhow::Result<()> {
    if !(local && pastor::ipc::remote_head().is_some()) {
        return edit_file(paths, ConfigFile::Config, head).await;
    }
    let path = paths.config_file();
    let check = ConfigFile::Config.checker(paths)?;
    let editor = pastor::edit::editor();
    match pastor::edit::edit_here(&path, &editor, &check, &mut |_| ask_reopen()).await? {
        Outcome::Unchanged => println!("no changes to {}", path.display()),
        Outcome::Saved => println!("saved {}; {}", path.display(), reload_here(paths).await?),
    }
    Ok(())
}

/// Tasks matching `filter`, from the head when one runs, else the store.
async fn tasks_matching(
    paths: &Paths,
    head: Head,
    filter: TaskFilter,
) -> anyhow::Result<Vec<Task>> {
    if head.is_live() {
        let resp = ask(paths, IpcRequest::List { filter }).await?;
        let IpcResponse::Tasks(ts) = resp else {
            return Err(unexpected(resp));
        };
        Ok(ts)
    } else {
        Ok(open_store(paths)?.list_tasks(&filter)?)
    }
}

/// The events log, or nothing when it cannot be read: a description is
/// still worth printing without it.
fn events_log(paths: &Paths) -> Vec<pastor::events::EventRecord> {
    pastor::events::read(&paths.events_file(), None).unwrap_or_default()
}

fn print_description<T: serde::Serialize>(
    d: &T,
    json: bool,
    text: fn(&T) -> String,
) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(d)?);
    } else {
        println!("{}", text(d));
    }
    Ok(())
}

async fn job_describe(paths: &Paths, name: &str, json: bool, head: Head) -> anyhow::Result<()> {
    let d = if head.is_live() {
        let resp = ask(
            paths,
            IpcRequest::JobDescribe {
                name: name.to_string(),
            },
        )
        .await?;
        let IpcResponse::Job(d) = resp else {
            return Err(unexpected(resp));
        };
        d
    } else {
        // The file first, so a missing job says so before the scheduler
        // loads anything.
        ConfigFile::Job(name.to_string()).path(paths)?;
        let mut s = standalone(paths)?;
        s.reload();
        let statuses = s.statuses(chrono::Utc::now());
        pastor::describe::job(paths, name, statuses, &open_store(paths)?)?
    };
    print_description(&d, json, pastor::describe::job_text)
}

/// `machine describe`: the head's description when one runs, else one
/// built from flock.toml, a probe and the store.
async fn machine_describe(paths: &Paths, name: &str, json: bool, head: Head) -> anyhow::Result<()> {
    if head.is_live() {
        let req = IpcRequest::MachineDescribe {
            name: name.to_string(),
        };
        let resp = ask(paths, req).await?;
        let IpcResponse::MachineDescription(d) = resp else {
            return Err(unexpected(resp));
        };
        return print_description(&d, json, pastor::describe::machine_text);
    }
    let f = Flock::load(&paths.flock_file())?;
    let Some(m) = f.get(name) else {
        fail(
            "unknown_machine",
            &format!("no machine {name} in the flock"),
        );
    };
    paths.ensure()?;
    let store = Store::open(&paths.db_file())?;
    let config = PastorConfig::load(&paths.config_file())?;
    let row = probe_machine(m, &f, &config, paths, &store).await?;
    let tasks = store.list_tasks(&pastor::describe::machine_tasks(name))?;
    let d = pastor::describe::MachineDescription {
        row,
        session: m.session.clone(),
        model: m.model.clone(),
        agents_by_kind: m.agents.clone(),
        tasks,
        recent_errors: pastor::describe::machine_errors(events_log(paths), name),
    };
    print_description(&d, json, pastor::describe::machine_text)
}

/// `flock describe`: the head's description when one runs, else one built
/// from flock.toml and the store.
async fn flock_describe(paths: &Paths, name: &str, json: bool, head: Head) -> anyhow::Result<()> {
    if head.is_live() {
        let req = IpcRequest::FlockDescribe {
            name: name.to_string(),
        };
        let resp = ask(paths, req).await?;
        let IpcResponse::FlockDescription(d) = resp else {
            return Err(unexpected(resp));
        };
        return print_description(&d, json, pastor::describe::flock_text);
    }
    let f = Flock::load(&paths.flock_file())?;
    if !f.has_flock(name) {
        fail("unknown_flock", &format!("no flock {name}"));
    }
    let tasks = open_store(paths)?.list_tasks(&pastor::describe::flock_tasks(name))?;
    let d = pastor::describe::flock_description(&f, name, None, tasks).expect("has_flock");
    print_description(&d, json, pastor::describe::flock_text)
}

async fn toggle(paths: &Paths, name: &str, enabled: bool, head: Head) -> anyhow::Result<()> {
    // Let through from a task's pane for an orchestrator
    // (`orchestrator_may`), whose role only the head knows.
    if !head.is_live()
        && let Some(task) = pastor::ipc::caller_task()
        && !agents_change_fleet(paths)
    {
        return Err(CliError::err(
            "agent_refused",
            format!(
                "{task} is an agent pastor started; with no pastor serve running to check its role, it may not {} a job",
                if enabled { "enable" } else { "disable" }
            ),
        ));
    }
    if head.is_live() {
        let req = IpcRequest::JobSetEnabled {
            name: name.to_string(),
            enabled,
        };
        let resp = ask(paths, req).await?;
        let IpcResponse::Text(done) = resp else {
            return Err(unexpected(resp));
        };
        println!("{done}");
        return Ok(());
    }
    set_enabled(&ConfigFile::Job(name.to_string()).path(paths)?, enabled)?;
    let verb = if enabled { "enabled" } else { "disabled" };
    println!("{verb} {name}; applies when pastor serve starts");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pastor::ipc::RequestError;

    fn run_args(argv: &[&str]) -> RunArgs {
        let mut full = vec!["pastor", "task", "run"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full).unwrap().command.unwrap() {
            Command::Task {
                cmd: TaskCmd::Run(a),
            } => *a,
            other => panic!("{other:?}"),
        }
    }

    /// `task run --role orchestrator` is what no task may send; from a
    /// task's pane, the commands an orchestrator may make are left to the
    /// head, which knows its role.
    #[test]
    fn only_a_person_makes_an_orchestrator() {
        let parse = |argv: &[&str]| Cli::try_parse_from(argv).unwrap().command.unwrap();
        let orch = parse(&["pastor", "task", "run", "x", "--role", "orchestrator"]);
        assert!(makes_orchestrator(&orch));
        let plain = parse(&["pastor", "task", "run", "x"]);
        assert!(!makes_orchestrator(&plain));
        assert!(Cli::try_parse_from(["pastor", "task", "run", "x", "--role", "boss"]).is_err());
        for argv in [
            &["pastor", "task", "run", "x"][..],
            &["pastor", "task", "retry", "t-1"],
            &["pastor", "task", "send", "t-1", "go"],
            &["pastor", "job", "disable", "j"],
            &["pastor", "task", "close", "t-1"],
            &["pastor", "job", "enable", "j"],
        ] {
            assert!(orchestrator_may(&parse(argv)), "{argv:?}");
        }
        for argv in [
            &["pastor", "task", "prune", "--done", "--older-than", "1d"][..],
            &["pastor", "job", "run", "j"],
            &["pastor", "machine", "add", "m", "--local"],
        ] {
            assert!(!orchestrator_may(&parse(argv)), "{argv:?}");
        }
    }
    /// An agent flag starts with `-`, so `--agent-arg` must take it as its
    /// value in both spellings rather than read it as a pastor flag.
    #[test]
    fn agent_arg_takes_values_that_start_with_a_dash() {
        for argv in [
            &[
                "hi",
                "--agent-arg",
                "--model",
                "--agent-arg",
                "claude-opus-5-5",
            ][..],
            &["hi", "--agent-arg=--model", "--agent-arg=claude-opus-5-5"][..],
            &[
                "--agent-arg",
                "--model",
                "--agent-arg",
                "claude-opus-5-5",
                "hi",
            ][..],
        ] {
            let a = run_args(argv);
            assert_eq!(a.agent_args, vec!["--model", "claude-opus-5-5"], "{argv:?}");
            assert_eq!(a.prompt.as_deref(), Some("hi"), "{argv:?}");
        }
        assert!(run_args(&["hi"]).agent_args.is_empty());
        // One value per flag: the next word is the prompt, not a second arg.
        let a = run_args(&["--agent-arg", "-v", "hi", "--json"]);
        assert_eq!(a.agent_args, vec!["-v"]);
        assert!(a.json);
    }

    #[test]
    fn run_agent_args_fall_back_to_defaults_only_when_none_are_given() {
        let config = PastorConfig {
            defaults: pastor::config::Defaults {
                agent_args: vec!["--model".into(), "claude-sonnet-5".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let spec = run_spec(&run_args(&["hi"]), &config).unwrap();
        assert_eq!(spec.agent_args, vec!["--model", "claude-sonnet-5"]);
        let spec = run_spec(&run_args(&["hi", "--agent-arg=--verbose"]), &config).unwrap();
        assert_eq!(spec.agent_args, vec!["--verbose"]);
    }

    /// Only `queue move` changes the fleet.
    #[test]
    fn only_a_queue_move_changes_the_fleet() {
        let parse = |argv: &[&str]| Cli::try_parse_from(argv).unwrap().command.unwrap();
        assert!(!changes_fleet(&parse(&["pastor", "queue", "--flock", "w"])));
        assert!(changes_fleet(&parse(&[
            "pastor", "queue", "move", "t-1", "--to", "2"
        ])));
    }
    /// `flock join|leave` are fleet edits: an agent pastor started may not
    /// run them.
    #[test]
    fn flock_join_and_leave_change_the_fleet() {
        let parse = |argv: &[&str]| Cli::try_parse_from(argv).unwrap().command.unwrap();
        for argv in [
            &["pastor", "flock", "join", "work", "desk"][..],
            &["pastor", "flock", "join", "work", "desk", "--max", "2"],
            &["pastor", "flock", "leave", "work", "desk"],
            &["pastor", "flock", "add", "work", "desk"],
        ] {
            assert!(changes_fleet(&parse(argv)), "{argv:?}");
        }
    }
    /// `task run --summary` takes the modes a task can have, not `always`.
    #[test]
    fn summary_setting_takes_only_task_modes() {
        assert!(
            Cli::try_parse_from(["pastor", "task", "run", "hi", "--summary", "require"]).is_ok()
        );
        assert!(
            Cli::try_parse_from(["pastor", "task", "run", "hi", "--summary", "always"]).is_err()
        );
    }
    /// `task attach` refuses a paused task: its session resumes on its own,
    /// and a second resume would put two agents on one conversation.
    #[test]
    fn attach_refuses_a_paused_task() {
        let mut t: Task = serde_json::from_value(serde_json::json!({
            "id": 4, "job": "run", "item": null, "prompt": "p",
            "spec": {"agent": "claude"}, "machine": "pi-1", "workspace_id": null,
            "pane_id": null, "agent_name": "t-4", "state": "running", "error": null,
            "last_completion_seq": null,
            "created_at": "2026-09-28T10:00:00Z", "started_at": null,
            "finished_at": null, "updated_at": "2026-09-28T10:00:00Z"
        }))
        .unwrap();
        assert_eq!(paused_attach_refusal(&t), None);
        t.state = TaskState::Paused;
        t.pause.paused_for = Some(9);
        let why = paused_attach_refusal(&t).unwrap();
        assert!(why.starts_with("t-4 is paused for t-9;"), "{why}");
        assert!(why.contains("pi-1"), "{why}");
    }

    /// `task run --label` sends its template; a bad one is refused before
    /// anything is sent.
    #[test]
    fn a_label_is_checked_before_it_is_sent() {
        let parse = |argv: &[&str]| Cli::try_parse_from(argv).unwrap().command.unwrap();
        let Command::Task {
            cmd: TaskCmd::Run(a),
        } = parse(&["pastor", "task", "run", "hi", "--label", "x/{{ task.id }}"])
        else {
            panic!()
        };
        let spec = run_spec(&a, &PastorConfig::default()).unwrap();
        assert_eq!(spec.label.template.as_deref(), Some("x/{{ task.id }}"));
        let spec = run_spec(&run_args(&["hi"]), &PastorConfig::default()).unwrap();
        assert_eq!(spec.label.template, None);
        let err = Cli::try_parse_from(["pastor", "task", "run", "hi", "--label", "{{ nope }}"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown placeholder"), "{err}");
    }
    /// `task priority` changes the fleet.
    #[test]
    fn a_priority_changes_the_fleet() {
        let parse = |argv: &[&str]| Cli::try_parse_from(argv).unwrap().command.unwrap();
        assert!(changes_fleet(&parse(&[
            "pastor", "task", "priority", "t-1", "low"
        ])));
    }
    /// The trust and describe commands ask the head, never a copy here,
    /// and only a trust change counts against an agent.
    #[test]
    fn trust_and_describe_ask_the_head() {
        let parse = |argv: &[&str]| Cli::try_parse_from(argv).unwrap().command.unwrap();
        for argv in [
            &["pastor", "trust", "list"][..],
            &["pastor", "trust", "add", "m", "/r"][..],
            &["pastor", "trust", "remove", "m", "/r"][..],
            &["pastor", "flock", "describe", "f"][..],
            &["pastor", "machine", "describe", "m"][..],
        ] {
            assert_eq!(head_use(&parse(argv)), Some(false), "{argv:?}");
        }
        assert!(!changes_fleet(&parse(&["pastor", "trust", "list"])));
        assert!(changes_fleet(&parse(&[
            "pastor", "trust", "add", "m", "/r"
        ])));
        assert!(changes_fleet(&parse(&[
            "pastor", "trust", "remove", "m", "/r"
        ])));
    }
    #[test]
    fn run_sends_only_the_agent_its_flags_name() {
        assert_eq!(agent_choice(&run_args(&["hi"])), AgentChoice::default());
        let a = run_args(&["hi", "--agent", "codex", "--agent-arg=-v"]);
        assert_eq!(
            agent_choice(&a),
            AgentChoice {
                agent: Some("codex".into()),
                agent_args: Some(vec!["-v".into()]),
                ..Default::default()
            }
        );
        // Sent as asked, so the head knows they win over the flock's.
        let a = run_args(&["hi", "--timeout", "5m", "--place", "own"]);
        assert_eq!(
            agent_choice(&a),
            AgentChoice {
                timeout_secs: Some(300),
                place: Some(pastor::task::Place::Own),
                ..Default::default()
            }
        );
    }

    fn list_args(argv: &[&str]) -> ListArgs {
        let mut full = vec!["pastor", "task", "list"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full).unwrap().command.unwrap() {
            Command::Task {
                cmd: TaskCmd::List(a),
            } => a,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_busy_head_is_a_timeout_not_a_missing_daemon() {
        let CliError { code, message } =
            pastor::cli::request_error(&RequestError::Timeout(std::time::Duration::from_secs(120)));
        assert_eq!(code, "timeout");
        assert!(message.contains("did not answer within 120s"), "{message}");
        assert!(message.contains("may still complete"), "{message}");
        assert!(message.contains("pastor task list"), "{message}");
        assert!(message.contains("pastor job list"), "{message}");
        assert!(!message.contains("retry"), "{message}");
        assert!(!message.contains("try again"), "{message}");
        assert!(!message.contains("not running"), "{message}");

        for kind in [
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::NotFound,
        ] {
            let err = std::io::Error::from(kind);
            let CliError { code, message } =
                pastor::cli::request_error(&RequestError::Connect(err));
            assert_eq!(code, "daemon_not_running");
            assert!(message.contains("not running"), "{kind:?}: {message}");
            assert!(message.contains("start it with"), "{kind:?}: {message}");
        }
    }

    #[test]
    fn a_connect_that_is_not_refused_does_not_say_the_head_is_down() {
        // probe_daemon treats these as "something may be there", so the CLI
        // must not tell the user to start a second head.
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::Other,
        ] {
            let err = std::io::Error::from(kind);
            let CliError { code, message } =
                pastor::cli::request_error(&RequestError::Connect(err));
            assert_eq!(code, "runtime_error");
            assert!(!message.contains("not running"), "{kind:?}: {message}");
            assert!(!message.contains("start it"), "{kind:?}: {message}");
            assert!(message.contains("could not connect"), "{kind:?}: {message}");
        }
    }

    #[test]
    fn orphans_show_only_when_state_and_job_are_not_filtered() {
        for argv in [
            &[][..],
            &["--all"],
            &["--machine", "a"],
            &["--all", "--machine", "a"],
        ] {
            assert!(list_shows_orphans(&list_args(argv)), "{argv:?}");
        }
        for argv in [
            &["--blocked"][..],
            &["--done"],
            &["--job", "j"],
            &["--all", "--job", "j"],
        ] {
            assert!(!list_shows_orphans(&list_args(argv)), "{argv:?}");
        }
    }

    #[test]
    fn default_list_shows_only_live_tasks() {
        let states = list_states(&list_args(&[])).expect("the default view filters");
        assert_eq!(
            states,
            vec![
                TaskState::Queued,
                TaskState::Starting,
                TaskState::Running,
                TaskState::Blocked,
                TaskState::Paused,
            ]
        );
        for s in [
            TaskState::Done,
            TaskState::Failed,
            TaskState::Stale,
            TaskState::Closed,
        ] {
            assert!(!states.contains(&s), "{s} is finished, not live");
        }
        // `--job` and `--machine` narrow the live set; they do not widen it.
        let a = list_args(&["--job", "hourly", "--machine", "pi-3"]);
        assert_eq!(list_states(&a), Some(states));
        assert!(list_empty_hint(&a).is_some());
    }

    #[test]
    fn list_all_shows_every_state() {
        assert_eq!(list_states(&list_args(&["--all"])), None);
        assert_eq!(list_states(&list_args(&["--all", "--job", "hourly"])), None);
        assert_eq!(list_empty_hint(&list_args(&["--all"])), None);
    }

    #[test]
    fn list_blocked_and_done_stay_single_state_views() {
        let blocked = list_args(&["--blocked"]);
        assert_eq!(list_states(&blocked), Some(vec![TaskState::Blocked]));
        assert_eq!(list_empty_hint(&blocked), None);
        let done = list_args(&["--done"]);
        assert_eq!(list_states(&done), Some(vec![TaskState::Done]));
        assert_eq!(list_empty_hint(&done), None);
    }

    #[test]
    fn empty_default_list_points_at_all() {
        assert_eq!(
            list_empty_hint(&list_args(&[])),
            Some("no live tasks; pastor task list --all shows finished ones")
        );
        assert_eq!(
            list_empty_hint(&list_args(&["--json"])),
            list_empty_hint(&list_args(&[]))
        );
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
        let (channel, version, protocol, agents, error) =
            probe_fields(Ok(pong()), Some(Err(agent_list_error())));
        assert_eq!(channel, "probed", "herdr answered the ping");
        assert_eq!(version, Some("0.9.1".into()));
        assert_eq!(protocol, Some(pastor::MIN_HERDR_PROTOCOL));
        assert_eq!(agents, None, "agent count must show as absent, not zero");
        let error = error.expect("the agent.list error must be surfaced");
        assert!(error.contains("boom"), "{error}");
    }

    #[test]
    fn agent_list_success_reads_as_probed() {
        let (channel, _, _, agents, error) = probe_fields(Ok(pong()), Some(Ok(3)));
        assert_eq!(channel, "probed");
        assert_eq!(agents, Some(3));
        assert_eq!(error, None);
    }

    #[test]
    fn an_old_herdr_is_probed_with_the_protocol_as_its_error() {
        let old = pastor::herdr::Pong {
            version: "0.8.0".into(),
            protocol: pastor::MIN_HERDR_PROTOCOL - 1,
        };
        let (channel, _, _, _, error) = probe_fields(Ok(old), Some(Ok(0)));
        assert_eq!(channel, "probed");
        assert!(error.unwrap().starts_with("protocol "));
    }

    #[test]
    fn a_failed_ping_is_unreachable_with_the_reason() {
        let err = pastor::herdr::CallError::from(pastor::herdr::HerdrError::Closed);
        let (channel, version, protocol, agents, error) = probe_fields(Err(err), None);
        assert_eq!(channel, "unreachable");
        assert_eq!(version, None);
        assert_eq!(protocol, None);
        assert_eq!(agents, None);
        assert!(error.is_some());
    }

    /// A `Local` endpoint's own connect error names its `herdr.sock`, the way
    /// `connect` in `herdr/transport.rs` formats it; that reads as `server
    /// down`, not the generic `unreachable` a network failure gets.
    /// `tests/cli.rs`'s `machine_list_without_daemon_reads_a_local_machine_with_no_server_as_down`
    /// covers the same case end to end.
    #[test]
    fn a_local_connect_failure_naming_its_socket_reads_as_server_down() {
        let err = pastor::herdr::CallError::from(pastor::herdr::ConnectError {
            message: "connect /home/fake/.config/herdr/herdr.sock: connection refused".into(),
        });
        let (channel, _, _, _, error) = probe_fields(Err(err), None);
        assert_eq!(channel, "server down");
        assert!(error.unwrap().contains("herdr.sock"));
    }

    /// A ping that fails without being a transport failure (herdr answered
    /// with an API error rather than the connection dying) is `error`, not
    /// `unreachable`.
    #[test]
    fn a_non_transport_ping_failure_reads_as_error() {
        let err = pastor::herdr::CallError::from(pastor::herdr::HerdrError::Api {
            code: "internal_error".into(),
            message: "boom".into(),
        });
        let (channel, _, _, _, error) = probe_fields(Err(err), None);
        assert_eq!(channel, "error");
        assert!(error.unwrap().contains("boom"));
    }

    /// With no head, `machine list` probes each machine, but a pull machine
    /// is never connected to: its row comes from flock.toml and the store,
    /// not from the herdr that happens to run here.
    #[tokio::test]
    async fn a_pull_machine_is_not_probed_without_a_head() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        paths.ensure().unwrap();
        let f = Flock::parse(
            std::path::Path::new("flock.toml"),
            "[[machine]]\nname = \"laptop\"\npull = true\nmax_agents = 2\n",
        )
        .unwrap();
        let store = Store::open(&paths.db_file()).unwrap();
        let row = probe_machine(
            f.get("laptop").unwrap(),
            &f,
            &PastorConfig::default(),
            &paths,
            &store,
        )
        .await
        .unwrap();
        assert_eq!(row.host, "pull");
        assert_eq!(row.endpoint, pastor::machine::PULL_ENDPOINT);
        assert_eq!(row.channel, "not probed");
        assert_eq!(row.error, None);
        assert_eq!(row.live, Some(0));
        assert_eq!(row.max_agents, 2);
    }

    #[test]
    fn list_blocked_done_all_are_mutually_exclusive() {
        fn err(args: &[&str]) -> clap::Error {
            match Cli::try_parse_from(args) {
                Ok(_) => panic!("{args:?}: expected a usage error"),
                Err(e) => e,
            }
        }

        let e = err(&["pastor", "task", "list", "--blocked", "--done"]);
        assert_eq!(e.kind(), clap::error::ErrorKind::ArgumentConflict);
        assert_eq!(e.exit_code(), 2);

        assert_eq!(
            err(&["pastor", "task", "list", "--blocked", "--all"]).kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
        assert_eq!(
            err(&["pastor", "task", "list", "--done", "--all"]).kind(),
            clap::error::ErrorKind::ArgumentConflict
        );

        // Each flag alone, and none of them, must still parse.
        for args in [
            vec!["pastor", "task", "list"],
            vec!["pastor", "task", "list", "--blocked"],
            vec!["pastor", "task", "list", "--done"],
            vec!["pastor", "task", "list", "--all"],
        ] {
            if let Err(e) = Cli::try_parse_from(&args) {
                panic!("{args:?}: {e}");
            }
        }
    }

    /// `--command` used to take every following word, pastor's own options
    /// included. It is one value now, and options after it are pastor's.
    #[test]
    fn machine_add_command_is_one_value() {
        let cli = Cli::try_parse_from([
            "pastor",
            "machine",
            "add",
            "fake",
            "--command",
            "fake-herdr --connect /tmp/h.sock",
            "--max-agents",
            "1",
            "--tag",
            "t",
        ])
        .unwrap();
        let Some(Command::Machine {
            cmd:
                MachineCmd::Add {
                    command,
                    max_agents,
                    tags,
                    ..
                },
        }) = cli.command
        else {
            panic!("parsed as another command")
        };
        assert_eq!(
            command.as_deref().map(command_argv),
            Some(vec![
                "fake-herdr".to_string(),
                "--connect".into(),
                "/tmp/h.sock".into()
            ])
        );
        assert_eq!(max_agents, 1, "options after --command are pastor's");
        assert_eq!(tags, vec!["t".to_string()]);
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
            vec!["pastor", "job", "list", "--json"],
            vec!["pastor", "job", "enable", "a"],
            vec!["pastor", "job", "disable", "a"],
            vec!["pastor", "job", "run", "a"],
            vec!["pastor", "job", "reload"],
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
    fn task_commands_parse_under_task() {
        for args in [
            vec!["pastor", "task", "run", "hi"],
            vec!["pastor", "task", "list", "--json"],
            vec!["pastor", "task", "describe", "t-1"],
            vec!["pastor", "task", "read", "t-1"],
            vec!["pastor", "task", "attach", "t-1"],
        ] {
            if let Err(e) = Cli::try_parse_from(&args) {
                panic!("{args:?}: {e}");
            }
        }
    }

    /// The old names are gone outright, with no alias left behind: one verb
    /// per action across the nouns.
    #[test]
    fn renamed_commands_answer_only_to_their_new_names() {
        for args in [
            vec!["pastor", "task", "show", "t-1"],
            vec!["pastor", "connector", "run", "c", "--job", "j"],
            vec!["pastor", "open", "pi-1"],
        ] {
            let err = Cli::try_parse_from(&args).expect_err(&format!("{args:?} still parses"));
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::InvalidSubcommand,
                "{args:?}: {err}"
            );
        }
        for args in [
            vec!["pastor", "task", "describe", "t-1"],
            vec!["pastor", "connector", "try", "c", "--job", "j"],
            vec!["pastor", "machine", "open", "pi-1"],
        ] {
            if let Err(e) = Cli::try_parse_from(&args) {
                panic!("{args:?}: {e}");
            }
        }
        fn aliases(cmd: &clap::Command) -> Vec<String> {
            let mut out: Vec<String> = cmd.get_all_aliases().map(str::to_string).collect();
            for sub in cmd.get_subcommands() {
                out.extend(aliases(sub));
            }
            out
        }
        use clap::CommandFactory;
        assert_eq!(aliases(&Cli::command()), Vec::<String>::new());
    }

    /// `--help` is the first thing a user or an agent reads: every command
    /// says what it does and every argument what it takes. `--json` says the
    /// shape it prints, and a task argument the two ways to write one.
    #[test]
    fn every_command_and_argument_has_help() {
        fn walk(cmd: &clap::Command, path: &str, missing: &mut Vec<String>) {
            for arg in cmd.get_arguments() {
                let id = arg.get_id().as_str();
                if matches!(id, "help" | "version") {
                    continue;
                }
                let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
                if help.is_empty() {
                    missing.push(format!("{path} {id}: no help"));
                }
                if id == "json"
                    && ![
                        "Print as a JSON array",
                        "Print as a JSON object",
                        "Print one JSON record per line",
                    ]
                    .iter()
                    .any(|p| help.starts_with(p))
                {
                    missing.push(format!("{path} --json: {help:?} does not say the shape"));
                }
                if arg.is_positional()
                    && id == "task"
                    && !help.starts_with("A task, like t-12 or 12")
                {
                    missing.push(format!("{path} <task>: {help:?}"));
                }
            }
            for sub in cmd.get_subcommands() {
                let name = sub.get_name();
                if name == "help" {
                    continue;
                }
                let path = format!("{path} {name}");
                if sub.get_about().is_none_or(|a| a.to_string().is_empty()) {
                    missing.push(format!("{path}: no about"));
                }
                walk(sub, &path, missing);
            }
        }
        use clap::CommandFactory;
        let mut missing = Vec::new();
        walk(&Cli::command(), "pastor", &mut missing);
        assert!(missing.is_empty(), "{}", missing.join("\n"));
    }

    /// A machine is reached one way: over ssh, locally, or by a command.
    #[test]
    fn machine_add_takes_one_way_to_reach_the_machine() {
        for args in [
            vec!["pastor", "machine", "add", "x", "user@h", "--local"],
            vec![
                "pastor",
                "machine",
                "add",
                "x",
                "user@h",
                "--command",
                "fake",
            ],
            vec![
                "pastor",
                "machine",
                "add",
                "x",
                "--local",
                "--command",
                "fake",
            ],
        ] {
            let err = Cli::try_parse_from(&args).expect_err(&format!("{args:?} parses"));
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{args:?}: {err}"
            );
        }
        for args in [
            vec!["pastor", "machine", "add", "x", "user@h"],
            vec!["pastor", "machine", "add", "x", "--local"],
            vec!["pastor", "machine", "add", "x", "--command", "fake"],
        ] {
            if let Err(e) = Cli::try_parse_from(&args) {
                panic!("{args:?}: {e}");
            }
        }
    }

    #[test]
    fn job_reload_help_names_every_file_it_rereads() {
        use clap::CommandFactory;
        let root = Cli::command();
        let reload = root
            .find_subcommand("job")
            .and_then(|j| j.find_subcommand("reload"))
            .unwrap();
        let about = reload.get_about().unwrap().to_string();
        assert!(
            about.contains("flock.toml") && about.contains("pastor.toml"),
            "{about}"
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

    /// Every `pastor ...` command a skill or the docs show, in inline code or
    /// a code block, must be a real command with real long flags, so they
    /// cannot drift from the CLI. Words that are not commands end the walk
    /// (`t-12`, a quoted prompt); flags are checked on the command reached.
    /// Every Markdown file under `skills/` counts: the reference files and
    /// worked examples are read by agents as much as the SKILL.md itself.
    /// README.md, docs/manual.md, docs/recommended-setup.md and the
    /// website's docs pages are read by people, and count too.
    #[test]
    fn skills_mention_only_real_commands_and_flags() {
        let mut files = Vec::new();
        markdown_files(&skills_dir(), &mut files);
        assert!(files.len() > 1, "found only {files:?}");
        let repo = skills_dir().parent().unwrap().to_path_buf();
        files.push(repo.join("README.md"));
        files.push(repo.join("docs/manual.md"));
        files.push(repo.join("docs/recommended-setup.md"));
        markdown_files(&repo.join("docs/website/content"), &mut files);
        let mut wrong = Vec::new();
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            let (checked, errors) = check_commands(&text);
            wrong.extend(errors.iter().map(|e| format!("{}: {e}", file.display())));
            if file.ends_with("pastor/SKILL.md")
                || file.ends_with("README.md")
                || file.ends_with("manual.md")
            {
                assert!(checked > 20, "only {checked} commands found in {file:?}");
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
        // The binary prints the copy it was built with; it is the same file.
        assert!(check_commands(SKILL).0 > 20);
    }

    /// The homepage's live terminal types the commands in
    /// `docs/website/data/demo.toml`; each must be a real command with real
    /// long flags, as the docs' commands are, or the demo would show a CLI
    /// that does not exist.
    #[test]
    fn website_demo_commands_are_real() {
        let path = skills_dir()
            .parent()
            .unwrap()
            .join("docs/website/data/demo.toml");
        let demo: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let cmds: Vec<&str> = demo["act"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|act| act["step"].as_array().unwrap())
            .filter_map(|step| step.get("cmd").and_then(|c| c.as_str()))
            .collect();
        assert!(cmds.len() > 10, "only {} commands in {path:?}", cmds.len());
        for cmd in &cmds {
            assert!(
                cmd.starts_with("pastor "),
                "{cmd:?} is not a pastor command"
            );
        }
        let (checked, wrong) = check_commands(&format!("```sh\n{}\n```\n", cmds.join("\n")));
        assert_eq!(checked, cmds.len(), "{cmds:?}");
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// The website's CLI reference is rendered from this command tree. It
    /// fails when the checked-in page is stale; `make cli-reference` runs it
    /// with PASTOR_WRITE_CLI_REFERENCE=1, which writes the page instead.
    #[test]
    fn website_cli_reference_is_current() {
        let path = skills_dir()
            .parent()
            .unwrap()
            .join("docs/website/content/docs/reference/cli.md");
        let page = std::fs::read_to_string(&path).unwrap();
        let fresh = pastor::cli_reference::refresh(&completion_tree(), &page);
        if std::env::var_os("PASTOR_WRITE_CLI_REFERENCE").is_some() {
            std::fs::write(&path, &fresh).unwrap();
            return;
        }
        assert!(
            page == fresh,
            "{} is stale: run make cli-reference",
            path.display()
        );
    }

    /// `llms.txt` is the home page in a second output format, so Hugo writes
    /// it at the site's root from `layouts/home.llms.txt`.
    #[test]
    fn website_serves_llms_txt_at_its_root() {
        let site = skills_dir().parent().unwrap().join("docs/website");
        let config: toml::Table =
            toml::from_str(&std::fs::read_to_string(site.join("hugo.toml")).unwrap()).unwrap();
        let llms = &config["outputFormats"]["llms"];
        assert_eq!(llms["baseName"].as_str(), Some("llms"));
        assert_eq!(llms["mediaType"].as_str(), Some("text/plain"));
        let home = config["outputs"]["home"].as_array().unwrap();
        assert!(home.iter().any(|f| f.as_str() == Some("llms")), "{home:?}");
        let template = std::fs::read_to_string(site.join("layouts/home.llms.txt")).unwrap();
        assert!(template.contains("# pastor\n"), "{template}");
    }

    /// A subpath `baseURL` (the site is served at cacari.co/pastor/) makes
    /// `.RelPermalink` absolute under that subpath, so `llms.txt`'s docs
    /// links must be trimmed relative to the home page, not just its
    /// leading slash, or they resolve one `docs/` level too deep. Needs
    /// hugo from mise; skips where it is not installed.
    #[test]
    fn website_llms_txt_links_resolve_under_a_subpath_base_url() {
        if std::process::Command::new("hugo")
            .arg("version")
            .output()
            .is_err()
        {
            eprintln!("skipping: hugo not found in PATH");
            return;
        }
        let site = skills_dir().parent().unwrap().join("docs/website");
        let out = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("hugo")
            .arg("--source")
            .arg(&site)
            .arg("--destination")
            .arg(out.path())
            .args(["--baseURL", "https://cacari.co/pastor/"])
            .status()
            .unwrap();
        assert!(status.success());
        let llms = std::fs::read_to_string(out.path().join("llms.txt")).unwrap();
        for line in llms.lines().filter(|l| l.starts_with("- [")) {
            let link = line.split('(').nth(1).unwrap().split(')').next().unwrap();
            if link.starts_with("http") {
                continue; // the Optional section links out to github.com
            }
            assert!(
                link.starts_with("docs/"),
                "link should be relative to llms.txt, not doubled under the subpath: {line}"
            );
        }
    }

    /// The docs are five sections, each an index with its pages, and every
    /// docs link on the homepage lands on one of them, so a moved or renamed
    /// page shows here rather than as a dead link.
    #[test]
    fn website_sections_and_home_links_resolve() {
        let site = skills_dir().parent().unwrap().join("docs/website");
        let docs = site.join("content/docs");
        for section in ["start", "concepts", "deploy", "examples", "reference"] {
            let dir = docs.join(section);
            assert!(dir.join("_index.md").is_file(), "no {section}/_index.md");
            let pages = std::fs::read_dir(&dir)
                .unwrap()
                .filter(|e| e.as_ref().unwrap().file_name() != "_index.md")
                .count();
            assert!(pages > 0, "{section} has no pages");
        }
        let home = std::fs::read_to_string(site.join("layouts/home.html")).unwrap();
        assert!(
            home.contains("what you can do"),
            "no \"what you can do\" pane"
        );
        let mut links = 0;
        for link in home.split("\"docs/").skip(1) {
            let path = link.split('"').next().unwrap().trim_end_matches('/');
            let path = path.split('#').next().unwrap();
            let page = docs.join(format!("{path}.md"));
            let index = docs.join(path).join("_index.md");
            assert!(
                path.is_empty() || page.is_file() || index.is_file(),
                "home links docs/{path}/, which is no page"
            );
            links += 1;
        }
        assert!(links > 5, "only {links} docs links on the home");
    }

    /// The manual's remote head section lists which commands go to the head,
    /// which stay here, and names the ones refused; each list must be what
    /// `remote_route` does.
    #[test]
    fn manual_remote_head_lists_match_remote_route() {
        let path = skills_dir().parent().unwrap().join("docs/manual.md");
        let text = std::fs::read_to_string(path).unwrap();
        let between = |start: &str, end: &str| {
            let from = text.find(start).unwrap_or_else(|| panic!("no {start:?}"));
            let rest = &text[from + start.len()..];
            rest[..rest.find(end).unwrap_or_else(|| panic!("no {end:?}"))].to_string()
        };
        let spans = |part: &str| -> Vec<String> {
            let mut out = Vec::new();
            for span in part.split('`').skip(1).step_by(2) {
                // `task run|list|read` is three commands.
                let (group, alts) = span.rsplit_once(' ').unwrap_or(("", span));
                for alt in alts.split('|') {
                    out.push(format!("{group} {alt}").trim().to_string());
                }
            }
            out
        };
        let route = |words: &str| remote_route(&sample_command(words));
        for words in spans(&between(
            "These commands go to a remote head:",
            ". `machine list`'s",
        )) {
            assert_eq!(route(&words), RemoteRoute::Head, "{words}");
        }
        for words in spans(&between("Local on purpose, as with no head set:", "\n\n")) {
            assert_eq!(route(&words), RemoteRoute::Here, "{words}");
        }
        let refused = between("Every other command runs on the head only.", "\n\n");
        let refused = spans(&refused);
        assert_eq!(refused[0], "machine authorized-key");
        assert!(matches!(route(&refused[0]), RemoteRoute::Unsupported(_)));
    }

    /// A job whose file is here runs and is edited here, not by the remote
    /// head, so the caller's guard here applies to it; the head's own jobs
    /// are the head's to judge.
    #[test]
    fn a_local_job_is_not_the_remote_heads_to_judge() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        std::fs::create_dir_all(paths.jobs_dir()).unwrap();
        std::fs::write(paths.jobs_dir().join("here.toml"), "").unwrap();
        let parse = |argv: &[&str]| Cli::try_parse_from(argv).unwrap().command.unwrap();
        for verb in ["run", "edit", "enable", "disable"] {
            let here = parse(&["pastor", "job", verb, "here"]);
            assert!(!head_decides(&paths, &here, true), "{verb}");
            let theirs = parse(&["pastor", "job", verb, "theirs"]);
            assert!(head_decides(&paths, &theirs, true), "{verb}");
            assert!(!head_decides(&paths, &theirs, false), "{verb}");
        }
        assert!(head_decides(
            &paths,
            &parse(&["pastor", "job", "list"]),
            true
        ));
    }

    /// Attach and open stay here with a remote head, but ask it for the
    /// task and its files, so they are probed for `FILE_PROTOCOL`.
    #[test]
    fn attach_and_open_read_the_remote_heads_files() {
        for words in ["task attach", "machine open"] {
            let command = sample_command(words);
            assert_eq!(remote_route(&command), RemoteRoute::Here, "{words}");
            assert!(reads_head_files(&command), "{words}");
        }
        assert!(!reads_head_files(&sample_command("completions")));
    }

    /// A parsed command for `words`, with what its required arguments need.
    fn sample_command(words: &str) -> Command {
        let extra: &[&str] = match words {
            "task run" | "task describe" | "task read" | "task attach" | "task retry"
            | "task close" | "task done" | "job run" | "machine open" => &["x"],
            "flock add" | "flock remove" | "flock describe" | "machine remove"
            | "machine describe" | "profile describe" => &["x"],
            "machine add" => &["x", "--local"],
            "machine authorized-key" => &["x", "--key", "-"],
            "flock default" => &["show"],
            "task send" | "task priority" | "machine move" | "trust add" | "trust remove" => {
                &["x", "y"]
            }
            "queue move" => &["x", "--top"],
            "task prune" => &["--older-than", "1d", "--done"],
            "completions" => &["bash"],
            "setup" => &["systemd"],
            "head" => &["show"],
            "connector" => &["list"],
            _ => &[],
        };
        let argv = std::iter::once("pastor")
            .chain(words.split_whitespace())
            .chain(extra.iter().copied())
            .collect::<Vec<_>>();
        Cli::try_parse_from(&argv)
            .unwrap_or_else(|e| panic!("{argv:?}: {e}"))
            .command
            .unwrap()
    }

    /// Each skill is loaded by its frontmatter alone until it triggers, so a
    /// name that does not match its directory, or a description past the
    /// 1,024 characters Claude Code reads, breaks it without an error.
    #[test]
    fn skills_have_valid_frontmatter() {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(skills_dir()).unwrap() {
            let dir = entry.unwrap().path();
            let text = std::fs::read_to_string(dir.join("SKILL.md"))
                .unwrap_or_else(|e| panic!("{dir:?}: {e}"));
            let front = text
                .strip_prefix("---\n")
                .and_then(|rest| rest.split_once("\n---\n"))
                .unwrap_or_else(|| panic!("{dir:?}: no frontmatter"))
                .0;
            let field = |key: &str| {
                front
                    .lines()
                    .find_map(|l| l.strip_prefix(&format!("{key}: ")))
                    .map(|v| v.trim_matches('"').to_string())
                    .unwrap_or_else(|| panic!("{dir:?}: no {key}"))
            };
            let name = field("name");
            assert_eq!(Some(name.as_str()), dir.file_name().unwrap().to_str());
            let description = field("description");
            assert!(
                !description.is_empty() && description.len() <= 1024,
                "{dir:?}: description is {} characters",
                description.len()
            );
            assert!(text.lines().count() < 500, "{dir:?}: SKILL.md too long");
            names.push(name);
        }
        names.sort();
        assert_eq!(names, ["pastor", "spec"]);
    }

    /// The repository installs as a Claude Code plugin named `pastor`, which
    /// is what makes the skills `/pastor:spec` and friends.
    #[test]
    fn plugin_manifest_names_the_plugin_pastor() {
        let path = skills_dir()
            .parent()
            .unwrap()
            .join(".claude-plugin/plugin.json");
        let text = std::fs::read_to_string(&path).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(manifest["name"], "pastor", "{manifest}");
        assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"), "{manifest}");
    }

    fn skills_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills")
    }

    fn markdown_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                markdown_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }

    /// Checks every `pastor` command in `text`; returns how many it saw and
    /// what is wrong with them.
    fn check_commands(text: &str) -> (usize, Vec<String>) {
        use clap::CommandFactory;
        let root = Cli::command();
        let mut checked = 0;
        let mut errors = Vec::new();
        for code in code_spans(text) {
            for line in code.lines() {
                let line = line.split(" # ").next().unwrap_or(line);
                let words: Vec<&str> = line.split_whitespace().collect();
                for (at, _) in words.iter().enumerate().filter(|(_, w)| **w == "pastor") {
                    if let Err(e) = check_command(&root, &words[at + 1..]) {
                        errors.push(format!("{line:?}: {e}"));
                    }
                    checked += 1;
                }
            }
        }
        (checked, errors)
    }

    /// Inline code spans and fenced blocks of a Markdown text, in order.
    /// A fence tagged with a language that is not a shell (`toml`, `json`,
    /// `text` for a diagram or sample output) holds no commands and is left
    /// out. An inline span may wrap onto the next line of its paragraph.
    fn code_spans(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut fence: Option<(bool, String)> = None;
        let mut para = String::new();
        let inline = |para: &mut String, out: &mut Vec<String>| {
            out.extend(para.split('`').skip(1).step_by(2).map(str::to_string));
            para.clear();
        };
        for line in text.lines() {
            if fence.is_none() && (line.trim().is_empty() || line.starts_with("```")) {
                inline(&mut para, &mut out);
            }
            if let Some(lang) = line.strip_prefix("```") {
                match fence.take() {
                    Some((true, block)) => out.push(block),
                    Some((false, _)) => {}
                    None => {
                        let shell = matches!(lang.trim(), "" | "sh" | "bash" | "fish" | "console");
                        fence = Some((shell, String::new()));
                    }
                }
                continue;
            }
            if let Some((_, block)) = fence.as_mut() {
                // A trailing backslash continues the command on the next line.
                block.push_str(line.trim_end_matches('\\'));
                if !line.ends_with('\\') {
                    block.push('\n');
                }
                continue;
            }
            para.push_str(line);
            para.push(' ');
        }
        inline(&mut para, &mut out);
        out
    }

    fn check_command(root: &clap::Command, words: &[&str]) -> Result<(), String> {
        // What the completion scripts run at TAB; it is not in the clap tree.
        if words.first() == Some(&"__complete") {
            return Ok(());
        }
        let mut cmd = root;
        let mut rest = words;
        while let Some(word) = rest.first() {
            match cmd.find_subcommand(word) {
                Some(sub) => {
                    cmd = sub;
                    rest = &rest[1..];
                }
                None if cmd.has_subcommands() && !word.starts_with('-') => {
                    return Err(format!(
                        "`{word}` is not a subcommand of `{}`",
                        cmd.get_name()
                    ));
                }
                None => break,
            }
        }
        let mut words = rest.iter();
        while let Some(word) = words.next() {
            let Some(flag) = word.strip_prefix("--") else {
                continue;
            };
            let (name, inline) = match flag.split_once('=') {
                Some((name, _)) => (name, true),
                None => (flag, false),
            };
            if name == "help" || name == "version" {
                continue;
            }
            let arg = cmd
                .get_arguments()
                .find(|a| a.get_long() == Some(name))
                .ok_or_else(|| format!("`{}` has no --{name}", cmd.get_name()))?;
            if !inline && arg.get_action().takes_values() {
                // A quoted value runs to the word that closes the quote.
                if let Some(value) = words.next()
                    && let Some(q) = value.chars().next().filter(|c| matches!(c, '"' | '\''))
                    && (value.len() == 1 || !value.ends_with(q))
                {
                    for w in words.by_ref() {
                        if w.ends_with(q) {
                            break;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
