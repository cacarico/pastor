### Added

- `env` in `[defaults]` (pastor.toml), in a `[[flock]]` and in a `[[machine]]`
  (flock.toml) sets environment for the pane of every agent, so one
  `NOTES_URL` or `CARGO_BUILD_JOBS` need not be repeated in each
  `[agents.<name>]`. A task's agent gets them merged key by key, the agent's
  own `env` over the machine's over the task's flock's over `[defaults]`'s,
  and the pane `task attach` reopens for a closed task's session gets them
  too. A pull machine's serve applies the layers of its own config. A value
  that looks like a secret is refused at load, naming the layer and the key.
- `pastor task describe` lists the env the task's agent gets from the config,
  as an `env` row and in `--json`, each key with the layer that gave it and
  the value of a secret-looking key hidden; through a remote head it reads the
  head's config. For a task on a pull machine it says that machine's own config
  decides.
