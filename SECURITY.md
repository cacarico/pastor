# Security Policy

pastor runs coding agents on machines you own and talks to herdr over local or
SSH transports. Treat configuration, prompts, logs, task output, sockets, SSH
control paths, and SQLite state as sensitive.

## Trust model

The user that runs `pastor serve` on the head is the fleet's trust boundary.
Any process running as that user on the head controls every machine pastor
drives: it can send any request to `pastor.sock` (queue tasks with any
prompt and agent arguments, type into live tasks, edit the flock), rewrite
`flock.toml`, the job files and the plugins, and reuse the ssh ControlMaster
sockets under the state directory to reach every ssh machine. The socket's
0600 mode keeps other users out; it does not keep out other processes of the
same user.

That includes agents on a `local = true` machine, which run on the head as
that user. Do not give a `local = true` machine untrusted work, such as jobs
fed by issues or chat messages from outside your team; run herdr for that
work as a separate user and add it as an ssh machine instead. Plugins also
run as that user, so installing one grants it control of the fleet.

## What plugins inherit

A plugin command (connector or hook) runs on the head as the head's user and
inherits the full environment of the pastor process that starts it, plus its
own `.env` and the `PASTOR_*` variables. It is not limited to its `.env`:
`SSH_AUTH_SOCK`, API tokens and cloud credentials in that environment reach
every plugin, and only the secrets its manifest declares are redacted from
its run logs. It can read the head user's files, including other plugins'
`.env` files, and `PASTOR_STATE_DIR` points it at `pastor.sock` and the ssh
ControlMaster sockets. Hooks that do not set `only_own = true` receive every
task's item and prompt. Start `pastor serve` from a minimal environment, and
review a plugin as you would any program you run with your own account.

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

A useful report includes:

- Affected version or commit.
- Reproduction steps in a test or disposable environment.
- Impact and attacker capabilities required.
- Whether credentials, prompts, logs, workspaces, agents, or remote machines are
  exposed.

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
other. Its malware alerts flag a dependency version that has been reported as
malicious.

New dependencies should be justified in the pull request. Reviewers should
consider maintenance status, transitive dependencies, license compatibility, and
whether a small in-tree implementation is safer.
