---
name: feature-list
description: features.json — one named feature per entry, progress carried by a boolean pass and never by a percentage.
catalog: false
---

# The feature list

`.long-horizon/features.json` is the record of what the session set out to build
and what of it is done. It is written at initialization and rewritten in place
each time a feature passes.

## Schema

```json
{
  "goal": "one line: what the session is for",
  "baseline": "4f2c9ab",
  "features": [
    {
      "name": "config-loads-plugin-toml",
      "intent": "one line: what this feature does when it works",
      "verify": "cargo test --test plugin",
      "pass": false
    }
  ]
}
```

- `name` — stable, lowercase, hyphenated. It is quoted in the progress log and
  in commit messages, so renaming a feature breaks the link between the three.
- `intent` — one line, present tense, describing the feature working.
- `verify` — the single command that decides. Written at initialization, before
  the feature is built, so the check is not shaped around whatever the
  implementation turned out to do.
- `pass` — `false` until the command in `verify` succeeds.

## Why a boolean and not a percentage

A percentage cannot be verified. "Sixty percent done" is an estimate produced by
the same agent whose work it is measuring, it moves without anything on disk
changing, and no command disagrees with it. `pass: true` is a claim a command
either reproduces or refuses: run `verify`, and the boolean is right or it is
wrong. The unit of progress is therefore the feature, and the count of passing
features is the only progress figure this protocol reports.

## Flipping a feature

1. Run the feature's `verify` command.
2. On success, set `pass` to `true` — nothing else in the entry changes.
3. Append the entry to `progress.md` before touching the next feature.
4. `git_add` and `git_commit` the code, `features.json` and `progress.md`
   together, so the three never disagree in the history.

On failure, leave `pass` at `false`. A feature is never flipped on inspection,
on a reading of the diff, or because it looks finished.

## Changing the list mid-session

Adding a feature is ordinary: append an entry with `pass: false`. Removing one
requires a line in `progress.md` saying it was dropped and why, because a
feature that vanishes from the list and never appears in the log is
indistinguishable from one that was silently abandoned.

A feature that was `true` and is now broken flips back to `false` and gets a new
log entry. `pass` describes the tree as it is now, not what was true once.
