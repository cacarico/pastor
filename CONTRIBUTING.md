# Contributing

Thanks for helping. pastor is young, and the changes I can review best are
small ones with tests.

## Before you start

- Work on a branch and open a pull request. Do not push to `main`.
- Read `AGENTS.md`, `README.md` and `docs/manual.md` before changing
  behavior.
- Keep vocabulary consistent: machine, flock, head, job, task, connector,
  and agent.
- Do not add runtime dependencies unless the pull request explains why the
  dependency is worth the maintenance and supply-chain cost.

## Development checks

`make check` is the pull request gate. It runs the changelog check,
formatting checks, clippy with warnings as errors, and the full test suite.

Useful targets:

```bash
make help
make test
make test-machine
make test-ssh
make smoke SESSION=s
```

The normal test suite must not require a real herdr. Use `make smoke` only when
you intentionally test against a real herdr instance, and `make
smoke-profiles` (a live review task per agent through a head running your
build) before trusting a change to permission profiles. `make test-ssh` runs
the CLI against a head over a real ssh, through an sshd it starts on a
localhost port as you; it needs OpenSSH's server installed, and CI runs it.

## Pull requests

A good pull request says:

- What it fixes and why that matters.
- The design it picked, above all where that differs from the docs.
- Which tests cover it, or why there are none.
- Any compatibility, security, migration or operational risk.
- Where AI help, generated code or copied material came in, when its
  provenance or license needs a look.

Anything a user would notice also gets a changelog entry, in its own file
`changes/<branch>.md` (slashes in the branch name as dashes), never a line in
`CHANGELOG.md`: lines under one heading made every merge conflict with the
next. `changes/README.md` has the format, and `make check` fails on an
Unreleased section in `CHANGELOG.md` or a malformed change file.

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
  `head_too_old`; the connector protocol; and the minimum herdr version, which
  only rises in a minor bump, noted under Changed.
- Platforms: the tiers follow the README's Platforms table. Tier 1 is built,
  tested in CI and released (Linux x86_64). Tier 2 is built and released;
  Linux aarch64 and macOS arm64 are smoke-run on release too, armv7, riscv64
  and macOS x86_64 are not tested at all. Tier 3 is best effort: compiled in
  CI, no binaries (FreeBSD). Moving a platform down a tier is a breaking
  change.
- The minimum Rust version is `rust-version` in `Cargo.toml`. Raising it is
  a minor bump, never a patch.
- Release when there is something worth shipping, not on a calendar. Cut an
  `-rc.N` prerelease for anything that touches the store schema, the IPC or
  the transport, and smoke it on a fleet machine before the final tag:
  `make smoke-rc TAG=vX.Y.Z-rc.N LABEL=<machine>` checks the tag out in a
  scratch worktree, builds it, runs `make smoke` there (and `make
  smoke-profiles` with `PROFILES=1`, once the head runs the rc) and prints
  a Markdown block for the release card; it never touches your checkout.
  Prereleases never become `install.sh`'s "latest".
- Artifacts are never replaced and a tag is never moved. A bad release gets
  a new patch release and a warning in its own notes.
- Before 1.0, only the latest minor gets fixes.

To release:

1. On a release branch, bump the version in `Cargo.toml` and run `make
   changelog VERSION=X.Y.Z`. It writes `## X.Y.Z - <today>` into
   `CHANGELOG.md` from `changes/*.md`, in the order their files reached
   `main`, and deletes the files. Commit both.
2. Merge the release pull request, and merge nothing else until the tag is
   pushed: a change merged in between would ship in the tag with no line in
   its notes.
3. Push the signed tag `vX.Y.Z` on the release pull request's merge commit.
   `.github/workflows/release.yml` builds the tarballs and drafts the GitHub
   release with that section as notes.
4. Check the tag's signature with `git tag -v vX.Y.Z`. An unsigned or
   unverified tag is deleted, not published.
5. Publish the draft. That also publishes the crate: the workflow's
   `publish-crate` job runs `cargo publish` from the tag with the
   `CARGO_REGISTRY_TOKEN` repo secret. It goes to crates.io as `pastor-cli`,
   since `pastor` is taken there, and installs the `pastor` binary only
   (`fake-herdr` is left out of the package). `cargo publish --dry-run` from
   the tag shows what a manual one would upload.

A prerelease tag leaves the change files in place and takes its notes from
them (`scripts/changelog.sh notes`), and is skipped by `publish-crate`.

## Commits

Use a conventional prefix and a plain subject. Include a body when it helps
reviewers understand the why. Do not add `Co-Authored-By` or similar trailers.
