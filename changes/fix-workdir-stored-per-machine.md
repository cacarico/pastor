### Fixed

- `pastor bundle add` of a workdir bundle now checks each `per_machine`
  instructions file it holds as that machine would get it, so a bad one is
  refused at add and not when a task starts on that machine.
- A `per_machine` file of a workdir bundle that is there but cannot be read
  is an error, no longer dropped without a word.
