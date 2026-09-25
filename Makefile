# Developer entry points. Every target maps to one cargo command so the
# Makefile stays the single list of "what you can run here".

.PHONY: help build release check fmt lint test test-machine leaks smoke install completions clean

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
# branch can loosen the policy it is checked against. Needs the gitleaks
# binary (a system package, not a cargo one).
GITLEAKS_VERSION ?= 8.30.1
leaks: ## scan the whole git history for secrets, as CI does
	@set -e; tmp=$$(mktemp -d); trap 'git worktree remove --force $$tmp/wt >/dev/null 2>&1 || true; rm -rf $$tmp' EXIT; \
	curl -sSfL -o $$tmp/gitleaks.toml https://raw.githubusercontent.com/gitleaks/gitleaks/v$(GITLEAKS_VERSION)/config/gitleaks.toml; \
	mkdir $$tmp/no-ignore; \
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
completions: ## regenerate contrib/completions/pastor.{bash,fish} from the CLI
	cargo build -q
	mkdir -p contrib/completions
	target/debug/pastor completions bash > contrib/completions/pastor.bash
	target/debug/pastor completions fish > contrib/completions/pastor.fish

clean:
	cargo clean
