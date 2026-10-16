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

## Bootstrapping and joining a machine

The head's own user is the trust root for every machine it bootstraps or
takes a join from, not just the ones it already had: its `head_key` reaches
every pushed machine's agent user, and the ed25519 key a join token hands
out reaches the head itself, scoped to one `pastor join` request. Treat the
head's account with the same care on day one as on the hundredth machine.

**User mode has no isolation.** Without root (no `--sudo`, not already
root), bootstrap makes no separate user: herdr, the shepherd and every agent
pane run as the login user, so an agent can read that user's ssh keys,
including the machine key that logs the machine into the head. Root mode's
`pastor-agent`, with its own `0700` state directory, is what keeps a
prompt-injected agent from reading the service user's machine key; pick it
for any machine that runs work you do not fully trust.

**A leaked join token** reaches only `bridge --join <id>`, a forced command
answering exactly one `pastor join` request before the connection ends.
Before it is redeemed or its `--ttl` runs out, that one request can add one
machine, under a name not already in `flock.toml` (or the one name the
token's own `--replace` targets). Once it is used, revoked or past its
`--ttl`, the same token's ssh gets `Permission denied`: its line is gone
from `authorized_keys`, and no later request, successful or not, adds a
second machine or extends its life.

**A hijacked pull machine's shepherd** reaches the head through its own
`bridge --shepherd --machine NAME` key, which answers for `NAME` and nothing
else: `TaskClaim`, `TaskReport`, `PullTrust` and `BootstrapReport` all get
their `machine` field forced to `NAME` whatever the request said, so it can
claim, report or mark bootstrapped only itself, never another machine. A
job it submits is pinned to `NAME` the same way, and one that sets
`agent_args`, `allow`, `deny`, `agent_source` or `tags`, or names a flock
`NAME` is not in, is refused outright (`not_allowed_for_shepherd`): a
compromised pull machine cannot pick what an agent runs with or widen its
own reach. It can read its own machine's events and fetch pastor or herdr
from the head's artifact cache, and nothing past that.

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
