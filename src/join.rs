//! Join tokens on the head: `pastor join create|list|revoke`, the
//! `bridge --join <id>` way in for a machine that redeems one, and the
//! sweep of expired ones.
//!
//! A token is a per-token ed25519 key pair from `ssh-keygen`. Its public
//! half goes in the head's `authorized_keys`, locked to `bridge --join
//! <id>` and to the token's expiry; its private half goes only into the
//! token, so the head keeps no copy. sshd itself checks the token, and once
//! the line is gone (used, revoked or expired) the key gets no ssh session
//! at all. The store keeps the token's id and what it grants
//! (`store::JoinToken`), never the token.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use clap::Subcommand;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::authkeys::{self, Marker};
use crate::cli::CliError;
use crate::ipc::{IpcRequest, IpcResponse};
use crate::machine::PastorEvent;
use crate::setup::Runner;
use crate::store::{JoinToken, JoinTokenUse, Store};

/// What every token starts with, so a pasted one is recognisable.
pub const PREFIX: &str = "pjt_";

/// The code of a token, or a join id, that is not one pastor made, or no
/// longer knows.
pub const INVALID: &str = "join_token_invalid";

/// The code of a token that was used already.
pub const USED: &str = "join_token_used";

/// The code of a token past its expiry.
pub const EXPIRED: &str = "join_token_expired";

/// The code of a join under a name flock.toml has, with a token not made
/// to replace it.
pub const NAME_TAKEN: &str = "machine_name_taken";

/// The code of a join that spent its token and then could not finish.
pub const FAILED: &str = "join_failed";

/// What a joining machine needs to reach the head: where it is, the host key
/// to pin, and the private key whose line `authorized_keys` holds.
#[derive(Clone, PartialEq, Eq)]
pub struct Token {
    /// The head's ssh destination (`user@host`).
    pub head: String,
    /// The head's host key, as `ssh-keyscan` prints it: `host type key`.
    pub head_host_key: String,
    /// The OpenSSH private key text.
    pub key: String,
}

/// Debug output (logs, panics) never carries the private key.
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("head", &self.head)
            .field("head_host_key", &self.head_host_key)
            .field("key", &"<redacted>")
            .finish()
    }
}

fn invalid(why: &str) -> anyhow::Error {
    CliError::err(INVALID, format!("not a pastor join token: {why}"))
}

impl Token {
    /// `pjt_` + base64url, no padding, of `head\nhost_key\nkey`: one word to
    /// paste.
    pub fn encode(&self) -> String {
        let plain = format!("{}\n{}\n{}", self.head, self.head_host_key, self.key);
        format!("{PREFIX}{}", base64url_encode(plain.as_bytes()))
    }

    pub fn decode(s: &str) -> anyhow::Result<Token> {
        let body = s
            .trim()
            .strip_prefix(PREFIX)
            .ok_or_else(|| invalid("it does not start with pjt_"))?;
        let bytes = base64url_decode(body).ok_or_else(|| invalid("it is not base64url"))?;
        let text = String::from_utf8(bytes).map_err(|_| invalid("it is not text"))?;
        let mut parts = text.splitn(3, '\n');
        let (Some(head), Some(host_key), Some(key)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid("it is not a head, a host key and a key"));
        };
        if head.is_empty() || host_key.trim().is_empty() || key.trim().is_empty() {
            return Err(invalid("a part of it is empty"));
        }
        // The head goes on ssh's command line: one word, never an option.
        if head.starts_with('-') || head.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(invalid("its head is not an ssh destination"));
        }
        Ok(Token {
            head: head.into(),
            head_host_key: host_key.into(),
            key: key.into(),
        })
    }
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// RFC 4648 base64url with no padding; a few lines spare a crate.
fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..=chunk.len() {
            out.push(B64URL[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// The bytes of `s`, or `None` for anything `base64url_encode` cannot give:
/// another alphabet, padding, or a length no input has.
fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 2);
    for chunk in s.as_bytes().chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            let v = B64URL.iter().position(|a| a == c)? as u32;
            n |= v << (18 - 6 * i);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..chunk.len() - 1]);
    }
    Some(out)
}

