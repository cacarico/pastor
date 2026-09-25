# Demo recordings

The gifs in this directory are what the README shows. They are recorded with
[vhs](https://github.com/charmbracelet/vhs) from the `.tape` files here, by
typing real commands at a real head, so they stay honest: re-record them when
the output changes.

```sh
cp docs/demo/flock.example.toml docs/demo/local/flock.toml   # then edit: ssh aliases you can reach
make demo
```

`make demo` starts a head with `PASTOR_CONFIG_DIR=docs/demo/local`, so your
own `~/.config/pastor` is never read or shown, records every tape, and stops
the head. `docs/demo/local/` is ignored by git. Needs `vhs`, `ttyd` and
`ffmpeg` on the recording machine, and `~/work/api` (or whatever the tapes
name) on the machine that takes the task.
