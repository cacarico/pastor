# pastor open refuses inside herdr: Implementation Plan

> **For agentic workers:** this plan runs on a pastor flock, one task at a time. Each task's prompt file is under `docs/superpowers/plans/2026-09-26-nested-herdr/`. Do only the task your prompt names.

**Goal:** `pastor open <machine>` run inside a herdr pane fails at once with the code `nested_herdr` and a message that says to use a plain terminal, instead of exec'ing herdr and letting herdr refuse.

**Architecture:** herdr sets `HERDR_ENV=1` in every pane it starts. `open` in `src/main.rs` checks it first, before loading the flock, and calls the existing `fail` helper. No new dependency, no daemon change.

**Spec:** the Known gaps list in `AGENTS.md`: "`pastor open` should detect a nested herdr and say so instead of herdr refusing to start."

**Global Constraints:**

- Rust edition 2024, toolchain from mise. `make check` is the gate.
- Runtime CLI errors are JSON on stderr with a stable code and exit 1.
- Conventional commits with a body that says why. No trailers.
- Nothing from the fleet in a commit: no addresses, user names or home paths.

**Review Focus:** the check runs before anything else in `open`, and only `HERDR_ENV=1` counts.

## Pastor

- Plan branch: `pastor/nested-herdr` (the plan, the ledger and the prompt files live here)
- Ledger: `docs/superpowers/plans/2026-09-26-nested-herdr.ledger.md`
- Repo on the machines: `~/work/pastor`
- Check: `make check`
- Order: series. Start task N+1 only when the ledger has `Task N: complete`.
- Before merging: drop `docs/superpowers/plans/2026-09-26-nested-herdr*` from the branch; this repo keeps its plans on the `docs` branch.

---

### Task 1: Refuse `pastor open` inside a herdr pane

**Files:**

- Modify: `src/main.rs` (`fn open`)
- Test: `tests/cli.rs`

**Interfaces:**

- Consumes: `fail(code, message)` in `src/main.rs`; `pastor()` and `error_code` in `tests/cli.rs`.
- Produces: the error code `nested_herdr`.

- [ ] **Step 1: Write the failing test**

Append to `tests/cli.rs`:

```rust
#[test]
fn open_refuses_inside_a_herdr_pane() {
    let tmp = tempfile::tempdir().unwrap();
    let out = pastor()
        .args(["open", "pi-3"])
        .env("PASTOR_CONFIG_DIR", tmp.path())
        .env("PASTOR_STATE_DIR", tmp.path())
        .env("HERDR_ENV", "1")
        .output()
        .unwrap();
    assert_eq!(error_code(&out), "nested_herdr");
}
```

The machine does not exist on purpose: the check must come before the flock is read.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --test cli open_refuses_inside_a_herdr_pane`
Expected: FAIL, the code is `unknown_machine`, not `nested_herdr`.

- [ ] **Step 3: Implement**

At the top of `async fn open` in `src/main.rs`, before `Flock::load`:

```rust
    // herdr refuses to start inside one of its own panes, and says so only
    // after the terminal has been handed over; saying it here is clearer.
    if std::env::var_os("HERDR_ENV").is_some_and(|v| v == "1") {
        fail(
            "nested_herdr",
            "this terminal is a herdr pane; run pastor open from a plain terminal",
        );
    }
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cargo test --test cli open_refuses_inside_a_herdr_pane`
Expected: PASS.

- [ ] **Step 5: Check and commit**

Run: `make check`
Expected: fmt, clippy and the suite all pass.

```bash
git add src/main.rs tests/cli.rs
git commit -m "fix(open): refuse to start inside a herdr pane"
```

with a body saying herdr would refuse anyway, after taking the terminal.

**Dispatch:**

- flock: `default`, tags: none, machine: any
- model: `sonnet`. Why: two files, the plan holds the code and the test.
- timeout: `45m`
- depends on: nothing

```bash
pastor task run --prompt-file docs/superpowers/plans/2026-09-26-nested-herdr/task-1.md \
  --flock default --repo '~/work/pastor' --worktree --branch pastor/nested-herdr-1 \
  --agent claude --agent-arg --model --agent-arg sonnet --timeout 45m --json
```

---

### Task 2: Document the refusal

**Files:**

- Modify: `docs/manual.md` (the paragraph on `pastor open` under "How it works")
- Modify: `AGENTS.md` (Known gaps)
- Modify: `CHANGELOG.md` (Unreleased)

**Interfaces:**

- Consumes: the `nested_herdr` code from Task 1.
- Produces: nothing.

- [ ] **Step 1: Manual**

In `docs/manual.md`, replace "herdr refuses to start inside one of its own panes, so run it from a plain terminal." with "herdr refuses to start inside one of its own panes, so pastor checks `HERDR_ENV` first and fails with `nested_herdr`; run it from a plain terminal."

- [ ] **Step 2: Known gaps**

In `AGENTS.md`, delete the Known gaps item that starts "`pastor open` should detect a nested herdr".

- [ ] **Step 3: Changelog**

Under `## Unreleased` in `CHANGELOG.md`, add a `### Fixed` section if there is none, with the item: "`pastor open` inside a herdr pane fails with `nested_herdr` instead of handing the terminal to a herdr that refuses to start."

- [ ] **Step 4: Check and commit**

Run: `make check`
Expected: pass.

```bash
git add docs/manual.md AGENTS.md CHANGELOG.md
git commit -m "docs: say that pastor open refuses inside herdr"
```

with a body saying the known gap is closed.

**Dispatch:**

- flock: `default`, tags: none, machine: any
- model: `sonnet`. Why: three small text edits, all spelled out.
- timeout: `30m`
- depends on: Task 1

```bash
pastor task run --prompt-file docs/superpowers/plans/2026-09-26-nested-herdr/task-2.md \
  --flock default --repo '~/work/pastor' --worktree --branch pastor/nested-herdr-2 \
  --agent claude --agent-arg --model --agent-arg sonnet --timeout 30m --json
```
