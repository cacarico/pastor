---
name: connector
description: "Scaffolds a pastor connector: a poll or stream script plus pastor-connector.toml, tested with pastor connector try and pastor tick --dry-run. Use when the user wants to write a new pastor connector, feed a pastor job from an external source (a chat channel, an issue tracker, a feed, a directory, an API), or asks to scaffold pastor-connector.toml, a connector's poll.sh or stream.sh, or a poll or stream connector for pastor."
argument-hint: "[what the connector watches]"
model: sonnet
effort: high
paths: "**/pastor-connector.toml"
---

# pastor:connector

Scaffolds a pastor connector — a poll or stream script plus its
`pastor-connector.toml` — and tests it before anyone installs it for real.

**REQUIRED BACKGROUND:** [protocol.md](protocol.md), a copy of the manual's
connector section: the manifest schema, the stdin/stdout handshake, secrets
and redaction, and the CLI. This skill does not repeat it. For the rest of
pastor (jobs, machines, `pastor tick` beyond `--dry-run`), the `pastor` skill
(or `pastor --skill`).

## Checklist

1. **Ask at most two things**, and only what the request does not already
   answer:
   - Poll or stream — checked on a timer, or does it push updates and stay
     open?
   - Which secrets it needs (API tokens, credentials), each with a one-line
     description; "none" is a fine answer.

   Everything else — the id, its name and description, which config keys a
   job must pass in, the timeout — pick the simplest fit for what was asked
   and say so when you report back; do not ask about it.

2. **Name it and place it.** `<id>` is lowercase,
   `[a-z0-9][a-z0-9_.-]{0,63}`, taken from what it watches (`github-issues`,
   `slack`, `local-files`). Scaffold it in a directory of the user's own —
   the current project, or a new `<id>/` folder next to it — never straight
   into `~/.local/share/pastor/connectors/`; that is where `connector link`
   or `connector install` puts it, once the user is ready (step 5).

3. **Write `pastor-connector.toml`** from [protocol.md](protocol.md)'s
   manifest section: `id`, `name`, `version` (start at `0.1.0`),
   `description`, a `[connector]` table with `mode` (`poll` or `stream`) and
   a `command` pointing at the script, one `[connector.config.<key>]` per
   value a job must supply (a channel, a repo, a directory, a feed URL —
   `required = true` for what the script cannot run without), and one
   `[secrets.<NAME>]` per secret from step 1. Leave `authors`, `homepage`,
   `repository` and `license` out unless the user gives them; leave out
   `[[events]]`, `[finish]` and `[watch]` unless asked for a hook or a watch
   line too.

4. **Write the script** — `poll.sh` for `mode = "poll"`, `stream.sh` for
   `mode = "stream"` — to [protocol.md](protocol.md)'s contract exactly: read
   one JSON handshake line from stdin (`config`, `cursor`, `since`), then
   print `item`/`log`/`cursor` JSON lines to stdout.
   - A poll script may ignore `cursor` and `since` and simply emit
     everything it currently sees each run: pastor drops item keys it has
     already turned into a task, so that alone stops duplicates. Track
     `cursor`/`since` only when re-fetching everything every run would be
     slow or costly.
   - A stream script reads the handshake once, then keeps its own
     connection open and emits lines as they happen; it must not exit on
     its own, and pastor restarts it with backoff if it does.
   - Read secrets only from the environment (`$SECRET_NAME`); never write a
     value into the script or the manifest — the connector's `.env` is the
     user's file, not this skill's to write.
   - `chmod +x` the script. A relative program with a slash in `command`
     (`./poll.sh`) means the connector's own file, run from its directory.

5. **Test it, in this order:**
   - `pastor connector link <path>` registers the local directory under its
     id; it prints the secrets still unset. Fill real values into
     `~/.config/pastor/connectors/<id>/.env` yourself if you are the user
     testing this by hand — the skill does not write that file.
   - `pastor connector try <id> --job <id>-test` runs the command once and
     prints its items and logs; it creates no tasks and saves no cursor.
     `try` uses an empty config when no job file named `<id>-test` exists,
     so write one first only if the manifest has `required` config it
     needs to see. Fix the script or the manifest and re-run until the
     items look right.
   - Before `pastor tick --dry-run`, always write the throwaway job file,
     `~/.config/pastor/jobs/<id>-test.toml`: a schedule (`every = "1h"` is
     fine for a file you're about to delete), `[connector]` naming
     `use = "<id>"` plus any required config, and a placeholder
     `[dispatch] prompt`. `tick` validates a job strictly and rejects one
     with no schedule.
   - `pastor tick --dry-run --job <id>-test` shows what tasks a real run
     would create, still writing nothing — but only for `mode = "poll"`.
     For `mode = "stream"`, `pastor tick` with no daemon running reports
     the job as failed, needing `pastor serve`, which step 6 forbids
     starting; skip this step for a stream connector.
   - Remove the throwaway job file once you are done with it, unless the
     user wants to keep and finish it into a real job (a real `repo`,
     `prompt` and schedule).

6. **Stop there.** `pastor connector install` clones a pushed repository
   from GitHub; that is the user's own call once the connector is committed
   and pushed somewhere, not this skill's. This skill also never edits
   `.env`, starts `pastor serve`, or writes a real (non-test) job.

## Worked example

[example/local-files/](example/local-files/pastor-connector.toml) is a
small real poll connector: it watches a directory and turns each file in it
into one item, with no secrets and one required config key, `dir`.

## Common mistakes

| Thought | Reality |
|---|---|
| "I'll put a secret's real value in `.env` so it's ready to test." | Don't write secret values; only declare their names in the manifest. Filling `.env` is the user's step. |
| "`connector try` always needs a real job file." | Only when the manifest marks config `required`; otherwise `--job` need not name a file that exists. |
| "I should run `connector install` to prove it works for real." | That clones from GitHub. Scaffolding and testing stay local until the user pushes it somewhere and installs it themselves. |
| "A poll script must track its own cursor to avoid duplicates." | pastor already dedupes by item `key`; keep a cursor only to avoid re-fetching everything each run. |
