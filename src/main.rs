use std::io::Read;
use std::os::unix::process::CommandExt;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use pastor::cli::{CliError, request_failure};
use pastor::config::flock::{DEFAULT_FLOCK, EditError, Flock, FlockDoc, MachineConfig};
use pastor::config::job::{check_name, job_path, set_enabled};
use pastor::config::{AgentChoice, PastorConfig, Paths, parse_duration};
use pastor::herdr::{Connector, ConnectorExt, Endpoint, shell_quote};
use pastor::ipc::{Head, HeadPing, IpcRequest, IpcResponse, request};
use pastor::scheduler::{JobRunReport, JobStatus, Scheduler};
use pastor::store::{Store, TaskFilter};
use pastor::task::{DispatchSpec, Place, Task, TaskState, parse_task_id};

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
    args_conflicts_with_subcommands = true,
    after_help = AGENT_FOOTER
)]
struct Cli {
    /// Print the agent skill (SKILL.md) for this version and exit
    #[arg(long, exclusive = true)]
    skill: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the daemon: scheduler, machine channels, dispatch
    Serve,
    /// Manage tasks
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
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
    /// Open the full herdr UI on a machine
    Open { machine: String },
    /// Run one scheduler pass now and report what it did
    Tick(TickArgs),
    /// Manage jobs (files in ~/.config/pastor/jobs/)
    Job {
        #[command(subcommand)]
        cmd: JobCmd,
    },
    /// pastor.toml: the head's settings and the task defaults
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Print a shell completion script (fish, bash, zsh, ...) to stdout
    Completions { shell: clap_complete::Shell },
    /// Show the events log (task, job and machine events)
    Events(pastor::events::EventsArgs),
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
    /// The repos whose folder-trust prompt pastor answers on each machine
    Trust {
        #[command(subcommand)]
        cmd: pastor::trust_cli::TrustCmd,
    },
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
    /// Enable a job file
    Enable { name: String },
    /// Disable a job file
    Disable { name: String },
    /// Fire a job now, ignoring its schedule, the overlap rule and `enabled`
    Run { name: String },
    /// Re-read the job files now instead of at the next tick
    Reload,
    /// Open a job file in $VISUAL or $EDITOR; save it only once it is valid
    Edit { name: String },
    /// One job in full: schedule, connector, dispatch, last runs, recent tasks
    Describe {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Open pastor.toml in $VISUAL or $EDITOR; save it only once it is valid
    Edit,
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
    #[arg(long)]
    repo: Option<String>,
    /// Only this flock's machines take the task (default: the flock of
    /// --machine, else the default flock)
    #[arg(long)]
    flock: Option<String>,
    #[arg(long)]
    machine: Option<String>,
    #[arg(long)]
    agent: Option<String>,
    /// One argument for the agent; repeat it, in order, for more. Replaces
    /// the flock's and `[defaults]` agent_args. The next word is always the
    /// value, dashes and all.
    #[arg(long = "agent-arg", value_name = "ARG", allow_hyphen_values = true)]
    agent_args: Vec<String>,
    /// A git worktree per task, branched from --repo (so it needs --repo)
    #[arg(long, requires = "repo")]
    worktree: bool,
    /// Branch for the worktree (needs --worktree; a plain workspace has no branch)
    #[arg(long, requires = "worktree")]
    branch: Option<String>,
    #[arg(long = "tag")]
    tags: Vec<String>,
    #[arg(long)]
    timeout: Option<String>,
    /// Where the agent's pane goes: repo (under the repo it works on), own
    /// (its own workspace), pastor (the `pastor` workspace) or
    /// pane:<workspace> (default: `[defaults] place`, else repo)
    #[arg(long, value_name = "PLACE")]
    place: Option<Place>,
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
    /// Print full task records as JSON instead of a table
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand, Debug)]
enum TaskCmd {
    /// Create a one-off task and dispatch it
    Run(RunArgs),
    /// List live tasks across the flock; --all adds finished ones
    List(ListArgs),
    /// Show one task row
    #[command(visible_alias = "describe")]
    Show {
        task: String,
        #[arg(long)]
        json: bool,
    },
    /// Read recent output from a task's pane
    Read {
        task: String,
        #[arg(long, default_value_t = 40)]
        lines: u32,
    },
    /// Attach to a task's agent terminal (ctrl+b q detaches)
    Attach { task: String },
    /// Re-dispatch a failed or stale task as a new task (retry_of points back)
    Retry(pastor::task_cli::RetryArgs),
    /// Close a task's pane (and with --remove-worktree its worktree), or an orphaned agent
    Close(pastor::task_cli::CloseArgs),
    /// Delete old finished tasks; their items stay seen
    Prune(pastor::task_cli::PruneArgs),
    /// Type text or press keys in a live task's agent, to answer what it is waiting on
    Send(pastor::task_cli::SendArgs),
}

