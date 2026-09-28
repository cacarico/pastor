---
title: install
summary: one binary, and herdr on each machine
weight: 1
aliases:
  - /docs/install/
---
pastor is a single binary. Install it on the machine that will run the head.
The other machines need herdr and the agents; pastor on them is optional
(see [fleet](../../deploy/fleet/)).

## one line

```sh
curl -fsSL https://cacari.co/pastor/install.sh | sh
```

It downloads the latest release for your OS and CPU, checks it against the
release's `SHA256SUMS`, and puts `pastor` in `~/.local/bin`. When a logged-in
`gh` is around, it also verifies the release's provenance attestation. It
needs `curl`, `tar` and `sha256sum` (or `shasum`): no Rust toolchain and no
sudo. It warns when the folder is not on your `PATH`.

| variable | does |
|---|---|
| `PASTOR_VERSION` | install this version instead of the latest |
| `PASTOR_INSTALL_DIR` | put the binary here instead of `~/.local/bin` |
| `PASTOR_DOWNLOAD_URL` | fetch the tarball from a mirror, or `file:///path` for an offline copy |

On a Raspberry Pi with a 64-bit kernel and a 32-bit userland, it picks the
armv7 build, since a 64-bit binary would not run there.

## with cargo

The crate is called `pastor-cli`; the command is `pastor`.

```sh
cargo binstall pastor-cli  # the prebuilt release
cargo install pastor-cli --locked  # build from source: Rust 1.88+ and a C compiler
```

From a checkout, `make install` puts pastor in `~/.cargo/bin` and writes the
fish and bash completions.

## what else you need

| where | what |
|---|---|
| every machine | herdr 0.9 or newer, with its server running |
| every machine | the agents themselves: `claude`, `codex`, `opencode`, ... |
| the head | ssh to the other machines, without a password or passphrase prompt |
| the head | git, for `pastor connector install` |

A service cannot answer a passphrase prompt, so give the head a key without
one, or an ssh-agent the service can reach. Each connector says what else
it needs.

## check it

```sh
pastor --version
```

## completions

The script comes from the binary, so it always matches it. In fish and bash
it also completes the names it finds in your files: jobs, orchestrators,
flocks, machines, task ids, connectors, models, profiles and priority
levels. zsh, elvish and powershell get the commands and flags only.

```sh
pastor completions fish > ~/.config/fish/completions/pastor.fish
pastor completions bash > ~/.local/share/bash-completion/completions/pastor
pastor completions zsh > ~/.zfunc/_pastor  # any folder on your $fpath
```

## upgrade

Run the same install again: the one line, `cargo binstall`, or
`cargo install`. The script writes the new binary beside the old one and
renames it into place, so a running head keeps the old binary until it
restarts. Restart it yourself:

```sh
pastor serve stop  # a head you started by hand
pastor serve
systemctl --user restart pastor  # a head that runs as a service
```

Until it does, a command that needs something only the new version has
fails with `head_too_old`. Other machines show their new
version in `pastor machine list` within about eleven minutes, with no
restart. If the binary moved, run `pastor setup systemd` (or `setup
launchd`) again so the service points at the new path.

## platforms

| platform | support |
|---|---|
| Linux x86_64 | tested in CI, released |
| Linux aarch64 | built, smoke-run and released |
| Linux armv7, riscv64 | built and released |
| macOS arm64 | built, smoke-run and released |
| macOS x86_64 | built and released |
| Windows | not supported; use WSL2 |

Linux binaries are static (musl), so one download per CPU runs on any
distro.
