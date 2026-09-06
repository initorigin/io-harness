---
name: git-baseline
description: The commit or branch a long-horizon session measures "done" against, and how a resumed session recovers it.
catalog: false
---

# The git baseline

The baseline is one ref — a commit sha, or the branch point of a branch cut for
the session. Everything the session changed is `git_diff` from that ref to the
working tree. Everything else in the repository was already there.

Without it a resumed session can read `git_status` and still not answer the only
question that matters: which of these changes are mine. `git_status` describes
the tree, not the session.

## Cutting it

At initialization, on a clean tree, take the current commit from `git_log`. On a
dirty tree, either commit the pre-existing changes first or cut a branch with
`git_branch` — the point is that the baseline names a state where nothing
uncommitted belongs to this session.

Record it with `remember`, key `long-horizon/baseline`, workspace scope:

```
long-horizon/baseline = 4f2c9ab
```

Store the resolved sha, not `HEAD`. `HEAD` moves with every commit the session
makes, so a baseline recorded as `HEAD` diffs the session against itself and
reports nothing changed.

## Using it

- **What has this session changed?** `git_diff` against the baseline ref.
- **What has it committed?** `git_log` from the baseline ref.
- **Is a change on disk but unrecorded?** A file in the diff whose feature is
  still `pass: false` in `features.json` is either work in progress or a feature
  that was built and never verified. Verify it before flipping anything.

## Committing as the session runs

Commit each feature as it passes, with `git_add` and `git_commit`, rather than
batching the session into one commit at the end. A per-feature commit makes the
progress log's entries and the git history the same list, so a disagreement
between them is visible instead of silent.

## Worktrees

When the session must not disturb the checkout it started from, cut a worktree
with `git_worktree` and take its branch point as the baseline. The rest of the
protocol is unchanged; the state files live in the worktree, beside the code
they describe.

## Recovering it

A resumed session reads the baseline back from `remember`'s store before it
reads anything else. If the key is absent, the session was never initialized —
run `long-horizon-init` rather than guessing a ref from `git_log`, because a
guessed baseline silently attributes another session's commits to this one.

Call `forget` on the key only when the session's work is merged and the baseline
no longer describes anything.
