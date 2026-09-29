# AI use

Much of pastor is written by coding agents, many of them dispatched by
pastor itself, and I review and merge what they produce. That is the point
of the tool, so AI help is welcome here too. Whoever opens the pull request
answers for it, whatever wrote it.

## When you contribute

- Understand the change you send, and keep it small enough to review.
- Run `make check`, or say why a test is missing.
- Say in the pull request when an agent shaped the code, tests, docs or
  design, and where copied or generated material came from if its license
  could matter.
- Keep secrets, hostnames, private prompts and logs out of what you give an
  agent, and out of the commit. The repository is public.

Where an agent's output disagrees with the manual, the code or the license,
fix it before you open the pull request.

## When you run pastor

pastor starts agents with the head's user's access on each machine, and it
does decide part of what they may do: an agent it started cannot change the
fleet unless you set `agents_change_fleet`, a task runs under the permission
profile and the `allow` and `deny` lists you give it, and no task can start
an orchestrator. What it cannot decide is what that user can reach: files,
keys, tokens and the network. [Trust model](manual.md#trust-model) in the
manual has the details, and [SECURITY.md](../SECURITY.md) what that means
for connectors.

So run it on machines and repositories you trust, with keys that reach no
more than the work needs, treat pane output, logs and the task store as
possibly holding secrets, and read what an agent did before you merge or
deploy it.

## When you change pastor

A change to what an agent may do, how a task's state moves, how credentials
flow or when a person has to look is documented in the manual in the same
pull request. Anything safety-related is explicit configuration, not a
hidden default, and prompt injection or data leaking through a pane counts
as a security bug.
