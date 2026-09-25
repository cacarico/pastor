# AI Governance

AI tools are allowed in pastor development, but people remain responsible for
the work. AI output is never authority by itself.

## Contributor rules

- Understand every submitted change.
- Keep generated changes small enough for real review.
- Run or explain the required tests.
- Disclose AI assistance in the pull request when it shaped code, tests, docs,
  or design.
- Check provenance and license compatibility for generated, copied, or adapted
  material.
- Never feed secrets, private prompts, logs, hostnames, credentials, customer
  data, or private repositories to an AI tool unless that use is approved.

If generated code or text conflicts with the manual, README, behavior, security
practice, or license obligations, fix it before submitting.

## Operator rules

pastor dispatches coding agents to machines you control. It does not decide what
agents are allowed to do; operators are responsible for the prompts,
repositories, credentials, tools, and machines they make available.

- Run pastor only on trusted machines and repositories.
- Use least-privilege SSH keys and service accounts.
- Keep secrets out of prompts when possible.
- Assume agent panes, logs, task output, and transcripts may contain sensitive
  data.
- Review agent changes before merging or deploying them.
- Keep audit-relevant task history in the SQLite store and logs according to
  your retention policy.

## Project rules

- Document behavior that affects agent autonomy, task state, credential flow, or
  operator review.
- Prefer explicit configuration over hidden defaults for safety-sensitive
  behavior.
- Treat prompt injection, unsafe tool access, and accidental data exposure as
  security-relevant design concerns.
- Justify new dependencies and automation in pull requests.
