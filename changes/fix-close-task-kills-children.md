### Fixed

- Closing a task ends what its agent left running. A build started under
  `flock`, `nohup` or `setsid` used to outlive the pane that closed with the
  task, for hours, holding its target directory. Pastor lists what
  runs under the pane's shell and the process groups they are in, and, when the
  checkout goes too, whatever works in it, then ends what still runs once the
  close went through (a refused close ends nothing): `TERM`, five seconds, then
  `KILL`. The task's events say what was ended
  (`task.processes_killed`).
