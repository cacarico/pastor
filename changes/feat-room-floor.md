### Added

- A machine under its room floor (`min_disk`, `min_memory`) takes no new
  task: dispatch passes it over and the queued task's note says which side
  is short, `waiting for a machine: pi-3 has 1.2 GB free under /tmp`. A
  machine whose room cannot be read counts as having room unless
  `unknown_room = "no_room"` holds it. `task run --now` ignores the floor.
