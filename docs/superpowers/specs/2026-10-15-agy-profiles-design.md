# Permission profiles for agy

Date: 2026-10-15. Branch `feat/agent-profiles`. Reviewed by an agy task
(t-1201) on 2026-10-15; its findings are folded in.

## Problem

A task with `--profile` is refused for an agy agent (`agent_tools_unsupported`)
unless `[agents.<name>]` says `own_permissions = true`, and with that field
pastor adds nothing: agy keeps its own permission mode and asks before every
command the machine's settings do not allow. Task t-1189, a review on agy,
sat `blocked` on agy's prompt for `git fetch` and would have asked again for
every `git log`, `git diff` and `git show`. A profile is supposed to mean the
agent never stops at a prompt and never does what the profile denies. Claude,
Codex and opencode have that; agy does not.

## What agy can take

Checked on the binary (`agy --help` of 1.2.14, the fleet's version, and
1.3.5) and the docs at antigravity.google (`/docs/permissions?tab=cli`,
`/docs/settings`, `/docs/cli/modes`, `/docs/cli/headless`).

- No flag and no environment variable carries permission rules. The only
  `AGY_CLI_*` variables are rendering and auth knobs.
- `--dangerously-skip-permissions` puts the session in the `always-proceed`
  tool permission mode: every `ask` is approved. A probe on 2026-10-14 showed
  `deny` rules still hold under that mode (`shutdown --help` was refused with
  `Matches user-configured deny rule`; `git push`, an `ask` rule, ran).
- `--mode accept-edits` skips the diff review before each file write that
  the default mode (`request-review`) stops for. `--mode auto` exists only
  from 1.3.5 and is not used.
- Rules live on the machine, in `permissions.allow`, `permissions.deny` and
  `permissions.ask` of `~/.gemini/antigravity-cli/settings.json` (global)
  and `~/.gemini/config/config.json` (shared with the desktop app), and in
  `~/.gemini/config/projects/*.json`, of which agy loads only the project
  that matches its active workspace. Deny wins over ask, ask over allow. A
  rule is `action(target)`: `command(<words>)` is a word-by-word prefix
  match, `command(regex:<re>)` a regular expression, `command(*)` any
  command; `write_file(<path>)` and `read_file(<path>)` take an absolute or
  workspace-relative path, a directory covering what is under it, or `*`;
  `read_url(<host>)`, `execute_url(<host>)`, `mcp(<server>/<tool>)`,
  `mcp(<server>/*)`; `unsandboxed(<words>)` is a command outside the
  sandbox. Denying `read_file` on a path also blocks `write_file` on it.
- agy refuses by design to read rules from the checkout, so a branch under
  review cannot lift them.

So agy's policy is per machine, not per task. Pastor cannot narrow one agy
session without narrowing every agy session on that machine, and cannot
express "refuse what the profile does not allow" at all: the closest it gets
is "never ask, and deny what the profile denies".

And a deny of the edit tools is a deny of agy's edit tools only. Under the
skip flag every shell command the settings do not deny runs, so `sed -i`,
`echo >` or `git commit` in a command can still change the checkout. A
`review` task on agy therefore promises: no prompt, no `Edit`/`Write`-class
tool, no `git push`, no `rm -rf`, no `sudo`; it does not promise a read-only
checkout. Codex under `review` has the same shape (its sandbox may write the
workspace), and the manual says so for both.

## Design

### Launch

A task under a profile whose agent is of kind `agy`, without
`own_permissions`, starts with `--dangerously-skip-permissions --mode
accept-edits` after its args, under every profile. No tool flags: agy has
none, and `allow_flag`/`deny_flag` on an agy definition keep working as for
any kind when no profile applies.

Agent args that already set `--mode`, `--mode=<x>` or
`--dangerously-skip-permissions` are refused with `profile_args_conflict`,
as for claude and codex. `--sandbox` is not a permission mode and passes.

### Translating the deny list

Each Claude pattern in the task's settled `deny` list (`Profiles::apply`
has already merged the profile's and the task's own) translates to the agy
rule it requires (`config::agy::required_rule`):

| Claude pattern | agy rule |
| --- | --- |
| `Edit`, `Write`, `NotebookEdit`, `MultiEdit` | `write_file(*)` |
| `Edit(<path>)` and the others with a path | `write_file(<path>)` |
| `Read` / `Read(<path>)` | `read_file(*)` / `read_file(<path>)` |
| `Bash` | `command(*)` |
| `Bash(<words>:*)` and `Bash(<words>)` | `command(<words>)` |
| `WebFetch` / `WebFetch(domain:<host>)` | `read_url(*)` / `read_url(<host>)` |
| `mcp__<server>__<tool>` / `mcp__<server>` | `mcp(<server>/<tool>)` / `mcp(<server>/*)` |
| anything else (`Glob`, `Grep`, `WebSearch`, `Task`, an unknown tool) | none: refused |

