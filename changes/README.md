# Change files

A pull request adds its changelog entry here as `changes/<branch>.md`, the
branch name with slashes as dashes (`fix/foo` is `changes/fix-foo.md`),
instead of a line in `CHANGELOG.md`. Every pull request editing the same
lines under Unreleased made each merge conflict with the next one; separate
files never do.

A change file holds [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
subsections and their entries, written as they will read in the release
notes:

```markdown
### Fixed

- `pastor machine open` inside a herdr pane fails with `nested_herdr`
  instead of handing the terminal to a herdr that refuses to start.
```

The headings are `Added`, `Changed`, `Deprecated`, `Removed`, `Fixed` and
`Security`. A later commit on the same branch edits the same file.

`make check` fails when `CHANGELOG.md` has an Unreleased section or a change
file is malformed (`scripts/changelog.sh check`). The release pull request
runs `make changelog VERSION=X.Y.Z`, which writes `## X.Y.Z - <today>` into
`CHANGELOG.md` from these files, one subsection per heading and the entries
in the order their files reached `main`, and deletes them.
`scripts/changelog.sh notes` prints what the next release would say.
