### Added

- `workdir place --local` and `remove --local` on a pull or command machine
  tell the head what they did, so `workdir list` and the `workdir.*` events
  see those machines too. A head that does not answer is told with the next
  run, or the next tick of `pastor serve` on a pull machine; a set with a
  profile still needs the head and fails `head_unreachable` before it writes.
  `workdir remove --local` takes `--machine`, like `place`.
