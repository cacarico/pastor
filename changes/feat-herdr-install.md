### Added

- `pastor machine install` also takes herdr to the version `[artifacts] herdr`
  pins in pastor.toml (`--kind herdr` for it alone). The head fetches the
  release from herdr's release index into the artifact cache, checks its
  SHA-256 there and again on the machine, and links `~/.local/bin/herdr` to
  it only once it says the pinned version; on any failure the old version
  stays. A `herdr` that is a file is never replaced, and a checksum mismatch
  stops with no fallback. An update is skipped, and the row says so, while
  herdr lists an agent named `t-<id>` on the machine, whatever its task's
  state.
- `pastor machine check` has a `herdr-version` row against `[artifacts]
  herdr`, naming `pastor machine install <machine> --kind herdr` when it
  drifts, and a `herdr-running` warning when the herdr server running on the
  machine is not the version installed, since a running server keeps its
  version until it is restarted.