`Bash(<words>)` without `:*` is an exact match in Claude and a prefix match
in agy; a broader deny is safe, so it maps the same way. A path's trailing
`/**` or `/*` is dropped, since an agy directory rule covers what is under
it, and a leading `~` passes as written, as it does in Claude's flags.

A pattern with no agy rule refuses the task at queue time, in
`launch_args`, with `agy_deny_unsupported`: the message names the pattern
and says agy has no rule for it; drop it from the profile or the task's
list, or run without a profile. This needs no machine, so `task run` and a
job's dispatch refuse it at once, as they do `agent_tools_unsupported`.

### Picking a machine

An agy machine is one profile at a time (see below), and its own profile
(its `[[machine]]` entry, else its flock's, else `[defaults]`) says which.
When a task of kind agy under a profile is settled on a machine whose own
profile is set, the task's deny patterns must each be in the machine's
profile's resolved deny list (string equality on Claude patterns, before
translation); otherwise the machine refuses it with `agy_profile_mismatch`.
An unpinned task skips such a machine the way it skips one with
`model_kind_mismatch`, and waits with that reason when none fits; a pinned
one is refused. A machine with no own profile is tried: the dispatch check
below decides.

### Check at dispatch

Before launch, in `dispatch` where the opencode conflict check runs, pastor
asks the machine for agy's rules and checks that every required rule is
covered.

The probe is one shell command (`config::agy::RULES_COMMAND`), run through
the transport's `probe` like opencode's. It prints, for the global file and
the shared file, a line `==== <path>` and the file's content when the file
exists. Project files are not read: agy applies only the one matching the
active workspace, which for a task is its checkout, and pastor cannot tell
which file that is; a rule only a project file holds does not count. The
probe refuses more than 64 KiB; two files stay well under it, and an
oversized or unreadable one fails the task, on the safe side.

The head parses each file as JSON and collects its top-level
`permissions.deny` strings. A file that does not parse refuses the task,
naming it: a rule pastor cannot read may be one it needs.

A required rule is covered when a deny in either file is:

- the same rule;
- the action's `*`;
- for `command`, a rule whose words are a leading subsequence of the
  required words (`command(git push)` covers `command(git push --force)`);
