### Changed

- `pastor bundle add` warns when `flock.toml` already lacks a var its
  bundles need. Such a gap is not the new version's doing, so the version
  still goes in, but a head will not load `flock.toml` until it is set.
