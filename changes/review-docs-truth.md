### Fixed

- `pastor task run --help` names the flock before the machine for
  `--model`, `--profile` and `--priority`, the order pastor reads them in.
- The manual, the website pages and the skill no longer contradict the
  code on the points the docs review of 0.8.0 found wrong or stale, among
  them: orchestrator agents take a `max_agents` slot, the remote-head ssh
  line and the commands sent to a head, what a headless serve runs, the
  protocol and schema numbers, and the permission mode under a profile.
- `pastor task attach` on a closed Claude task with no repo reopens its
  session in `~/pastor-tasks`, where it ran. It passed no directory, so
  herdr opened the pane wherever its focused pane was and `claude --resume`
  could not find the session.
- The second docs review's findings: a remote `task run` takes only
  `timeout` and `place` from the built-in defaults, `queue move` ignores
  paused tasks, a paused task resumes only while its flock has room on its
  machine, the full list of commands refused from a task's pane, a failed
  task holds no slot, hooks without `only_own` get other connectors' text
  blanked, and the spec skill names models with `--model` so a plan task
  keeps its flock's agent and account.
- `make demo` stops its demo head again: it runs `pastor serve
  --foreground`, since a plain `pastor serve` forks and returns.
