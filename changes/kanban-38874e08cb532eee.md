### Added

- A flock's number on a machine can be a share and a max:
  `machines = { desk = { share = 2, max = 4 } }`. Under its share the flock
  takes a free slot as usual; between share and max it takes one only while
  no task of a flock under its share on that machine is waiting, so slots a
  quiet project leaves idle get used and it gets its share back as soon as it
  has work. The machine's own room still caps everything, and the plain
  `desk = 2` stays a hard ceiling. `flock list` shows `desk 1/2/4`,
  `machine list` `work:2/4`, and `--json` carries `share` and `max`. A head
  needs IPC protocol 26 to read the form, and the CLI refuses an older one
  while flock.toml uses it.