/// A new join id: 16 lowercase hex chars from 8 bytes of `/dev/urandom`.
pub fn new_id() -> anyhow::Result<String> {
    use std::io::Read;
    let mut b = [0u8; 8];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .context("read /dev/urandom")?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Whether `s` is an id `new_id` could give, and so one that may go in a
/// marker and a forced command.
pub fn is_join_id(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

fn unknown(id: &str) -> anyhow::Error {
    CliError::err(INVALID, format!("no join token {id}"))
}

/// The files a token touches on the head.
#[derive(Debug, Clone)]
pub struct HeadFiles {
    /// The head's `authorized_keys` (`authkeys::default_file`).
    pub authorized_keys: PathBuf,
    /// sshd's ed25519 host key, public half.
    pub host_key: PathBuf,
    /// The pastor the forced commands run.
    pub pastor: PathBuf,
    /// Where `ssh-keygen` writes the pair before it is read and deleted.
    pub work: PathBuf,
}

/// `PASTOR_SSH_HOST_KEY` stands in for sshd's host key in tests.
pub const HOST_KEY_ENV: &str = "PASTOR_SSH_HOST_KEY";

impl HeadFiles {
    pub fn for_head(paths: &crate::config::Paths) -> anyhow::Result<HeadFiles> {
        Ok(HeadFiles {
            authorized_keys: authkeys::default_file(),
            host_key: std::env::var_os(HOST_KEY_ENV)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/etc/ssh/ssh_host_ed25519_key.pub")),
            pastor: std::env::current_exe().context("locate the pastor binary")?,
            work: paths.ssh_dir().join("join"),
        })
    }
}

/// What `join create` fixes in the token: the joining machine cannot
/// choose any of it.
#[derive(Debug, Clone)]
pub struct Ask {
    pub flock: Option<String>,
    pub tags: Vec<String>,
    /// The one name already in `flock.toml` this token may take over.
    pub replace: Option<String>,
    pub ttl: Duration,
}

/// A token just made, with the stored row.
#[derive(Debug, Clone)]
pub struct Created {
    pub token: Token,
    pub row: JoinToken,
}

/// The host part of ssh destination `dest` as known_hosts names it: `host`,
/// or `[host]:port` for an `ssh://` destination off port 22.
pub(crate) fn known_hosts_name(dest: &str) -> String {
    let (url, rest) = match dest.strip_prefix("ssh://") {
        Some(rest) => (true, rest),
        None => (false, dest),
    };
    let host = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    if url && let Some((h, port)) = host.rsplit_once(':') {
        let h = h.trim_start_matches('[').trim_end_matches(']');
        return if port == "22" {
            h.to_string()
        } else {
            format!("[{h}]:{port}")
        };
    }
    host.to_string()
}

/// The head's host key as `ssh-keyscan` prints it for `head`: the name
/// ssh looks it up by, the key type and the key.
fn keyscan_line(head: &str, file: &Path) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(file).map_err(|e| {
        CliError::err(
            "config_error",
            format!(
                "read the head's ssh host key {}: {e}; a joining machine pins it",
                file.display()
            ),
        )
    })?;
    let mut parts = text.split_whitespace();
    let (Some(kind), Some(key)) = (parts.next(), parts.next()) else {
        return Err(CliError::err(
            "config_error",
            format!("{} is not an ssh public key", file.display()),
        ));
    };
    Ok(format!("{} {kind} {key}", known_hosts_name(head)))
}

/// The head's ssh destination: `head_address`, else `user@hostname`.
pub fn head_destination(config: &crate::config::PastorConfig) -> String {
    config.head_address.clone().unwrap_or_else(|| {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "pastor".into());
        format!("{user}@{}", crate::config::hostname())
    })
}

