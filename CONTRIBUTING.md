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

## Releases

- SemVer. While pastor is 0.x, a minor bump (0.4 to 0.5) may break and a
  patch never does. From 1.0 the usual rules apply.
- What "break" covers: the CLI (command names, flags, exit codes and the
  error `code` strings that scripts match on); `--json` output, where fields
  are added but never removed or retyped; the config files, which must keep
  loading (a removed key gets one deprecation cycle with a warning); the
  store, whose migrations are forward-only and automatic, with no downgrade
  (an older binary refuses a newer database); the CLI to head IPC, where a
  CLI and head from the same minor always work together and a mismatch is
  `head_too_old`; the plugin protocol; and the minimum herdr version, which
  only rises in a minor bump, noted under Changed.
- Platforms: the README lists the tiers. Tier 1 is built and tested on every
  release, tier 2 built but not tested, tier 3 best effort. Moving a platform
  down a tier is a breaking change.
- The minimum Rust version is `rust-version` in `Cargo.toml`. Raising it is
  a minor bump, never a patch.
- Release when there is something worth shipping, not on a calendar. Cut an
  `-rc.N` prerelease for anything that touches the store schema, the IPC or
  the transport, and run `make smoke` on a fleet machine against it before
  the final tag. Prereleases never become `install.sh`'s "latest".
- To release: bump the version in `Cargo.toml`, turn `Unreleased` in
  `CHANGELOG.md` into `## X.Y.Z - date`, merge, and push the signed tag
  `vX.Y.Z` on `main`. `.github/workflows/release.yml` builds the tarballs
  and drafts the GitHub release with that section as notes; read it, then
  publish.
- Artifacts are never replaced and a tag is never moved. A bad release gets
  a new patch release and a warning in its own notes.
- Before 1.0, only the latest minor gets fixes.

## Commits

Use a conventional prefix and a plain subject. Include a body when it helps
reviewers understand the why. Do not add `Co-Authored-By` or similar trailers.
