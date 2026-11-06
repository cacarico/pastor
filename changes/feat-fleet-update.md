### Added

- `pastor fleet update <version> [--machine M] [--dry-run] [--json]` puts a
  pastor release from the head's artifact cache on every machine the head
  reaches over ssh, and on the head last. It looks at every machine first
  and changes nothing if the cache lacks a build; then, one machine at a
  time, copies the binary and checks its sha256 there, installs it into
  `~/.local/bin`, reruns `pastor setup` for the units the machine has,
  restarts pastor's serve if one runs and waits for it to answer as the new
  version. The first failure stops the rollout and puts that machine's old
  binary back. herdr and the agents' panes are not touched, so tasks keep
  running; pull and `command` machines are skipped.
