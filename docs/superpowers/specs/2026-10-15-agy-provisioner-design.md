# The agy provisioner

Date: 2026-10-15. Branch `feat/agy-provisioner`. Builds on
`docs/superpowers/specs/2026-10-15-agy-profiles-design.md` (branch
`feat/agent-profiles`), which it does not repeat: the rule syntax, the files
agy reads, the translation of Claude patterns to agy rules and the covering
rules are that design's.

## Problem

With `feat/agent-profiles`, a profiled agy task starts with
`--dangerously-skip-permissions --mode accept-edits`, and only after a check
that agy's settings on the machine deny what the profile denies. When they do
not, the task is refused with `agy_permissions_missing` and the rules to add.
Someone then logs into the machine and edits
`~/.gemini/antigravity-cli/settings.json` by hand. Every new agy machine
needs that edit before its first profiled task, and the edit can come undone:
agy keeps a copy of its settings in memory and writes the whole file back
when it saves one of its own, so a running agy session can put back a file
without the rules.

The goal is a machine that joins the flock and runs profiled agy tasks with
no hand edit, the way a node joins a Kubernetes cluster: the head knows the
desired state (the machine's own profile), and something on the way to each
launch makes the machine match it.

## What it is

The first provisioner, in the vocabulary of `AGENTS.md`: what checks and sets
up a machine for agents. Built into pastor, not installed like a connector,
because it needs what only the head has: the machine's own profile,
resolved through `[profiles]`. An installable provisioner (a manifest, a
command, `pastor provisioner install`) is later work; this one is the case
that shaped it.

## Desired state

The machine's own profile, as `Fleet::own_profile` already settles it for
the `unrestricted` rule: its `[[machine]]` `profile`, else its primary
flock's (`Flock::primary_flock`), else `[defaults] profile`. Its deny list,
each pattern through `config::agy::required_rule`, is the set of agy deny
rules the machine must have. A pattern with no agy rule (`Glob`, `WebSearch`)
adds nothing here; the check refuses a task that needs it
(`agy_deny_unsupported`) as before.

Not the task's profile. agy's rules are per machine, not per session
(`agy-profiles-design`, "Consequence for the fleet"): writing a `review`
task's `write_file(*)` deny into a `develop` machine's settings would stop
every later edit there, the owner's own sessions included. A task under a
profile that asks for more than the machine's own still reaches the check
and is refused with the exact rules missing; that refusal is a statement
about the fleet's layout, which a human decides.

A machine with no own profile, or one whose profile denies nothing
(`unrestricted`), wants no rules: pastor reads and writes nothing.

## The write

Into the global file, `~/.gemini/antigravity-cli/settings.json`, only:
agy's shared `~/.gemini/config/config.json` belongs to the desktop app too,
and the project files to a checkout.

- Additive. The rules are appended to `permissions.deny`, after the
  owner's, in the profile's order. A rule already covered by a deny in the
  file (the covering rules of `config::agy::missing`: the same rule, the
  action's `*`, a shorter command prefix, a directory above the path, a
  `read_file` deny for a `write_file` rule) is not added. A rule covered
  only by agy's shared or project files is added anyway: the global file is
  the one pastor owns a part of, and a duplicate deny is harmless.
- Idempotent. A file that already covers every rule is not written at all,
  not even reformatted: no write, no change of its mtime. Running the merge
  on its own output finds nothing to add.
- Nothing else changes. No rule is removed or moved, and `allow`, `ask`,
  `mode` and every other key keep their values and their order: the file is
  parsed into an order-preserving tree (`provision::agy::Json`), not
  `serde_json::Value`, whose maps sort their keys. It is written back
  pretty-printed with two spaces; agy rewrites the file in its own format on
  its next save anyway.
- Created when missing, as `{"permissions": {"deny": [...]}}`, its
  directory too.
- Atomic. The new content goes to a temp file in the same directory, made
  under `umask 077`, and is renamed over the old one.
- Compare and swap. The write ships the content pastor read along with the
  new one, and the remote shell renames only when the file still holds what
  was read (`cmp`). agy writes its file atomically too, so the file is
  always one whole version; when agy saved between pastor's read and write,
  the write answers `changed`, and pastor reads, merges and tries once more.
  A second `changed` fails the task (below). The window between `cmp` and
  `mv` remains; what closes it is the next point.
- Checked again before each launch. agy holds its settings in memory and
  can write back a copy from before pastor's edit. Nothing pastor writes can
  stop that, so pastor does not trust a past write: the reconcile runs before
  every profiled agy launch, and a file agy reverted is fixed on the next
  one. After `feat/agent-profiles` lands, its check runs right after the
  reconcile, on the same launch, so a task never starts on a file that lost
  the rules between the two.
- A symlink is left alone: renaming over it would replace a dotfile link
  with a plain file. When such a file needs rules, the task is refused with
  them (below); when it already has them, nothing happens.

## When it runs

Three places were weighed.

1. **At `pastor machine add`.** One shot, at the moment a human is already
   at the keyboard. But the machine may not have agy yet, the profile can
   change later (a flock edit, a `[defaults]` edit), `machine add` is not the
   only way a machine enters `flock.toml`, and agy can revert the file at any
   time after. It cannot be the only place, and as a second place it adds a
   probe to a command that today only edits a file.
2. **At head start and on each reconcile** (`reconcile_every`, 60s by
   default). Converges like a controller loop and catches a revert within a
   minute. But it writes to every machine whose own profile denies
   anything, whether or not it ever runs agy, an ssh probe per machine per
   reconcile, and a revert between two reconciles still meets a launch
   unchecked. It edits a file on the owner's machine for no task.
3. **Just before a profiled agy launch.** The refusal becomes a reconcile.
   The probe runs only where a task needs it, the file is read at the
   moment it matters, and the read is the same work the check does anyway.
   A machine joins with no step at all: its first profiled agy task writes
   the rules and runs. The cost is one probe (two when it writes) on each
   such launch, inside the dispatch lock (a known gap already).

Pastor does 3, and only 3. It is the only place that gives the guarantee the
check needs (the file right before the launch), it touches no machine that
never runs agy under a profile, and it needs no new command or timer. 1 and
2 would be optimisations on top of 3, not replacements, and neither saves a
human step 3 needs.

The reconcile runs on resume too (a waiting task starting again on agy is a
launch), and on a headless serve's own machines with the profile its
`pastor.toml` gives, which for a pull machine is usually none: a pull
machine's agy file is not provisioned (known gap).

## Failure

When the machine wants rules and pastor cannot read or write the file, the
task is refused with `agy_permissions_missing`, the code the check uses for a
file it cannot read, before anything is made on the machine:

- a `command` machine (no probe there);
- a file that is not JSON, or whose top level, `permissions` or
  `permissions.deny` is not of the right type;
- a symlink that needs rules;
- a write that fails (a read-only home, a full disk), or a second `changed`.

The message says why, names the file, and lists the missing rules one per
line, as the check's does, so the fix by hand is the same one. A probe that
cannot reach the machine is a transport failure, as for every probe.

A task whose agent is not of kind `agy`, that has no profile, or whose agent
has `own_permissions = true` is untouched: no probe, no write.

## Code

- `src/config/agy.rs`: `KIND`, `required_rule(pattern)`, `covers(have,
  want)` and `missing(required, denies)`, with the names and meaning of
  `agy-profiles-design`. Written here because the provisioner needs them and
  `feat/agent-profiles` had pushed only its design. When both branches
  merge, that branch's `agy.rs` is the one to keep: it has the same
  functions and more (`RULES_COMMAND`, `parse_denies`, `NO_ASK`).
- `src/provision/mod.rs`, `src/provision/agy.rs` (new): `Json`, the
  order-preserving tree; `merge(content, rules) -> Result<Merge, String>`,
  the pure step; `SETTINGS_FILE`; `read_command()` and
  `write_command(expected, new)`, the shell commands; `reconcile(conn,
  own_deny, machine)`, the step dispatch calls.
- `src/herdr/transport.rs`: `Connector::agy_settings()` (the file's
  content, absent, or a link; `None` on a `command` machine) and
  `Connector::write_agy_settings(expected, new)` (`written`, `changed`,
  `link`, or a failure), through `probe` on local and ssh endpoints;
  `src/herdr/fake.rs` stubs both with a file held in memory.
- `src/machine.rs`: `MachineSettings::own_deny`, the machine's own profile's
  deny list. A change of profile is a change of settings, so a reload
  replaces that machine's actor, as an `[agents]` edit does.
- `src/daemon.rs`: `Fleet::own_deny(m)` fills it when an actor is spawned
  or compared (`apply_flock_locked`, `spawn`).
- `src/dispatch.rs`: `dispatch_with` and `resume_with` take a `Provision`
  (the machine's own deny list); `dispatch` and `resume` keep their
  signatures and pass none. `dispatch_steps` runs `provision::agy::reconcile`
  for an agy task under a profile (`Agents::agy_profile`) before
  `Agents::launch`, so on this branch alone a profiled agy task is still
  refused afterwards (`agent_tools_unsupported`), and on top of
  `feat/agent-profiles` the check runs after it.

## Tests

- `config::agy`: each row of the translation table; covering (exact, `*`,
  command prefix, directory, `read_file` for `write_file`, `regex:` never);
  proptests: never panics, covering is reflexive, `*` covers its action.
- `provision::agy::merge`: a missing file; a file with the owner's allow,
  ask, deny, mode and other keys, whose order and values survive; nothing
  added when covered (and then no write); a covered rule skipped and the
  rest appended; a non-object top level, a `permissions` that is not an
  object, a `deny` that is not an array, a broken file; proptest: merging
  the output again finds nothing, and every owner rule is still there in
  order.
- The shell commands, run under `sh` with a temp `HOME`: read of a missing
  file, a file, a link; write creating the directory, compare and swap
  refusing a changed file, a link refused, the mode 0600.
- `dispatch`: the fake's file gets the machine's rules before the launch; an
  existing owner file keeps its rules first; a covered file is not written;
  a `command` machine (no file) fails `agy_permissions_missing` naming the
  rules; a broken file the same; a `changed` answer retried once; no probe
  for a claude task, a task without a profile, `own_permissions`, or an
  empty own deny list.
- `daemon`: a machine's own profile reaches its actor settings, from the
  machine, its flock or `[defaults]`.

## Known gaps

- agy can revert the file between pastor's write and agy's start; the check
  of `feat/agent-profiles` refuses that launch, and the next launch writes
  again. Nothing makes agy reload.
- A pull machine (its shepherd dispatches) and a headless serve's own
  machines provision from that serve's `pastor.toml`, not the head's
  profiles.
- A rule pastor added stays when the machine's profile changes to one that
  does not deny it: pastor never removes a rule, since it cannot tell its
  own from the owner's. A narrower profile then still leaves the machine
  narrow until someone removes the rule by hand.
