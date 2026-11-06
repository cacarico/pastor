### Fixed

- A workdir removal reported again for a set the head no longer has a record
  of (sent twice, or answered late) no longer raises `workdir.removed` a
  second time.
- `machine describe` shows the workdir sets of a pull or command machine as
  `reported`, not `unread`: the head never reads those machines.
