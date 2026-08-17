# Agency: a plan, a question, a named agent, a template

0.21.0 gives an agent four things it could not do before. It can write down what
it intends, so an operator watching a long run can tell where it thinks it is. It
can ask the operator what they actually meant instead of guessing and spending a
whole run on the guess. It can spawn a sub-agent that is *somebody in particular*
— a role, a model, a narrower boundary — rather than a copy of itself with a
different goal. And a run can start from a prompt an operator wrote once and
filled in per task.

Four primitives, and one line that none of them crosses. A plan is a claim, not a
fact: nothing verifies it and no outcome depends on it. An answer is text the
model reads, not permission for anything. A definition can only make a child
narrower, and has no syntax for making one wider. A template returns a `String`
and sets nothing. Everything the agent gains here, it gains inside the same
`Policy`, checked by the same code as before.

Two behaviour changes ship with them: repetition detection now catches a shape the
0.11.0 window could not see, and a refused `git` built-in costs a step instead of
the run.

## The plan an operator can read mid-run

```rust,no_run
use io_harness::{Store, TodoState};

# fn demo(store: &Store, run_id: i64) -> io_harness::Result<()> {
// Another process, while the run is still going.
for item in store.todos(run_id)? {
    let mark = match item.state {
        TodoState::Done => "x",
        TodoState::Active => ">",
        TodoState::Pending => " ",
    };
    println!("[{mark}] {}", item.text);
}
# Ok(()) }
```

`todo_write` takes the **whole list every call**. There are no item ids, no
partial updates, and no merge: the old rows are deleted and the new ones inserted
in one transaction. That is the design and not an economy — a model that
addressed items by id would mis-address them, and the wholesale replace leaves
nothing to get wrong.

The single transaction is also the whole reason the tool exists. A reader on
another connection sees the previous plan or the next one and never a half-written
mixture of the two, which is what makes reading a plan mid-run worth doing. The
suite proves it with a second `Store` opened inside an `Observer` — which is what
a UI in another process has — reading the plan back at step 1 of a run that then
goes on for two more.

**It is not gated.** There is no `Act` for it and no `Approver` in front of it,
because it writes into the harness's own store rather than into the workspace, the
network or a binary. Inventing an act would put a permission rule in front of an
agent stating its intentions.

**It is inert.** Nothing verifies a plan. No `RunOutcome` depends on one, no
verification consults one, and an item whose state says `Done` is a claim the
agent made and not a fact about the workspace. What a plan buys is a long run that
can be recognised as going the wrong way before it ends — nothing more than that.

Three states, `pending`, `active` and `done`, and no more: every state past them
— blocked, deferred, in-review — is a distinction the crate would have to define,
the model would have to choose correctly, and nothing would ever check. A state
outside the three is an observation naming the three rather than a guessed
default, so the correction costs one step. An empty list clears the plan and is
not an error: an agent that finished its work and emptied its list is not an agent
that never had one.

The list is bounded like every other tool result in the crate — capped and told,
not refused. At most `TODO_MAX_ITEMS` (64) items, and the observation the model
reads back says how many were dropped, so a plan does not quietly lose its tail.
Each item's text is capped at `TODO_TEXT_CAP` (200) characters.

## Asking what was meant, which is not asking permission

This is the distinction the release keeps everywhere, and the reason `Question` is
a separate type from `Request` rather than a variant of it:

| | asks | an answer can |
| --- | --- | --- |
| `Request` / `Approver` | may I do this action? | only **narrow** what happens |
| `Question` / `Responder` | what did you actually want? | only add **text the model reads** |

**An answer authorizes nothing.** Every tool call that follows one is checked
against the same `Policy` by the same code — the rule 0.20.0 set for steering, for
the same reason: "just do it, I authorize it" is the most natural thing anyone will
ever type, and the boundary must not care. The suite answers a question with
literally that sentence and asserts the denied write still refused, with the deny
lifted as the control so the refusal is provably the policy's.

### Answered in this process

```rust,no_run
use io_harness::{AnswerFuture, Question, Responder, TaskContract};
use std::sync::Arc;

/// Answers from what the UI already knows, and declines what it does not.
#[derive(Debug)]
struct FromUi;

impl Responder for FromUi {
    fn answer<'a>(&'a self, question: &'a Question) -> AnswerFuture<'a> {
        Box::pin(async move { question.choices.first().cloned() })
    }
}

# fn demo(root: &str) -> io_harness::Result<()> {
let contract = TaskContract::workspace("port the parser", root)
    .with_responder(Arc::new(FromUi));
# let _ = contract;
# Ok(()) }
```

