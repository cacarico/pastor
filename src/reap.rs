//! Ending what a closed task's agent left running.
//!
//! Closing a task closes its pane, and the pane's shell goes with it. What
//! the agent started under that shell does not necessarily: a build run with
//! `flock`, `nohup` or `setsid`, or by an agent tool that gives each command
//! its own process group, outlives the pane and keeps running (and holding
//! its target directory) for as long as it likes.
//!
//! It is done in two steps, because the pane has to be alive to be asked and
//! a close can still be refused after that:
//!
//! 1. Before the close, `find` lists, from the pane's shell `root` (herdr's
//!    `pane.process_info`), what is running under it: every descendant of
//!    `root`; every process in a process group one of them is in, unless a
//!    process that must stay is in it too (this finds a member whose parent
//!    already exited); and, when the task's checkout is going away, every
//!    process whose working directory is inside it (Linux, through `/proc`),
//!    which finds a daemonised build that left the tree and the groups. Each
//!    is listed with its start time, to know it again. Nothing is signalled.
//! 2. Once the close went through, `end` signals what is still running of
//!    that list, and the same process (same start time): `TERM`, a grace
//!    period, then `KILL`. What went with the pane (the agent, its tools) is
//!    gone by then and is not reported; a close that was refused ends
//!    nothing.
//!
//! `root` itself and its ancestors (herdr, the shell this runs in) are never
//! listed; closing the pane takes `root`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long a process has to end on `TERM` before it gets `KILL`.
pub const GRACE: Duration = Duration::from_secs(5);

/// What to look for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// The pane's shell: its descendants go, and the groups they are in.
    pub root: Option<u32>,
    /// A directory whose every process goes (an absolute path).
    pub cwd: Option<String>,
}

impl Spec {
    /// Whether there is anything to look for.
    pub fn is_empty(&self) -> bool {
        self.root.is_none() && self.cwd.is_none()
    }
}

/// How a process was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Found {
    /// A descendant of the pane's shell.
    Tree,
    /// In a process group of one of those, but no longer its descendant.
    Group,
    /// Working in the task's checkout, out of the tree and the groups.
    Cwd,
}

impl Found {
    fn word(self) -> &'static str {
        match self {
            Found::Tree => "tree",
            Found::Group => "group",
            Found::Cwd => "cwd",
        }
    }

    fn from_word(w: &str) -> Option<Found> {
        match w {
            "tree" => Some(Found::Tree),
            "group" => Some(Found::Group),
            "cwd" => Some(Found::Cwd),
            _ => None,
        }
    }
}

/// A process `find` listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub pid: u32,
    pub found: Found,
    /// `ps`'s start time, which tells it from a later process that took its pid.
    pub start: String,
    /// The command's name, as `ps` shows it.
    pub name: String,
}

/// A process `end` signalled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Killed {
    pub pid: u32,
    pub name: String,
    pub found: Found,
    /// It did not end on `TERM` and was killed.
    pub forced: bool,
    /// It was signalled. False for one that cannot be (another user's): it
    /// is still running.
    pub ended: bool,
}

/// The `sh` command that lists what `spec` names, a line for each process:
/// pid, how it was found, start time and name, separated by tabs.
pub fn find_command(spec: &Spec) -> String {
    let root = spec.root.map(|p| p.to_string()).unwrap_or_default();
    let cwd = spec.cwd.as_deref().unwrap_or("");
    format!(
        "root={root}; cwd={cwd}\n{FIND}",
        cwd = crate::herdr::shell_quote(cwd),
    )
}

