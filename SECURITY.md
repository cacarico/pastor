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

Reports that need the head user's own access (writing its config, or running
code as it) are in scope only where pastor makes that access easier to get
than the model above says.

## Reporting a vulnerability

Please do not publish exploit details in a public issue. If GitHub private
vulnerability reporting is enabled for this repository, use it. Otherwise, open
a minimal public issue asking for a private security contact and include no
secrets, tokens, hostnames, exploit payloads, or sensitive logs.

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

New dependencies should be justified in the pull request. Reviewers should
consider maintenance status, transitive dependencies, license compatibility, and
whether a small in-tree implementation is safer.
