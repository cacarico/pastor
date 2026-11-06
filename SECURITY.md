# Security Policy

pastor runs coding agents on your machines, over ssh or a local socket to
herdr. Everything it keeps can hold secrets: config, prompts, logs, task
output, its sockets, the ssh control paths and the SQLite store.

## Trust model

The user that runs `pastor serve` on the head is the fleet's trust boundary.
Any process running as that user on the head controls every machine pastor
drives: it can send any request to `pastor.sock` (queue tasks with any
prompt and agent arguments, type into live tasks, edit the flock), rewrite
`flock.toml`, the job files and the connectors, and reuse the ssh ControlMaster
sockets under the state directory to reach every ssh machine. The socket's
0600 mode keeps other users out; it does not keep out other processes of the
same user.

That includes agents on a `local = true` machine, which run on the head as
that user. Do not give a `local = true` machine untrusted work, such as jobs
fed by issues or chat messages from outside your team; run herdr for that
work as a separate user and add it as an ssh machine instead. Connectors also
run as that user, so installing one grants it control of the fleet.

## What a machine is trusted with

The head holds every machine's trust. `flock.toml` says which machines exist;
each ssh machine's host key is pinned in pastor's own `known_hosts` (a key
trusted the first time it is seen, unless `--host-key` names it, and never
replaced by a later run: a changed key stops the next connection, and a
reinstalled machine is removed and added again); the head's `authorized_keys`
holds a marked line for each machine's agent key and each pull machine's
shepherd key; `machine_state` in the store records how each machine's bootstrap
ended. Whoever controls the head's user controls all of it, as above.

A machine's way back to the head is one ssh key locked, by its `authorized_keys`
line, to one command (`restrict`, no terminal, no forwarding): the agent bridge
for its agents, the shepherd bridge for a pull machine's headless serve. The
bridge, on the head, passes on only a short list of requests and only about
that machine: its tasks, its flock's task list, and for a shepherd its claims
and reports, its saved trust, jobs pinned to it, the events about it, its
bootstrap report and files from the head's artifact cache. Everything else is
refused before it reaches the head, and a shepherd's key stops working when the
head moves its enrollment on or the machine is removed. No machine can name
another machine, change the flock or queue work elsewhere through its key.

`pastor bootstrap` makes those keys as the machine's login user, and the agents
on the machine run as that user (`isolation: none`). So an agent, or a prompt
that steers one, can read the machine key and the agent key and use them: it can
do on the head what the machine's own agents and shepherd can, for that
machine, such as end its tasks, report them done or failed, claim work for it or
report it bootstrapped, and it can read everything else that user can,
`~/.ssh` and the agent CLIs' logins included. It cannot reach another machine.
To keep agents from the machine's keys, run them as another user on the machine
and add that user as its own ssh machine, as for a `local = true` machine
above.

A pull machine can join with a one-time token (`pastor join create` on the head).
The token holds the private half of an ed25519 key whose public half sits in the
head's `authorized_keys` locked to `bridge --join <id>` with sshd's `expiry-time`,
so it can do one thing, once, until it expires: send a join request. It is a
secret and pastor treats it as one: the head keeps only its id, and the machine
that joins reads it from stdin, `PASTOR_JOIN_TOKEN` or the terminal, never from
an argument, takes the variable out of its own environment before it starts any
command, writes the key to a 0600 file in a 0700 directory only for the length
of the ssh that uses it, deletes that directory on every way out that can be
caught, and writes it in no log and no file it keeps. The head's host key
travels in the token and is pinned before the first connection, so a join does
not trust the network. Whoever has the token before its machine uses it can
take the name it was made for: give it over a channel you trust, make it short
lived, and revoke it (`pastor join revoke`) if it leaves your hands.

Bootstrap, by hand or from the head, runs as the login user and calls no
`sudo`; what needs root is left to the machine's owner. The push copies pastor
and herdr from the head's artifact cache, which hashes each download against
its published sum before it is cached, and hashes them again on the machine
before they are moved into place.

## What connectors inherit

A connector command (a connector's run or one of its hooks) runs on the head,
or on the machine whose headless serve owns the job, as that machine's user.
It inherits the full environment of the pastor process that starts it, plus its
own `.env` and the `PASTOR_*` variables. It is not limited to its `.env`:
`SSH_AUTH_SOCK`, API tokens and cloud credentials in that environment reach
every connector, and only the secrets its manifest declares are redacted from
its run logs. It can read that machine's user's files, including other
connectors' `.env` files, and `PASTOR_STATE_DIR` points it at that machine's
`pastor.sock`; on the head, that also means the ssh ControlMaster sockets to
every machine the head drives. A headless serve's shepherd holds one such
socket too, but only to reach its own remote head (`ssh/head-%C` under its
own state directory, not the fleet's machine masters, which only the head
opens): a connector on a headless serve can reuse that one socket to act as
the shepherd on the head, but cannot reach the other ssh machines directly.
Hooks that do not set
`only_own = true` hear every task's events, but get another connector's
items, prompts and summaries blanked. Start `pastor serve` from a minimal
environment, and review a connector as you would any program you run with
your own account.

Reports that need the head user's own access (writing its config, or running
code as it) are in scope only where pastor makes that access easier to get
than the model above says.

## Reporting a vulnerability

Please do not publish exploit details in a public issue. Report it privately
through GitHub's private vulnerability reporting instead: the "Report a
vulnerability" button under the repository's Security tab
(<https://github.com/cacarico/pastor/security/advisories/new>). Only the
maintainers see the report, and the fix can be discussed and prepared in a
private advisory before it is published.

A useful report says which version or commit is affected, how to reproduce
it somewhere disposable, what an attacker needs and gains, and whether
credentials, prompts, logs, workspaces, agents or other machines are exposed.

## Handling secrets

- Never commit real credentials, tokens, private keys, production hostnames, or
  private prompts.
- Redact secrets in issues, pull requests, logs, screenshots, and AI transcripts.
- Use dedicated SSH keys or Tailscale SSH for service access, not interactive
  passphrase prompts.
- Rotate any secret that may have been exposed, even if it was later removed
  from git history.

## Dependency and license risk

Dependabot watches the crates in `Cargo.lock` and the GitHub Actions the
workflows use (`.github/dependabot.yml`): it opens security updates as soon as
an advisory lands, and version updates weekly, grouping minor and patch bumps
into one pull request per ecosystem. Its pull requests go through CI like any
other. Work happens in a private copy of the repository, which publishes to
the public one, so in the public repository Dependabot's pull requests are
closed by a workflow (`.github/workflows/dependabot-close.yml`): the same
update is merged in the private copy and arrives with the next sync. Its malware alerts flag a dependency version that has been reported as
malicious.

New dependencies should be justified in the pull request. Reviewers should
consider maintenance status, transitive dependencies, license compatibility, and
whether a small in-tree implementation is safer.
