---
name: progress-log
description: progress.md — append-only, one entry per feature that passed, carrying what changed and how it was verified.
---

# The progress log

`.long-horizon/progress.md` is what a session resumed hours later reads to learn
what the previous one did. It is append-only: entries are added at the end and
existing entries are never edited or removed.

Append-only because an editable log records the story the current session
believes, and the value of the log is that it records what the earlier session
actually observed — including the entry that turns out to have been wrong.

## One entry per passing feature

Write the entry after the feature's `verify` command succeeds and before
starting the next feature. No entry is written for work in progress.

```markdown
## config-loads-plugin-toml

- Changed: `src/plugin.rs` (Manifest gains `bin`), `tests/plugin.rs` (+2 cases).
- Verified: `cargo test --test plugin` — 41 passed, 0 failed.
- Commit: 9c1d004.
- Note: `[[bin]]` paths are checked lexically, not resolved; symlinks are out of
  scope for this feature.
```

Four required lines and one optional:

- **Changed** — the files, and what changed in each in a clause. A reader who
  cannot open the diff still learns the shape of it.
- **Verified** — the command that was run and its result, quoted from the
  output. "Tests pass" is not a verification; the command and its counts are.
- **Commit** — the sha from `git_commit`, which links the entry to the diff.
- **Note** — only for something the next session would otherwise rediscover: a
  known limit, a decision taken against the obvious alternative, a trap.

## Corrections

A later entry corrects an earlier one; the earlier one stays as written.

```markdown
## config-loads-plugin-toml (correction)

- Was: verified against `--test plugin` only, which does not build the bundle
  fixtures.
- Now: `pass` back to `false` in features.json; the fixture path is a new
  feature, `bundle-fixture-loads`.
```

## What does not go in it

- Plans. Those are `features.json` and `todo_write`.
- Narration of work that did not finish. An abandoned attempt earns a line only
  when the next session would otherwise repeat it, and then it is a Note on the
  entry that eventually passed.
- Percentages, and any summary of "how far along" the session is. The count of
  `pass: true` entries in `features.json` is that number, and it is derived
  rather than written.

## Reading it on resume

Read the last entry first — it names the last commit and the last verification,
which together say whether the tree matches the log. If the last entry's commit
is not in `git_log` from the baseline, the log is ahead of the repository:
trust `git_log`, and append a correction saying so.
