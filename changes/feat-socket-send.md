### Changed

- For a Claude task, `pastor task send` text with its Enter, the retry line
  after a short usage limit, and an orchestrator session's last message go
  to the session's inbox socket instead of being typed into its pane, as
  its prompts already do. Each is typed as before when the socket does not
  take it, when herdr shows the agent blocked or still starting, or with
  `delivery = "type"`; `--no-enter`, `--key`, `--pick` and `--trust` stay
  keystrokes. A retry past the first says its attempt, so Claude does not
  drop it as a duplicate. `task.input` carries `socket: true` for a send
  that went that way.
