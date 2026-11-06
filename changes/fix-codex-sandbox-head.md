### Fixed

- A command run by an agent inside a sandbox that maps file owners, such as
  Codex's, reaches a remote head again: when ssh stops on a system
  configuration file with `Bad owner or permissions`, the request is tried
  once more with the user's own `~/.ssh/config` as the whole configuration.
