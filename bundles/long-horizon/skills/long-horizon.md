---
name: long-horizon
description: The four parts of the long-horizon session protocol and the order they run in.
---

# The long-horizon session protocol

A session that outlives one context window loses the two things it needs most:
what it set out to build, and what it has already changed. The protocol keeps
both on disk, where a resumed session reads them back.

Four parts, in this order:

1. **`long-horizon-init`** — run once, at the start. It cuts the git baseline,
   writes the feature list and opens the progress log, and does nothing else
   until all three exist.
2. **`git-baseline`** — the commit or branch every later diff is taken against.
   It answers "what did this session change", which `git_status` alone cannot,
   because a dirty tree looks the same whether the work is this session's or was
   already there.
3. **`feature-list`** — `features.json`, a named feature per entry with a boolean
   `pass`. The unit of progress is the boolean, never a percentage.
4. **`progress-log`** — `progress.md`, append-only, one entry per feature that
   flipped to `pass: true`, carrying what changed and how it was verified.

## Where the state lives

Under `.long-horizon/` in the workspace root:

```
.long-horizon/features.json    the feature list
.long-horizon/progress.md      the progress log
```

Two files rather than one, because the feature list is rewritten in place on
every flip and the log is only ever appended to — mixing them would put a
rewrite through the file that must never be rewritten.

The baseline is not a file. It is a git ref, recorded with `remember` under the
key `long-horizon/baseline`, so a session that starts with an empty context
recovers it without reading anything.

## The loop, once initialized

For each feature in turn:

1. Mirror the remaining features into `todo_write` so the plan is visible for
   this turn.
2. Build the feature.
3. Verify it. A feature is verified by the verification gate, or by a command
   whose output is quoted in the log entry. Nothing else counts.
4. On a passing verification: flip `pass` to `true` in `features.json`, append
   the entry to `progress.md`, and commit with `git_add` and `git_commit`.
5. On a failing verification: leave `pass` at `false`, append nothing, and fix
   the feature. A log entry is written after the check passes, never before.

## Resuming

A session resumed hours later reads, in this order: the baseline from
`remember`'s store, `features.json` for what is left, `progress.md` for what the
last session did, and `git_diff` against the baseline for what is on disk. The
first feature with `pass: false` is the next one.