A `Responder` is registered on the contract rather than passed to every entry
point, exactly as a `Toolbox` is — adding an argument to `run`, `run_with`,
`run_tree` and their observed and resume variants would break every existing call
site to add something almost all of them would pass `None` for. It is `&self` and
`Send + Sync`, so one responder serves a whole tree.

`ResponderNone` answers nothing and is the default. `FixedResponder` answers
everything with one string, for tests. `StdinResponder` prints the question and
reads a line, mapping a bare number onto an offered choice; an empty line means "I
would rather not answer here" and pauses the run.

`Question::choices` is an offer, not a menu. An answer need not be one of them,
because an operator whose real answer is "neither, do this instead" must not be
forced to pick a wrong one.

### Answered by nobody, then by a human tomorrow

With no responder — the honest default for unattended work — the question is
persisted, the run stops, and the outcome says so:

```rust,no_run
use io_harness::{resume_with_answer, ApproveAll, Policy, RunOutcome, Store, TaskContract};

# async fn demo(contract: &TaskContract, provider: &impl io_harness::Provider,
#               store: &Store, policy: &Policy, outcome: RunOutcome, run_id: i64)
#               -> io_harness::Result<()> {
if let RunOutcome::AwaitingAnswer { question_id, .. } = outcome {
    // Possibly in another process, tomorrow, with nothing but the two ids.
    let q = store.question(question_id)?.expect("asked earlier");
    println!("the agent asked: {}", q.question);
    for choice in &q.choices {
        println!("  - {choice}");
    }

    resume_with_answer(
        contract, provider, store, run_id, question_id,
        "io.local.toml — the committed one is only a template",
        policy, &ApproveAll,
    ).await?;
}
# Ok(()) }
```

The run is not left looking as though it were still going: its status is no longer
`Running`, which is the state a resume already understands.

The delivery is deliberately thin. The step that asked **was** committed before
the run paused, so a resume starts at the step *after* it and the `ask_question`
call is never replayed. `resume_with_answer` records the answer and appends it to
the run's observation ledger, which is what puts it in the next assembled prompt —
the same path a 0.20.0 steer takes. It is an `ObsKind::Message` with no target, so
nothing can supersede it away: an answer is not an observation *of* anything, and
the assembler must not stub it as stale when a later read touches the same path.

Two guards, both errors rather than shrugs. Answering an already-answered question
is an `Error::Resume`: two answers to one question means one of them was never
acted on, and a caller should hear which. So is an answer to a question belonging
to another run — that would replay a step which then asked again and paused again,
which reads as a hang.

`answered_by` is `"responder"` or `"human"`, and the distinction is kept because
"the machine decided" and "a person decided" are different facts about a run. An
in-process answer is recorded even though nothing paused, precisely so the trace
can say which of the two happened. `Store::questions(run_id)` is the whole
conversation, in the order it was asked.

### A child's question pauses the whole tree

```rust,no_run
use io_harness::{resume_tree_with_answer, ApproveAll, Containment, Policy, Store, TaskContract};

# async fn demo(contract: &TaskContract, provider: &impl io_harness::Provider,
#               store: &Store, policy: &Policy, containment: &Containment,
#               root_run_id: i64, question_id: i64) -> io_harness::Result<()> {
// `root_run_id` is the ROOT's, even when a child asked. The question id says who.
resume_tree_with_answer(
    contract, provider, store, root_run_id, question_id,
    "keep the old column; the migration is not reversible",
    policy, &ApproveAll, containment,
).await?;
# Ok(()) }
```

Exactly as a child's deferred approval does. The question is resolved against its
own run id rather than the root's, the resume walks the tree, and every agent
continues from its own last committed step. The parent's spawn step is left
*uncommitted* on purpose so the resume replays it and re-adopts the paused child —
only the parent re-entering the spawn can wait on that child again.

That propagation is where this was found: without it, a child's `AwaitingAnswer`
fell through to the composer and read as a child that had finished, so the tree
carried on having never heard the question.

## Named agents

