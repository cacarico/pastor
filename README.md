# pastor

Run coding agents on machines you own, while your laptop is closed.

pastor keeps a small fleet of always-on machines busy with agent work. You
hand it a task, or a schedule that produces tasks, and it picks a machine,
opens a terminal there through [herdr](https://herdr.dev), starts the agent,
sends the prompt, and tells you when the agent stops. When you come back you
attach to the agent's terminal and carry on.

<p align="center">
  <img src="docs/demo/tasks.gif" alt="pastor machine list, task run, task list and task read in a terminal" width="800">
</p>

## Install

```sh
cargo install --git https://github.com/cacarico/pastor --locked
```

Every machine in the fleet needs [herdr](https://herdr.dev) 0.9 or newer with
its server running. A machine on another host is reached over ssh, which must
work from the head without a passphrase prompt; the head itself joins as a
local machine with no ssh at all. The agents themselves (`claude`, `opencode`,
...) are installed on each machine the usual way.

## Quick start

```sh
pastor machine add pi-1 user@pi-1        # a machine you can ssh to
pastor machine add here --local          # this machine can take tasks too
pastor machine add pi-2 user@pi-2 --flock work   # after `pastor flock add work`
pastor setup systemd                     # run the head as a user service
pastor task run "Fix the flaky test in ci.yml" --repo '~/work/api'
pastor task list                         # queued, starting, running, blocked
pastor task read t-1                     # the agent's recent output
pastor task attach t-1                   # sit in its terminal; ctrl+b q detaches
pastor task send t-1 --trust             # accept its folder-trust prompt, and trust that repo
pastor task send t-1 "yes, go on"        # or type an answer to what it is waiting on
```

A job is one TOML file in `~/.config/pastor/jobs/`. This one starts an agent
every hour:

```toml
every = "1h"

[connector]
use = "clock"

[dispatch]
repo = "~/work/api"
prompt = "It is {{ item.key }}. Run the test suite and fix what broke."
```

Plugins add other connectors, so a job can turn Slack messages or GitHub
issues into tasks, and event hooks that run when a task finishes or blocks.

<p align="center">
  <img src="docs/demo/jobs.gif" alt="a job file, pastor job list, job run and the task it made" width="800">
</p>

## How it fits together

- **The head** runs `pastor serve`. It owns the queue, the schedule and the
  task history, in SQLite under `~/.local/state/pastor/`.
- **The machines** are listed in `~/.config/pastor/flock.toml`: the head
  itself as a local machine, and other hosts reached over one multiplexed ssh
  connection each. A machine takes up to `max_agents` tasks at once; tags
  steer a task to the right one.
- **A flock** is a named group of machines, such as work and personal ones
  that run agents on different accounts. Every machine is in one flock, every
  task and job targets one, and only that flock's machines take its tasks.
  With no flock declared there is one, `default`.
- **A task** is one agent in one herdr pane, in a repo or a fresh worktree of
  it. Its states are `queued`, `starting`, `running`, `blocked`, `done`, `stale`, `failed`
  and `closed`; `done` means the agent stopped, not that the work is good.
- **herdr** owns the terminals and detects what the agent is doing. pastor
  never reads the agent's screen itself.

## Learn more

- [The manual](docs/manual.md): every command, state, file and setting, and
  how pastor talks to herdr.
- [Skill for agents](skills/pastor/SKILL.md): what a coding agent needs to
  know to use pastor, or to behave when pastor started it. `pastor --skill`
  prints the copy built into the binary.
- [Changelog](CHANGELOG.md).

Development: `make check` runs fmt, clippy and the whole suite against a fake
herdr; `make help` lists the rest. Issues and pull requests are welcome.
