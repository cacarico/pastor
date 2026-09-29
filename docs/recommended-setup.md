# Recommended setup

How to set up a fleet you will run for a while: what each machine gets, which
credentials it holds, and the settings that keep agents moving without anyone
watching. It comes from running pastor to build pastor, with a desktop as the
head and a few Raspberry Pis taking tasks. `docs/manual.md` says how each
piece works; this page says how to put them together.

Names here are placeholders: `pi-1` and `pi-2` are Pis, `user@pi-1` is how
the head reaches one over ssh, and `you/app` is the one repository their
agents work on.

## The shape

- **One head, on a machine that stays on.** It owns the queue and the jobs.
  A job that reads local files (a notes vault, say) must run where those files
  are, so pin it with `machine = "..."` in its `[dispatch]`.
- **A flock per account.** Put work machines in a `work` flock and personal
  ones in `personal`, and never give a machine both accounts' logins. A task
  only goes to machines of its flock, so the two never mix.
- **Slots to match the machine.** `max_agents` is how many agents a machine
  runs at once. A Pi 5 with 8 GB handles 2 or 3 Claude agents; a desktop,
  more. A machine that every pinned job needs fills up first, so give it the
  most slots.

```bash
pastor flock add personal --default
pastor flock add work
pastor machine add pi-1 user@pi-1 --flock personal --max-agents 3 --herdr
pastor machine add pi-2 user@pi-2 --flock work --max-agents 3 --herdr
pastor machine list
```

## Services and network

- Give every machine a stable name the head can reach, such as a Tailscale
  name, and a passphrase-less ssh key from the head. A service has no
  ssh-agent to ask.
- Run herdr as a user service on each machine, and the head as one on the
  head: `pastor setup systemd --herdr --yes` on each machine, then
  `pastor setup systemd --yes` on the head (`setup launchd` on macOS).

## Least privilege for agent machines

An agent runs as its machine's user, with every credential that user holds.
So give an agent machine only what its tasks need, and only for the
repository they work on.

**Push with a deploy key, not your account's key.** Your account's ssh key
can push to every repository you own. A deploy key belongs to one repository.
On the machine, make a key under its own name, and add it to `you/app` under
Settings, then Deploy keys, with write access:

```bash
ssh user@pi-1 'ssh-keygen -t ed25519 -N "" -f ~/.ssh/app_deploy -C pi-1-app && cat ~/.ssh/app_deploy.pub'
```

Then make the clone use that key and no other. In the machine's
`~/.ssh/config`:

```text
Host github-app
  HostName github.com
  User git
  IdentityFile ~/.ssh/app_deploy
  IdentitiesOnly yes
```

```bash
ssh user@pi-1 'git -C ~/app remote set-url origin git@github-app:you/app.git'
ssh user@pi-1 'ssh -T git@github-app'    # "Hi you/app!": the deploy key
ssh user@pi-1 'ssh -T git@github.com'    # should fail; "Hi you!" means an account key is still there
```

`IdentitiesOnly` stops ssh from offering other keys, such as ones in an
ssh-agent. An account key left on the machine still reaches every repository,
so remove it from the machine and from your account. GitHub also refuses a
deploy key that is already on your account. Your branch rules still apply to
a deploy key: with a pull request required on `main`, agents can push
branches but not `main`.

**Give `gh` a fine-grained token for that repository alone.** A token from
`gh auth login` reaches every repository. Make one under Settings, then
Developer settings, then Fine-grained tokens:

- Repository access: only `you/app`.
- Pull requests: read and write. Contents, Actions and Commit statuses: read.
  Nothing else.
- An expiry (90 days, say), and a calendar reminder to renew it.

With Contents read-only, `gh` on that machine can open pull requests,
comment, reply to and resolve review threads, and read Actions runs, but it
cannot merge: merging needs Contents write. Pushes still work, through the
deploy key. Keep merging on the head, where you or your own review gate
decide. Fine-grained tokens have no Checks permission, so check runs from
apps other than Actions may be out of its reach.

```bash
ssh -t user@pi-1 'gh auth logout --hostname github.com; gh auth login --hostname github.com --with-token'
ssh user@pi-1 'gh pr list --repo you/app --limit 1'     # works
ssh user@pi-1 'gh run list --repo you/app --limit 1'    # works: CI through Actions
ssh user@pi-1 'gh repo view you/other'                  # fails: out of reach
```

`gh auth logout` only deletes the old token from the machine. GitHub keeps
accepting it until you revoke "GitHub CLI" under Settings, then Applications,
which logs out every machine's `gh`.

**A second account on the same machine.** When the head is also your desktop
and its plain `claude` is logged in to another account, define an agent that
points at its own login, and make it the machine's agent:

```toml
# pastor.toml
[agents.claude-personal]
kind = "claude"
env = { CLAUDE_CONFIG_DIR = "~/.claude-personal" }
```

```toml
# flock.toml (open it with `pastor flock edit`)
[[machine]]
name = "desk"
local = true
agent = "claude-personal"
```

Never put tokens in `pastor.toml`: every agent's pane can read it.

## Permissions

A dispatched agent has nobody watching its pane. When its own permission
system asks before a command, the task sits `blocked` until someone answers.

- **Claude:** give each flock allow and deny lists, so common commands go
  through and dangerous ones never do (see "Tool allow and deny lists" in the
  manual):

  ```toml
  # fragment of flock.toml
  [[flock]]
  name = "personal"
  allow = ["Bash(git:*)", "Bash(cargo:*)", "Bash(make:*)", "Bash(gh pr:*)"]
  deny = ["Bash(sudo:*)", "Bash(git push --force:*)", "Read(~/.ssh/**)"]
  ```

- **opencode:** a `permission` block in `~/.config/opencode/opencode.json`
  with `"bash": {"*": "ask"}` stops every task at its first shell command.
  On a machine that runs opencode tasks, allow what those tasks run, or give
  review tasks everything they need in the prompt so they don't need a shell.
- Keep `agents_change_fleet` off (the default), so an agent can't start
  tasks or change the fleet.
- Don't pass a "skip all permissions" flag through `agent_args` except on a
  machine you would wipe without a second thought.
- Accept a repository's folder-trust prompt once per machine with
  `pastor task send t-3 --trust`; later tasks there go through on their own.

## Fresh code for every task

`--worktree` branches from the clone's local `HEAD`. Pull requests merged on
GitHub don't move it, so a task can start from code days old. For a
`--worktree` task, whose worktree and branch are new and hold nothing yet,
start the prompt, or the job's prompt template, with:

```text
First run `git fetch origin` and `git reset --hard origin/main`.
```

Use your default branch's name in place of `main`. Never put this in a task
without `--worktree`: it runs in your existing clone, and the reset throws
away whatever is uncommitted there. Pull that clone yourself instead.

For a review, fetch and check out the pull request's branch.

## Keep slots free

A `done` task holds its machine slot until its pane closes, which by default
is 5 seconds later, so finished work does not keep machines full. When you
want time to attach and read a task's last screen, keep the pane longer:

```toml
# pastor.toml
close_done_after = "15m"
```

A task that goes `stale` keeps its slot until you close it, and so does a
`failed` or `blocked` one. Check with `pastor machine describe pi-1` and free
it with `pastor task close t-7`, which keeps its worktree on disk.

## Reviews

- A second review by a different model catches what one reviewer misses. An
  agent definition for another agent kind or model makes that a job like any
  other.
- A review agent should never run on the model that wrote the work.
- Give a reviewer the text it reviews in its prompt, and have it write its
  findings to a file in its own directory. It then needs no shell, and can't
  stall on a permission prompt.
