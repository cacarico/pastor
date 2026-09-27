//! A head on another machine: the `[head]` setting in client.toml, the ssh
//! transport every request to it takes (`pastor bridge` on the far end), and
//! `pastor head set|show|unset`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use clap::Subcommand;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::cli::{CliError, request_failure};
use crate::config::Paths;
use crate::ipc::{IpcRequest, IpcResponse, RequestError};

/// The variable that names a head for one shell, over client.toml.
pub const HEAD_ENV: &str = "PASTOR_HEAD";

/// pastor on the head when the setting names no path: whatever a
/// non-interactive shell there finds on its PATH.
const DEFAULT_PASTOR: &str = "pastor";

/// How long `head set` and a command's first ping wait for the head, the ssh
/// handshake included. A local ping gets 2s; a new ssh connection alone can
/// take longer than that.
pub const PING_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a dead ssh gets to finish writing its stderr once it has exited.
/// A `ControlPersist` master forked from it may hold the pipe open for as
/// long as it lives, so the read is never waited out.
const STDERR_GRACE: Duration = Duration::from_millis(500);

/// The most of ssh's stderr kept for an error message.
const STDERR_LIMIT: usize = 4096;

/// `[head]` in client.toml.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadSetting {
    /// An ssh destination, as ssh takes it (`user@pi-1`, a Host alias).
    pub ssh: String,
    /// pastor's path on the head, when it is not on a non-interactive
    /// shell's PATH there. The remote shell expands it, so `~` works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pastor: Option<String>,
}

/// Where the head in use was named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadSource {
    Flag,
    Env,
    File,
}

/// The head a command talks to over ssh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHead {
    pub ssh: String,
    pub pastor: String,
    pub source: HeadSource,
    /// ssh's `ControlPath` for this head, under the state dir. Built by
    /// `Paths::ssh_control_path`, which escapes a `%` in the state dir so ssh
    /// does not expand it as one of its own tokens.
    control_path: PathBuf,
    /// The directory the control socket lives in, created private before ssh
    /// runs.
    control_dir: PathBuf,
}

pub fn client_file(paths: &Paths) -> PathBuf {
    paths.config_dir.join("client.toml")
}

/// The `[head]` client.toml holds, `None` without a file or a table.
pub fn load(path: &Path) -> anyhow::Result<Option<HeadSetting>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(CliError::err(
                "config_error",
                format!("{}: {e}", path.display()),
            ));
        }
    };
    #[derive(Deserialize)]
    struct File {
        head: Option<HeadSetting>,
    }
    let file: File = toml::from_str(&text)
        .map_err(|e| CliError::err("config_error", format!("{}: {e}", path.display())))?;
    Ok(file.head)
}

/// Writes `head` as client.toml's `[head]`, or removes the table for
/// `None`, leaving the rest of the file as it was.
pub fn save(path: &Path, head: Option<&HeadSetting>) -> anyhow::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| CliError::err("config_error", format!("{}: {e}", path.display())))?;
    match head {
        Some(h) => {
            let mut table = toml_edit::Table::new();
            table["ssh"] = toml_edit::value(&h.ssh);
            if let Some(p) = &h.pastor {
                table["pastor"] = toml_edit::value(p);
            }
            doc["head"] = toml_edit::Item::Table(table);
        }
        None => {
            doc.remove("head");
        }
    }
    if let Some(dir) = path.parent() {
        crate::config::create_private_dir(dir)?;
    }
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

