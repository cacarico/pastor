//! The flock.toml edits behind `flock add|default` and `machine
//! add|remove|move`. The CLI calls them when no head runs; the head calls the
//! same ones for its IPC requests, so a CLI that is not on the head's machine
//! changes the head's file and not its own copy. Each returns what it did,
//! for the caller to print with what it knows about the reload.

use std::path::Path;

use crate::config::flock::{FlockDoc, MachineConfig};

/// `flock add`. `queued` names the queued tasks in the implicit `default`
/// flock; it is asked only for a first default flock, which would otherwise
/// take that flock's machines and strand those tasks.
pub fn add_flock(
    file: &Path,
    name: &str,
    default: bool,
    queued: impl FnOnce() -> anyhow::Result<Vec<String>>,
) -> anyhow::Result<String> {
    let mut doc = FlockDoc::open(file)?;
    let queued = if default && doc.flock().map_err(anyhow::Error::msg)?.flocks.is_empty() {
        queued()?
    } else {
        Vec::new()
    };
    let added = doc.add_flock(name, default, &queued)?;
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
    Ok(format!(
        "moved {name} to flock {flock}; tasks already on it stay"
    ))
}