Before 0.21.0 a spawned sub-agent was its parent with a different goal string:
same model, same system prompt, same boundary. So "search with the cheap model,
write with the strong one" — the largest cost lever this crate has — was
unexpressible, and a role was something you smuggled into the goal text.

```rust,no_run
use io_harness::{AgentDef, Agents, TaskContract};

# fn demo(root: &str) -> io_harness::Result<()> {
let contract = TaskContract::workspace("port the tokenizer", root)
    .with_agents(
        Agents::new()
            .with(
                AgentDef::new("searcher")
                    .with_role("You find things. You report paths and line numbers, never edits.")
                    .with_model("anthropic/claude-haiku-4.5")
                    .deny_write()
                    .deny_net()
                    .with_max_steps(8),
            )
            .with(
                AgentDef::new("author")
                    .with_role("You make the edit the searcher located, and only that edit.")
                    .with_model("anthropic/claude-opus-4.5"),
            ),
    );
# let _ = contract;
# Ok(()) }
```

The agent then names one in `spawn_agent`'s optional `agent` argument. Every field
past `name` is optional, and a bare `AgentDef::new(name)` produces exactly the
child a bare spawn produced before 0.21.0 — which is what makes the roster
additive rather than a second spawn path.

**A definition can only narrow.** This is the property the whole feature rests on,
and it is not enforced by a new check: `deny_write` and `deny_net` compose through
`Policy::contain`, the same function that has bounded every child since 0.5.0 —
allows intersect, denies union, at any depth. There is deliberately no
`allow_write` and no `allow_net`, and there must never be one: "give the writer
agent write access" is the natural thing to reach for, and it has to be impossible
or a roster in a config file becomes a privilege-escalation path. A unit test
asserts on the serialized shape for exactly that reason.

A definition **silent** about a path its parent denies still yields a child that is
refused it. Silence is not restoration; there is no code path here that adds an
allow.

**A role is prepended, never a replacement.** The tree's own system prompt is what
tells a child how to use its tools and that its result composes back into its
parent. A role that replaced it would produce an agent that did not know how to be
one.

**A model is a request, not a fact.** `AgentDef::model` travels as
`CompletionRequest::model`, which a vendor may substitute or alias. What actually
served a call is `CompletionResponse::model`, and that is what the per-step trace
keeps — so "which model ran this child" is answerable from the store rather than
from the roster.

A definition's `max_steps` outranks the model's own request in the spawn call: the
cap is the operator's. An unknown name is an error observation naming what *is*
available, and no child — a spawn that silently became an unnarrowed agent because
its definition was misspelled is the failure a roster must not have. The roster
reaches the model in the spawn tool's *description* rather than as a schema `enum`,
because a plain error recovers in one step whereas an `enum` violation makes the
whole call malformed at the provider. Registering the same name twice replaces
rather than shadows, and the trace records which definition ran: the spawn event's
detail reads `as searcher: <goal>`.

## Prompt templates

A template is a markdown file holding the text of a goal, with the parts that
change marked.

```text
templates/
  bugfix.md              -> template "bugfix"
  review/
    TEMPLATE.md          -> template "review"
```

```text
---
name: bugfix
description: Fix one failing test and change nothing else.
---

Fix the failing test {{test}} in {{file}}. $ARGUMENTS
```

```rust,no_run
use io_harness::{TaskContract, Templates};

# fn demo(root: &str) -> io_harness::Result<()> {
let goal = Templates::discover("./templates")?.render("bugfix", &[
    ("test", "parses_a_crlf_header"),
    ("file", "src/parse.rs"),
    ("hint", "it only fails on CI"),          // claimed by no placeholder
])?;
// "Fix the failing test parses_a_crlf_header in src/parse.rs. it only fails on CI"

let contract = TaskContract::workspace(goal, root);
# let _ = contract;
# Ok(()) }
```

`{{placeholder}}` and `$ARGUMENTS` are the whole grammar. `$ARGUMENTS` collects
every argument no placeholder claimed, joined by one space in the order given —
and it collects correctly even when it appears *before* the placeholder that
claims one, because which arguments are claimed is worked out before the walk.