/// The head a command uses: `--head`, else `PASTOR_HEAD`, else client.toml.
/// An empty flag or variable names none, so the next one down decides. A
/// flag or variable takes pastor's path from the file when the file names
/// the same destination.
pub fn resolve(
    paths: &Paths,
    flag: Option<&str>,
    env: Option<&str>,
    file: Option<HeadSetting>,
) -> Option<RemoteHead> {
    let named = |ssh: &str, source| {
        let pastor = file
            .as_ref()
            .filter(|f| f.ssh == ssh)
            .and_then(|f| f.pastor.clone());
        Some(RemoteHead::new(paths, ssh, pastor, source))
    };
    match (
        flag.filter(|s| !s.is_empty()),
        env.filter(|s| !s.is_empty()),
    ) {
        (Some(ssh), _) => named(ssh, HeadSource::Flag),
        (None, Some(ssh)) => named(ssh, HeadSource::Env),
        (None, None) => {
            let f = file?;
            Some(RemoteHead::new(paths, &f.ssh, f.pastor, HeadSource::File))
        }
    }
}

impl RemoteHead {
    pub fn new(paths: &Paths, ssh: &str, pastor: Option<String>, source: HeadSource) -> Self {
        RemoteHead {
            ssh: ssh.to_string(),
            pastor: pastor.unwrap_or_else(|| DEFAULT_PASTOR.to_string()),
            source,
            control_path: paths.ssh_control_path("head"),
            control_dir: paths.ssh_dir(),
        }
    }

    /// The ssh command line, after `ssh`. `--` keeps `self.ssh` from ever being
    /// read as an option (a `PASTOR_HEAD` or `client.toml` value starting with
    /// `-`), and the remote command is quoted for the login shell that parses
    /// it, the same way the machine transport does.
    pub fn ssh_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        for opt in [
            "BatchMode=yes".to_string(),
            "ControlMaster=auto".to_string(),
            "ControlPersist=60s".to_string(),
            format!("ControlPath={}", self.control_path.display()),
        ] {
            args.push("-o".to_string());
            args.push(opt);
        }
        args.push("--".to_string());
        args.push(self.ssh.clone());
        args.push(crate::herdr::transport::posix_command(&format!(
            "{} bridge",
            self.pastor
        )));
        args
    }

    /// One request, one reply, over ssh to `pastor bridge` on the head.
    /// ssh that exits with no reply is `RequestError::Unreachable` with its
    /// stderr; an error the bridge itself writes (`no_head`) is
    /// `RequestError::Refused` with the bridge's code.
    pub async fn request(
        &self,
        line: &str,
        timeout: Duration,
    ) -> Result<IpcResponse, RequestError> {
        // ssh creates the ControlPath socket there, and the dir must be private.
        crate::config::create_private_dir(&self.control_dir)
            .map_err(|e| RequestError::Unreachable(format!("{e:#}")))?;
        let mut child = tokio::process::Command::new("ssh")
            .args(self.ssh_args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| RequestError::Unreachable(format!("could not run ssh: {e}")))?;
        let mut stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let stderr = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.take(STDERR_LIMIT as u64).read_to_end(&mut buf).await;
            buf
        });
        let exchange = async {
            // A write that fails means ssh is gone; its exit says why.
            let _ = stdin.write_all(line.as_bytes()).await;
            let _ = stdin.flush().await;
            drop(stdin);
            let mut lines = BufReader::new(stdout).lines();
            let mut reply = None;
            while let Some(l) = lines.next_line().await? {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(l.trim())
                    && v.is_object()
                {
                    reply = Some(v);
                    break;
                }
            }
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((reply, status))
        };
        let (reply, status) = match tokio::time::timeout(timeout, exchange).await {
            Ok(Ok(done)) => done,
            Ok(Err(e)) => return Err(RequestError::Exchange(e.into())),
            Err(_) => return Err(RequestError::Timeout(timeout)),
        };
        if let Some(v) = reply {
            if v.get("kind").is_some() {
                return serde_json::from_value(v).map_err(|e| RequestError::Exchange(e.into()));
            }
            if let (Some(code), Some(message)) = (v["code"].as_str(), v["message"].as_str()) {
                return Err(RequestError::Refused {
                    code: code.to_string(),
                    message: message.to_string(),
                });
            }
            return Err(RequestError::Exchange(anyhow::anyhow!(
                "the head's bridge wrote an unexpected reply: {v}"
            )));
        }
        let stderr = tokio::time::timeout(STDERR_GRACE, stderr)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
        if status.success() {
            return Err(RequestError::Exchange(anyhow::anyhow!(
                "the head's bridge closed without a reply"
            )));
        }
        // A pastor from before `bridge` answers clap's usage error.
        if stderr.contains("unrecognized subcommand") {
            return Err(RequestError::Refused {
                code: "head_too_old".into(),
                message: format!(
                    "pastor on {} has no `bridge` command; upgrade it ({stderr})",
                    self.ssh
                ),
            });
        }
        let why = if stderr.is_empty() {
            format!("ssh exited with {status}")
        } else {
            stderr
        };
        Err(RequestError::Unreachable(format!(
            "ssh {} `{} bridge` failed: {why}",
            self.ssh, self.pastor
        )))
    }
}