/// `join create` with its seams: `keygen` runs `ssh-keygen`, `files` say
/// where everything is, `head` is the destination that goes in the token.
pub fn create_with(
    keygen: &dyn Runner,
    files: &HeadFiles,
    store: &Store,
    head: &str,
    ask: &Ask,
    now: DateTime<Utc>,
) -> anyhow::Result<Created> {
    if let Some(name) = &ask.replace
        && !authkeys::valid_name(name)
    {
        return Err(CliError::err(
            "invalid_machine",
            format!("{name:?} cannot be a machine name"),
        ));
    }
    let expires = now
        + chrono::Duration::from_std(ask.ttl)
            .map_err(|_| CliError::err("invalid_args", "--ttl is too long"))?;
    // Before anything is written: a token without it is useless.
    let head_host_key = keyscan_line(head, &files.host_key)?;
    let id = new_id()?;
    crate::config::create_private_dir(&files.work)?;
    let tmp = files.work.join(format!("join-{id}"));
    let tmp_pub = files.work.join(format!("join-{id}.pub"));
    let made = (|| -> anyhow::Result<(String, String)> {
        let out = keygen
            .run(
                "ssh-keygen",
                &[
                    "-q",
                    "-t",
                    "ed25519",
                    "-N",
                    "",
                    "-C",
                    &Marker::Join(id.clone()).render(),
                    "-f",
                    &tmp.to_string_lossy(),
                ],
            )
            .context("run ssh-keygen")?;
        if !out.success {
            anyhow::bail!("ssh-keygen failed: {}", out.stderr.trim());
        }
        let key = std::fs::read_to_string(&tmp).context("read the new private key")?;
        let public = std::fs::read_to_string(&tmp_pub).context("read the new public key")?;
        Ok((key, public))
    })();
    // The private half lives on only in the token.
    for f in [&tmp, &tmp_pub] {
        if let Err(err) = std::fs::remove_file(f)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(file = %f.display(), %err, "delete a join token's key file");
        }
    }
    let (key, public) = made?;
    let marker = Marker::Join(id.clone());
    let row = JoinToken {
        id: id.clone(),
        flock: ask.flock.clone(),
        tags: ask.tags.clone(),
        replace: ask.replace.clone(),
        expires_at: expires,
        used_at: None,
    };
    // sshd refuses the key after `expires` by itself; the sweep only tidies
    // the line away.
    authkeys::upsert_until(
        &files.authorized_keys,
        &marker,
        &files.pastor,
        &format!("bridge --join {id}"),
        &public,
        Some(expires),
    )?;
    if let Err(err) = store.put_join_token(&row) {
        // A line with no row would let the key in for nothing.
        let _ = authkeys::remove(&files.authorized_keys, &marker);
        return Err(err);
    }
    Ok(Created {
        token: Token {
            head: head.into(),
            head_host_key,
            key,
        },
        row,
    })
}

/// `join revoke`: the line and the row go. Unknown is `join_token_invalid`.
pub fn revoke(authorized_keys: &Path, store: &Store, id: &str) -> anyhow::Result<()> {
    if !is_join_id(id) || store.join_token(id)?.is_none() {
        return Err(unknown(id));
    }
    authkeys::remove(authorized_keys, &Marker::Join(id.into()))?;
    store.drop_join_token(id)?;
    Ok(())
}

/// Mark token `id` used at `now` and return what it grants: unknown is
/// `join_token_invalid`, used `join_token_used`, expired
/// `join_token_expired`. Of two joins with one token only one gets the row
/// (`Store::use_join_token`).
pub fn spend(store: &Store, id: &str, now: DateTime<Utc>) -> anyhow::Result<JoinToken> {
    if !is_join_id(id) {
        return Err(unknown(id));
    }
    match store.use_join_token(id, now)? {
        JoinTokenUse::Ok => store.join_token(id)?.ok_or_else(|| unknown(id)),
        JoinTokenUse::Used => Err(CliError::err(
            USED,
            format!("join token {id} was already used"),
        )),
        JoinTokenUse::Expired => Err(CliError::err(EXPIRED, format!("join token {id} expired"))),
        JoinTokenUse::NotFound => Err(unknown(id)),
    }
}

/// How long `join list` keeps showing a used token.
const KEEP_USED: chrono::TimeDelta = chrono::TimeDelta::days(7);

/// Drop the lines and rows of the tokens expired by `now`, announcing each
/// as `machine.join_token_expired`. A token whose line cannot be removed
/// stays, so the next pass tries it again. A used token's line goes too if
/// a join left it behind, and its row once it is a week old. Returns how
/// many expired.
pub fn sweep(
    store: &Store,
    authorized_keys: &Path,
    events: &tokio::sync::broadcast::Sender<PastorEvent>,
    now: DateTime<Utc>,
) -> usize {
    let ids = match store.expired_join_tokens(now) {
        Ok(ids) => ids,
        Err(err) => {
            tracing::error!(%err, "list expired join tokens; trying again next pass");
            return 0;
        }
    };
    let mut swept = 0;
    for id in ids {
        let gone = authkeys::remove(authorized_keys, &Marker::Join(id.clone()))
            .and_then(|_| store.drop_join_token(&id));
        if let Err(err) = gone {
            tracing::error!(id, %err, "sweep an expired join token; trying again next pass");
            continue;
        }
        tracing::info!(id, "join token expired");
        let _ = events.send(PastorEvent {
            kind: "machine.join_token_expired".into(),
            task_id: None,
            machine: None,
            job: None,
            detail: Some(serde_json::json!({ "id": id })),
            summary: None,
        });
        swept += 1;
    }
    sweep_used(store, authorized_keys, now);
    swept
}

