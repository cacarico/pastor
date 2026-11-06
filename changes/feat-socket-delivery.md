### Changed

- A Claude task's prompt goes to its session as one message through the
  session's inbox socket instead of being typed into its pane: the first
  prompt, one held while the agent was blocked or launching, and one sent
  again because the agent did not take it. Typing stays the fallback for
  anything the session does not accept, and `[agents.<name>] delivery =
  "type"` keeps an agent on typing.
