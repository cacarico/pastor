### Added

- `pastor machine check` picks a built-in provisioner for a derivative of a
  distribution it is for, through the `ID_LIKE` of `/etc/os-release`: Linux
  Mint and elementary OS get `debian` (or `raspberry-pi-os` on a Raspberry
  Pi), EndeavourOS and Manjaro get `arch`. The `debian` built-in now names
  Ubuntu itself. The report starts with a `provisioner`
  row that says which was picked and why, and the row of a machine nothing
  fits names its `ID_LIKE`.