const FIND: &str = r#"
table=$(ps -A -o pid= -o ppid= -o pgid= -o lstart= -o comm= 2>/dev/null) || exit 0
cwds=
if [ -n "$cwd" ] && [ -d /proc/self ]; then
  for d in /proc/[0-9]*; do
    l=$(readlink "$d/cwd" 2>/dev/null) || continue
    case "$l" in "$cwd"|"$cwd"/*) cwds="$cwds ${d#/proc/}";; esac
  done
fi
printf '%s\n' "$table" | awk -v root="$root" -v me="$$" -v cwds="$cwds" '
  { pid = $1; parent[pid] = $2; group[pid] = $3
    start[pid] = $4 " " $5 " " $6 " " $7 " " $8
    name = $9; for (i = 10; i <= NF; i++) name = name " " $i
    comm[pid] = name; known[pid] = 1 }
  END {
    # Never: this shell and its ancestors, the pane shell and its ancestors.
    for (p = me; p > 1 && p in known; p = parent[p]) keep[p] = 1
    if (root != "") for (p = root; p > 1 && p in known; p = parent[p]) keep[p] = 1
    if (root != "" && (root in known)) {
      tree[root] = 1
      do {
        changed = 0
        for (p in known) if (!(p in tree) && (parent[p] in tree)) { tree[p] = 1; changed = 1 }
      } while (changed)
      # The groups of what the shell started, not its own (the shell of a
      # pane leads its own, but not every shell does), and never one that
      # holds a process that must stay.
      for (p in tree) if (p != root) groups[group[p]] = 1
      for (p in keep) delete groups[group[p]]
    }
    for (p in known) {
      if (p <= 1 || (p in keep)) continue
      how = ""
      if (p in tree) how = "tree"
      else if (group[p] in groups) how = "group"
      if (how == "") for (i = split(cwds, c, " "); i > 0; i--) if (c[i] == p) how = "cwd"
      if (how != "") printf "%s\t%s\t%s\t%s\n", p, how, start[p], comm[p]
    }
  }'
"#;

/// Reads what `find_command` printed. A line that is not one of its own is
/// left out.
pub fn parse_candidates(stdout: &str) -> Vec<Candidate> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut f = line.splitn(4, '\t');
            let pid: u32 = f.next()?.parse().ok()?;
            let found = Found::from_word(f.next()?)?;
            let start = f.next()?.to_string();
            let name = f.next()?.to_string();
            (pid > 1).then_some(Candidate {
                pid,
                found,
                start,
                name,
            })
        })
        .collect()
}

/// The `sh` command that signals those of `candidates` that are still the
/// same process, `TERM` first and `KILL` after `grace`, and prints a line
/// for each that was running: `ended`, `forced` (needed `KILL`) or `failed`
/// (it could not be signalled), then pid, how it was found and its name,
/// separated by tabs.
pub fn end_command(candidates: &[Candidate], grace: Duration) -> String {
    let list: Vec<String> = candidates
        .iter()
        .map(|c| format!("{}\t{}\t{}\t{}", c.pid, c.found.word(), c.start, c.name))
        .collect();
    // A fifth of a second a round, so a process that goes at once costs
    // little and one that does not costs the whole grace.
    let rounds = grace.as_millis() / 200;
    format!(
        "list={list}; rounds={rounds}\n{END}",
        list = crate::herdr::shell_quote(&list.join("\n")),
    )
}

const END: &str = r#"
tab=$(printf '\t')
nl='
'
# Running, and the process that was listed: its start time is the same.
same() {
  s=$(ps -o stat= -p "$1" 2>/dev/null) || return 1
  case "$s" in ""|Z*) return 1;; esac
  now=$(ps -o lstart= -p "$1" 2>/dev/null | awk '{ $1 = $1; print }')
  [ "$now" = "$2" ]
}
live=
failed=
IFS=$nl
for line in $list; do
  IFS=$tab
  # shellcheck disable=SC2086
  set -- $line
  IFS=$nl
  [ "$1" -gt 1 ] 2>/dev/null || continue
  same "$1" "$3" || continue
  live="$live$line$nl"
  kill -TERM "$1" 2>/dev/null || failed="$failed $1 "
done
i=0
while [ "$i" -lt "$rounds" ]; do
  left=
  for line in $live; do
    IFS=$tab
    # shellcheck disable=SC2086
    set -- $line
    IFS=$nl
    case "$failed" in *" $1 "*) continue;; esac
    same "$1" "$3" && left=1
  done
  [ -n "$left" ] || break
  sleep 0.2
  i=$((i + 1))
done
for line in $live; do
  IFS=$tab
  # shellcheck disable=SC2086
  set -- $line
  IFS=$nl
  case "$failed" in
    *" $1 "*) how=failed;;
    *)
      if same "$1" "$3"; then
        if kill -KILL "$1" 2>/dev/null; then how=forced; else how=failed; fi
      else
        how=ended
      fi;;
  esac
  printf '%s\t%s\t%s\t%s\n' "$how" "$1" "$2" "$4"
done
"#;

/// Reads what `end_command` printed. A line that is not one of its own is
/// left out.
pub fn parse(stdout: &str) -> Vec<Killed> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut f = line.splitn(4, '\t');
            let (forced, ended) = match f.next()? {
                "forced" => (true, true),
                "ended" => (false, true),
                "failed" => (false, false),
                _ => return None,
            };
            let pid = f.next()?.parse().ok()?;
            let found = Found::from_word(f.next()?)?;
            Some(Killed {
                pid,
                name: f.next().unwrap_or("").to_string(),
                found,
                forced,
                ended,
            })
        })
        .collect()
}

async fn run_here(command: String) -> std::io::Result<String> {
    let out = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `find_command` on this machine, as an ssh machine runs it in its shell.
pub async fn find_here(spec: &Spec) -> std::io::Result<Vec<Candidate>> {
    Ok(parse_candidates(&run_here(find_command(spec)).await?))
}

/// `end_command` on this machine.
pub async fn end_here(candidates: &[Candidate], grace: Duration) -> std::io::Result<Vec<Killed>> {
    if candidates.is_empty() {
        return Ok(vec![]);
    }
    Ok(parse(&run_here(end_command(candidates, grace)).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(pid: u32, found: Found) -> Candidate {
        Candidate {
            pid,
            found,
            start: "Wed Oct 28 04:45:28 2026".into(),
            name: "a name".into(),
        }
    }

    #[test]
    fn the_answers_are_read_line_by_line() {
        let got = parse_candidates(
            "41\ttree\tWed Oct 28 04:45:28 2026\tcargo\nnoise\n1\ttree\tx\tinit\n42\tcwd\ts\trustc wrapper\n",
        );
        assert_eq!(
            got,
            vec![
                Candidate {
                    pid: 41,
                    found: Found::Tree,
                    start: "Wed Oct 28 04:45:28 2026".into(),
                    name: "cargo".into()
                },
                Candidate {
                    pid: 42,
                    found: Found::Cwd,
                    start: "s".into(),
                    name: "rustc wrapper".into()
                },
            ],
            "pid 1 is never listed"
        );
        let got = parse(
            "ended\t41\ttree\tcargo\nforced\t42\tcwd\trustc wrapper\nfailed\t43\tgroup\tsudo\nnoise\nended\tx\ttree\ty\n",
        );
        assert_eq!(got.len(), 3);
        assert!(!got[0].forced && got[0].ended);
        assert!(got[1].forced && got[1].ended);
        assert_eq!(got[2].found, Found::Group);
        assert!(!got[2].forced && !got[2].ended);
    }

    #[test]
    fn the_commands_quote_what_they_carry() {
        let spec = Spec {
            root: Some(7),
            cwd: Some("/work/it's here".into()),
        };
        let c = find_command(&spec);
        assert!(c.starts_with("root=7; cwd='/work/it'\\''s here'\n"), "{c}");
        let c = end_command(&[cand(9, Found::Tree)], GRACE);
        assert!(
            c.starts_with("list='9\ttree\tWed Oct 28 04:45:28 2026\ta name'; rounds=25\n"),
            "{c}"
        );
    }

    async fn spawn_pane(script: &str) -> tokio::process::Child {
        let pane = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // Let the shell start its children.
        tokio::time::sleep(Duration::from_millis(500)).await;
        pane
    }

    fn alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// The real script, on this machine: `find` lists a shell's child and
    /// grandchild and signals nothing, never the shell itself nor a
    /// bystander; `end` ends them, killing the one that ignores `TERM`.
    #[tokio::test]
    async fn find_lists_a_tree_and_end_ends_it() {
        let bystander = tokio::process::Command::new("sleep")
            .arg("300")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut pane = spawn_pane("sleep 300 & sh -c 'trap \"\" TERM; sleep 300' & wait").await;
        let root = pane.id().unwrap();
        let spec = Spec {
            root: Some(root),
            cwd: None,
        };
        let found = find_here(&spec).await.unwrap();
        assert!(found.len() >= 3, "{found:?}");
        assert!(found.iter().all(|c| c.found == Found::Tree), "{found:?}");
        assert!(found.iter().all(|c| c.pid != root));
        assert!(found.iter().all(|c| Some(c.pid) != bystander.id()));
        assert!(found.iter().all(|c| alive(c.pid)), "find signals nothing");

        let killed = end_here(&found, Duration::from_secs(1)).await.unwrap();
        assert_eq!(killed.len(), found.len(), "{killed:?}");
        assert!(killed.iter().all(|k| k.ended), "{killed:?}");
        assert!(killed.iter().any(|k| k.forced), "{killed:?}");
        assert!(killed.iter().any(|k| !k.forced), "{killed:?}");
        assert!(found.iter().all(|c| !alive(c.pid) || c.pid == root));
        assert!(alive(bystander.id().unwrap()));
        let _ = pane.kill().await;
    }

    /// What is not running any more, or is another process by now (the pid
    /// was handed out again), is left alone.
    #[tokio::test]
    async fn end_signals_only_the_process_it_listed() {
        let mut pane = spawn_pane("sleep 300 & wait").await;
        let spec = Spec {
            root: pane.id(),
            cwd: None,
        };
        let mut found = find_here(&spec).await.unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        let pid = found[0].pid;
        found[0].start = "Mon Jan  1 00:00:00 1990".into();
        assert!(
            end_here(&found, Duration::from_secs(1))
                .await
                .unwrap()
                .is_empty(),
            "another process by that pid"
        );
        assert!(alive(pid));
        found[0].start = find_here(&spec).await.unwrap()[0].start.clone();
        let _ = pane.kill().await;
        // The shell went; the child is its own now. Ended by its start time.
        let killed = end_here(&found, Duration::from_secs(1)).await.unwrap();
        assert_eq!(killed.len(), 1);
        assert!(!alive(pid));
    }

    /// A process the user cannot signal is reported as not ended, not as
    /// killed.
    #[tokio::test]
    async fn a_process_that_cannot_be_signalled_is_not_reported_ended() {
        let me = std::process::Command::new("id").arg("-u").output().unwrap();
        if String::from_utf8_lossy(&me.stdout).trim() == "0" {
            return;
        }
        // Another user's process: any root-owned one.
        let ps = std::process::Command::new("ps")
            .args(["-U", "root", "-o", "pid="])
            .output()
            .unwrap();
        let Some(pid) = String::from_utf8_lossy(&ps.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .find(|p| *p > 1)
        else {
            return;
        };
        let line = std::process::Command::new("ps")
            .args(["-o", "lstart=,comm=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&line.stdout).trim().to_string();
        let mut words = text.split_whitespace();
        let start = words.by_ref().take(5).collect::<Vec<_>>().join(" ");
        let name = words.collect::<Vec<_>>().join(" ");
        let c = Candidate {
            pid,
            found: Found::Tree,
            start,
            name,
        };
        let killed = end_here(&[c], Duration::from_secs(1)).await.unwrap();
        assert_eq!(killed.len(), 1, "{killed:?}");
        assert!(!killed[0].ended && !killed[0].forced, "{killed:?}");
        let ps = std::process::Command::new("ps")
            .args(["-p", &pid.to_string()])
            .output()
            .unwrap();
        assert!(ps.status.success(), "still running");
    }

    /// A member of a group whose leader is still under the shell, but which
    /// is no longer the shell's descendant (its parent exited): found by its
    /// group.
    #[tokio::test]
    async fn find_lists_an_orphan_by_its_group() {
        let mut pane = spawn_pane("setsid sh -c '(sleep 302 &); exec sleep 301' & wait").await;
        let spec = Spec {
            root: pane.id(),
            cwd: None,
        };
        let found = find_here(&spec).await.unwrap();
        assert!(found.iter().any(|c| c.found == Found::Group), "{found:?}");
        let killed = end_here(&found, Duration::from_secs(1)).await.unwrap();
        assert!(killed.iter().all(|k| k.ended), "{killed:?}");
        let _ = pane.kill().await;
    }

    /// A build that left the tree and the group (`setsid`, its parent gone)
    /// is found by where it works, when a directory is named.
    #[tokio::test]
    async fn find_lists_a_daemon_by_its_directory() {
        if !std::path::Path::new("/proc/self").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let start = |at: &std::path::Path| {
            std::process::Command::new("sh")
                .current_dir(at)
                .arg("-c")
                .arg("setsid sleep 303 >/dev/null 2>&1 &")
                .status()
                .unwrap();
        };
        start(dir.path());
        start(other.path());
        tokio::time::sleep(Duration::from_millis(500)).await;
        let spec = Spec {
            root: None,
            cwd: Some(dir.path().to_string_lossy().into_owned()),
        };
        let found = find_here(&spec).await.unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].found, Found::Cwd);
        let killed = end_here(&found, Duration::from_secs(1)).await.unwrap();
        assert_eq!(killed.len(), 1);
        // The one working elsewhere is left; end it.
        let again = Spec {
            root: None,
            cwd: Some(other.path().to_string_lossy().into_owned()),
        };
        let found = find_here(&again).await.unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        end_here(&found, Duration::from_secs(1)).await.unwrap();
    }
}
