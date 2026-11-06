### Added

- A Claude or agy task ends when its agent goes idle with `DONE` alone as the
  last line of its last message, as if it had run `pastor task done`: it is
  done at once, with the same event, and the message is its summary (so a
  task that requires a summary does not fail as one stopped without). A
  `DONE` that is not the last line, a different word on the line, or a
  message that stops on a question ends nothing.
- Every Claude task is allowed `Bash(pastor task done:*)` whatever its
  profile, and a profiled agy task gets `command(pastor task done)` in the
  allow list pastor writes into its settings, so `review` and a task with no
  profile can always end themselves. `pastor profile render` shows it.
- From a task's pane `pastor task done` refuses the `--head` flag and
  `--summary-file` (standard input included): the summary is `--summary`
  text, so a task cannot reach another head or read a file past its deny
  rules. The head `PASTOR_HEAD` names is its own and still works. pastor's
  own prompts and error hints say `--summary`.
