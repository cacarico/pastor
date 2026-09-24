# Security Policy

pastor runs coding agents on machines you own and talks to herdr over local or
SSH transports. Treat configuration, prompts, logs, task output, sockets, SSH
control paths, and SQLite state as sensitive.

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