/// What a failed request to a remote head tells the user.
pub fn failure(err: &RequestError) -> anyhow::Error {
    let (code, message) = request_failure(err);
    CliError::err(&code, message)
}

#[derive(Subcommand, Debug)]
pub enum HeadCmd {
    /// Use the head on another machine, reached over ssh; checked with one ping first
    Set {
        /// An ssh destination, as ssh takes it (user@host, or a Host alias)
        dest: String,
        /// pastor's path on the head, if it is not on the PATH of a non-interactive shell there
        #[arg(long, value_name = "PATH")]
        pastor: Option<String>,
        /// Save it even if the head does not answer
        #[arg(long)]
        force: bool,
    },
    /// Print the head this CLI uses
    Show {
        /// Print as a JSON object
        #[arg(long)]
        json: bool,
    },
    /// Use the head on this machine again
    Unset,
}

/// `pastor head ...`. `active` is the head the command line resolved, which
/// `show` reports.
pub async fn run(paths: &Paths, cmd: HeadCmd, active: Option<&RemoteHead>) -> anyhow::Result<()> {
    let file = client_file(paths);
    match cmd {
        HeadCmd::Set {
            dest,
            pastor,
            force,
        } => {
            let setting = HeadSetting { ssh: dest, pastor };
            let head = RemoteHead::new(
                paths,
                &setting.ssh,
                setting.pastor.clone(),
                HeadSource::File,
            );
            match check(&head).await {
                Ok(version) => {
                    save(&file, Some(&setting))?;
                    println!("head: {} (remote, pastor {version})", setting.ssh);
                }
                Err(err) if force => {
                    save(&file, Some(&setting))?;
                    let why = err
                        .downcast_ref::<CliError>()
                        .map_or_else(|| format!("{err:#}"), |e| e.message.clone());
                    eprintln!("saved anyway (--force); the head did not answer: {why}");
                    println!("head: {} (remote)", setting.ssh);
                }
                Err(err) => return Err(err),
            }
        }
        HeadCmd::Show { json } => {
            if json {
                let v = match active {
                    Some(h) => serde_json::json!({
                        "remote": true,
                        "ssh": h.ssh,
                        "pastor": h.pastor,
                        "from": h.source,
                    }),
                    None => serde_json::json!({
                        "remote": false,
                        "ssh": null,
                        "pastor": null,
                        "from": null,
                    }),
                };
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                match active {
                    Some(h) => println!("head: {} (remote)", h.ssh),
                    None => println!("head: this machine"),
                }
            }
        }
        HeadCmd::Unset => {
            save(&file, None)?;
            println!("head: this machine");
        }
    }
    Ok(())
}

