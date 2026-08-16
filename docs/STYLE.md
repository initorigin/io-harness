# The register this crate's prose is written in

Written down in 0.60.3 because it was already being followed by imitation, and a
convention held only by imitation drifts the first time someone imitates the wrong
paragraph. Nothing here is new. It describes `docs/CONTRACT.md`, the rustdoc, the
CHANGELOG and the test names as they already are.

It is a register, not a lint. No test enforces it, deliberately: a checker for prose
this small would reject good sentences more often than it caught bad ones.

## Say what is true of the thing being described

The one rule the rest follow from. A sentence composed for a classifying turn must be
true of a classifying turn; a doc comment on `exec_sandbox` must describe what
`exec_sandbox` does today. Three releases — 0.48.0, 0.49.0, 0.60.3 — exist because a
block was accurate about a different case than the one it was emitted for, and 0.60.2
exists because a contract paragraph was accurate about a release fifteen versions back.

## Present tense, and no diary

State what the crate does. Do not narrate what it used to do, except where a reader
would otherwise draw the wrong conclusion from what they already believe — then name
the release that changed it, in one clause, and move on:

> A run is contained unless it asked for `ExecMode::FullAccess` (0.46.0).

not

> In 0.45.0 we made `WorkspaceWrite` the default, and then in 0.46.0 we extended
> containment to every run, so today a run is contained unless…

History belongs in `CHANGELOG.md`. A version number inside a sentence is a citation,
not a story.

## Name the reason, once

Every non-obvious decision carries its reason where the decision is, and nowhere else.
The reason is the part that survives: an unexplained rule gets deleted by whoever finds
it inconvenient, and a rule explained twice gets corrected once.

> Denying rather than filtering the tool list, because `Policy::explain` resolves
> deny-first across every layer and every mutating path already goes through it.

## Prefer the concrete failure over the abstract quality

"A prompt composed once cannot follow a rule an approver remembers mid-run" says more
than "the boundary section is best-effort". Where a limit exists, name what it costs
and what still holds:

> The wall clock reaches neither — which is a deliberate property, not a gap, because a
> dev server killed at the sandbox ceiling would be containment deleting the tool's
> purpose.

## A claim is asserted or it is hedged

If a test asserts it, state it flatly. If nothing asserts it, say what is actually
known — "what a model then does with a prompt is not a claim this crate can make" — and
do not reach for "should", "generally" or "typically" to cover the gap.

## Test names are sentences about behaviour

`a_plan_gated_classifying_turn_may_still_answer`, not `test_planning_directive_2`. A
failing test's name is the first line of the bug report, and it is read by someone who
has not opened the file.

## No first person, no marketing, no adjectives doing an argument's work

The crate is the subject: "the harness refuses", "a run is contained". "Powerful",
"robust", "seamless" and "simply" are removed on sight — each is a claim with no
assertion behind it, and "simply" is usually attached to the part that is not.

## Em dashes carry the aside, and lists carry the enumeration

Prose runs to one idea per sentence with an em dash for the qualification. When there
are more than three parallel items, they become a list. A paragraph enumerating five
things is a list that has not been written yet.
