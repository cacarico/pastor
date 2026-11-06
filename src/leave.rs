//! What pastor puts on a host for itself, and taking it away again: for a
//! machine the head stops using (`machine remove`), and for a host that
//! stops being part of a fleet (`pastor leave`).
//!
//! Only what pastor marked or made is touched: the lines of `authorized_keys`
//! that end in one of pastor's markers (`authkeys`), the entries of pastor's
//! own `known_hosts` for the machine's host, the machine's bootstrap row, and
//! the key files at the places the state directory names for them. A user's
//! own key, `~/.ssh/known_hosts`, the config, the flock and the task database
//! stay as they are.

use std::path::Path;

use crate::config::Paths;
use crate::store::Store;

/// The files in the state directory that hold pastor's own ssh keys and
/// host keys: the head's key, a machine's key and an agent's key with their
/// `.pub`, the known_hosts, and a bootstrap report a join left unsent. Only regular files are taken; a symlink or a
/// directory at one of these names is left.
pub fn own_files(paths: &Paths) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for key in [paths.head_key(), paths.machine_key(), paths.agent_key()] {
        let public = std::path::PathBuf::from(format!("{}.pub", key.display()));
        out.push(key);
        out.push(public);
    }
    out.push(paths.known_hosts_file());
    out.push(crate::bootstrap::report_file(&paths.state_dir));
    out
}

/// The host of an ssh target (`user@host`): what a `known_hosts` entry
/// names. None for a target that is a bracketed address or empty.
pub fn host_of(target: &str) -> Option<&str> {
    let host = target.rsplit_once('@').map_or(target, |(_, h)| h);
    (!host.is_empty() && !host.starts_with('[') && !host.contains(char::is_whitespace))
        .then_some(host)
}

/// Whether `pattern`, one name of a `known_hosts` line, names `host`: the
/// host itself, or the host with a port, `[host]:22`.
fn names_host(pattern: &str, host: &str) -> bool {
    pattern == host
        || pattern
            .strip_prefix('[')
            .and_then(|p| p.split_once("]:"))
            .is_some_and(|(h, port)| h == host && port.chars().all(|c| c.is_ascii_digit()))
}

/// What a `known_hosts` file holds for `host`: the lines that name it
/// (alone, or in a list with other names) and the hashed lines, which hide
/// the name they are for and so cannot be told from here.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HostEntries {
    pub named: usize,
    pub hashed: usize,
}

fn read(file: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(file) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::from(e).context(format!("read {}", file.display()))),
    }
}

/// `text` without `host`: a line for that host alone goes, a line that lists
/// it among others keeps the others. Comments, blank lines and every other
/// line stay as they were.
fn without_host(text: &str, host: &str) -> (String, HostEntries) {
    let mut entries = HostEntries::default();
    let mut out = String::new();
    for l in text.lines() {
        let t = l.trim_start();
        let mut words = t.splitn(2, char::is_whitespace);
        let (names, rest) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
        if t.starts_with('#') || t.is_empty() {
            out.push_str(l);
            out.push('\n');
            continue;
        }
        // A marker word (`@cert-authority`, `@revoked`) comes first.
        let (marker, names, rest) = if names.starts_with('@') {
            let mut more = rest.trim_start().splitn(2, char::is_whitespace);
            (
                Some(names),
                more.next().unwrap_or(""),
                more.next().unwrap_or(""),
            )
        } else {
            (None, names, rest)
        };
        if names.starts_with("|1|") {
            entries.hashed += 1;
            out.push_str(l);
            out.push('\n');
            continue;
        }
        let list: Vec<&str> = names.split(',').collect();
        let kept: Vec<&str> = list
            .iter()
            .copied()
            .filter(|p| !names_host(p, host))
            .collect();
        if kept.len() == list.len() {
            out.push_str(l);
            out.push('\n');
            continue;
        }
        entries.named += 1;
        if !kept.is_empty() {
            if let Some(m) = marker {
                out.push_str(m);
                out.push(' ');
            }
            out.push_str(&kept.join(","));
            out.push(' ');
            out.push_str(rest);
            out.push('\n');
        }
    }
    (out, entries)
}

/// What `known_hosts` holds for `host`, for a dry run. A file that is not
/// there holds nothing.
pub fn host_entries(file: &Path, host: &str) -> anyhow::Result<HostEntries> {
    Ok(read(file)?.map_or_else(HostEntries::default, |t| without_host(&t, host).1))
}

/// Drop `host` from pastor's own `known_hosts` (`Paths::known_hosts_file`);
/// what it found. The file is replaced whole by rename and keeps its mode.
pub fn remove_host(file: &Path, host: &str) -> anyhow::Result<HostEntries> {
    let Some(text) = read(file)? else {
        return Ok(HostEntries::default());
    };
    let (out, entries) = without_host(&text, host);
    if entries.named == 0 {
        return Ok(entries);
    }
    if std::fs::symlink_metadata(file)?.file_type().is_symlink() {
        anyhow::bail!(
            "{} is a symlink; pastor does not write through it",
            file.display()
        );
    }
    let tmp = file.with_extension(format!("tmp.{}", std::process::id()));
    let mode = std::fs::metadata(file)?.permissions();
    std::fs::write(&tmp, out)?;
    std::fs::set_permissions(&tmp, mode)?;
    std::fs::rename(&tmp, file)?;
    Ok(entries)
}

