//! `pastor bridge`: a remote CLI's way to this machine's head. It runs over
//! ssh, so the head never listens on a network port; each request line on
//! stdin goes to the head's socket unread, and each reply goes back on stdout.

use std::path::Path;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::cli::{CliError, request_failure};
use crate::ipc::{RequestError, connect_error_means_no_daemon, relay_line};

/// Passes `input`'s lines to the head on `socket` and writes each reply to
/// `output`, until `input` ends. A request the head cannot take stops the
/// bridge with that error, and no later line is sent: the client reads one
/// reply per request, so it must see the failure where the reply would be.
/// No head at all is `no_head`, never a reason to start one.
pub async fn run(
    socket: &Path,
    mut input: impl AsyncBufRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
) -> anyhow::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        // A blank line holds no request; sending it would only draw an error.
        if line.trim().is_empty() {
            continue;
        }
        let reply = match relay_line(socket, &line).await {
            Ok(reply) => reply,
            Err(RequestError::Connect(e)) if connect_error_means_no_daemon(&e) => {
                return Err(CliError::err(
                    "no_head",
                    format!("no pastor serve is running on this machine ({e})"),
                ));
            }
            Err(err) => {
                let (code, message) = request_failure(&err);
                return Err(CliError::err(code, message));
            }
        };
        output.write_all(reply.as_bytes()).await?;
        output.flush().await?;
    }
}
