# Developer entry points. Every target maps to one cargo command so the
# Makefile stays the single list of "what you can run here".

.PHONY: help build release check changelog changelog-check fmt lint test test-machine test-ssh leaks smoke smoke-profiles smoke-rc mutants mutants-diff install install-completions completions cli-reference demo site site-serve clean

help: ## list targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F ':.*## ' '{ printf "  %-14s %s\n", $$1, $$2 }'

build: ## debug build of pastor and fake-herdr
	cargo build --all-targets

release: ## optimised build
	cargo build --release

check: changelog-check fmt-check lint test ## what CI and the PR gate run

# A pull request's changelog entry is its own file, changes/<branch>.md, so
# two pull requests never edit the same lines; see changes/README.md.
changelog-check:
	@scripts/changelog.sh check

changelog: ## release PR: gather changes/*.md into CHANGELOG.md: make changelog VERSION=X.Y.Z
	@test -n "$(VERSION)" || { echo 'make changelog: set VERSION=X.Y.Z' >&2; exit 1; }
	scripts/changelog.sh gather $(VERSION)

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

# Starts an sshd of its own on a localhost port, as the user running it, and
# drives the CLI against a head through the real ssh and `pastor bridge`;
# see tests/real_ssh.rs. Needs OpenSSH's server (sshd) and ssh-keygen.
test-ssh: ## the CLI against a head over a real ssh, through a throwaway sshd
	cargo test --test real_ssh -- --ignored

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

# Mutation testing with cargo-mutants (`cargo install cargo-mutants`, or
# mise's cargo:cargo-mutants); which files and what is skipped is in
# .cargo/mutants.toml. Every mutant rebuilds and runs the whole suite, which
# itself runs tests in parallel, so a low job count keeps the host from
# thrashing into timeouts even on a many-core machine (cargo-mutants itself
# recommends starting at 2-3: https://mutants.rs/parallelism.html). A full
# run is hours: run it on an idle machine, not on a pull request. Results
# land in mutants.out/; mutants.out/missed.txt lists the survivors. Override
# with `make mutants MUTANTS_JOBS=n` on a host that can take more.
MUTANTS_JOBS ?= 2
mutants: ## mutation test the transport, head, dispatch and config (hours)
	cargo mutants --jobs $(MUTANTS_JOBS)

# Only the mutants in lines this branch changes since it left origin/main.
# The diff is written to a file first because --in-diff reads a path.
mutants-diff: ## mutation test only what this branch changed against origin/main
	@mkdir -p target
	git diff origin/main... > target/mutants.diff
	cargo mutants --jobs $(MUTANTS_JOBS) --in-diff target/mutants.diff

# Needs herdr 0.9+ running with the named session on this host. Nothing else
# in the suite touches a real herdr, so this is the smoke test to run on a
# fleet machine before trusting it.
smoke: ## opt-in test against a real herdr: make smoke SESSION=default
	PASTOR_REAL_HERDR_SESSION=$(or $(SESSION),default) cargo test --test real_herdr -- --ignored --nocapture

# Needs a running head that runs this pastor, and the machines named in its
# flock.toml; see scripts/smoke-profiles.sh. Starts real agents on them.
smoke-profiles: ## live review task per agent: make smoke-profiles REPO='~/src/x' CLAUDE=m1 OPENCODE=m2
	REPO="$(REPO)" CLAUDE="$(CLAUDE)" OPENCODE="$(OPENCODE)" scripts/smoke-profiles.sh

# Checks the tag out in a scratch worktree under $TMPDIR, builds it there and
# runs make smoke (and make smoke-profiles with PROFILES=1, which needs the
# head on the tag); prints a Markdown block for the release card. LABEL names
# the machine in it; see scripts/smoke-rc.sh.
smoke-rc: ## smoke a release candidate: make smoke-rc TAG=v0.9.0-rc.1 SESSION=default LABEL=arm64
	PROFILES="$(PROFILES)" REPO="$(REPO)" CLAUDE="$(CLAUDE)" OPENCODE="$(OPENCODE)" \
	  scripts/smoke-rc.sh "$(TAG)" "$(or $(SESSION),default)" "$(LABEL)"

install: ## install pastor into ~/.cargo/bin, with bash and fish completions
	cargo install --path . --force --bin pastor
	@$(MAKE) --no-print-directory install-completions

# Writes the completion scripts for the binary `install` just put in place;
# PASTOR_BIN overrides the binary run (for testing against a debug build
# without touching the installed one).
install-completions:
	@set -e; bin="$${PASTOR_BIN:-$${CARGO_HOME:-$$HOME/.cargo}/bin/pastor}"; \
	fish_dir="$${XDG_CONFIG_HOME:-$$HOME/.config}/fish/completions"; \
	bash_dir="$${XDG_DATA_HOME:-$$HOME/.local/share}/bash-completion/completions"; \
	mkdir -p "$$fish_dir" "$$bash_dir"; \
	"$$bin" completions fish > "$$fish_dir/pastor.fish.tmp"; \
	mv "$$fish_dir/pastor.fish.tmp" "$$fish_dir/pastor.fish"; \
	echo "wrote $$fish_dir/pastor.fish"; \
	"$$bin" completions bash > "$$bash_dir/pastor.tmp"; \
	mv "$$bash_dir/pastor.tmp" "$$bash_dir/pastor"; \
	echo "wrote $$bash_dir/pastor"

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
	cp docs/demo/jobs.example/morning.toml docs/demo/local/jobs/morning.toml
	rm -f docs/demo/local/jobs/issues.toml
	rm -rf docs/demo/local/state docs/demo/local/data
	@set -e; \
	export PASTOR_CONFIG_DIR=$(CURDIR)/docs/demo/local PASTOR_STATE_DIR=$(CURDIR)/docs/demo/local/state \
	  PASTOR_DATA_DIR=$(CURDIR)/docs/demo/local/data PATH=$(CURDIR)/target/debug:$$PATH; \
	pastor serve & pid=$$!; trap 'kill $$pid' EXIT; \
	for i in $$(seq 1 100); do pastor task read t-1 2>&1 | grep -q 'not running' || break; sleep 0.3; done; \
	for t in docs/demo/*.tape; do vhs $$t; done; \
	for i in 1 2 3 4; do pastor task close t-$$i >/dev/null 2>&1 || true; done

# The scripts are generated from the clap definitions, so they cannot drift
# from the real command tree; `pastor completions <shell>` prints the same
# thing at runtime for shells not listed here.
completions: ## regenerate contrib/completions/pastor.{bash,fish} from the CLI
	cargo build -q
	mkdir -p contrib/completions
	target/debug/pastor completions bash > contrib/completions/pastor.bash
	target/debug/pastor completions fish > contrib/completions/pastor.fish

# Rendered from the clap definitions like the completions; the page's prose
# above the generated marker is kept. `make check` fails when it is stale.
cli-reference: ## regenerate the website's CLI reference page from the CLI
	PASTOR_WRITE_CLI_REFERENCE=1 cargo test -q --bin pastor website_cli_reference_is_current

# The website is a Hugo site in docs/website; the docs page is docs/manual.md
# itself, mounted, so it cannot drift from the manual. Needs hugo from mise.
site: ## build the website into docs/website/public
	hugo --source docs/website --cleanDestinationDir

site-serve: ## serve the website with live reload on http://127.0.0.1:1313
	hugo server --source docs/website

clean:
	cargo clean
