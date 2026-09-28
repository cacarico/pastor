//! `pastor queue` and `pastor queue move`: the CLI side of `IpcRequest::Queue`
//! and `QueueMove` (`crate::queue` has the rest). The listing needs the head
//! to say why a task waits; without one it shows the store's queue and says
//! so. A move needs the head, like `task priority`.
use clap::{Args, Subcommand};

use crate::cli::{CliError, table};
use crate::config::Paths;
use crate::config::flock::Flock;
use crate::ipc::{Head, IpcRequest, IpcResponse};
use crate::queue::{QUEUE_HEADER, QueueEntry, QueueSpot};
use crate::store::{Moved, Store};
use crate::task::parse_task_id;

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct QueueArgs {
    #[command(subcommand)]
    pub cmd: Option<QueueCmd>,
    /// Only the tasks waiting in this flock
    #[arg(long)]
    pub flock: Option<String>,
    /// Only the tasks pinned to this machine
    #[arg(long)]
    pub machine: Option<String>,
    /// Print as a JSON array
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum QueueCmd {
    /// Put a queued task elsewhere in the queue; it takes the level of where it lands
    Move(MoveArgs),
}

#[derive(Args, Debug)]
pub struct MoveArgs {
    /// A task, like t-12 or 12: a queued one
    pub task: String,
    #[command(flatten)]
    pub to: SpotArgs,
    /// Print as a JSON object
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
#[group(required = true, multiple = false)]
pub struct SpotArgs {
    /// First in the queue, lifted to the first task's level if that is higher
    #[arg(long)]
    pub top: bool,
    /// Just ahead of this queued task
    #[arg(long, value_name = "TASK")]
    pub before: Option<String>,
    /// Just behind this queued task
    #[arg(long, value_name = "TASK")]
    pub after: Option<String>,
    /// At this position, as `pastor queue` numbers it (1 is first)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    pub to: Option<u64>,
}

/// Whether `a` changes the fleet: a move does, the listing does not.
pub fn changes_fleet(a: &QueueArgs) -> bool {
    a.cmd.is_some()
}

fn task_id(s: &str) -> anyhow::Result<i64> {
    parse_task_id(s).ok_or_else(|| CliError::err("usage_error", crate::task::bad_task_id(s)))
}

impl SpotArgs {
    fn spot(&self) -> anyhow::Result<QueueSpot> {
        Ok(match (self.top, &self.before, &self.after, self.to) {
            (true, ..) => QueueSpot::Top,
            (_, Some(t), ..) => QueueSpot::Before(task_id(t)?),
            (_, _, Some(t), _) => QueueSpot::After(task_id(t)?),
            (.., Some(n)) => QueueSpot::To(usize::try_from(n).unwrap_or(usize::MAX)),
            _ => unreachable!("clap requires one"),
        })
    }
}

pub async fn run(paths: &Paths, a: QueueArgs, head: Head) -> anyhow::Result<()> {
    match a.cmd {
        Some(QueueCmd::Move(m)) => move_task(paths, m).await,
        None => list(paths, a, head).await,
    }
}

async fn ask(paths: &Paths, req: IpcRequest) -> anyhow::Result<IpcResponse> {
    match crate::ipc::request_head(paths, &req).await {
        Ok(IpcResponse::Error { code, message }) => Err(CliError::err(&code, message)),
        Ok(other) => Ok(other),
        Err(err) => Err(crate::task_cli::request_error(&err)),
    }
}

fn unexpected(resp: IpcResponse) -> anyhow::Error {
    CliError::err("internal", format!("unexpected daemon reply: {resp:?}"))
}

async fn list(paths: &Paths, a: QueueArgs, head: Head) -> anyhow::Result<()> {
    let entries = if head.is_live() {
        let req = IpcRequest::Queue {
            flock: a.flock.clone(),
            machine: a.machine.clone(),
        };
        match ask(paths, req).await? {
            IpcResponse::Queue(entries) => entries,
            other => return Err(unexpected(other)),
        }
    } else {
        eprintln!("pastor serve is not running; showing the last known queue");
        offline(paths, a.flock.as_deref(), a.machine.as_deref())?
    };
    if a.json {
        let v: Vec<_> = entries.iter().map(QueueEntry::to_json).collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if entries.is_empty() {
        println!("no queued tasks");
    } else {
        println!("{}", table(&QUEUE_HEADER, &crate::queue::rows(&entries)));
    }
    Ok(())
}

/// The store's queue with no head to say why each task waits.
fn offline(
    paths: &Paths,
    flock: Option<&str>,
    machine: Option<&str>,
) -> anyhow::Result<Vec<QueueEntry>> {
    let queued = if paths.db_file().exists() {
        Store::open_read_only(&paths.db_file())?.queued_tasks()?
    } else {
        Vec::new()
    };
    let default = Flock::load(&paths.flock_file())
        .map(|f| f.default_flock().to_string())
        .unwrap_or_else(|_| "default".into());
    let mut entries = crate::queue::entries(queued, &[], &default);
    for e in &mut entries {
        e.why = "pastor serve is not running".into();
    }
    entries.retain(|e| e.matches(flock, machine));
    Ok(entries)
}

/// `pastor queue move t-N --top|--before|--after|--to`.
async fn move_task(paths: &Paths, a: MoveArgs) -> anyhow::Result<()> {
    let id = task_id(&a.task)?;
    let to = a.to.spot()?;
    let moved = match ask(paths, IpcRequest::QueueMove { id, to }).await? {
        IpcResponse::Moved(m) => m,
        other => return Err(unexpected(other)),
    };
    if a.json {
        let v = serde_json::json!({
            "pos": moved.pos,
            "of": moved.of,
            "priority_was": moved.was,
            "task": moved.task.to_json(),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!("{}", moved_line(&moved));
    }
    Ok(())
}

/// What `queue move` says: where the task is now, and its level when that
/// changed.
pub fn moved_line(m: &Moved) -> String {
    let mut line = format!(
        "{} is {} of {} in the queue",
        m.task.display_id(),
        m.pos,
        m.of
    );
    if m.was != m.task.priority {
        let how = if m.task.priority > m.was {
            "lifted"
        } else {
            "lowered"
        };
        line.push_str(&format!("; {how} from {} to {}", m.was, m.task.priority));
    }
    line
}
