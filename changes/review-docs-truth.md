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
- A Copilot pass over this docs review: the Tier 2 platform table no longer
  claims every Tier 2 target goes untested, when Linux aarch64 and macOS
  arm64 are smoke-run on release; the connector security model no longer
  calls every connector's files and sockets the head user's, when a
  headless serve's own connectors are the shepherd machine's; the manual's
  refreshed `machine list` transcript no longer prints a stale pastor
  version; the recommended setup no longer claims a permission profile
  stops a task from ever blocking on a question, when it only silences
  tool prompts; and the skill's remote-head section now names `task
  attach`, `machine open` and `serve status`/`serve stop` among the
  commands that stay local.
