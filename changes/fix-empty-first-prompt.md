### Fixed

- A task's prompt waits while a startup dialog of the agent's own
  `[[agents.<name>.dialogs]]` rows shows, such as agy's new-model banner,
  instead of being typed into it and lost: herdr reads agy's dialogs idle
  and ready. pastor answers the row, then sends the prompt, also right after
  a `--trust` answer that the banner follows.
- An `agy` task in a folder agy has not seen waits at agy's folder-trust
  dialog with its prompt held, as a Claude task does, instead of having the
  prompt typed into the dialog: it was lost, and its Enter took "Yes, I
  trust this folder" with nobody asked. agy's trust marker and keys
  (`Enter`) are built in, so `pastor task send --trust` answers it without
  any `[agents.<name>]` setting, and checks the dialog is on that option
  before it presses `Enter`.
