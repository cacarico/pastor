# Developer entry points. Every target maps to one cargo command so the
# Makefile stays the single list of "what you can run here".

.PHONY: help build release check fmt lint test test-machine leaks smoke install completions demo clean

help: ## list targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F ':.*## ' '{ printf "  %-14s %s\n", $$1, $$2 }'

build: ## debug build of pastor and fake-herdr
	cargo build --all-targets

release: ## optimised build
	cargo build --release

check: fmt-check lint test ## what CI and the PR gate run

fmt: ## rewrite sources with rustfmt
	cargo fmt

fmt-check:
	cargo fmt --check

lint: ## clippy with warnings as errors
	cargo clippy --all-targets -- -D warnings

test: ## whole suite, including the end-to-end CLI tests against the fake herdr
	cargo test

test-machine: ## the machine actor tests five times, to catch timing flakes
	@for i in 1 2 3 4 5; do cargo test --lib machine:: -q || exit 1; done

# The repository is public; CI runs this same scan. The rules are gitleaks'
# own defaults at the pinned version, fetched outside the checkout; the scan
# runs in a throwaway worktree with any .gitleaksignore or gitleaks.toml
# removed (gitleaks reads ./.gitleaksignore whatever --gitleaks-ignore-path
# says), and inline gitleaks:allow comments are ignored, so nothing in a
# branch can loosen the policy it is checked against. Every remote's refs are
# fetched first and a shallow clone is refused, so the scan covers the same
# history CI sees. Needs the gitleaks binary (a system package, not a cargo
# one).
GITLEAKS_VERSION ?= 8.30.1
leaks: ## scan the whole git history for secrets, as CI does
	@set -e; tmp=$$(mktemp -d); trap 'git worktree remove --force $$tmp/wt >/dev/null 2>&1 || true; rm -rf $$tmp' EXIT; \
	curl -sSfL -o $$tmp/gitleaks.toml https://raw.githubusercontent.com/gitleaks/gitleaks/v$(GITLEAKS_VERSION)/config/gitleaks.toml; \
	mkdir $$tmp/no-ignore; \
	if [ "$$(git rev-parse --is-shallow-repository)" = true ]; then echo 'make leaks: this clone is shallow; run git fetch --unshallow first' >&2; exit 1; fi; \
	git fetch --quiet --all; \
	git worktree add --quiet --detach $$tmp/wt HEAD; \
	rm -f $$tmp/wt/.gitleaksignore $$tmp/wt/.gitleaks.toml $$tmp/wt/gitleaks.toml; \
	gitleaks git --redact --no-banner --exit-code 1 --ignore-gitleaks-allow --config $$tmp/gitleaks.toml --gitleaks-ignore-path $$tmp/no-ignore --log-opts="--all" $$tmp/wt

# Needs herdr 0.9+ running with the named session on this host. Nothing else
# in the suite touches a real herdr, so this is the smoke test to run on a
# fleet machine before trusting it.
smoke: ## opt-in test against a real herdr: make smoke SESSION=default
	PASTOR_REAL_HERDR_SESSION=$(or $(SESSION),default) cargo test --test real_herdr -- --ignored --nocapture

install: ## install pastor and fake-herdr into ~/.cargo/bin
	cargo install --path . --force

# The scripts are generated from the clap definitions, so they cannot drift
# from the real command tree; `pastor completions <shell>` prints the same
# thing at runtime for shells not listed here.
# Records docs/demo/*.gif with vhs (https://github.com/charmbracelet/vhs; needs
# vhs, ttyd, ffmpeg and fish). A demo head runs against docs/demo/local/,
# which is not committed: copy flock.example.toml there and point it at
# machines you can ssh to. Config, state and data dirs are all under it, so
# nothing from your real setup is read, run or shown, and state is reset
# before each recording so task ids start at t-1. The tapes run the pastor
# just built, not an installed one, and start once the head answers.
demo: build ## record the README gifs with vhs against a demo head
	mkdir -p docs/demo/local/jobs
	cp -n docs/demo/flock.example.toml docs/demo/local/flock.toml
	cp -n docs/demo/jobs.example/hourly.toml docs/demo/local/jobs/hourly.toml
	rm -rf docs/demo/local/state docs/demo/local/data
	@set -e; \
	export PASTOR_CONFIG_DIR=$(CURDIR)/docs/demo/local PASTOR_STATE_DIR=$(CURDIR)/docs/demo/local/state \
	  PASTOR_DATA_DIR=$(CURDIR)/docs/demo/local/data PATH=$(CURDIR)/target/debug:$$PATH; \
	pastor serve & pid=$$!; trap 'kill $$pid' EXIT; \
	for i in $$(seq 1 100); do pastor task read t-1 2>&1 | grep -q 'not running' || break; sleep 0.3; done; \
	for t in docs/demo/*.tape; do vhs $$t; done

completions: ## regenerate contrib/completions/pastor.{bash,fish} from the CLI
	cargo build -q
	mkdir -p contrib/completions
	target/debug/pastor completions bash > contrib/completions/pastor.bash
	target/debug/pastor completions fish > contrib/completions/pastor.fish

clean:
	cargo clean
