# pastor

pastor runs coding agents on always-on machines you own, so they pick up
tasks while your laptop is closed. It sits on top of [herdr](https://herdr.dev):
herdr owns the terminals and the agents, pastor owns the fleet and the
bookkeeping. When you open the laptop you attach to the panes through herdr.

Status: core only. One-off tasks work end to end. Scheduled jobs, connector
plugins and systemd setup are the next milestones; see
`docs/superpowers/specs/2026-09-23-pastor-design.md`.

## How it works

`pastor serve` runs on one machine, the head. For every machine in the flock it
keeps an SSH connection running `herdr --session <s> remote-api-bridge`, which
pipes herdr's socket protocol over stdio. Through it pastor creates a workspace,
starts an agent named after the task, sends the prompt, and subscribes to agent
status events. Task state lives in SQLite under `~/.local/state/pastor/`.

The CLI talks to `pastor serve` over a unix socket (`pastor.sock`) with
newline-delimited JSON; each response is `{"kind": ..., "data": ...}`.
`pastor serve` refuses to start if a daemon already holds that socket, or if
something answers it but not a ping within 2 seconds — it only removes and
replaces a socket file whose connection is refused. `list` and `task show`
read from `pastor serve` when it's running and fall back to the SQLite store
when it's not (`list` says so on stderr); `attach` always reads the store
directly, since it only needs the task's machine and agent name to hand off
to `ssh`/`herdr`.

`pastor list` shows every task except closed ones, including blocked, stale
and failed tasks; `--all` adds closed tasks back, and `--blocked`/`--done`
narrow to just those. `pastor task read t-1` fetches recent output from the
task's pane over the machine channel. `pastor open pi-3` execs the full herdr
UI against a flock machine (`herdr --remote` for an SSH one, `herdr` directly
for a local one) instead of showing pastor's own view.

Runtime errors print JSON on stderr with a stable `code` and exit 1; a
malformed command line gets clap's plain usage text and exit 2.

Each machine needs herdr 0.9 or newer (protocol 22 or newer) with its server
running, and SSH access from the head without a passphrase prompt (a key in
ssh-agent won't be there for a service; use a dedicated key or Tailscale SSH).

## Try it

```bash
make install                         # pastor and fake-herdr into ~/.cargo/bin
pastor flock add pi-3 fleet@pi-3 --max-agents 2
pastor flock add here --local
pastor flock status                  # ssh, herdr version, protocol
pastor serve &                       # or run it under systemd later
pastor run "Fix the flaky test in ci.yml" --repo ~/work/api --machine pi-3
pastor list
pastor task read t-1                 # recent pane output, without attaching
pastor attach t-1                    # lands in the agent's pane; ctrl+b q detaches
pastor open pi-3                     # the full herdr UI on that machine
```

Without a real herdr, a fake one speaks the same protocol:

```bash
pastor flock add fake --command fake-herdr
FAKE_HERDR_AUTO_DONE_MS=500 pastor serve
```

## Files

```
~/.config/pastor/pastor.toml      tick, settle, reconcile_every, defaults (all optional)
~/.config/pastor/flock.toml       machines
~/.local/state/pastor/pastor.db   tasks
~/.local/state/pastor/pastor.sock daemon socket
```

`PASTOR_CONFIG_DIR` and `PASTOR_STATE_DIR` override the locations.

## Shell completions

`pastor completions <shell>` prints a completion script generated from the
command definitions, so it always matches the installed binary. Ready-made
copies for bash and fish live in `contrib/completions/`.

```bash
pastor completions fish > ~/.config/fish/completions/pastor.fish
pastor completions bash > ~/.local/share/bash-completion/completions/pastor
```

## Development

The Makefile is the list of things you can run here; `make help` prints it.

```bash
make check            # fmt check, clippy with warnings as errors, full test suite
make test             # unit tests plus an end-to-end run against fake-herdr
make test-machine     # the machine actor tests five times, to catch timing flakes
make smoke SESSION=s  # opt-in test against a real herdr running session s on this host
make build            # debug build of both binaries; cargo run -- --help works from there
```

`make check` is what a pull request has to pass. Nothing in the suite talks to
a real herdr, so run `make smoke` on a fleet machine before trusting it there.
