### Added

- `pastor machine add NAME me@host --bootstrap` and `pastor machine bootstrap
  NAME` set an ssh machine up from the head, as its login user and never with
  sudo: the machine is held while it is pending, its host key is pinned once
  in pastor's own `known_hosts` (`--host-key` checks it against a fingerprint
  or key), pastor and herdr are copied from the head's cache and hashed on the
  machine, `pastor bootstrap` runs there, and on success its agent key goes on
  its marked `authorized_keys` line and the machine is `ok`. A pinned key is
  never replaced: a key that changed, or a scan or record that disagrees with
  it, stops the push. A rerun of a pending or failed machine goes on.
