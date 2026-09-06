---
name: long-horizon-init
description: Start a long-horizon session — cut the git baseline, write the feature list, open the progress log, in that order.
---

# Initializing a long-horizon session

Run this once, before the first edit. It ends with three things in place: a
baseline to diff against, a feature list to work through, and an empty log to
append to. Do no feature work until all three exist — a feature built before the
baseline is indistinguishable from what was already in the tree.

## 1. Establish the baseline

Call `git_status`. Then, depending on what it reports:

- **A clean tree.** Call `git_log` and take the current commit. That sha is the
  baseline.
- **A dirty tree.** The uncommitted changes are somebody else's work, so they
  must not be attributed to this session. Either commit them with `git_add` and
  `git_commit` under a message saying they predate the session, or cut a branch
  with `git_branch` and treat the branch point as the baseline. Ask before
  committing another author's changes.

Record it: `remember` the baseline ref under the key `long-horizon/baseline`,
with the workspace scope. `remember` writes by key, so re-running this skill
overwrites the entry rather than accumulating a second one.

See `git-baseline` for what the baseline is used for and how a later session
recovers it.

## 2. Write the feature list

Create `.long-horizon/features.json` with one entry per feature, every `pass`
starting `false`. See `feature-list` for the schema and for why the field is a
boolean.

Split the goal until each feature is a thing that can be verified by one
command. A feature that needs three commands to demonstrate is two features that
have not been separated yet.

## 3. Open the progress log

Create `.long-horizon/progress.md` holding its heading and nothing else:

```markdown
# Progress

Baseline: <ref recorded in step 1>
```

Entries are appended by `progress-log`, one per feature that reaches
`pass: true`. The file is opened empty here so that a resumed session can tell
an unstarted run from a lost one: a missing file means initialization never
finished, an empty one means it did and no feature has passed yet.

## 4. Commit the scaffolding

`git_add` the two files and `git_commit` them before building anything. The
commit puts the feature list on the baseline's side of the diff, so `git_diff`
against the baseline later shows the work and not the plan.

## 5. Mirror into the turn

Call `todo_write` with the feature names, in order. This is per-turn state and
is lost with the context — `features.json` is the record, `todo_write` is the
view of it the current turn gets.
