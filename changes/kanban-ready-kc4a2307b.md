### Added

- Provisioners, the config half: `pastor-provisioner.toml` lists, for one
  platform, the checks a machine must pass to run agents and the command
  that would fix each. `macos`, `raspberry-pi-os` and `arch` are built in,
  and more can be copied into `~/.local/share/pastor/provisioners/<id>/`,
  where a copy with a built-in's id takes its place. A `[[machine]]` names
  its provisioner with `provisioner`, and a `[[flock]]` lists the accounts
  its machines must be logged into with `accounts` (`github`, `claude`);
  both are checked when `flock.toml` loads. Nothing runs a check yet.
