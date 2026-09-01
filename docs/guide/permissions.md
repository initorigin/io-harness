# Permissions and approval

The permission boundary decides what an agent may read, write, execute, and
connect to, and routes what is sensitive-but-permitted to a human before it
happens.

A `Policy` is a stack of named layers plus a per-action default. It is evaluated
**deny-first across the whole stack**: a deny in any layer beats an allow in any
other, so a layer can add capability but can never re-allow what a layer beneath
it denied.

```rust,no_run
use io_harness::{ApproveAll, OpenRouter, Policy, Session, Store};

# async fn demo() -> io_harness::Result<()> {
let policy = Policy::default() // reads open, writes/execs ask, egress denied, secrets denied
    .layer("project")
    .allow_read("*")
    .deny_read("secrets/*")
    .deny_write("secrets/*");

// The boundary belongs to the turn, not to the session: every turn of the
// conversation is a run, checked by this policy the same way a one-shot is.
let store = Store::open("runs.db")?;
let provider = OpenRouter::from_env()?;
let mut session = Session::open(&store, "/path/to/repo")?;
session
    .turn("read the config and tell me what it sets", &provider, &store, &policy, &ApproveAll)
    .await?;

// Why was that refused? Same function the tool layer enforces with.
let verdict = policy.explain(io_harness::Act::Write, "secrets/key.txt");
println!("{:?} by rule {:?} in layer {:?}", verdict.effect, verdict.rule, verdict.layer);
# Ok(()) }
```

The one-shot form takes the same policy: `run_with(&contract, &provider, &store,
&policy, &ApproveAll)` runs a `TaskContract` under exactly this boundary, with
the same checks and the same trace.

`Policy::check` and `Policy::explain` are literally the same function — `check`
calls `explain` — so an explanation can never describe a boundary different from
the one enforced.

**The default is permissive.** A caller who passes no policy — plain `run()` —
gets `Policy::permissive()`, which enforces nothing. The boundary is opt-in.
This is a deliberate trade-off for backward compatibility, not an oversight.
`Policy::default()` and `Policy::permissive()` are two different things:
`default()` is the tiered policy described below, `permissive()` is no
enforcement at all.

**A policy requires workspace mode.** Single-file contracts have no
policy-aware tool layer, so passing a non-permissive policy to a
`TaskContract::new` contract fails with `Error::Config` rather than running with
nothing checking. Build the contract with `TaskContract::workspace` when you
want a boundary.

## What asks, what is refused

`Policy::default()` sets the tiers, following the same shape Claude Code uses:

| Action | Default | Note |
| --- | --- | --- |
| Read | allow | `.env`, `*.pem`, `id_rsa`, `id_ed25519`, `*.key` denied outright |
| Write | **ask** | including overwriting a file the path rules already allow; `.git/*`, `*/.git/*`, `io.toml` and `io.local.toml` denied outright |
| Exec | ask | `rustc` and `<test-binary>` allowed, so verification works |
| Net | **deny** | an outbound host is not something a human can meaningfully approve on sight mid-run; name your hosts with `allow_net` — your configured provider is allowed for you |

The secret patterns are denied for **both** read and write: nothing an agent
legitimately does rewrites a private key. They live in a layer named
`builtin-secrets`, and the two exec allows in one named `builtin-exec`, so a
refusal that comes from the built-ins is attributable to them in the trace.

The write denies on `.git/*`, `*/.git/*`, `io.toml` and `io.local.toml` are a
third layer, `builtin-config` (0.74.0), and it is its own layer rather than four
more rows in `builtin-secrets` because these are not secrets and are not denied
for reading. They are the files something *else* reads back: a repository's own
git config selects the filter and diff drivers `git` then runs, and a
configuration file is read by the next `Config::discover`. So a refusal here is
answered by a different sentence than "that is a private key", and the trace has
to be able to tell the two apart. The pattern is `.git/*` and not `.git*`, so
`.gitignore`, `.gitmodules` and `.gitattributes` are untouched, and the git
built-ins still cover every legitimate reason to write inside `.git`.

A **denied** action never reaches the approver — it is refused and reported to
the model as a tool result it can adapt to, and the refusal consumes a step, so
a model retrying it reaches the step cap rather than looping. Only the
**ask** tier prompts.

### Listing the three effects (0.71.0)

