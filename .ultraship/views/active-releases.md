<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Active releases

## io-harness 0.60.3

**Execution state:** DEVELOPING
**Release fit:** high
**Target mode:** published
**Outcome:** Every block a classifying turn is composed from says something true of that turn. Three of them do not today, and each is the same defect this crate has already paid for twice: a sentence that describes a different turn than the one being taken.

**The plan gate orders a turn that is allowed to answer.** `conversational_opening` (`src/run.rs:4078-4106`) composes `directive: planning.then(|| planning_directive(...))` and `ending: CONVERSATIONAL_ENDING` into one block. The directive opens " Before you do anything else you must call `propose_plan` with the ordered steps you intend to take, and wait" (`src/run.rs:9178-9183`); the ending, emitted last in the same block, answers "If a plain answer is the whole of what is wanted, write that answer and call no tool", and closes "When the two readings are both possible, act" (`src/run.rs:15251-15256`). An operator who types a greeting into a plan-gated session gets a plan proposed for it and a human asked to approve one. This is 0.48.0's `I03` restored on the one path that composes a directive above the ending, and it is present in both loops — the flat loop passes `planning` at `src/run.rs:4538` and the tree loop at `src/run.rs:7254`.

**On the tree loop all three defects are latent, and this release says so (`US-IO-HARNESS-0.60.3-I01`).** `Session::turn_contained` and `Session::turn_contained_observed` (`src/session.rs:582-603`, `src/session.rs:652-677`) build their contract from text via `default_contract` (`src/session.rs:731-733`) and are the only two callers that reach the tree loop as a turn, so a contained classifying turn can carry neither a plan gate nor a preset. The composition is wrong at `src/run.rs:7254-7255` and at `compose`; no caller can currently make it fire. It is fixed here because the fix is the same one edit — both loops share `conversational_opening` and `compose` — and asserted at unit level rather than end to end, which is the only level it can be asserted at. Making it reachable is roadmap entry 0.66.0.

**The same turn is told a boundary that is not in force.** The rule this crate states for itself is that the boundary an agent reads is the one that will refuse it — written out at `src/run.rs:4474-4478`, and honoured by the `system` block, whose planning arm composes from the plan-narrowed policy (`src/run.rs:4500-4505`, `src/run.rs:7228-7231`). The conversational opening is handed `after_planning` at **both** call sites (`src/run.rs:4539` and `src/run.rs:7255`), so a classifying turn under the gate reads a boundary derived from the un-narrowed `policy` while `plan_lock()` — `deny_write("*")`, `deny_exec("*")` (`src/run.rs:9159-9164`) — is what will actually refuse it. In the tree loop the correct value is `while_planning`, already computed one binding above at `src/run.rs:7185-7189` and not used by the call at 7255.

**A preset discards the framing 0.49.0 shipped.** `Preset::describe` (`src/contract.rs:116-125`) returns a whole replacement description, and `compose` honours it over `spec.base` (`src/run.rs:14804-14810`: `SystemPrompt::Preset(preset) => preset.describe().to_string()`). An embedder who selected `Concise` or `Careful` therefore has `CONVERSATION_PROMPT` thrown away on a classifying turn and gets back the two claims 0.49.0 removed — "to meet a stated specification" and "checked against the success criterion" — on every greeting. Reachable through `Session::turn_bounded` with `Verification::None`. On a contained turn it discards `CONVERSATION_TREE_PROMPT` and `TREE_PROMPT` instead, dropping the guidance that the agent may spawn, which makes `Preset`'s own rustdoc claim — "a preset shapes how the work is done and reported, never what the agent can reach" (`src/contract.rs:113-115`) — untrue as written.

After this release a preset is a **manner appended to a framing**, not a replacement for one. The two axes are separated: which world the agent is in is chosen by the loop and the classification, how it works and reports is chosen by the embedder, and the two compose. That also retires the duplication the collapse forced — the four tool sentences are copied into both preset bodies today, which is the shape this crate warns about in `compose`'s own doc comment: "a rule added to one of four prompts is a rule that lapses in three" (`src/run.rs:14800-14802`).



_Canonical sources: products/<id>/execution/active.yaml, products/<id>/releases/<version>.yaml_