**A substitution resolves or fails; it never empties.** A `{{placeholder}}` nobody
passed an argument for is `Error::Config`, and so is an unclosed `{{` and an
unknown template name (the error lists what does exist). This is the rule 0.19.0
set for `${env:}` and `${file:}`, for the same reason: a goal with a hole in it
still reads like a goal, so the run proceeds and pursues something the operator
never asked for. `$ARGUMENTS` is the one thing allowed to be empty, and it is not
an exception to the rule — it names a *remainder*, and a remainder is legitimately
empty.

Substitution is **single-pass**. An inserted value that itself contains `{{x}}` is
emitted literally rather than re-read, because a prompt builder that can recurse is
one whose output nobody can predict from its input.

Rendering is a pure function of the template and its arguments. It reads no file,
consults no `Policy`, draws on no budget and reaches no model; it returns a
`String` and that is the entire effect. Discovery mirrors `Skills` exactly — both
layouts, sorted by name, optional YAML frontmatter, and a directory that does not
exist, holds more than `MAX_TEMPLATES` (64), or holds two templates of the same
name is a rejected set rather than a silently truncated one.

## Repetition, even when work lands

There are now two stall signals, and `StallPolicy::window` is the threshold for
both.

* **Signal 1 (0.11.0).** `window` consecutive steps that changed nothing in the
  workspace **and** repeated a tool call the window already saw. Both halves, or a
  run reading four different files would be called stuck.
* **Signal 2 (0.21.0).** The same call `window` times in a row — *even if the
  workspace moved every time*.

Signal 1 has a blind spot a live run walked into. A spawned child that ran sets the
parent's `changed` unconditionally ("a child that ran did work the parent did not
have to"), so a parent respawning the *same* child reset its window on every step,
was never flagged, and simply spent its whole step budget. Signal 2 counts before
`changed` is consulted, which is why it sees it.

Note what signal 2 does *not* widen: **consecutive identical** signatures only. Two
different calls alternating, and the same call with different arguments, are left
alone — both are shapes a working agent takes. The suite's negative control is a
parent making three *different* spawns that each do work, and asserts no replan
row, no stall row, and a `Success`.

The threshold is the existing `window` rather than a field of its own, because "the
same call three times in a row" and "three unproductive steps" are one patience
setting; splitting them would add a knob whose only correct value is the other
knob's. `window: 0` still switches the whole thing off, and both signals escalate
on the same terms — nudged once per `max_replans`, then `RunOutcome::Stalled`.

## A refused `git` built-in costs a step, not the run

Until 0.21.0, `Git::run`'s refusal left the loop as `Error::Refused`, so one
speculative `git status` under a policy denying `Act::Exec` for `git` escalated the
whole run. Every other refusal in this crate is an observation the model reads and
adapts to; this is now one too, with the same refusal row a gate would have
written, so a reader cannot tell a git refusal from any other and does not have to.
Both of the refusals apply — the policy denying the `git` program, and a path that
would be read as an option, such as `--all` — and the step after either one still
runs.

This was a 0.20.0 live-run finding, recorded then and not fixed then because that
release touched no tool.

## In `io.toml`

```toml
[[agent]]
name = "searcher"
role = "You find things. You report paths and line numbers, never edits."
model = "anthropic/claude-haiku-4.5"
max_steps = 8
deny_write = true
deny_net = true

[[agent]]
name = "author"
role = "You make the edit the searcher located, and only that edit."
model = "anthropic/claude-opus-4.5"

[run]
templates = "./templates"
```

`[[agent]]` is top-level rather than part of `[run]`, and the tables
**accumulate** across scopes the way `policy.layers` do rather than the narrower
scope replacing the wider one: a project roster and a developer's own extra agent
are both wanted, and a local file that silently deleted the project's agents would
be a roster nobody could rely on. A later scope that registers the same *name*
still replaces that one definition, because `Agents` is keyed by name. An unknown
key inside an `[[agent]]` table is an error naming it, so a misspelled
`deny_writes` is not a boundary that quietly did not narrow.

`Config::apply_to` puts the roster on the contract. `[run] templates` is a
*pointer* and nothing more — `Config::templates()` hands back the path, and
discovering and rendering are the caller's, because discovery is fallible and
rendering happens before a run exists.

## The limits, stated plainly

**A plan is never enforced, and never gated.** Nothing verifies it, no
`RunOutcome` depends on it, and no verification consults it. An item marked `Done`
is the agent's claim about its own work. Writing a plan is also not an act the
policy checks, because it writes the harness's own store rather than the workspace
— so a plan is not evidence of anything and must not be read as any.