- for `write_file` and `read_file` with an absolute required path, a rule
  whose absolute path is the required path or a directory above it on
  component boundaries (`std::path::Path::starts_with`, so `/home/u/dir`
  does not cover `/home/u/dir2`), or a `read_file` deny of such a path when
  `write_file` is required (agy's implicit rule). A relative settings path
  covers only an identical required path: pastor does not know what agy
  resolves it against.

A `regex:` rule covers nothing: pastor does not evaluate regular
expressions. An `ask` or `allow` entry covers nothing: under the skip flag
an `ask` is approved.

Refusals, before anything is made on the machine, with the task's `error`
set as for `opencode_permissions_conflict`:

| Code | When | Message |
| --- | --- | --- |
| `agy_permissions_missing` | a required rule is in neither file | lists every missing rule as it should be written, one per line, and the global file's path |
| `agy_permissions_missing` | the probe cannot run (a `command` machine), or a file does not parse or is too large | says why pastor could not read the rules; unlike opencode's check, this does not go ahead, because going ahead means the skip flag with nothing verified behind it |

A task whose settled deny list is empty (`unrestricted` with no task list)
skips the probe. The unrestricted rule holds as for any agent.

### What `task describe` says

`lists_unapplied` is one note shown after both lists. For an agy task under
a profile it reads: `agy takes no lists: the deny was checked against its
settings on the machine, the allow is not passed`. Shown whenever either
list is not empty.

### `own_permissions`

Keeps its meaning for every kind: pastor adds no flag, no mode and runs no
check. An agy owner who manages the file by hand and does not want the skip
flag keeps it. The manual stops saying agy needs it.

### Other kinds

Claude, Codex and opencode are unchanged. A `kind` that is none of the four
is unchanged too: with `allow_flag`/`deny_flag` the lists are passed and no
never-ask mode is added; without them a list refuses the task
(`agent_tools_unsupported`). The Agent kinds table says so in a sentence
under it.

### Consequence for the fleet

An agy machine is one profile at a time. `review` needs `write_file(*)`
denied globally, which stops every edit on that machine, so a machine that
runs `review` tasks on agy cannot run `develop` ones, nor the owner's own
agy sessions that edit. The manual says: give each agy machine the profile
in its `[[machine]]` entry, write that profile's rules into its agy
settings, and let tasks on it use that profile. Writing the rules is a
human step in this change; pastor's provisioner work takes it over.

## Code

- `src/config/agy.rs` (new): `KIND`, `NO_ASK` (the two flags),
  `RULES_COMMAND`, `required_rule(pattern) -> Option<String>`,
  `parse_denies(output) -> Result<Vec<String>, String>` (the probe's output
  to the deny rules found, or the file that failed), `missing(required,
  denies) -> Vec<String>`, `is_permission_arg(arg)`,
  `AGY_DENY_UNSUPPORTED`, `AGY_PERMISSIONS_MISSING`, `AGY_PROFILE_MISMATCH`.
  Unit tests and proptests as in `opencode.rs`: translation never panics,
  covering is reflexive and `*` covers all.
- `src/config/mod.rs`: `launch_args` adds the agy branch (conflict check,
  `agy_deny_unsupported`, then `NO_ASK`, no tool flags), `lists_unapplied`
  the agy note, a new `agy_profile(spec) -> bool` like `opencode_profile`.
  Without a profile nothing changes: a list still needs flags
  (`agent_tools_unsupported`). Where a task is settled on a machine
  (`resolve_agent_on` and its callers in `daemon.rs`), the own-profile
  comparison and `agy_profile_mismatch`, skippable like
  `model_kind_mismatch`.
- `src/herdr/transport.rs`: `agy_rules(&self) -> Future<Option<String>>`
  on the `Connector` trait, `None` by default, the probe's raw output on
  local and ssh endpoints; `src/herdr/fake.rs` gets a field to stub it.
- `src/dispatch.rs`: after the opencode check, the agy check with
  `agy_permissions_missing`.
- `scripts/smoke-profiles.sh`: `AGY=<machine>` and `AGY_AGENT`, a third
  `smoke` call. Its prompt already asks for a write that must be refused.
- Docs: `docs/manual.md` (Permission profiles: an agy section in place of
  the `own_permissions` paragraph, which moves after it, saying what agy's
  `review` does and does not promise; the error codes table; the Agent
  kinds table's Profile cell and a sentence on other kinds; the smoke
  section), `skills/pastor/SKILL.md` (its table and the profile bullets),
  `AGENTS.md` (known gap below), `changes/feat-agent-profiles.md`.

## Tests

- `config::agy` unit tests: each row of the translation table; covering
  rules (exact, `*`, command prefix, path directory on component
  boundaries, relative paths, `read_file` for `write_file`, `regex:` never,
  `ask` never); `parse_denies` on the probe's output with one, two and no
  files and a broken file; the built-in `review` and `develop` denies
  against a settings file that holds them and one that lacks
  `command(git push)`.
- `config::mod` tests: `launch_args` for agy under a profile adds the two
  flags and no tool flags; `--mode` or the skip flag in args is
  `profile_args_conflict`; a deny of `WebSearch` is `agy_deny_unsupported`;
  `own_permissions` still adds nothing; `lists_unapplied` text.
- `daemon` tests, on the pattern of
  `an_agy_task_under_a_profile_needs_its_own_permissions`: a profiled agy
  task with the fake reporting covering rules starts with the flags; with a
  rule missing it fails `agy_permissions_missing` before `agent.start`, the
  message naming the rule; with the fake reporting nothing (a command
  machine) it fails the same; `unrestricted` on an unrestricted machine
  runs no probe; an unpinned `review` task skips a machine whose own
  profile is `develop` and lands on one whose own profile is `review`,
  and pinned to the first is refused `agy_profile_mismatch`.
- `transport` test: `agy_rules` prints the files that exist from a temp
  `HOME`, like `the_config_check_command_reads_the_opencode_config_dir`.
- The doc tests that check every command and flag named in the manual and
  the skill, and that every `toml` fence parses, run under `make check`.

## Known gaps

- agy's `artifactReviewPolicy` defaults to `asks-for-review`, which can stop
  an interactive session to review a plan artifact. Pastor's dialog reading
  does not know that prompt. Recorded in AGENTS.md; a machine that runs
  agy tasks should set it to `agent-decides` or `always-proceed`.
- A deny a project file holds does not count, and a project file's allow
  cannot lift a global deny (deny wins across tiers), so leaving them out
  errs on the side of refusing.
- The probe reads `~/.gemini` under the machine's `HOME`; agy has no
  variable to move it.

## Follow-up, not in this change

Pastor writing the rules onto the machine (additive, idempotent, before
launch or at `machine add`) is the provisioner work. It would turn the
`agy_permissions_missing` refusal into a reconcile, and make an agy machine
join the flock with no hand edit.