/// What taking `machine` out of the fleet removes besides its flock.toml
/// entry, one sentence for each thing found (or, with `dry_run`, that
/// would go). Nothing found is nothing said. `host` is the machine's ssh
/// host, none for a machine not reached by ssh. `store` is the head's
/// database, none when it cannot be opened.
pub fn machine_cleanup(
    paths: &Paths,
    store: Option<&Store>,
    machine: &str,
    host: Option<&str>,
    dry_run: bool,
) -> anyhow::Result<Vec<String>> {
    let verb = if dry_run { "would remove" } else { "removed" };
    let mut said = Vec::new();
    let auth = crate::authkeys::default_file();
    let n = if dry_run {
        crate::authkeys::machine_lines(&auth, machine)?.len()
    } else {
        crate::authkeys::remove_machine(&auth, machine)?
    };
    if n > 0 {
        said.push(format!(
            "{verb} {n} line{} of {machine}'s from {}",
            if n == 1 { "" } else { "s" },
            auth.display()
        ));
    }
    if let Some(host) = host {
        let kh = paths.known_hosts_file();
        let found = if dry_run {
            host_entries(&kh, host)?
        } else {
            remove_host(&kh, host)?
        };
        if found.named > 0 {
            said.push(format!("{verb} {host} from {}", kh.display()));
        }
        if found.hashed > 0 {
            said.push(format!(
                "{} hashed line{} of {} stay: they hide their host; ssh-keygen -R {host} -f {} drops it",
                found.hashed,
                if found.hashed == 1 { "" } else { "s" },
                kh.display(),
                kh.display()
            ));
        }
    }
    if let Some(store) = store
        && store.machine_state(machine)?.is_some()
    {
        if !dry_run {
            store.drop_machine_state(machine)?;
        }
        said.push(format!("{verb} {machine}'s bootstrap state"));
    }
    if let Some(store) = store
        && store.cordon(machine)?.is_some()
    {
        if !dry_run {
            store.drop_cordon(machine)?;
        }
        said.push(format!("{verb} {machine}'s cordon"));
    }
    Ok(said)
}

/// `pastor leave`: take from this host what pastor put there for itself,
/// the lines it marked in `authorized_keys` and its own key and known_hosts
/// files in the state directory, and nothing else. One sentence for each
/// thing found; with `dry_run`, what would go.
pub fn leave(paths: &Paths, dry_run: bool) -> anyhow::Result<Vec<String>> {
    let verb = if dry_run { "would remove" } else { "removed" };
    let mut said = Vec::new();
    let auth = crate::authkeys::default_file();
    let n = if dry_run {
        crate::authkeys::all_lines(&auth)?.len()
    } else {
        crate::authkeys::remove_all(&auth)?
    };
    if n > 0 {
        said.push(format!(
            "{verb} {n} line{} pastor marked from {}",
            if n == 1 { "" } else { "s" },
            auth.display()
        ));
    }
    for file in own_files(paths) {
        match std::fs::symlink_metadata(&file) {
            Ok(m) if m.file_type().is_file() => {
                if !dry_run {
                    std::fs::remove_file(&file)?;
                }
                said.push(format!("{verb} {}", file.display()));
            }
            Ok(_) => said.push(format!(
                "left {}: it is not a plain file pastor made",
                file.display()
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::Error::from(e).context(format!("look at {}", file.display())));
            }
        }
    }
    Ok(said)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_is_what_follows_the_at() {
        assert_eq!(host_of("user@pi-3"), Some("pi-3"));
        assert_eq!(host_of("pi-3"), Some("pi-3"));
        assert_eq!(host_of("user@[::1]"), None);
        assert_eq!(host_of(""), None);
    }

    /// A line for the host alone goes, a list keeps its other names, a port
    /// form goes, and hashed lines, comments, other hosts and a prefix of the
    /// name stay.
    #[test]
    fn known_hosts_loses_only_the_host() {
        let text = "# mine\n\
            pi-3 ssh-ed25519 AAAAone\n\
            pi-3,10.0.0.3 ssh-ed25519 AAAAtwo\n\
            [pi-3]:2222 ssh-ed25519 AAAAthree\n\
            pi-30 ssh-ed25519 AAAAfour\n\
            other ssh-ed25519 AAAAfive\n\
            |1|c2FsdA==|aGFzaA== ssh-ed25519 AAAAsix\n\
            @cert-authority pi-3 ssh-ed25519 AAAAseven\n\
            \n";
        let (out, found) = without_host(text, "pi-3");
        assert_eq!(
            found,
            HostEntries {
                named: 4,
                hashed: 1
            }
        );
        assert_eq!(
            out,
            "# mine\n10.0.0.3 ssh-ed25519 AAAAtwo\n\
             pi-30 ssh-ed25519 AAAAfour\nother ssh-ed25519 AAAAfive\n\
             |1|c2FsdA==|aGFzaA== ssh-ed25519 AAAAsix\n\n"
        );
        assert_eq!(without_host(text, "nowhere").1.named, 0);
    }
}
