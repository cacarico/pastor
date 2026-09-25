# Contributing

Thanks for helping make pastor better. pastor is early software; small,
well-scoped changes with clear tests are easiest to review.

## Before you start

- Work on a branch and open a pull request. Do not push to `main`.
- Read `AGENTS.md`, `README.md` and `docs/manual.md` before changing
  behavior. The design spec and implementation plans live on the `docs`
  branch, under `docs/superpowers/`.
- Keep vocabulary consistent: machine, flock, head, job, task, plugin,
  connector, and agent.
- Do not add runtime dependencies unless the pull request explains why the
  dependency is worth the maintenance and supply-chain cost.

## Development checks

`make check` is the pull request gate. It runs formatting checks, clippy with
warnings as errors, and the full test suite.

Useful targets:

```bash
make help
make test
make test-machine
make smoke SESSION=s
```

The normal test suite must not require a real herdr. Use `make smoke` only when
you intentionally test against a real herdr instance.

## Pull requests

A good pull request includes:

- The problem being solved and why it matters.
- The design choice made, especially when it differs from existing docs.
- Tests or a clear reason tests were not added.
- Any compatibility, security, data migration, or operational risk.
- Any AI assistance, generated code, copied snippets, or third-party material
  that needs provenance or license review.

Runtime CLI errors should stay JSON on stderr with stable `code` values and
exit 1. Clap usage errors should stay plain text and exit 2.

## Commits

Use a conventional prefix and a plain subject. Include a body when it helps
reviewers understand the why. Do not add `Co-Authored-By` or similar trailers.
