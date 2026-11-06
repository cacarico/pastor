### Added

- `pastor machine install <machine> [--kind K | --agent A] [--dry-run]
  [--json]` installs or updates each harness the machine's flock pins to the
  pinned version, as the machine's user, with no sudo. It tries the
  manifest's sources in order. A release file is fetched by the head into
  the artifact cache, checked against its published SHA-256, and checked
  again on the machine; a mismatch is refused. npm and the vendor's
  installer run on the machine. A release or npm install goes into
  `~/.local/share/pastor/harnesses/<id>/<version>/` and is linked from
  `~/.local/bin` only once it says the pinned version; a vendor's installer
  that brings another version is undone. Either way, a failed install
  leaves the old version in place. `--dry-run` lists the source and steps
  and changes nothing.
- A harness manifest's `[update] env` is set for agents of a kind their
  machine pins to a version, so the harness does not update itself. The
  built-in `claude` sets `DISABLE_AUTOUPDATER=1`, and passes the pin to its
  installer through the new `args` of a `script` source.

### Changed

- `pastor machine check`'s `harness-version` warning names the fix,
  `pastor machine install <machine> --kind <kind>`.