/// One ping through the bridge: the head's version, or why not.
async fn check(head: &RemoteHead) -> anyhow::Result<String> {
    let line = crate::ipc::request_line(&IpcRequest::Ping, None)?;
    match head.request(&line, PING_TIMEOUT).await {
        Ok(IpcResponse::Pong { version, protocol }) if protocol < crate::ipc::IPC_PROTOCOL => {
            Err(CliError::err(
                "head_too_old",
                format!(
                    "pastor serve on {} ({version}) speaks protocol {protocol}, older than this CLI's {}; upgrade it, or pass --force",
                    head.ssh,
                    crate::ipc::IPC_PROTOCOL
                ),
            ))
        }
        Ok(IpcResponse::Pong { version, .. }) => Ok(version),
        Ok(other) => Err(CliError::err(
            "runtime_error",
            format!("the head answered ping with {other:?}"),
        )),
        Err(e) => Err(failure(&e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        (tmp, p)
    }

    #[test]
    fn the_flag_beats_the_variable_beats_the_file() {
        let (_tmp, p) = paths();
        let file = HeadSetting {
            ssh: "user@pi-1".into(),
            pastor: Some("~/.local/bin/pastor".into()),
        };
        let h = resolve(&p, None, None, Some(file.clone())).unwrap();
        assert_eq!(
            (h.ssh.as_str(), h.pastor.as_str(), h.source),
            ("user@pi-1", "~/.local/bin/pastor", HeadSource::File)
        );
        let h = resolve(&p, None, Some("pi-2"), Some(file.clone())).unwrap();
        assert_eq!(
            (h.ssh.as_str(), h.pastor.as_str(), h.source),
            ("pi-2", "pastor", HeadSource::Env)
        );
        let h = resolve(&p, Some("user@pi-1"), Some("pi-2"), Some(file.clone())).unwrap();
        assert_eq!(
            (h.ssh.as_str(), h.pastor.as_str(), h.source),
            ("user@pi-1", "~/.local/bin/pastor", HeadSource::Flag),
            "the file's path goes with its own destination"
        );
        let h = resolve(&p, Some(""), Some(""), Some(file)).unwrap();
        assert_eq!(h.source, HeadSource::File, "empty names none");
        assert!(resolve(&p, None, None, None).is_none());
    }

    #[test]
    fn save_writes_and_removes_only_the_head_table() {
        let (_tmp, p) = paths();
        let path = client_file(&p);
        assert_eq!(load(&path).unwrap(), None);
        std::fs::create_dir_all(&p.config_dir).unwrap();
        std::fs::write(&path, "# mine\n[other]\nx = 1\n").unwrap();
        let h = HeadSetting {
            ssh: "pi-1".into(),
            pastor: None,
        };
        save(&path, Some(&h)).unwrap();
        assert_eq!(load(&path).unwrap(), Some(h));
        save(&path, None).unwrap();
        assert_eq!(load(&path).unwrap(), None);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# mine") && text.contains("[other]"),
            "{text}"
        );
    }

    #[test]
    fn ssh_multiplexes_under_the_state_dir_and_never_prompts() {
        let (_tmp, p) = paths();
        let h = RemoteHead::new(&p, "user@pi-1", None, HeadSource::File);
        let args = h.ssh_args();
        let control = format!("ControlPath={}", p.ssh_control_path("head").display());
        for opt in [
            "BatchMode=yes",
            "ControlMaster=auto",
            "ControlPersist=60s",
            control.as_str(),
        ] {
            assert!(
                args.windows(2).any(|w| w[0] == "-o" && w[1] == opt),
                "{opt}: {args:?}"
            );
        }
        assert_eq!(
            &args[args.len() - 3..],
            ["--", "user@pi-1", "sh -c 'pastor bridge'"]
        );
    }

    #[test]
    fn ssh_args_never_reads_a_dash_prefixed_destination_as_an_option() {
        let (_tmp, p) = paths();
        let h = RemoteHead::new(&p, "-oProxyCommand=evil", None, HeadSource::File);
        let args = h.ssh_args();
        let dash_pos = args.iter().position(|a| a == "--").expect("has --");
        assert_eq!(args[dash_pos + 1], "-oProxyCommand=evil");
    }
}
