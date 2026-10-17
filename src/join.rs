//! Join tokens on the head: `pastor join create|list|revoke`, the
//! `bridge --join <id>` way in for a machine that redeems one, and the
//! sweep of expired ones.
//!
//! A token is a per-token ed25519 key pair from `ssh-keygen`. Its public
//! half goes in the head's `authorized_keys`, locked to `bridge --join
//! <id>`; its private half goes only into the token, so the head keeps no
//! copy. sshd itself checks the token, and once the line is gone (used,
//! revoked or expired) the key gets no ssh session at all.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Utc};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::authkeys::{self, Marker};
use crate::cli::CliError;
use crate::ipc::{IpcRequest, IpcResponse};
use crate::machine::PastorEvent;
use crate::setup::Runner;
use crate::store::{JoinToken, Store};

/// What every token starts with, so a pasted one is recognisable.
pub const PREFIX: &str = "pjt_";

/// The code of a token, or a join id, that is not one pastor made, or no
/// longer knows.
pub const INVALID: &str = "join_token_invalid";

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

/// The code of a joining machine's name that cannot be one.
pub const NAME_INVALID: &str = "machine_name_invalid";

/// Fail unless `name` is `^[A-Za-z0-9][A-Za-z0-9._-]*$`, the charset the
/// artifact cache takes: it goes into a forced command in authorized_keys,
/// which sshd runs through a shell, and onto ssh and herdr command lines,
/// so `;`, `$(...)`, quotes, spaces and a leading `-` or `.` never get
/// that far. The head checks it before the token is spent.
pub fn check_name(name: &str) -> anyhow::Result<()> {
    let mut chars = name.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if !ok {
        return Err(CliError::err(
            NAME_INVALID,
            format!(
                "{name:?} cannot be a machine name: use letters, digits, '.', '_' and '-', starting with a letter or digit"
            ),
        ));
    }
    Ok(())
}

/// Whether `s` is an id `new_id` could give, and so one that may go in a
/// marker and a forced command.
pub fn is_join_id(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// Times in `join_tokens` are compared as text, so they are all written
/// the same way.
fn stamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, false)
}

/// The key types a joining machine's keys may be: what sshd takes for a
/// login. DSA and anything unknown are refused.
pub(crate) const KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "rsa-sha2-256",
    "rsa-sha2-512",
    "ssh-rsa",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

/// The base64 word of public key line `key`, if its type is one of
/// `KEY_TYPES`; `what` names the key in the error.
pub fn key_blob<'a>(what: &str, key: &'a str) -> anyhow::Result<&'a str> {
    let mut parts = key.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(kind), Some(blob)) if KEY_TYPES.contains(&kind) => Ok(blob),
        (Some(kind), Some(_)) => Err(CliError::err(
            "invalid_key",
            format!(
                "{what} is of type {kind:?}; pastor takes {}",
                KEY_TYPES.join(", ")
            ),
        )),
        _ => Err(CliError::err(
            "invalid_key",
            format!("{what} is not a public key line, like ssh-ed25519 AAAA..."),
        )),
    }
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

/// The head's authorized_keys as a join edits it. A seam, so a test can
/// fail one write and see what the join left behind.
pub trait Keys: Send + Sync {
    fn upsert(&self, marker: &Marker, line: &str) -> anyhow::Result<()>;
    fn remove(&self, marker: &Marker) -> anyhow::Result<bool>;
    /// Whether a line not marked with one of `except` holds the key `blob`.
    fn holds(&self, blob: &str, except: &[Marker]) -> anyhow::Result<bool>;
}

