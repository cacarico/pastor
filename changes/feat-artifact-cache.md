### Added

- The artifact cache: the head keeps the pastor and herdr binaries it will
  put on machines under `~/.local/state/pastor/artifacts/`, downloaded
  once over https and renamed into the cache only once their SHA-256
  matches the published sum. pastor comes from its release tarballs (or the
  head's own binary when it is that version, built for that target), herdr from its release
  index. `[artifacts]` in pastor.toml sets the herdr version and a mirror of
  its index. Nothing installs from the cache yet.
