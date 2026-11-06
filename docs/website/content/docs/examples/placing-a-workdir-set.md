---
title: a reviewer set in a working directory
summary: files and permission rules placed before an agent starts
weight: 9
---
A script of yours starts a review agent in `~/src/app` for each pull
request. The agent needs a skill, a line of instructions and a permission
list that lets it read and nothing more. Instead of writing those files by
hand on every machine, the script hands pastor one set. Pastor places the
files, keeps a record of what it wrote, keeps them out of git, and answers
with the flags the agent starts with.

## what you need

- pastor on the machine that has `~/src/app`, and a Claude agent there.
- The built-in `review` profile, or one of your own under `[profiles]` in
  `pastor.toml`. The set names it; the lists are never copied into the set.

## the set

One JSON or TOML document. Save it as `reviewer.json`:

```json
{
  "id": "reviewer",
  "kind": "claude",
  "profile": "review",
  "skills": [
    {"name": "reviewing-a-pr", "files": {"SKILL.md": "---\nname: reviewing-a-pr\ndescription: Use to review a pull request.\n---\nRead the diff, then list what to change.\n"}}
  ],
  "settings": {
    "env": {"TICKET_URL": "{ticket_url}"},
    "permissions": {"additionalDirectories": ["{home}/notes"]}
  },
  "instructions": "You review pull requests for this repository. Do not edit files.",
  "vars": {"ticket_url": "https://tickets.example.com"}
}
```

`id` names the set in the directory, so two sets can share it. `skills`
become folders under `.claude/skills/`, `settings` merge into
`.claude/settings.local.json`, and `instructions` become a block in
`CLAUDE.local.md`. `{home}` and `{ticket_url}` are filled in when it is
placed. `permissions.allow` and `deny` cannot be in `settings`: they come
from the profile.

## look first

```sh
pastor workdir place --local ~/src/app --set reviewer.json --dry-run
```

```text
/home/user/src/app reviewer (dry run, nothing written): 8 added, 1 updated
  added .claude/skills/reviewing-a-pr/SKILL.md
  added .claude/settings.local.json env.TICKET_URL
  added .claude/settings.local.json permissions.additionalDirectories
  added .claude/settings.local.json permissions.allow
  added .claude/settings.local.json permissions.deny
  added .claude/settings.local.json permissions.ask
  added .claude/settings.local.json permissions.defaultMode
  added CLAUDE.local.md
  updated /home/user/src/app/.git/info/exclude: excluded /.claude/settings.local.json, /.claude/skills/reviewing-a-pr/SKILL.md, /CLAUDE.local.md
profile review: start the agent with --permission-mode dontAsk --allowedTools Read --allowedTools Glob ...
```

The last line is one line in the terminal, shortened here. The `updated`
row is the repo's `info/exclude`: the placed files are listed there, so
`git status` stays clean and the agent does not commit them.

## place it

Without `--dry-run` the files are written:

```sh
pastor workdir place --local ~/src/app --set reviewer.json
```

Run it again and nothing changes:

```text
/home/user/src/app reviewer: 8 kept, nothing to change
profile review: start the agent with --permission-mode dontAsk --allowedTools Read --allowedTools Glob ...
```

## start the agent with the flags

`--json` carries the same rows and the launch under `profile`: the args and
env the agent starts with, after its own. A launcher takes the files and the
command line from one answer, and pastor does not start the agent:

```sh
mapfile -t flags < <(pastor workdir place --local ~/src/app --set reviewer.json --json | jq -r '.profile.flags[]')
cd ~/src/app && claude "${flags[@]}"
```

The flags and `.claude/settings.local.json` carry the same lists, so the
agent is held to them either way. Claude reads that file and `CLAUDE.local.md`
only with its `local` setting source on, so do not start it with
`--setting-sources` that leaves `local` out.

## when the agent adds a rule

Pastor owns the permission keys in that file whole. A rule the agent had
allowed for good during a session is replaced the next time you place, and
the row says how many went:

```text
  updated .claude/settings.local.json permissions.allow: agent_rules_dropped: 1
```

Anything else in the file, such as a key of your own, is left alone. A file
that has its own `permissions.allow` before pastor wrote anything is a
`conflict` and stays as it is.

## on another machine

Without `--local` the set goes through the head, which renders the profile
and places the set on a machine of the flock that it can reach by ssh:

```sh
pastor workdir place server-1 /home/user/src/app --set reviewer.json
```

The head records what it placed and reads it back every five minutes. `check`
places each recorded set again as a dry run, says how it stands (`ok`,
`stale`, `drift`, `gone` and the like) and exits 1 if anything would change,
and `list` shows the record with each set's state:

```sh
pastor workdir check server-1 /home/user/src/app
pastor workdir list server-1
```

On a pull machine, run `place --local` there.

## take it out

```sh
pastor workdir remove --local ~/src/app --id reviewer
```

Pastor takes out exactly what it wrote, as long as each file still holds it,
and the exclude lines with it. A file you changed since stays, and the row
says `changed by hand`.

## next

- `docs/manual.md`, "Workdir bundles": every field, row and refusal
- [profiles and trust](../../concepts/profiles-and-trust/): the profiles a set names
- [cli: workdir](../../reference/cli/#workdir)