**An answer is not authorization.** It is text in the observation log. Every tool
call that follows one is re-checked against the same `Policy` by the same code, and
a human saying "I authorize it" changes nothing about a denied write. If you want
an answer to widen a boundary, that is an `Approver` decision, in the other
channel, on a `Request`.

**There is no per-agent provider and no per-agent API key.** One provider serves a
whole tree. A definition names a model *string*, which travels as the request's
`model` field — so a roster can move work between models the configured provider
serves, and cannot reach a second vendor, a second base URL or a second key. A
`Fallback` still applies to the whole tree or to none of it.

**A definition cannot grant.** `deny_write` and `deny_net` are the only two
boundary fields, they are whole-cloth denies rather than globs, and they compose
through `Policy::contain`. There is no way to express an allow, a per-path
exception, or "the same as the parent but for this one path". A child that needs to
be *wider* than its parent is a design that this crate refuses rather than a
configuration it supports.

**A definition does not touch the root.** A role, a model and a step cap apply to
spawned children only; the root of a tree has no identity, so `[[agent]]` cannot
change which model answers the top-level agent or what its system prompt says.

**There is no per-agent skills directory.** The tree's skills catalogue and its MCP
tools are shared: every agent in a tree is offered the same set, and a definition
cannot add or remove one. This was left out of 0.21.0 deliberately — a per-agent
catalogue is a second discovery path over the same directory, and the roster is not
worth that yet.

**Templates are not a template language.** No conditionals, no loops, no includes,
no partials, no nesting, and no escape for a literal `{{`. Substitution is
single-pass, so a value is never re-substituted. An argument that no placeholder
claims, in a template with no `$ARGUMENTS`, is silently dropped — and any `.md`
file in the directory is a template, including a `README.md`, because a name-based
exception is the start of a list nobody can predict.

**The stall window does not survive a resume.** `Progress` is in-memory and is not
checkpointed, so a resumed run — and a re-adopted child — starts its window at
zero, for both signals. A run that crashed three steps into a loop resumes with the
loop forgotten. Stall detection is also still a heuristic on a signal rather than a
proof: an agent making real progress the harness cannot see could in principle be
flagged, which is why the window is configurable and can be disabled.

**These tools are workspace tools.** `todo_write` and `ask_question` are offered by
the workspace loop and inside a tree. A single-file run (`TaskContract::new` plus
`run`) is offered one tool and has neither.

**A plain session turn has no responder.** An unbounded `Session::turn` builds its
own contract, so a question asked during one pauses the turn for a human rather
than being answered in-process. Register a `Responder` by giving that turn a
`TaskContract` through `turn_bounded`.

**One session or run takes one driver (0.62.0).** A run is driven under a lease, so
a second process driving the same run is refused with `Error::Conflict` before it
takes a step. A session head advances by compare-and-swap, so two processes taking
turns on the same session id do not both land on the head path — the one that loses
is told, and its turn row is left intact. Answering an answered question is still
an error, which is a report of the race rather than protection from it.

**A plan and a question belong to one run.** Both are keyed by `run_id`, so every
turn of a session and every child of a tree has its own plan and its own questions.
There is no aggregate view and no inheritance — a child does not see its parent's
plan.

**An over-long plan item is truncated silently.** The *item count* past
`TODO_MAX_ITEMS` is reported back to the model in the observation; text past
`TODO_TEXT_CAP` characters is cut without a marker.

## See also

- [Permissions and approval](permissions.md) — the `Approver`/`Request` channel an
  answer is not, and the `Policy::contain` narrowing a definition composes through
- [Composition: sub-agent trees](composition.md) — the containment caps a named
  agent is spawned inside
- [Sessions](sessions.md) — steering, the other thing that is text and not
  authorization, and the turn a question can pause
- [Resilience](resilience.md) — the stall window signal 2 shares its threshold with
- [Configuration — `io.toml`](configuration.md) — the scopes `[[agent]]` accumulates
  across, and the resolve-or-fail rule templates inherit
- [Tools and skills](tools-and-skills.md) — the catalogue a tree shares
- [Observability and replay](observability.md) — the `TodoWrote`, `QuestionAsked`
  and `QuestionAnswered` events, and the checkpoint a paused question resumes from
- [The public contract](../CONTRACT.md)
- [README](../../README.md)