/// The used half of `sweep`.
fn sweep_used(store: &Store, authorized_keys: &Path, now: DateTime<Utc>) {
    let rows = match store.join_tokens() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(%err, "list join tokens; trying again next pass");
            return;
        }
    };
    for row in rows {
        let Some(used_at) = row.used_at else {
            continue;
        };
        match authkeys::remove(authorized_keys, &Marker::Join(row.id.clone())) {
            Ok(true) => tracing::warn!(id = row.id, "removed a used join token's line"),
            Ok(false) => {}
            Err(err) => {
                tracing::error!(id = row.id, %err, "remove a used join token's line; trying again next pass");
                continue;
            }
        }
        if used_at <= now - KEEP_USED
            && let Err(err) = store.drop_join_token(&row.id)
        {
            tracing::error!(id = row.id, %err, "forget a used join token; trying again next pass");
        }
    }
}

/// The longest join request line, newline excluded: two keys, a host key
/// and a name fit many times over.
pub const MAX_JOIN_LINE: usize = 64 * 1024;

/// `bridge --join <id>`: one request line, which must be a `Join`; its `id`
/// is replaced with the one the forced command names. Anything else gets
/// `join_token_invalid`, and no later line is read.
pub async fn bridge(
    socket: &Path,
    id: &str,
    mut input: impl AsyncBufRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut line = Vec::new();
    loop {
        line.clear();
        let mut limited = (&mut input).take(MAX_JOIN_LINE as u64 + 1);
        if limited.read_until(b'\n', &mut line).await? == 0 {
            return Ok(());
        }
        if line.len() - usize::from(line.last() == Some(&b'\n')) > MAX_JOIN_LINE {
            let reply = reply_line(&IpcResponse::error(
                "request_too_large",
                format!("a join request is at most {MAX_JOIN_LINE} bytes"),
            ))?;
            output.write_all(&reply).await?;
            output.flush().await?;
            return Ok(());
        }
        if !line.iter().all(u8::is_ascii_whitespace) {
            break;
        }
    }
    let req = std::str::from_utf8(&line)
        .ok()
        .and_then(|t| crate::ipc::parse_request_line(t.trim()).ok());
    let reply = match req {
        Some((
            IpcRequest::Join {
                name,
                machine_key,
                agent_key,
                host_key,
                ..
            },
            _,
        )) if is_join_id(id) => {
            let req = IpcRequest::Join {
                id: id.into(),
                name,
                machine_key,
                agent_key,
                host_key,
            };
            let line = crate::ipc::request_line(&req, None)?;
            match crate::ipc::relay_line(socket, line.as_bytes()).await {
                Ok(reply) => reply,
                Err(crate::ipc::RequestError::Connect(e))
                    if crate::ipc::connect_error_means_no_daemon(&e) =>
                {
                    reply_line(&IpcResponse::error(
                        "no_head",
                        format!("no pastor serve is running on this machine ({e})"),
                    ))?
                }
                Err(err) => {
                    let e = crate::cli::request_error(&err);
                    reply_line(&IpcResponse::error(&e.code, e.message))?
                }
            }
        }
        _ => reply_line(&IpcResponse::error(
            INVALID,
            "a join token's key may send one join request and nothing else",
        ))?,
    };
    output.write_all(&reply).await?;
    output.flush().await?;
    Ok(())
}

fn reply_line(resp: &IpcResponse) -> anyhow::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(resp)?;
    line.push(b'\n');
    Ok(line)
}

/// `pastor join`: a subcommand for the head's tokens, or, with none, this
/// machine joining the head a token names.
#[derive(clap::Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct JoinCli {
    #[command(subcommand)]
    pub cmd: Option<JoinCmd>,
    #[command(flatten)]
    pub join: JoinArgs,
}