impl Keys for PathBuf {
    fn upsert(&self, marker: &Marker, line: &str) -> anyhow::Result<()> {
        authkeys::upsert(self, marker, line)
    }
    fn remove(&self, marker: &Marker) -> anyhow::Result<bool> {
        authkeys::remove(self, marker)
    }
    fn holds(&self, blob: &str, except: &[Marker]) -> anyhow::Result<bool> {
        authkeys::holds_key(self, blob, except)
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

/// A token just made, with its id and the stored row.
#[derive(Debug, Clone)]
pub struct Created {
    pub id: String,
    pub token: Token,
    pub row: JoinToken,
}

/// The head's host key as `ssh-keyscan` prints it for `head`: the host
/// part of the destination (`[host]:port` for a `ssh://` destination off
/// port 22, which is how ssh looks it up), the key type and the key.
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
    let (host, port) = crate::ssh::destination_host_port(head);
    let name = crate::ssh::known_hosts_name(host, port);
    Ok(format!("{name} {kind} {key}"))
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

/// `create` with its seams: `keygen` runs `ssh-keygen`, `files` say where
/// everything is, `head` is the destination that goes in the token.
pub fn create_with(
    keygen: &dyn Runner,
    files: &HeadFiles,
    store: &Store,
    head: &str,
    ask: &Ask,
    now: DateTime<Utc>,
) -> anyhow::Result<Created> {
    // Before anything is written: a token without it is useless.
    let head_host_key = keyscan_line(head, &files.host_key)?;
    let id = new_id()?;
    crate::config::create_private_dir(&files.work)?;
    let tmp = files.work.join(format!("join-{id}"));
    let tmp_pub = files.work.join(format!("join-{id}.pub"));
    let made = (|| -> anyhow::Result<(String, String)> {
        let comment = format!("pastor:join:{id}");
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
                    &comment,
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
    let expires = now
        + chrono::Duration::from_std(ask.ttl)
            .map_err(|_| CliError::err("invalid_args", "--ttl is too long"))?;
    // sshd refuses the key after `expires` by itself; the sweep only tidies
    // the line away.
    let line = authkeys::line_until(
        &files.pastor,
        &format!("bridge --join {id}"),
        &public,
        &marker,
        Some(expires),
    )?;
    let row = JoinToken {
        id: id.clone(),
        flock: ask.flock.clone(),
        tags: ask.tags.clone(),
        replace: ask.replace.clone(),
        expires_at: stamp(expires),
        used_at: None,
    };
    authkeys::upsert(&files.authorized_keys, &marker, &line)?;
    if let Err(err) = store.put_join_token(&row) {
        // A line with no row would let the key in for nothing, forever.
        let _ = authkeys::remove(&files.authorized_keys, &marker);
        return Err(err);
    }
    Ok(Created {
        id,
        token: Token {
            head: head.into(),
            head_host_key,
            key,
        },
        row,
    })
}

/// `join create`: a token for one machine to join with.
pub fn create(files: &HeadFiles, store: &Store, head: &str, ask: &Ask) -> anyhow::Result<Created> {
    create_with(
        &crate::setup::SystemRunner,
        files,
        store,
        head,
        ask,
        Utc::now(),
    )
}

/// `join revoke`: the line and the row go. Unknown is `join_token_invalid`.
pub fn revoke(files: &HeadFiles, store: &Store, id: &str) -> anyhow::Result<()> {
    if !is_join_id(id) || store.join_token(id)?.is_none() {
        return Err(CliError::err(INVALID, format!("no join token {id}")));
    }
    authkeys::remove(&files.authorized_keys, &Marker::Join(id.into()))?;
    store.drop_join_token(id)?;
    Ok(())
}

/// The token `id` names, if it may be redeemed at `now`: unknown is
/// `join_token_invalid`, used `join_token_used`, expired
/// `join_token_expired`. It is not marked used; `Store::use_join_token`
/// does that, once the rest of the join is known to go through.
pub fn redeemable(store: &Store, id: &str, now: DateTime<Utc>) -> anyhow::Result<JoinToken> {
    let row = if is_join_id(id) {
        store.join_token(id)?
    } else {
        None
    };
    let Some(row) = row else {
        return Err(CliError::err(INVALID, format!("no join token {id}")));
    };
    if row.used_at.is_some() {
        return Err(CliError::err(
            "join_token_used",
            format!("join token {id} was already used"),
        ));
    }
    if row.expires_at <= stamp(now) {
        return Err(CliError::err(
            "join_token_expired",
            format!("join token {id} expired at {}", row.expires_at),
        ));
    }
    Ok(row)
}

/// Mark `id` used at `now`; a caller that lost the race gets the reason.
pub fn take(store: &Store, id: &str, now: DateTime<Utc>) -> anyhow::Result<()> {
    if store.use_join_token(id, &stamp(now))? {
        return Ok(());
    }
    redeemable(store, id, now)?;
    anyhow::bail!("join token {id} could not be marked used")
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
    let ids = match store.expired_join_tokens(&stamp(now)) {
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
    let old = stamp(now - KEEP_USED);
    for row in rows {
        let Some(used_at) = &row.used_at else {
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
        if *used_at <= old
            && let Err(err) = store.drop_join_token(&row.id)
        {
            tracing::error!(id = row.id, %err, "forget a used join token; trying again next pass");
        }
    }
}

/// The longest join request line, newline excluded: two keys, a host key
/// and a few facts fit many times over.
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
                facts,
                service_user,
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
                facts,
                service_user,
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

/// `pastor join`: the head's side as subcommands (`create`, `list`,
/// `revoke`), and the joining machine's own form, a token and no
/// subcommand.
#[derive(clap::Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct JoinArgs {
    #[command(subcommand)]
    pub cmd: Option<JoinCmd>,
    /// On the new machine: the token `join create` printed; `ps` shows it, so prefer --token-stdin
    #[arg(value_name = "TOKEN", conflicts_with = "token_stdin")]
    pub token: Option<String>,
    /// Read the join token from stdin
    #[arg(long)]
    pub token_stdin: bool,
    /// The name this machine takes in the head's flock.toml (default: its hostname)
    #[arg(long)]
    pub name: Option<String>,
    /// Do root work (packages, users, lingering) through `sudo -n`
    #[arg(long)]
    pub sudo: bool,
    /// The user that holds the machine key and runs the shepherd; created if missing (root mode)
    #[arg(long, value_name = "NAME")]
    pub service_user: Option<String>,
    /// Print as a JSON object: mode, isolation, each step, the public keys and versions
    #[arg(long)]
    pub json: bool,
    /// Join and name the shepherd only, for a machine set up beforehand; nothing is installed
    #[arg(long, hide = true)]
    pub skip_bootstrap: bool,
}

#[derive(clap::Subcommand, Debug)]
pub enum JoinCmd {
    /// Make a one-time token a new machine joins the fleet with, as a pull machine; prints the token
    Create {
        /// The flock the machine joins (default: the default flock)
        #[arg(long)]
        flock: Option<String>,
        /// A tag the machine gets; repeat for more
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// The one machine name already in flock.toml the token may take over
        #[arg(long)]
        replace: Option<String>,
        /// How long the token works, like 30m or 1h
        #[arg(long, default_value = "1h", value_parser = crate::config::parse_duration)]
        ttl: Duration,
        /// Print as a JSON object: the id, the token and its expiry
        #[arg(long)]
        json: bool,
    },
    /// The join tokens the head knows: id, flock, tags, expiry, whether used
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
pub fn changes_fleet(args: &JoinArgs) -> bool {
    !matches!(args.cmd, Some(JoinCmd::List { .. }))
}

/// `pastor join create|list|revoke`, through the head: the tokens are
/// redeemed there, against its store and its authorized_keys.
pub async fn run(client: &crate::ipc::Client, args: JoinArgs) -> anyhow::Result<()> {
    let Some(cmd) = args.cmd else {
        return Err(CliError::err(
            "invalid_args",
            "pastor join needs a subcommand: create, list or revoke",
        ));
    };
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
                let rows: Vec<Vec<String>> = rows
                    .iter()
                    .map(|t| {
                        vec![
                            t.id.clone(),
                            t.flock.clone().unwrap_or_else(|| "-".into()),
                            if t.tags.is_empty() {
                                "-".into()
                            } else {
                                t.tags.join(",")
                            },
                            t.replace.clone().unwrap_or_else(|| "-".into()),
                            t.expires_at.clone(),
                            t.used_at.clone().unwrap_or_else(|| "-".into()),
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
mod tests {
    use super::*;
    use crate::store::Store;
    use std::path::Path;

    fn code(e: &anyhow::Error) -> String {
        e.downcast_ref::<crate::cli::CliError>()
            .map(|e| e.code.clone())
            .unwrap_or_else(|| format!("not a CliError: {e:#}"))
    }

    fn sample() -> Token {
        Token {
            head: "me@head.example".into(),
            head_host_key: "head.example ssh-ed25519 AAAAHOST".into(),
            key: "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA+/=\n-----END OPENSSH PRIVATE KEY-----\n".into(),
        }
    }

    #[test]
    fn token_round_trips() {
        let t = sample();
        let s = t.encode();
        assert!(s.starts_with("pjt_"), "{s}");
        // One copy-pasteable word: base64url, no padding.
        assert!(
            s["pjt_".len()..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{s}"
        );
        assert_eq!(Token::decode(&s).unwrap(), t);
        // Whitespace around a pasted token is not part of it.
        assert_eq!(Token::decode(&format!("  {s}\n")).unwrap(), t);
        for n in 0..8 {
            let t = Token {
                head: "h".repeat(n + 1),
                ..sample()
            };
            assert_eq!(Token::decode(&t.encode()).unwrap(), t);
        }
    }

    #[test]
    fn base64url_matches_known_vectors() {
        for (plain, enc) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
            ("\u{fb}\u{ff}", "w7vDvw"),
        ] {
            assert_eq!(base64url_encode(plain.as_bytes()), enc);
            assert_eq!(base64url_decode(enc).unwrap(), plain.as_bytes());
        }
        assert_eq!(base64url_encode(&[0xfb, 0xff, 0xbf]), "-_-_");
    }

    #[test]
    fn decode_rejects_garbage() {
        let good = sample().encode();
        for bad in [
            "".to_string(),
            "pjt_".into(),
            "nope".into(),
            good.replacen("pjt_", "pjx_", 1),
            format!("{good}="),
            format!("{good}!"),
            // a length base64 cannot have
            format!("pjt_{}", "A".repeat(5)),
            // not three parts
            format!("pjt_{}", base64url_encode(b"head\nkey")),
            // a part empty
            format!("pjt_{}", base64url_encode(b"head\n\nkey")),
            // not UTF-8
            format!(
                "pjt_{}",
                base64url_encode(&[0xff, 0xfe, b'\n', b'a', b'\n', b'b'])
            ),
            // a head with a space or a newline in it
            format!("pjt_{}", base64url_encode(b"me@h x\nssh-ed25519 K\nkey")),
        ] {
            let e = Token::decode(&bad).unwrap_err();
            assert_eq!(code(&e), "join_token_invalid", "{bad:?}");
        }
    }

    #[test]
    fn join_ids_are_16_lowercase_hex() {
        let a = new_id().unwrap();
        let b = new_id().unwrap();
        assert_ne!(a, b);
        for id in [&a, &b] {
            assert!(is_join_id(id), "{id}");
        }
        for bad in [
            "",
            "abc",
            "0123456789ABCDEF",
            "0123456789abcdeg",
            "0123456789abcdef0",
        ] {
            assert!(!is_join_id(bad), "{bad}");
        }
    }

    /// `ssh-keygen` as far as `create` sees it: writes the pair it was asked
    /// for, and keeps the comment.
    struct FakeKeygen;

    impl crate::setup::Runner for FakeKeygen {
        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<crate::setup::CmdOutput> {
            assert_eq!(program, "ssh-keygen");
            let at = |flag: &str| args[args.iter().position(|a| *a == flag).unwrap() + 1];
            assert_eq!(at("-t"), "ed25519");
            assert_eq!(at("-N"), "");
            let file = Path::new(at("-f"));
            std::fs::write(
                file,
                "-----BEGIN OPENSSH PRIVATE KEY-----\nFAKE\n-----END OPENSSH PRIVATE KEY-----\n",
            )?;
            std::fs::write(
                file.with_extension("pub"),
                format!("ssh-ed25519 AAAAFAKEPUB {}\n", at("-C")),
            )?;
            Ok(crate::setup::CmdOutput {
                success: true,
                ..Default::default()
            })
        }
    }

    fn files(d: &Path) -> HeadFiles {
        let host_key = d.join("ssh_host_ed25519_key.pub");
        std::fs::write(&host_key, "ssh-ed25519 AAAAHOST root@head\n").unwrap();
        let keys = d.join("authorized_keys");
        std::fs::write(&keys, "ssh-ed25519 AAAA me@laptop\n").unwrap();
        HeadFiles {
            authorized_keys: keys,
            host_key,
            pastor: "/usr/local/bin/pastor".into(),
            work: d.join("join"),
        }
    }

    fn ask(ttl: std::time::Duration) -> Ask {
        Ask {
            flock: Some("builders".into()),
            tags: vec!["gpu".into()],
            replace: None,
            ttl,
        }
    }

    /// A head on another port than 22 is pinned under `[host]:port`, the
    /// name ssh looks it up by; on 22 the host alone.
    #[test]
    fn the_host_key_line_names_the_port_off_22() {
        let d = tempfile::tempdir().unwrap();
        let f = files(d.path());
        for (head, want) in [
            ("me@head.example", "head.example ssh-ed25519 AAAAHOST"),
            ("ssh://me@head.example", "head.example ssh-ed25519 AAAAHOST"),
            (
                "ssh://me@head.example:22",
                "head.example ssh-ed25519 AAAAHOST",
            ),
            (
                "ssh://me@head.example:2222",
                "[head.example]:2222 ssh-ed25519 AAAAHOST",
            ),
        ] {
            assert_eq!(keyscan_line(head, &f.host_key).unwrap(), want, "{head}");
        }
    }

    #[test]
    fn create_writes_the_line_and_keeps_no_private_key() {
        let d = tempfile::tempdir().unwrap();
        let f = files(d.path());
        let store = Store::open_in_memory().unwrap();
        let now = chrono::Utc::now();
        let made = create_with(
            &FakeKeygen,
            &f,
            &store,
            "me@head.example",
            &ask(std::time::Duration::from_secs(3600)),
            now,
        )
        .unwrap();
        assert!(is_join_id(&made.id));
        assert_eq!(made.token.head, "me@head.example");
        assert_eq!(
            made.token.head_host_key,
            "head.example ssh-ed25519 AAAAHOST"
        );
        assert!(made.token.key.contains("PRIVATE KEY"));
        let keys = std::fs::read_to_string(&f.authorized_keys).unwrap();
        assert!(keys.contains("ssh-ed25519 AAAA me@laptop"));
        let line = keys.lines().find(|l| l.contains("pastor:join:")).unwrap();
        assert_eq!(
            line,
            format!(
                "restrict,expiry-time=\"{}\",command=\"/usr/local/bin/pastor bridge --join {}\",{} ssh-ed25519 AAAAFAKEPUB pastor:join:{}",
                (now + chrono::Duration::hours(1)).format("%Y%m%d%H%M%SZ"),
                made.id,
                crate::authkeys::restrict_options(),
                made.id
            )
        );
        let row = store.join_token(&made.id).unwrap().unwrap();
        assert_eq!(row.flock.as_deref(), Some("builders"));
        assert_eq!(row.tags, ["gpu"]);
        assert_eq!(row.used_at, None);
        assert!(row.expires_at > now.to_rfc3339());
        // Both halves of the pair are gone from the work dir.
        let left: Vec<_> = std::fs::read_dir(&f.work)
            .map(|r| r.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn create_without_a_host_key_writes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let mut f = files(d.path());
        f.host_key = d.path().join("missing.pub");
        let store = Store::open_in_memory().unwrap();
        let e = create_with(
            &FakeKeygen,
            &f,
            &store,
            "me@h",
            &ask(std::time::Duration::from_secs(60)),
            chrono::Utc::now(),
        )
        .unwrap_err();
        assert_eq!(code(&e), "config_error");
        assert_eq!(
            std::fs::read_to_string(&f.authorized_keys).unwrap(),
            "ssh-ed25519 AAAA me@laptop\n"
        );
        assert!(store.join_tokens().unwrap().is_empty());
    }

    #[test]
    fn expired_swept() {
        let d = tempfile::tempdir().unwrap();
        let f = files(d.path());
        let store = Store::open_in_memory().unwrap();
        let now = chrono::Utc::now();
        let old = create_with(
            &FakeKeygen,
            &f,
            &store,
            "me@h",
            &ask(std::time::Duration::from_secs(1)),
            now,
        )
        .unwrap();
        let live = create_with(
            &FakeKeygen,
            &f,
            &store,
            "me@h",
            &ask(std::time::Duration::from_secs(3600)),
            now,
        )
        .unwrap();
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        let later = now + chrono::Duration::seconds(5);
        assert_eq!(sweep(&store, &f.authorized_keys, &tx, later), 1);
        let keys = std::fs::read_to_string(&f.authorized_keys).unwrap();
        assert!(!keys.contains(&format!("pastor:join:{}", old.id)), "{keys}");
        assert!(keys.contains(&format!("pastor:join:{}", live.id)), "{keys}");
        assert!(keys.contains("me@laptop"));
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.kind, "machine.join_token_expired");
        assert_eq!(ev.detail.unwrap()["id"], old.id.as_str());
        assert!(store.join_token(&old.id).unwrap().is_none());
        // Swept once: the next pass has nothing to say.
        assert_eq!(sweep(&store, &f.authorized_keys, &tx, later), 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_sweep_that_cannot_edit_the_file_keeps_the_token_for_the_next_pass() {
        let d = tempfile::tempdir().unwrap();
        let f = files(d.path());
        let store = Store::open_in_memory().unwrap();
        let now = chrono::Utc::now();
        let old = create_with(
            &FakeKeygen,
            &f,
            &store,
            "me@h",
            &ask(std::time::Duration::from_secs(1)),
            now,
        )
        .unwrap();
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        let later = now + chrono::Duration::seconds(5);
        // A directory where the file should be: the edit fails.
        let blocked = d.path().join("blocked");
        std::fs::create_dir_all(blocked.join("x")).unwrap();
        assert_eq!(sweep(&store, &blocked, &tx, later), 0);
        assert!(rx.try_recv().is_err());
        assert!(store.join_token(&old.id).unwrap().is_some());
        assert_eq!(sweep(&store, &f.authorized_keys, &tx, later), 1);
    }

    #[test]
    fn a_used_token_whose_line_was_left_behind_loses_it_on_the_sweep() {
        let d = tempfile::tempdir().unwrap();
        let f = files(d.path());
        let store = Store::open_in_memory().unwrap();
        let now = chrono::Utc::now();
        let made = create_with(
            &FakeKeygen,
            &f,
            &store,
            "me@h",
            &ask(std::time::Duration::from_secs(3600)),
            now,
        )
        .unwrap();
        // A join spent it but could not drop its line.
        assert!(store.use_join_token(&made.id, &stamp(now)).unwrap());
        let marker = format!("pastor:join:{}", made.id);
        assert!(
            std::fs::read_to_string(&f.authorized_keys)
                .unwrap()
                .contains(&marker)
        );
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        let later = now + chrono::Duration::seconds(5);
        assert_eq!(sweep(&store, &f.authorized_keys, &tx, later), 0);
        let keys = std::fs::read_to_string(&f.authorized_keys).unwrap();
        assert!(!keys.contains(&marker), "{keys}");
        assert!(keys.contains("me@laptop"));
        assert!(rx.try_recv().is_err(), "a used token did not expire");
        // `join list` still shows it as used, for a while.
        assert!(store.join_token(&made.id).unwrap().is_some());
        let week = now + chrono::Duration::days(6);
        sweep(&store, &f.authorized_keys, &tx, week);
        assert!(store.join_token(&made.id).unwrap().is_some());
        let past_a_week = now + chrono::Duration::days(8);
        sweep(&store, &f.authorized_keys, &tx, past_a_week);
        assert!(store.join_token(&made.id).unwrap().is_none());
    }

    fn reply_code(out: &[u8]) -> String {
        let v: serde_json::Value = serde_json::from_slice(out).unwrap();
        assert_eq!(v["kind"], "error", "{v}");
        v["data"]["code"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn bridge_refuses_a_line_past_the_cap() {
        let id = "0011223344556677";
        let sock = Path::new("/nonexistent/pastor.sock");
        let big = vec![b'x'; MAX_JOIN_LINE + 1];
        let mut out = Vec::new();
        bridge(sock, id, &big[..], &mut out).await.unwrap();
        assert_eq!(reply_code(&out), "request_too_large");
        // A line just at the cap is read and judged on what it says.
        let mut at = vec![b'x'; MAX_JOIN_LINE];
        at.push(b'\n');
        let mut out = Vec::new();
        bridge(sock, id, &at[..], &mut out).await.unwrap();
        assert_eq!(reply_code(&out), INVALID);
    }
}