#[derive(Subcommand, Debug)]
enum MachineCmd {
    Add {
        name: String,
        ssh: Option<String>,
        #[arg(long)]
        local: bool,
        /// Developer option: the bridge command as one string, split on
        /// whitespace (`--command "fake-herdr --connect /tmp/h.sock"`).
        /// Words containing spaces go in flock.toml by hand.
        #[arg(long, value_name = "COMMAND")]
        command: Option<String>,
        #[arg(long, default_value = "default")]
        session: String,
        #[arg(long, default_value_t = 2)]
        max_agents: u32,
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// The flock it joins (default: the default flock)
        #[arg(long)]
        flock: Option<String>,
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
    /// Put a machine in another flock; tasks already on it stay there
    Move { name: String, flock: String },
    /// A line about the head, then each machine: host, flock, channel, herdr, agents
    List {
        /// Only the machines of this flock
        #[arg(long)]
        flock: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// One machine in full: host, flock, channel, versions, agents, recent errors
    Describe {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum FlockCmd {
    /// Every flock: default or not, its machines, live agents, queued tasks
    List {
        #[arg(long)]
        json: bool,
    },
    /// Declare a flock; with --default, new tasks and jobs go to it
    Add {
        name: String,
        #[arg(long)]
        default: bool,
    },
    /// Remove a flock; refused while it has machines or queued tasks, or is the default
    Remove { name: String },
    /// Make another flock the default; machines stay in their flocks
    Default { name: String },
    /// Open flock.toml in $VISUAL or $EDITOR; save it only once it is valid
    Edit,
    /// One flock in full: default or not, its agent, machines, live tasks
    Describe {
        name: String,
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
    if cli.skill {
        print!("{SKILL}");
        return;
    }
    // `arg_required_else_help` means clap has already printed help for a bare
    // `pastor`; the only other way here without a command is `--skill`.
    let Some(command) = cli.command else {
        unreachable!("clap requires a command or --skill")
    };
    let paths = match Paths::from_env() {
        Ok(p) => p,
        Err(err) => fail("config_error", &err.to_string()),
    };
    if let Some(legacy) = pastor::config::legacy_macos_dir() {
        match pastor::config::migrate_legacy_dir(&legacy, &paths) {
            Ok(Some(note)) => eprintln!("{note}"),
            Ok(None) => {}
            Err(err) => fail("config_error", &format!("{err:#}")),
        }
    }
    pastor::ipc::set_caller_task(pastor::ipc::task_from_env());
    if let Some(task) = pastor::ipc::caller_task()
        && changes_fleet(&command)
        && !agents_change_fleet(&paths)
    {
        fail("agent_refused", &pastor::daemon::agent_refusal(&task));
    }
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async {
        // Commands that never talk to the head do not read `head`.
        let head = match head_use(&command) {
            Some(flocky) => {
                probe_head(
                    &paths,
                    flocky || flocks_declared(&paths),
                    needs_agent_protocol(&command),
                    needs_place_protocol(&command),
                )
                .await?
            }
            None => Head::Absent,
        };
        match command {
            Command::Serve => pastor::daemon::serve(paths).await,
            Command::Task { cmd } => task(&paths, cmd, head).await,
            Command::Machine { cmd } => machine(&paths, cmd, head).await,
            Command::Flock { cmd } => flock(&paths, cmd, head).await,
            Command::Open { machine } => open(&paths, &machine).await,
            Command::Tick(args) => tick(&paths, args, head).await,
            Command::Job { cmd } => job(&paths, cmd, head).await,
            Command::Config {
                cmd: ConfigCmd::Edit,
            } => config_edit(&paths, head).await,
            Command::Completions { shell } => {
                let mut cmd = completion_tree();
                clap_complete::generate(shell, &mut cmd, "pastor", &mut std::io::stdout());
                Ok(())
            }
            Command::Events(args) => pastor::events::cli(&paths, args).await,
            Command::Setup { cmd } => pastor::setup::cli(&paths, cmd),
            Command::Connector { cmd } => pastor::connector::cli::run(&paths, cmd, head).await,
            Command::Trust { cmd } => pastor::trust_cli::run(&paths, cmd),
        }
    });
    if let Err(err) = result {
        if let Some(e) = err.downcast_ref::<pastor::cli::CliError>() {
            fail(&e.code, &e.message);
        }
        fail("runtime_error", &format!("{err:#}"));
    }
}

/// The command tree `pastor completions` describes: the real one, since there
/// are no hidden subcommands left to strip out.
fn completion_tree() -> clap::Command {
    let mut cmd = <Cli as clap::CommandFactory>::command().version(env!("CARGO_PKG_VERSION"));
    // clap builds the `help` subtree from subcommand names only, so a
    // visible alias (`task describe`) completes directly but not after
    // `pastor help task`. Build first, then give each help node its aliases.
    cmd.build();
    let real = cmd.clone();
    cmd.mut_subcommand("help", |help| help_aliases(help, &real))
}

/// `help`, a node of the built help subtree, with an entry added for each
/// visible alias of the matching node `real` of the command tree.
fn help_aliases(help: clap::Command, real: &clap::Command) -> clap::Command {
    let base = help.get_bin_name().unwrap_or("pastor").to_string();
    let mut extra = Vec::new();
    for sub in real.get_subcommands() {
        for alias in sub.get_visible_aliases() {
            // clap names want `'static`; this runs once, for a completion script.
            let name: &'static str = Box::leak(alias.to_owned().into_boxed_str());
            let mut c = clap::Command::new(name)
                .bin_name(format!("{base} {name}"))
                .disable_help_flag(true);
            if let Some(about) = sub.get_about() {
                c = c.about(about.clone());
            }
            extra.push(c);
        }
    }
    help.mut_subcommands(|node| match real.find_subcommand(node.get_name()) {
        Some(r) if node.get_name() != "help" => help_aliases(node, r),
        _ => node,
    })
    .subcommands(extra)
}

fn fail(code: &str, message: &str) -> ! {
    eprintln!("{}", serde_json::json!({"code": code, "message": message}));
    std::process::exit(1)
}

async fn ask(paths: &Paths, req: IpcRequest) -> anyhow::Result<IpcResponse> {
    let socket = paths.socket_file();
    let resp = match request(&socket, &req).await {
        Ok(resp) => resp,
        Err(err) => {
            let (code, message) = request_failure(&err);
            fail(code, &message);
        }
    };
    if let IpcResponse::Error { code, message } = &resp {
        fail(code, message);
    }
    Ok(resp)
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
/// `agents` is for a command that can queue a task (`needs_agent_protocol`): a
/// head before `AGENT_PROTOCOL` would drop the task's tool lists, so it is
/// refused too.
///
/// `place` is for `task retry --place` (`needs_place_protocol`): a head
/// before `PLACE_PROTOCOL` would retry the task where it was and answer
/// success, so it is refused.
async fn probe_head(
    paths: &Paths,
    flocks: bool,
    agents: bool,
    place: bool,
) -> anyhow::Result<Head> {
    let socket = paths.socket_file();
    match pastor::ipc::ping_head(&socket).await {
        HeadPing::NotRunning => Ok(Head::Absent),
        HeadPing::Unresponsive => Err(pastor::cli::CliError::err(
            "head_unresponsive",
            format!(
                "pastor serve is listening on {} but not answering; nothing was done, run it again once it answers",
                socket.display()
            ),
        )),
        HeadPing::Pong { version, protocol } if place && protocol < pastor::ipc::PLACE_PROTOCOL => {
            Err(pastor::cli::CliError::err(
                "head_too_old",
                format!(
                    "the running pastor serve ({version}) predates `task retry --place` and would retry the task where it was; restart it"
                ),
            ))
        }
        HeadPing::Pong { version, protocol }
            if agents && protocol < pastor::ipc::AGENT_PROTOCOL =>
        {
            Err(pastor::cli::CliError::err(
                "head_too_old",
                format!(
                    "the running pastor serve ({version}) predates flock agents and tool allow and deny lists, and would start the agent without them; restart it"
                ),
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
        Command::Machine { cmd } => Some(match cmd {
            MachineCmd::List { flock, .. } => flock.is_some(),
            MachineCmd::Describe { .. } => false,
            _ => true,
        }),
        Command::Flock { cmd } => Some(!matches!(
            cmd,
            FlockCmd::List { .. } | FlockCmd::Describe { .. }
        )),
        Command::Tick(_) | Command::Job { .. } | Command::Config { .. } => Some(false),
        Command::Connector { cmd } => {
            (!matches!(cmd, ConnectorCmd::List { .. } | ConnectorCmd::Run { .. })).then_some(false)
        }
        _ => None,
    }
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
                | TaskCmd::Close(_)
                | TaskCmd::Prune(_)
                | TaskCmd::Send(_)
                // herdr's agent terminal types into any task's pane.
                | TaskCmd::Attach { .. }
        ),
        // Reading the fleet is fine: list and describe change nothing.
        Command::Machine { cmd } => {
            !matches!(cmd, MachineCmd::List { .. } | MachineCmd::Describe { .. })
        }
        Command::Flock { cmd } => !matches!(cmd, FlockCmd::List { .. } | FlockCmd::Describe { .. }),
        Command::Tick(_) => true,
        Command::Job { cmd } => !matches!(cmd, JobCmd::List { .. } | JobCmd::Describe { .. }),
        // pastor.toml holds agents_change_fleet itself.
        Command::Config { .. } => true,
        // Install, link, uninstall and unlink edit the catalog and reload the
        // head's jobs, restarting its stream connectors.
        Command::Connector { cmd } => {
            !matches!(cmd, ConnectorCmd::List { .. } | ConnectorCmd::Run { .. })
        }
        // A head started from an agent's pane schedules and dispatches with
        // no request to refuse; setup installs one that starts on login.
        Command::Serve | Command::Setup { .. } => true,
        // herdr's full UI drives every pane and agent on the machine.
        Command::Open { .. } => true,
        _ => false,
    }
}

/// `agents_change_fleet` in pastor.toml. A file that does not load counts
/// as off: the refusal is the safe side.
fn agents_change_fleet(paths: &Paths) -> bool {
    PastorConfig::load(&paths.config_file()).is_ok_and(|c| c.agents_change_fleet)
}

/// Whether `command` can make the head queue a task, whose agent the head
/// resolves: it needs a head at `AGENT_PROTOCOL` or later. A tick or a job
/// run queues through the head's own jobs, so they count too; a dry run
/// writes nothing, and a reload only re-reads the job files.
fn needs_agent_protocol(command: &Command) -> bool {
    match command {
        Command::Task { cmd } => matches!(cmd, TaskCmd::Run(_) | TaskCmd::Retry { .. }),
        Command::Tick(a) => !a.dry_run,
        Command::Job { cmd } => matches!(cmd, JobCmd::Run { .. }),
        _ => false,
    }
}

/// Whether `command` sends a request only a head of `PLACE_PROTOCOL` or later
/// honours: `task retry --place`.
fn needs_place_protocol(command: &Command) -> bool {
    matches!(
        command,
        Command::Task {
            cmd: TaskCmd::Retry(a)
        } if a.place.is_some()
    )
}

/// Whether flock.toml declares named flocks, so a command with no `--flock`
/// means the default flock rather than every machine. A flock.toml that does
/// not load counts as declaring them.
fn flocks_declared(paths: &Paths) -> bool {
    Flock::load(&paths.flock_file()).map_or(true, |f| !f.flocks.is_empty())
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
    let prompt = run_prompt(&a)?;
    let config = PastorConfig::load(&paths.config_file())?;
    let spec = run_spec(&a, &config)?;
    let IpcResponse::Task(t) = ask(
        paths,
        IpcRequest::Run {
            agent: Some(agent_choice(&a)),
            prompt,
            spec,
            flock: a.flock,
        },
    )
    .await?
    else {
        unreachable!()
    };
    print_task(&t, a.json);
    Ok(())
}

/// What `pastor task run`'s flags say about the agent; the head fills in the
/// rest from the task's flock and `[defaults]`.
fn agent_choice(a: &RunArgs) -> AgentChoice {
    AgentChoice {
        agent: a.agent.clone(),
        agent_args: (!a.agent_args.is_empty()).then(|| a.agent_args.clone()),
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
    })
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
async fn probe_machine(
    m: &MachineConfig,
    flock: &str,
    paths: &Paths,
    store: &Store,
) -> anyhow::Result<pastor::cli::MachineRow> {
    let ep = Endpoint::from_machine(m, paths);
    let ping = ep.ping().await;
    let agents = match &ping {
        Ok(_) => Some(ep.agent_list().await),
        Err(_) => None,
    };
    let orphans: Vec<String> = match &agents {
        Some(Ok(list)) => pastor::machine::orphan_agents(list, &store.tasks_on_machine(&m.name)?)
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
        channel: channel.into(),
        herdr_version,
        pastor_version,
        protocol,
        error,
        live,
        max_agents: m.max_agents,
        tags: m.tags.clone(),
        orphans,
    })
}

/// The head's row: this machine's hostname and the herdr it has, if any. Read
/// from the kernel and files rather than a new dependency for `gethostname`.
fn head_row() -> pastor::cli::HeadRow {
    let hostname = ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .find_map(|p| {
            std::fs::read_to_string(p)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "-".into());
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

/// `machine list`: the head's view when it runs; otherwise a probe of each
/// machine in flock.toml. The note that says so is printed only once
/// everything worked, since a failure must leave exactly one JSON value on
/// stderr.
async fn machine_list(
    paths: &Paths,
    flock: Option<&str>,
    json: bool,
    head: Head,
) -> anyhow::Result<()> {
    // A head that is up but busy never gets here (`probe_head`), so the
    // machines are probed directly only when nothing is listening.
    let (rows, note) = match head {
        Head::Live => {
            let IpcResponse::Machines(ms) = ask(paths, IpcRequest::FlockList).await? else {
                unreachable!()
            };
            let rows: Vec<pastor::cli::MachineRow> =
                ms.iter().map(pastor::cli::MachineRow::from).collect();
            (rows, None)
        }
        Head::Absent => {
            let f = Flock::load(&paths.flock_file())?;
            // Connects without the head, so the state dir the ssh master sockets
            // live under may not exist yet, and must be private.
            paths.ensure()?;
            let store = Store::open(&paths.db_file())?;
            let mut rows = Vec::new();
            // Only the machines asked for: a probe is an ssh round trip each.
            for m in f
                .machines
                .iter()
                .filter(|m| flock.is_none_or(|n| f.flock_of(m) == n))
            {
                rows.push(probe_machine(m, f.flock_of(m), paths, &store).await?);
            }
            (
                rows,
                Some("pastor serve is not running; probed the machines directly"),
            )
        }
    };
    let mut rows: Vec<pastor::cli::MachineRow> = rows
        .into_iter()
        .filter(|m| flock.is_none_or(|n| m.flock == n))
        .collect();
    pastor::cli::head_machine_first(&mut rows);
    let head = head_row();
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
        println!(
            "{}",
            pastor::cli::table(
                &pastor::cli::MACHINE_HEADER,
                &pastor::cli::machine_rows(&rows)
            )
        );
    }
    Ok(())
}

/// The states a task can be in while it still needs pastor or a human:
/// what `pastor task list` shows by default. Done, failed, stale and closed tasks
/// are finished; they appear only with `--all` (or `--done` for done ones).
const LIVE_STATES: [TaskState; 4] = [
    TaskState::Queued,
    TaskState::Starting,
    TaskState::Running,
    TaskState::Blocked,
];

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
        let IpcResponse::Tasks(ts) = ask(paths, IpcRequest::List { filter }).await? else {
            unreachable!()
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
        println!("{}", serde_json::to_string_pretty(&tasks)?);
    } else if tasks.is_empty() {
        if hint.is_none() {
            println!("no tasks");
        }
    } else {
        let mut rows = pastor::cli::task_rows(&tasks);
        // The flock file is the truth for "removed" whether or not a head
        // runs (a running head re-reads it every tick). `load_existing`
        // rather than `load`: a flock.toml that is momentarily missing (an
        // editor's delete-and-rename, or a race with `machine add|remove`
        // rewriting it) must mark nothing, not everything (Copilot
        // 4103271200, 4103271289, 4103271156).
        if let Ok(flock) = Flock::load_existing(&paths.flock_file()) {
            pastor::cli::mark_removed(&mut rows, &tasks, &flock);
        }
        println!("{}", pastor::cli::table(&pastor::cli::TASK_HEADER, &rows));
    }
    // Orphans have no row to list, so they get a line each under the table.
    // Only a running head knows them (its last reconcile); `--json` stays a
    // plain task array, and `machine list --json` carries them instead.
    if head.is_live() && !a.json && show_orphans {
        let IpcResponse::Machines(ms) = ask(paths, IpcRequest::FlockList).await? else {
            unreachable!()
        };
        for line in pastor::cli::orphan_lines(&ms, a.machine.as_deref(), a.flock.as_deref()) {
            println!("{line}");
        }
    }
    Ok(())
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
    parse_task_id(s)
        .unwrap_or_else(|| fail("usage_error", &format!("{s} is not a task id like t-12")))
}

async fn task(paths: &Paths, cmd: TaskCmd, head: Head) -> anyhow::Result<()> {
    match cmd {
        TaskCmd::Run(args) => run(paths, args).await?,
        TaskCmd::List(args) => list(paths, args, head).await?,
        TaskCmd::Show { task, json } => {
            let id = task_id(&task);
            let t = if head.is_live() {
                let IpcResponse::Task(t) = ask(paths, IpcRequest::TaskShow { id }).await? else {
                    unreachable!()
                };
                t
            } else {
                open_store(paths)?
                    .get_task(id)?
                    .unwrap_or_else(|| fail("task_not_found", &task))
            };
            if json {
                print_task(&t, true);
            } else {
                println!("{}", pastor::cli::task_detail(&t));
            }
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
        TaskCmd::Attach { task } => attach(paths, &task).await?,
        TaskCmd::Retry(a) => pastor::task_cli::retry(paths, a).await?,
        TaskCmd::Close(a) => pastor::task_cli::close(paths, a).await?,
        TaskCmd::Prune(a) => pastor::task_cli::prune(paths, a, head).await?,
        TaskCmd::Send(a) => pastor::task_cli::send(paths, a).await?,
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
            tags,
            flock,
            herdr,
        } => {
            let mut doc = FlockDoc::open(&path)?;
            let m = MachineConfig {
                name: name.clone(),
                local,
                ssh,
                command: command.as_deref().map(command_argv),
                session,
                max_agents,
                tags,
                flock,
                agent: None,
                agent_args: None,
            };
            doc.add_machine(&m).map_err(edit_error)?;
            doc.save(&path)?;
            let f = doc.flock().map_err(anyhow::Error::msg)?;
            println!(
                "added {name} to flock {} in {}",
                f.machine_flock(&name).unwrap_or_default(),
                path.display()
            );
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
            println!("{}", reload_running_head(paths, head).await);
        }
        MachineCmd::Remove { name, herdr } => {
            let mut doc = FlockDoc::open(&path)?;
            let target = doc
                .flock()
                .map_err(anyhow::Error::msg)?
                .get(&name)
                .and_then(|m| m.ssh.clone());
            doc.remove_machine(&name).map_err(edit_error)?;
            doc.save(&path)?;
            println!("removed {name}; {}", reload_running_head(paths, head).await);
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
            let mut doc = FlockDoc::open(&path)?;
            doc.move_machine(&name, &flock).map_err(edit_error)?;
            doc.save(&path)?;
            println!(
                "moved {name} to flock {flock}; tasks already on it stay; {}",
                reload_running_head(paths, head).await
            );
        }
        MachineCmd::List { flock, json } => {
            machine_list(paths, flock.as_deref(), json, head).await?
        }
        MachineCmd::Describe { name, json } => machine_describe(paths, &name, json, head).await?,
    }
    Ok(())
}

/// A refused flock.toml edit, with its stable code.
fn edit_error(e: EditError) -> anyhow::Error {
    pastor::cli::CliError::err(e.code(), e)
}

async fn flock(paths: &Paths, cmd: FlockCmd, head: Head) -> anyhow::Result<()> {
    let path = paths.flock_file();
    let edit = |f: &dyn Fn(&mut FlockDoc) -> Result<(), EditError>| -> anyhow::Result<()> {
        let mut doc = FlockDoc::open(&path)?;
        f(&mut doc).map_err(edit_error)?;
        doc.save(&path)
    };
    let done = match cmd {
        FlockCmd::List { json } => return flock_list(paths, json, head).await,
        FlockCmd::Describe { name, json } => {
            return flock_describe(paths, &name, json, head).await;
        }
        FlockCmd::Edit => {
            let check = |text: &str| {
                Flock::parse(&path, text)
                    .map(|_| ())
                    .map_err(|e| format!("{e:#}"))
            };
            return edit_file(paths, &path, &check, head).await;
        }
        FlockCmd::Add { name, default } => {
            // A first default flock would take the implicit flock's
            // machines, stranding the tasks queued there. As with `flock
            // remove` without a head, a task queued between this read and
            // the save is not seen; `pastor task close` recovers it.
            let queued: Vec<String> = if default && Flock::load(&path)?.flocks.is_empty() {
                queued_tasks(paths, head)
                    .await?
                    .iter()
                    .filter(|t| t.flock.as_deref() == Some(DEFAULT_FLOCK))
                    .map(|t| t.display_id())
                    .collect()
            } else {
                Vec::new()
            };
            let mut doc = FlockDoc::open(&path)?;
            let added = doc.add_flock(&name, default, &queued).map_err(edit_error)?;
            doc.save(&path)?;
            let mut done = if default {
                format!("added flock {name}, now the default")
            } else {
                format!("added flock {name}")
            };
            if !added.machines.is_empty() {
                let (noun, verb) = match added.machines.len() {
                    1 => ("machine", "stays"),
                    _ => ("machines", "stay"),
                };
                let machines = added.machines.join(", ");
                if added.moved {
                    done += &format!("; {noun} {machines} moved to it");
                } else {
                    done += &format!("; {noun} {machines} {verb} in flock {}", added.flock);
                }
                if !added.held_by.is_empty() {
                    done += &format!(", which has queued tasks: {}", added.held_by.join(", "));
                }
            }
            done
        }
        FlockCmd::Remove { name } => {
            // With a head, the head checks and edits under the lock `task
            // run` takes, so no task can be queued in the flock in between.
            if head.is_live() {
                let IpcResponse::Text(done) = ask(paths, IpcRequest::FlockRemove { name }).await?
                else {
                    unreachable!()
                };
                println!("{done}");
                return Ok(());
            }
            // With no head, the store check and the file edit are not atomic
            // against a head that starts in between. That head could accept
            // `task run --flock <name>` after the check and before the save;
            // the reload then drops the flock and the task stays queued with
            // no machine to take it (`pastor task close` recovers it). It
            // needs one user to start a head and submit to this flock while
            // removing it, so it is left open; closing it would take a file
            // lock shared by this edit and daemon startup.
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
        FlockCmd::Default { name } => {
            edit(&|d| d.set_default(&name))?;
            format!("{name} is the default flock; machines stay in their flocks")
        }
    };
    println!("{done}; {}", reload_running_head(paths, head).await);
    Ok(())
}

/// `flock list`: the flock file, the queued tasks, and with a head running
/// the live agents on each flock's machines.
async fn flock_list(paths: &Paths, json: bool, head: Head) -> anyhow::Result<()> {
    let f = Flock::load(&paths.flock_file())?;
    let live = if head.is_live() {
        let IpcResponse::Machines(ms) = ask(paths, IpcRequest::FlockList).await? else {
            unreachable!()
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
        println!(
            "{}",
            pastor::cli::table(&pastor::cli::FLOCK_HEADER, &pastor::cli::flock_rows(&rows))
        );
    }
    Ok(())
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

async fn attach(paths: &Paths, task: &str) -> anyhow::Result<()> {
    let id = task_id(task);
    let t = open_store(paths)?
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
            .args(["-t", "--", target])
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
    let store = Arc::new(open_store(paths)?);
    Ok(Scheduler::standalone(paths.clone(), &config, store)?.with_connectors())
}

async fn tick(paths: &Paths, a: TickArgs, head: Head) -> anyhow::Result<()> {
    let runs = if head.is_live() {
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
    let IpcResponse::Jobs(jobs) = ask(paths, IpcRequest::Reload).await? else {
        unreachable!()
    };
    print_jobs(&jobs, false)
}

async fn job(paths: &Paths, cmd: JobCmd, head: Head) -> anyhow::Result<()> {
    match cmd {
        JobCmd::List { json } => {
            let jobs = if head.is_live() {
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
        JobCmd::Enable { name } => toggle(paths, &name, true, head).await?,
        JobCmd::Disable { name } => toggle(paths, &name, false, head).await?,
        JobCmd::Run { name } => {
            let IpcResponse::Text(msg) = ask(paths, IpcRequest::JobRun { name }).await? else {
                unreachable!()
            };
            println!("{msg}");
        }
        JobCmd::Reload => reload(paths).await?,
        JobCmd::Edit { name } => job_edit(paths, &name, head).await?,
        JobCmd::Describe { name, json } => job_describe(paths, &name, json, head).await?,
    }
    Ok(())
}

/// The job file `name` names, which must exist. The name is checked before
/// it is joined under the jobs dir: `"../pastor"` would resolve outside it.
fn existing_job_file(paths: &Paths, name: &str) -> std::path::PathBuf {
    if let Err(e) = check_name(name) {
        fail("job_not_found", &e);
    }
    let path = job_path(&paths.jobs_dir(), name);
    if !path.exists() {
        fail("job_not_found", &format!("no job file {}", path.display()));
    }
    path
}

/// Edit `path` in the user's editor, checked with `check`; a saved edit
/// reloads a running head.
async fn edit_file(
    paths: &Paths,
    path: &std::path::Path,
    check: &dyn Fn(&str) -> Result<(), String>,
    head: Head,
) -> anyhow::Result<()> {
    let editor = pastor::edit::editor();
    match pastor::edit::edit(path, &editor, check, &mut |_| ask_reopen())? {
        pastor::edit::Outcome::Unchanged => println!("no changes to {}", path.display()),
        pastor::edit::Outcome::Saved => println!(
            "saved {}; {}",
            path.display(),
            reload_running_head(paths, head).await
        ),
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

/// A job file is checked as the head loads it: against pastor.toml's
/// `[defaults]` and the connectors installed here.
async fn job_edit(paths: &Paths, name: &str, head: Head) -> anyhow::Result<()> {
    let path = existing_job_file(paths, name);
    let defaults = PastorConfig::load(&paths.config_file())?.defaults;
    let catalog: Box<dyn pastor::connector::Catalog> =
        match pastor::connector::ConnectorCatalog::load(paths) {
            Ok(c) => Box::new(c),
            Err(_) => Box::new(pastor::connector::Builtins),
        };
    let check = |text: &str| {
        pastor::config::job::Job::parse(text, name, &defaults, catalog.as_ref()).map(|_| ())
    };
    edit_file(paths, &path, &check, head).await
}

async fn config_edit(paths: &Paths, head: Head) -> anyhow::Result<()> {
    let path = paths.config_file();
    let check = |text: &str| {
        PastorConfig::parse(&path, text)
            .map(|_| ())
            .map_err(|e| format!("{e:#}"))
    };
    edit_file(paths, &path, &check, head).await
}

/// Tasks matching `filter`, from the head when one runs, else the store.
async fn tasks_matching(
    paths: &Paths,
    head: Head,
    filter: TaskFilter,
) -> anyhow::Result<Vec<Task>> {
    if head.is_live() {
        let IpcResponse::Tasks(ts) = ask(paths, IpcRequest::List { filter }).await? else {
            unreachable!()
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
    let path = existing_job_file(paths, name);
    let statuses = if head.is_live() {
        let IpcResponse::Jobs(jobs) = ask(paths, IpcRequest::JobList).await? else {
            unreachable!()
        };
        jobs
    } else {
        let mut s = standalone(paths)?;
        s.reload();
        s.statuses(chrono::Utc::now())
    };
    let Some(status) = statuses.into_iter().find(|j| j.name == name) else {
        fail("job_not_found", &format!("the head has no job {name}"));
    };
    // The tables as written, so a file that does not parse still shows what
    // it says as far as TOML goes.
    let raw: Option<toml::Table> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| toml::from_str(&t).ok());
    let table = |key: &str| {
        raw.as_ref()
            .and_then(|r| r.get(key))
            .and_then(|v| serde_json::to_value(v).ok())
    };
    let state = open_store(paths)?.job_state(name)?.unwrap_or_default();
    let tasks = tasks_matching(
        paths,
        head,
        TaskFilter {
            job: Some(name.to_string()),
            ..Default::default()
        },
    )
    .await?
    .into_iter()
    .take(pastor::describe::RECENT)
    .collect();
    let events = pastor::describe::recent_events(events_log(paths), |e| {
        e.kind.starts_with("job.") && e.job.as_deref() == Some(name)
    });
    let d = pastor::describe::JobDescription {
        name: name.to_string(),
        file: path.display().to_string(),
        schedule: status.schedule,
        enabled: status.enabled,
        error: status.error,
        running: status.running,
        flock: status.flock,
        connector: table("connector"),
        dispatch: table("dispatch"),
        next_due: status.next_due,
        last_run_at: status.last_run_at.or(state.last_run_at),
        last_ok_at: state.last_ok_at,
        last_result: status.last_result.or(state.last_result),
        last_error: state.last_error,
        failures: state.failures,
        backoff_until: state.backoff_until,
        tasks,
        events,
    };
    print_description(&d, json, pastor::describe::job_text)
}

async fn machine_describe(paths: &Paths, name: &str, json: bool, head: Head) -> anyhow::Result<()> {
    let f = Flock::load(&paths.flock_file())?;
    let Some(m) = f.get(name) else {
        fail(
            "unknown_machine",
            &format!("no machine {name} in the flock"),
        );
    };
    let live = if head.is_live() {
        let IpcResponse::Machines(ms) = ask(paths, IpcRequest::FlockList).await? else {
            unreachable!()
        };
        ms.iter()
            .find(|s| s.name == name)
            .map(pastor::cli::MachineRow::from)
    } else {
        None
    };
    let row = match live {
        Some(row) => row,
        // No head, or one that has not picked the machine up yet.
        None => {
            paths.ensure()?;
            let store = Store::open(&paths.db_file())?;
            probe_machine(m, f.flock_of(m), paths, &store).await?
        }
    };
    let tasks = tasks_matching(
        paths,
        head,
        TaskFilter {
            machine: Some(name.to_string()),
            states: Some(pastor::task::PANE_OWNING_STATES.to_vec()),
            ..Default::default()
        },
    )
    .await?;
    let recent_errors = pastor::describe::recent_events(events_log(paths), |e| {
        let machine_error = e.kind.starts_with("machine.")
            && e.machine
                .as_ref()
                .is_some_and(|s| s.name == name && s.error.is_some());
        let failed_here = e.kind == "task.failed"
            && e.task.as_ref().and_then(|t| t.machine.as_deref()) == Some(name);
        machine_error || failed_here
    });
    let d = pastor::describe::MachineDescription {
        row,
        session: m.session.clone(),
        tasks,
        recent_errors,
    };
    print_description(&d, json, pastor::describe::machine_text)
}

async fn flock_describe(paths: &Paths, name: &str, json: bool, head: Head) -> anyhow::Result<()> {
    let f = Flock::load(&paths.flock_file())?;
    if !f.has_flock(name) {
        fail("unknown_flock", &format!("no flock {name}"));
    }
    let live = if head.is_live() {
        let IpcResponse::Machines(ms) = ask(paths, IpcRequest::FlockList).await? else {
            unreachable!()
        };
        Some(ms)
    } else {
        None
    };
    let row = pastor::cli::flock_list(&f, live.as_deref(), &[])
        .into_iter()
        .find(|r| r.name == name)
        .expect("has_flock");
    let tasks = tasks_matching(
        paths,
        head,
        TaskFilter {
            flock: Some(name.to_string()),
            states: Some(LIVE_STATES.to_vec()),
            ..Default::default()
        },
    )
    .await?;
    let entry = f.entry(name).cloned().unwrap_or_default();
    let d = pastor::describe::FlockDescription {
        name: row.name,
        default: row.default,
        agent: entry.agent,
        agent_args: entry.agent_args,
        allow: entry.allow,
        deny: entry.deny,
        machines: row.machines,
        agents: row.agents,
        tasks,
    };
    print_description(&d, json, pastor::describe::flock_text)
}

async fn toggle(paths: &Paths, name: &str, enabled: bool, head: Head) -> anyhow::Result<()> {
    let path = existing_job_file(paths, name);
    set_enabled(&path, enabled)?;
    let verb = if enabled { "enabled" } else { "disabled" };
    if head.is_live() {
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
    use pastor::ipc::RequestError;

    fn run_args(argv: &[&str]) -> RunArgs {
        let mut full = vec!["pastor", "task", "run"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full).unwrap().command.unwrap() {
            Command::Task {
                cmd: TaskCmd::Run(a),
            } => a,
            other => panic!("{other:?}"),
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

    /// The head resolves the agent with the task's flock, so the request
    /// carries only what the flags said: nothing, when they said nothing.
    /// A head from before `AGENT_PROTOCOL` would drop a task's deny list
    /// without a word, so every command that makes it queue a task refuses
    /// it; other commands still work with it.
    #[tokio::test]
    async fn queueing_a_task_refuses_a_head_before_tool_lists() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        paths.ensure().unwrap();
        let listener = tokio::net::UnixListener::bind(paths.socket_file()).unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut line = String::new();
                tokio::io::BufReader::new(r)
                    .read_line(&mut line)
                    .await
                    .unwrap();
                let pong = IpcResponse::Pong {
                    version: "0.4.0".into(),
                    protocol: 1,
                };
                let mut out = serde_json::to_string(&pong).unwrap();
                out.push('\n');
                w.write_all(out.as_bytes()).await.unwrap();
            }
        });
        let err = probe_head(&paths, false, true, false).await.unwrap_err();
        let err = err.downcast::<pastor::cli::CliError>().unwrap();
        assert_eq!(err.code, "head_too_old");
        assert!(
            err.message.contains("tool allow and deny"),
            "{}",
            err.message
        );
        assert_eq!(
            probe_head(&paths, true, false, false).await.unwrap(),
            Head::Live
        );

        let parse = |argv: &[&str]| Cli::try_parse_from(argv).unwrap().command.unwrap();
        assert!(needs_agent_protocol(&parse(&[
            "pastor", "task", "run", "hi"
        ])));
        assert!(needs_agent_protocol(&parse(&[
            "pastor", "task", "retry", "t-1"
        ])));
        assert!(needs_agent_protocol(&parse(&["pastor", "tick"])));
        assert!(needs_agent_protocol(&parse(&[
            "pastor", "tick", "--job", "j"
        ])));
        assert!(needs_agent_protocol(&parse(&["pastor", "job", "run", "j"])));
        assert!(!needs_agent_protocol(&parse(&["pastor", "task", "list"])));
        assert!(!needs_agent_protocol(&parse(&[
            "pastor",
            "tick",
            "--dry-run"
        ])));
        assert!(!needs_agent_protocol(&parse(&["pastor", "job", "list"])));
        assert!(!needs_agent_protocol(&parse(&["pastor", "job", "reload"])));
    }

    /// A head from before `PLACE_PROTOCOL` reads `task retry --place`
    /// without the place (serde skips the unknown field) and retries the
    /// task where it was, answering success. The CLI refuses to send it
    /// there; a retry without `--place` still goes.
    #[tokio::test]
    async fn retry_with_a_place_refuses_a_head_before_it() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        paths.ensure().unwrap();
        let listener = tokio::net::UnixListener::bind(paths.socket_file()).unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut line = String::new();
                tokio::io::BufReader::new(r)
                    .read_line(&mut line)
                    .await
                    .unwrap();
                let pong = IpcResponse::Pong {
                    version: "0.5.0".into(),
                    protocol: pastor::ipc::AGENT_PROTOCOL,
                };
                let mut out = serde_json::to_string(&pong).unwrap();
                out.push('\n');
                w.write_all(out.as_bytes()).await.unwrap();
            }
        });
        let err = probe_head(&paths, false, true, true).await.unwrap_err();
        let err = err.downcast::<pastor::cli::CliError>().unwrap();
        assert_eq!(err.code, "head_too_old");
        assert!(err.message.contains("--place"), "{}", err.message);
        assert_eq!(
            probe_head(&paths, false, true, false).await.unwrap(),
            Head::Live
        );

        let parse = |argv: &[&str]| Cli::try_parse_from(argv).unwrap().command.unwrap();
        assert!(needs_place_protocol(&parse(&[
            "pastor", "task", "retry", "t-1", "--place", "pastor"
        ])));
        assert!(!needs_place_protocol(&parse(&[
            "pastor", "task", "retry", "t-1"
        ])));
        assert!(!needs_place_protocol(&parse(&["pastor", "task", "list"])));
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
        let (code, message) =
            request_failure(&RequestError::Timeout(std::time::Duration::from_secs(120)));
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
            let (code, message) = request_failure(&RequestError::Connect(err));
            assert_eq!(code, "runtime_error");
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
            let (code, message) = request_failure(&RequestError::Connect(err));
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
            vec!["pastor", "task", "show", "t-1"],
            vec!["pastor", "task", "read", "t-1"],
            vec!["pastor", "task", "attach", "t-1"],
        ] {
            if let Err(e) = Cli::try_parse_from(&args) {
                panic!("{args:?}: {e}");
            }
        }
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

    /// Every `pastor ...` command a skill shows, in inline code or a code
    /// block, must be a real command with real long flags, so the skills
    /// cannot drift from the CLI. Words that are not commands end the walk
    /// (`t-12`, a quoted prompt); flags are checked on the command reached.
    /// Every Markdown file under `skills/` counts: the reference files and
    /// worked examples are read by agents as much as the SKILL.md itself.
    #[test]
    fn skills_mention_only_real_commands_and_flags() {
        let mut files = Vec::new();
        markdown_files(&skills_dir(), &mut files);
        assert!(files.len() > 1, "found only {files:?}");
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            let checked = check_commands(&text);
            if file.ends_with("pastor/SKILL.md") {
                assert!(checked > 20, "only {checked} commands found in {file:?}");
            }
        }
        // The binary prints the copy it was built with; it is the same file.
        assert!(check_commands(SKILL) > 20);
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

    /// Checks every `pastor` command in `text`; returns how many it saw.
    fn check_commands(text: &str) -> usize {
        use clap::CommandFactory;
        let root = Cli::command();
        let mut checked = 0;
        for code in code_spans(text) {
            for line in code.lines() {
                let line = line.split(" # ").next().unwrap_or(line);
                let words: Vec<&str> = line.split_whitespace().collect();
                for (at, _) in words.iter().enumerate().filter(|(_, w)| **w == "pastor") {
                    check_command(&root, &words[at + 1..], line);
                    checked += 1;
                }
            }
        }
        checked
    }

    /// Inline code spans and fenced blocks of a Markdown text, in order.
    fn code_spans(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut fence: Option<String> = None;
        for line in text.lines() {
            if line.starts_with("```") {
                match fence.take() {
                    Some(block) => out.push(block),
                    None => fence = Some(String::new()),
                }
                continue;
            }
            if let Some(block) = fence.as_mut() {
                // A trailing backslash continues the command on the next line.
                block.push_str(line.trim_end_matches('\\'));
                if !line.ends_with('\\') {
                    block.push('\n');
                }
                continue;
            }
            out.extend(line.split('`').skip(1).step_by(2).map(str::to_string));
        }
        out
    }

    fn check_command(root: &clap::Command, words: &[&str], line: &str) {
        let mut cmd = root;
        let mut rest = words;
        while let Some(word) = rest.first() {
            match cmd.find_subcommand(word) {
                Some(sub) => {
                    cmd = sub;
                    rest = &rest[1..];
                }
                None if cmd.has_subcommands() && !word.starts_with('-') => {
                    panic!(
                        "{line:?}: `{word}` is not a subcommand of `{}`",
                        cmd.get_name()
                    )
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
            if name == "help" {
                continue;
            }
            let arg = cmd
                .get_arguments()
                .find(|a| a.get_long() == Some(name))
                .unwrap_or_else(|| panic!("{line:?}: `{}` has no --{name}", cmd.get_name()));
            if !inline && arg.get_action().takes_values() {
                words.next();
            }
        }
    }
}