An application that puts the effects in front of an operator — a dropdown, an
`--effect` flag, a config validator naming what it accepts — needs them as data
rather than as three variants written out by hand. `Effect::ALL` is that list, in
the strictness order the derived `Ord` documents, and `Effect::as_str` is the
word a policy file spells each one with: the deserializer's own spelling, not a
second one to keep in step with it.

```rust
use io_harness::Effect;

assert_eq!(Effect::ALL, [Effect::Allow, Effect::Ask, Effect::Deny]);
assert_eq!(Effect::Ask.as_str(), "ask");

// Strictest-first is `.rev()` on the same list — the precedence
// `Policy::explain` resolves in, rather than a second list to keep in step.
let strictest_first: Vec<Effect> = Effect::ALL.into_iter().rev().collect();
assert_eq!(strictest_first, [Effect::Deny, Effect::Ask, Effect::Allow]);
```

**`Effect` is not `#[non_exhaustive]`, and `ExecMode` is** — which is why only
one of the two needs this list to be shipped for you. A hand-written match over
the three effects is self-policing: a fourth effect stops your build until you
handle it. A hand-written list of exec modes is not policed at all, because
`ExecMode` is `#[non_exhaustive]` and your match needs a wildcard arm — a mode
added in a later release lands in that arm, your build keeps passing, and the
mode is silently missing from your menu. Only a *removal* would break you, which
is the wrong way round for the enum that decides where model-produced code may
write. The enum whose values are the more security-relevant of the two is the one
that cannot be gated downstream, and that is the whole reason `ExecMode::ALL`
exists; it is in
[Running commands](command-execution.md#the-three-modes-0400-0460).

### How a rule matches

A rule's `pattern` is a glob — `*` matches any run of characters including `/`,
`?` matches one — tested against the target's full relative path. For `Act::Exec`
the target is the binary name. Specificity does not matter: a broad deny beats a
narrow allow.

**A deny is matched more loosely than an allow, and only a deny (0.74.0).** Three
relaxations belong to `Effect::Deny` rules and to nothing else, because each makes
a pattern cover more than its text says and that is only ever safe in the refusing
direction:

- the **basename retry**, which is what lets `.env` deny `config/.env`, the same
  way the `find` tool matches — and which, applied to allows, let
  `allow_exec("cargo")` also grant `./target/debug/cargo`, a binary the agent had
  built for itself. For `Act::Exec` the retry splits on `\` as well as `/`, so
  `deny_exec("kubectl.exe")` covers the Windows path a resolved argv carries;
- a **case-folded compile for `Act::Exec`**, because half the volumes this crate
  runs on will spawn `RM` for `rm` and nothing here can tell whether the volume a
  given argv resolves on folds case;
- the **host fold for `Act::Net`** below.

An allow keeps granting exactly what it names. One that misses a spelling —
`allow_exec("rustc")` against `RUSTC` — falls to the tier default, which asks or
refuses.

A network target arrives as `host:port`, and both forms are tried, so
`allow_net("api.example.com")` covers whatever port the URL resolved to while
`allow_net("api.example.com:443")` still means that port and no other. For a
**deny**, rule and target are folded to one spelling first — lowercased, with one
trailing root dot removed — because DNS is case-insensitive and `evil.example.`
names the same server, so `deny_net("evil.example")` has to catch
`EVIL.example:443` and `evil.example.` or it is not a boundary. Folding an allow
would widen it, so an allow is compared as written.

On Windows, patterns and targets are folded to one form before comparison — a
`\\?\` verbatim prefix is stripped and `\` becomes `/` — because a deny built
from a canonicalized `Path` otherwise never matched the backslash target it was
meant to cover, and a permission rule that misses fails open. The fold is a
deliberate no-op on unix, where `\` is an ordinary character in a filename.

### Egress and the provider

`Defaults::net` deserializes to `Deny` when the field is absent, which is the
case for every policy serialized before the `net` field existed. That is a
deliberate behaviour change — an old config that made outbound calls stops
making them until it carries a `net` allow — chosen because the alternative
silently leaves egress ungoverned for exactly the callers who upgraded to
govern it.

Your configured provider's own endpoint is allowed for you, but by a *named*
layer (`provider`) merged beneath your own, not by an exemption: an operator
reading the trace sees why that one host was allowed and which layer said so.
Because it is a merge and not a containment rule, a caller who explicitly denies
its own provider host still wins — deny is absolute across layers — and the run
fails fast as a refusal rather than hanging.

The rest of egress — MCP servers, the shape of a `Act::Net` refusal, and what
the policy does *not* govern once a stdio server is running — is in the
[MCP and network egress guide](mcp-and-network.md).

## The approver

```rust
use io_harness::approve::{Approver, Decision, DecisionFuture, Request};

impl Approver for MyUi {
    fn decide<'a>(&'a self, request: &'a Request) -> DecisionFuture<'a> {
        Box::pin(async move {
            match self.ask_the_human(request).await {
                Answer::Yes      => Decision::approve(),
                Answer::No       => Decision::deny("not this one"),
                Answer::NotNow   => Decision::Defer,   // persist and decide later
            }
        })
    }
}
```

The trait is object-safe (`Box<dyn Approver>`) and the future may stay pending
indefinitely — the run waits rather than timing out. `Decision::Approve` can
also carry a rewritten action (`modified`) or rules to `remember` for the rest
of the run. Both are re-checked against the policy: **an approval cannot move an
action across a deny, and a remembered allow cannot override one.**
Remembered rules come back on `RunResult::remembered` for you to persist.

**A `modified` request on an `Act::Exec` action is refused rather than applied
(0.74.0).** Nothing runs, and the refusal names both the argv that was asked about
and the one that came back. Every consumer of an exec approval — `exec`, `shell`,
the git built-ins, a registered tool, an MCP tool — dispatches the argv it parsed
*before* the gate was consulted and reads only `remember`, so the rewrite had no
consumer: a human approved one command while another ran, and the trace recorded
the one that did not. In the direction that matters more, an approver *narrowing*
an argv was overruled without ever being told. Approve the action as asked, deny
it, or narrow it with an exec rule. Rewrites of a read or a write are unaffected —
those two paths read the rewritten target and content back off the gate — and the
same refusal applies to a `modified` on a pending `exec` or `net` handed to
`resume_with_decision`.

Built-ins: `ApproveAll`, `DenyAll`, `StdinApprover`. `DenyAll` is the safe
default for an unattended run that must never take a sensitive action;
`StdinApprover` prompts on the terminal and treats anything other than `y` as a
denial.

### A model in the approver's chair (0.42.0)

`ApproveAll` opens the whole grey tier, `DenyAll` closes it, and `StdinApprover`
needs somebody at a terminal. For an unattended run that is a choice between too
much and nothing. `ModelApprover` is the fourth answer:

```rust
use io_harness::{ModelApprover, Policy, run_with};

// Its own provider and its own model — never the one making the calls.
let approver = ModelApprover::new(reviewing_provider, "a-different-model");

let policy = Policy::default()
    .layer("ops-baseline")
    .allow_read("*")
    .allow_write("src/*")
    .deny_write("src/main.rs")   // never reaches the model at all
    .ask_exec("cargo *");        // this is what it decides

let result = run_with(&contract, &provider, &store, &policy, &approver).await?;
```

It is told the act, the target, the bytes a write would land, **the rule and the
layer that flagged the call**, and the run's goal — then approves, denies with a
reason the agent reads and adapts to, or defers, which persists the action and
stops the run for a person to answer later with `resume_with_decision`. An answer
it cannot read is a defer: a machine standing in for an absent human parks what it
does not understand rather than waving it through.

Two bounds are worth knowing before you install one. The policy's denies never
reach it, so it is a filter over the grey tier and not a wall; and it may not
answer for its own model — a `ModelApprover` whose model is the model making the
call is refused at run start, before either provider is billed, unless you wrote
`allow_self_approval(true)` and meant it.

An approver of your own can have the same context: `Approver::decide_in_context`
receives an `ApprovalContext { goal, rule, layer }` and defaults to forwarding to
`decide`, so implementing it is optional and ignoring it costs nothing.

## Deferring past the end of the process

```rust
match result.outcome {
    RunOutcome::AwaitingApproval { request_id, .. } => {
        // ...hours later, another process, same rusqlite file
        let store = Store::open("runs.db")?;
        io_harness::resume_with_decision(
            &contract, &provider, &store, run_id, request_id,
            Decision::approve(), &policy, &approver,
        ).await?
    }
    _ => result,
};
```

The pending action is persisted with the content the human was shown, so the
resumed action is exactly the one approved. The policy is re-checked on resume,
so a deny that landed while it waited still holds — the pause grants no immunity
— and a re-check that comes back `Deny` resolves the request as denied and ends
the run with `RunOutcome::Denied`. Deferring again simply leaves the request
pending and the run paused.

An agent *tree* pauses the same way and continues with
`resume_tree_with_decision`; see [durable runs](durable-runs.md).

## Reviewing the approach, before there is anything to approve (0.31.0)

An `Approver` is asked about one action, after the agent has decided to take it.
A **plan gate** is asked about the whole approach, before the agent has done
anything at all:

```rust
use io_harness::{PlanGateNone, TaskContract};
use std::sync::Arc;

let contract = TaskContract::workspace("port the parser", "/repo")
    .with_plan_gate(Arc::new(PlanGateNone));
```

The run now opens in a planning phase. The agent may `grep`, `find` and
`read_file` as much as it likes and may change nothing: a `plan-gate` layer denies
every `Act::Write` and every `Act::Exec` for the duration, which covers the
built-ins, every registered `Tool` and every MCP tool through the same deny-first
resolution as everything else. The only tool that works is `propose_plan`.

What comes back is a `Plan` — ordered steps, each optionally naming the
`AgentDef` that will own it — and a `PlanGate` answers with one of three
verdicts:

```rust
use io_harness::{Plan, PlanGate, PlanReview, PlanVerdict};

#[derive(Debug)]
struct Frugal;

impl PlanGate for Frugal {
    fn review<'a>(&'a self, plan: &'a Plan) -> PlanReview<'a> {
        Box::pin(async move {
            Some(match plan.agents().any(|a| a == "deep-thinker") {
                true => PlanVerdict::revise("do not spawn `deep-thinker` for this"),
                false => PlanVerdict::Approve,
            })
        })
    }
}
```

`Approve` ends the phase and hands the plan back to the model as the approach it
agreed to. `Revise` puts the correction in front of it and leaves the phase on,
so it proposes again with nothing written. `Cancel` stops the run with
`RunOutcome::PlanRejected`.

Returning `None` — which is what `PlanGateNone` always does, and the honest
default for unattended work — persists the plan and pauses the run with
`RunOutcome::AwaitingPlan`. This process may then exit:

```rust
match result.outcome {
    RunOutcome::AwaitingPlan { plan_id, .. } => {
        // ...another day, another process, same rusqlite file
        let store = Store::open("runs.db")?;
        let pending = store.plan(plan_id)?.expect("proposed earlier");
        println!("{}", pending.plan.render());

        io_harness::resume_with_plan_decision(
            &contract, &provider, &store, pending.run_id, plan_id,
            PlanVerdict::Approve, &policy, &approver,
        ).await?
    }
    _ => result,
};
```

Whether the gate has been satisfied is read back from the store at every loop
entry, never carried in memory — so a run approved in one process and killed in
the next does not plan again, and one that was never approved does not start
writing because the approval died with the process that held it.

This is **not** the `todo_write` tool. That one records a plan the agent is
already executing so an operator can watch it; see
[agency](agency.md). This one executes nothing until an answer arrives.

## Sharing one policy between apps

`Policy` is `serde`-serializable, so an application layer reads the same format
and neither writes its own parser. Compose layers with `merge`:

```rust
let effective = shared_base.merge(app_local);
```

`merge` concatenates layers and tightens defaults only — for each action the
stricter of the two wins — so an overlay may add allows to widen the base's
*rules* but can never loosen its defaults, and can never re-allow one of its
denies.

The recommended convention is **user base → project layer → app overlay**, each
app keeping its own config file over a shared base. The crate composes a stack
it is handed; **it does not discover config files** — locations and precedence
are the adopting app's responsibility. Because denies are absolute across
layers, a shared base stays trustworthy no matter what an app stacks on top.

`merge` is the composition for peers. The one-way derivation used for a child
agent is `Policy::contain`, which can only narrow; see
[agent composition](composition.md).

Run it live: `cargo run --example policy_run`.

## See also

- [Agent composition](composition.md) — how a policy narrows down a tree
- [Execution sandbox](sandbox.md) — what confines an execution the policy allowed
- [MCP and network egress](mcp-and-network.md) — `Act::Net` and its boundary
- [Durable runs](durable-runs.md) — how a policy survives a process restart
- [Tools and skills](tools-and-skills.md) — what a registered tool is and is not governed by
- [The public contract](../CONTRACT.md)
- [README](../../README.md)
