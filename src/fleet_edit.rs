//! The flock.toml edits behind `flock add|join|leave|default` and `machine
//! add|remove|move`. The CLI calls them when no head runs; the head calls the
//! same ones for its IPC requests, so a CLI that is not on the head's machine
//! changes the head's file and not its own copy. Each returns what it did,
//! for the caller to print with what it knows about the reload.

use std::path::Path;

use crate::config::flock::{Flock, FlockDoc, MachineConfig};
use crate::task::Task;

/// A machine's flocks as `machine list` writes them: `pastor:2,life:1`, a
/// flock with no number (the old `flock` key) by its name alone.
fn flocks_label(f: &Flock, machine: &str) -> String {
    f.machine_flocks(machine)
        .unwrap_or_default()
        .iter()
        .map(|(name, n)| match n {
            Some(n) => format!("{name}:{n}"),
            None => name.to_string(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// `flock add`. `queued` names the queued tasks in the implicit `default`
/// flock; it is asked only for a first default flock, which would otherwise
/// take that flock's machines and strand those tasks. `machines` join the
/// new flock, each with its `max_agents`.
pub fn add_flock(
    file: &Path,
    name: &str,
    default: bool,
    description: Option<&str>,
    machines: &[String],
    queued: impl FnOnce() -> anyhow::Result<Vec<String>>,
) -> anyhow::Result<String> {
    let mut doc = FlockDoc::open(file)?;
    let queued = if default && doc.flock().map_err(anyhow::Error::msg)?.flocks.is_empty() {
        queued()?
    } else {
        Vec::new()
    };
    let mut added = doc.add_flock(name, default, &queued)?;
    added.machines.retain(|m| !machines.contains(m));
    if let Some(text) = crate::config::clean_description(description) {
        doc.describe_flock(name, &text)?;
    }
    for m in machines {
        doc.join_flock(m, name, None)?;
    }
    doc.save(file)?;
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
    if !machines.is_empty() {
        let f = doc.flock().map_err(anyhow::Error::msg)?;
        let joined: Vec<String> = machines
            .iter()
            .map(|m| format!("{m} ({})", flocks_label(&f, m)))
            .collect();
        done += &format!("; joined: {}", joined.join(", "));
    }
    Ok(done)
}

/// `flock join`.
pub fn join_flock(
    file: &Path,
    flock: &str,
    machine: &str,
    max: Option<u32>,
) -> anyhow::Result<String> {
    let mut doc = FlockDoc::open(file)?;
    let n = doc.join_flock(machine, flock, max)?;
    doc.save(file)?;
    let f = doc.flock().map_err(anyhow::Error::msg)?;
    Ok(format!(
        "{machine} is in flock {flock} with {n}; its flocks: {}",
        flocks_label(&f, machine)
    ))
}

/// `flock leave`. `queued` gives the queued tasks; those of `flock` pinned
/// to `machine` are named, since they now wait for it with a note.
pub fn leave_flock(
    file: &Path,
    flock: &str,
    machine: &str,
    queued: impl FnOnce() -> anyhow::Result<Vec<Task>>,
) -> anyhow::Result<String> {
    let mut doc = FlockDoc::open(file)?;
    let default = doc
        .flock()
        .map_err(anyhow::Error::msg)?
        .default_flock()
        .to_string();
    doc.leave_flock(machine, flock)?;
    let f = doc.flock().map_err(anyhow::Error::msg)?;
    // Out of its last flock, the default, it is back in that same flock.
    let still_in = f.get(machine).is_some_and(|m| f.in_flock(m, flock));
    let pinned: Vec<String> = if still_in {
        Vec::new()
    } else {
        queued()?
            .iter()
            .filter(|t| {
                t.spec.machine.as_deref() == Some(machine)
                    && t.flock.as_deref().unwrap_or(&default) == flock
            })
            .map(|t| t.display_id())
            .collect()
    };
    doc.save(file)?;
    let mut done = format!("{machine} left flock {flock}");
    if f.get(machine).is_some_and(|m| f.unplaced(m)) {
        done += &format!("; it is back in the default flock {}", f.default_flock());
    } else {
        done += &format!("; its flocks: {}", flocks_label(&f, machine));
    }
    if !pinned.is_empty() {
        done += &format!(
            "; queued tasks pinned to it wait: {}; tasks already on it stay",
            pinned.join(", ")
        );
    } else {
        done += "; tasks already on it stay";
    }
    Ok(done)
}

/// `flock default`.
pub fn set_default(file: &Path, name: &str) -> anyhow::Result<String> {
    let mut doc = FlockDoc::open(file)?;
    doc.set_default(name)?;
    doc.save(file)?;
    Ok(format!(
        "{name} is the default flock; machines stay in their flocks"
    ))
}

/// `machine add`, the flock.toml part: `--herdr` is the caller's.
pub fn add_machine(file: &Path, m: &MachineConfig) -> anyhow::Result<String> {
    let mut doc = FlockDoc::open(file)?;
    doc.add_machine(m)?;
    doc.save(file)?;
    let f = doc.flock().map_err(anyhow::Error::msg)?;
    Ok(format!(
        "added {} to flock {} in {}",
        m.name,
        f.machine_flock(&m.name).unwrap_or_default(),
        file.display()
    ))
}

/// `machine remove`, the flock.toml part: `--herdr` is the caller's.
pub fn remove_machine(file: &Path, name: &str) -> anyhow::Result<String> {
    let mut doc = FlockDoc::open(file)?;
    doc.remove_machine(name)?;
    doc.save(file)?;
    Ok(format!("removed {name}"))
}

/// `machine move`.
pub fn move_machine(file: &Path, name: &str, flock: &str) -> anyhow::Result<String> {
    let mut doc = FlockDoc::open(file)?;
    doc.move_machine(name, flock)?;
    doc.save(file)?;
    let f = doc.flock().map_err(anyhow::Error::msg)?;
    Ok(format!(
        "moved {name} to flock {flock} ({}); tasks already on it stay",
        flocks_label(&f, name)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskState;

    const FILE: &str = "[[flock]]\nname = \"home\"\ndefault = true\n\n[[flock]]\nname = \"work\"\n\n[[machine]]\nname = \"desk\"\nlocal = true\nmax_agents = 3\nflock = \"work\"  # the old way\n";

    fn queued(id: i64, flock: Option<&str>, machine: Option<&str>) -> Task {
        let mut t = crate::task::tests::task(TaskState::Queued, None);
        t.id = id;
        t.flock = flock.map(str::to_string);
        t.spec.machine = machine.map(str::to_string);
        t
    }

    /// Join then leave: the lines say where the machine is after each, and
    /// leaving names the queued tasks of that flock pinned to the machine.
    #[test]
    fn join_and_leave_say_where_the_machine_is() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("flock.toml");
        std::fs::write(&file, FILE).unwrap();
        assert_eq!(
            join_flock(&file, "home", "desk", Some(1)).unwrap(),
            "desk is in flock home with 1; its flocks: home:1,work:3"
        );
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains("flock = \"work\""), "{text}");
        assert!(text.contains("machines = { desk = 3 }"), "{text}");
        let said = leave_flock(&file, "work", "desk", || {
            Ok(vec![
                queued(1, Some("work"), Some("desk")),
                queued(2, Some("home"), Some("desk")),
                queued(3, Some("work"), None),
            ])
        })
        .unwrap();
        assert_eq!(
            said,
            "desk left flock work; its flocks: home:1; queued tasks pinned to it wait: t-1; tasks already on it stay"
        );
        let said = leave_flock(&file, "home", "desk", || {
            Ok(vec![queued(4, None, Some("desk"))])
        })
        .unwrap();
        assert_eq!(
            said,
            "desk left flock home; it is back in the default flock home; tasks already on it stay"
        );
        let said = add_flock(&file, "play", false, None, &["desk".into()], || Ok(vec![])).unwrap();
        assert_eq!(said, "added flock play; joined: desk (play:3)");
    }
}
