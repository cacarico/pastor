# Demo recordings

The gifs in this directory are what the README shows. They are recorded with
[vhs](https://github.com/charmbracelet/vhs) from the `.tape` files here, by
typing real commands at a real head, so they stay honest: re-record them when
the output changes.

```sh
mkdir -p docs/demo/local
cp docs/demo/flock.example.toml docs/demo/local/flock.toml   # then edit: ssh aliases you can reach
make demo
```

`make demo` starts a head whose config, state and data directories all sit
under `docs/demo/local/`, so your own config, task history and connectors are
never read, run or shown; it clears the demo state first so ids start at
`t-1`, records every tape, and stops the head. `docs/demo/local/` is ignored
by git. Needs `vhs`, `ttyd`, `ffmpeg` and `fish` (the tapes' shell) on the
recording machine, and the repo the tapes name (a checkout of pastor at
`~/ghq/github.com/cacarico/pastor`, one the agent there already trusts:
Claude Code asks before working in a folder it has never seen, and that
question blocks the task) on the machine that takes the task. The tapes are
numbered so the task recording runs before the jobs one fill the machine.
