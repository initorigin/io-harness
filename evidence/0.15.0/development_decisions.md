# 0.15.0 development decisions

Answers to the release contract's open questions, and one correction to it.
Recorded here rather than in `execution/active.yaml`, which has no field for them.

## D1

`CompletionRequest` derives `Default` and gains `media`, and callers using an exhaustive struct literal get a compile error with a one-line fix. The contract cites src/provider/mod.rs:150 as already telling callers to construct it with `..Default::default()`; that note is on `CompletionResponse`, not on `CompletionRequest`, which has no `Default` derive at all today. The substance of the contract is unaffected — N5 permits additive change and delivery.migrations already states the compile error and its fix — so the citation is corrected here rather than through an iteration. Recorded because a wrong file:line in a contract is worth correcting in writing, not silently.

## D2

Git runs on the host, not inside the 0.6.0 sandbox. Answers the contract's first open question. The sandbox's portable floor is an ephemeral tempdir with network denied, and git's whole subject is the real workspace and the real `.git`, so the floor would defeat the capability rather than contain it. Git's boundary is the fixed argv plus the path gates, which is what the release is built around; the sandbox continues to contain model-produced code, which is what it was built for. Stated in the doc comment rather than left to be inferred.

## D3

The commit identity is supplied by the harness on the invocation itself, defaulting to an agent identity at a reserved-for-invalid domain, and overridable by the caller. Answers the contract's second open question, which could not be left open: `git commit` fails outright when no `user.email` is configured. Inheriting the repository's identity would attribute the agent's commit to whichever human configured that machine, which is the wrong default; requiring configuration would fail on a fresh machine, which is the wrong other one.

## D4

`git_commit` gates on `Act::Write` against `.git`, so a policy that denies it denies commits explicitly rather than by accident. Answers the contract's third open question. A caller running git under a narrow write policy must allow `.git`, which is stated in the doc comment.

## D5

`git_add` honours `.gitignore`, because the flag that overrides it is not reachable. Answers the contract's fourth open question. A refused stage returns git's own message so the model can tell "ignored" from "no such path".

## D6

One `media` feature rather than a split. Answers the contract's sixth open question: `base64` is the only new crate, so the umbrella-plus-per-format pattern 0.14.0 set has nothing to divide.

