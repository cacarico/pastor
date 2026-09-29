---
title: install
summary: one binary, no root
group: start
weight: 10
---

pastor is a single binary. Install it on the machine that will run the head;
the other machines need herdr and the agents.

## one line

```sh
curl -fsSL https://cacari.co/pastor/install.sh | sh
```

It downloads the release for your OS and CPU, checks it against the
release's checksums, and puts `pastor` in `~/.local/bin`. No Rust toolchain
and no sudo.

## with cargo

The crate is called `pastor-cli`; the command is `pastor`.

```sh
cargo binstall pastor-cli  # the prebuilt release
cargo install pastor-cli --locked  # build from source: Rust 1.88+ and a C compiler
```

From a checkout, `make install` puts pastor in `~/.cargo/bin` and installs
the fish and bash completions.

## what else you need

| where | what |
|---|---|
| every machine | herdr 0.9 or newer, with its server running |
| every machine | the agents themselves: `claude`, `opencode`, ... |
| the head | ssh to the other machines, without a passphrase prompt |
| the head | git, for `pastor connector install` |

A service cannot answer a passphrase prompt or reach your ssh-agent, so use
a dedicated key or Tailscale SSH.

## check it

```sh
pastor --version
```

## completions

The script is generated from the binary, so it always matches it. In bash
and fish it also completes job, machine and flock names and task ids.

```sh
pastor completions fish > ~/.config/fish/completions/pastor.fish
pastor completions bash > ~/.local/share/bash-completion/completions/pastor
```

## platforms

| platform | support |
|---|---|
| Linux x86_64, aarch64 | built, tested in CI and released |
| Linux armv7, riscv64 | built and released |
| macOS arm64, x86_64 | built and released |
| Windows | not supported; use WSL2 |

Linux binaries are static, so one download per CPU runs on any distro.