/// What joining takes. The token is never an argument, which `ps` shows: it
/// comes from stdin, the environment or the terminal.
#[derive(clap::Args, Debug, Default)]
pub struct JoinArgs {
    /// This machine's name in the head's flock.toml (the hostname when unset)
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Read the token from stdin; else from PASTOR_JOIN_TOKEN, else it is asked for on the terminal
    #[arg(long)]
    pub token_stdin: bool,
    /// Join and name the serve only, for a machine set up beforehand: no herdr is fetched and no service installed
    #[arg(long)]
    pub skip_bootstrap: bool,
    /// Print as a JSON object: the bootstrap report
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum JoinCmd {
    /// Make a one-time token a new machine joins the fleet with, as a pull machine; prints the token
    Create {
        /// The flock the machine joins (default: the default flock)
        #[arg(long)]
        flock: Option<String>,
        /// A tag the machine gets; repeat for more
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// The one machine name already in flock.toml the token may take over, and the only name it joins as
        #[arg(long)]
        replace: Option<String>,
        /// How long the token works, like 30m or 1h
        #[arg(long, default_value = "1h", value_parser = crate::config::parse_duration)]
        ttl: Duration,
        /// Print as a JSON object: the id, the token and its expiry
        #[arg(long)]
        json: bool,
    },
    /// The join tokens the head knows: id, flock, tags, what it replaces, expiry, when used
    List {
        /// Print as a JSON array
        #[arg(long)]
        json: bool,
    },
    /// Remove a join token's key from authorized_keys and forget it
    Revoke {
        /// The token's id, as `join list` shows it
        id: String,
    },
}

/// Whether `cmd` changes the fleet: all but a list.
pub fn changes_fleet(cmd: &JoinCmd) -> bool {
    !matches!(cmd, JoinCmd::List { .. })
}

/// `pastor join create|list|revoke`, through the head: the tokens are
/// redeemed there, against its store and its authorized_keys.
pub async fn run(client: &crate::ipc::Client, cmd: JoinCmd) -> anyhow::Result<()> {
    let req = match &cmd {
        JoinCmd::Create {
            flock,
            tags,
            replace,
            ttl,
            ..
        } => IpcRequest::JoinCreate {
            flock: flock.clone(),
            tags: tags.clone(),
            replace: replace.clone(),
            ttl_secs: ttl.as_secs().max(1),
        },
        JoinCmd::List { .. } => IpcRequest::JoinList,
        JoinCmd::Revoke { id } => IpcRequest::JoinRevoke { id: id.clone() },
    };
    let reply = match crate::ipc::request_head(client, &req).await {
        Ok(r) => r,
        Err(crate::ipc::RequestError::Connect(e))
            if crate::ipc::connect_error_means_no_daemon(&e) =>
        {
            return Err(CliError::err(
                "daemon_not_running",
                "join tokens live on the head: start it with `pastor serve`",
            ));
        }
        Err(err) => return Err(crate::cli::request_error(&err).into()),
    };
    match (cmd, reply) {
        (_, IpcResponse::Error { code, message }) => Err(CliError::err(&code, message)),
        (
            JoinCmd::Create { json, .. },
            IpcResponse::JoinCreated {
                id,
                token,
                expires_at,
            },
        ) => {
            let expires_at = expires_at.to_rfc3339();
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "id": id, "token": token, "expires_at": expires_at })
                );
            } else {
                println!("{token}");
                eprintln!(
                    "join token {id}: good for one machine until {expires_at}; `pastor join revoke {id}` takes it back"
                );
            }
            Ok(())
        }
        (JoinCmd::List { json }, IpcResponse::JoinTokens(rows)) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if rows.is_empty() {
                println!("no join tokens");
            } else {
                let dash = |s: Option<String>| s.unwrap_or_else(|| "-".into());
                let rows: Vec<Vec<String>> = rows
                    .into_iter()
                    .map(|t| {
                        vec![
                            t.id,
                            dash(t.flock),
                            if t.tags.is_empty() {
                                "-".into()
                            } else {
                                t.tags.join(",")
                            },
                            dash(t.replace),
                            t.expires_at.to_rfc3339(),
                            dash(t.used_at.map(|d| d.to_rfc3339())),
                        ]
                    })
                    .collect();
                print!(
                    "{}",
                    crate::cli::table(
                        &["ID", "FLOCK", "TAGS", "REPLACE", "EXPIRES", "USED"],
                        &rows
                    )
                );
            }
            Ok(())
        }
        (JoinCmd::Revoke { .. }, IpcResponse::Text(t)) => {
            println!("{t}");
            Ok(())
        }
        (_, other) => anyhow::bail!("unexpected reply to a join request: {other:?}"),
    }
}

#[cfg(test)]
pub(crate) mod tests;
