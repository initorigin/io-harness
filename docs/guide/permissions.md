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
| Write | **ask** | including overwriting a file the path rules already allow |
| Exec | ask | `rustc` and `<test-binary>` allowed, so verification works |
| Net | **deny** | an outbound host is not something a human can meaningfully approve on sight mid-run; name your hosts with `allow_net` — your configured provider is allowed for you |

The secret patterns are denied for **both** read and write: nothing an agent
legitimately does rewrites a private key. They live in a layer named
`builtin-secrets`, and the two exec allows in one named `builtin-exec`, so a
refusal that comes from the built-ins is attributable to them in the trace.

A **denied** action never reaches the approver — it is refused and reported to
the model as a tool result it can adapt to, and the refusal consumes a step, so
a model retrying it reaches the step cap rather than looping. Only the
**ask** tier prompts.

### How a rule matches

A rule's `pattern` is a glob — `*` matches any run of characters including `/`,
`?` matches one — tested against the target's full relative path *or* its
basename, the same way the `find` tool matches. That is what lets `.env` deny
`config/.env`. For `Act::Exec` the target is the binary name. Specificity does
not matter: a broad deny beats a narrow allow.

A network target arrives as `host:port`, and both forms are tried, so
`allow_net("api.example.com")` covers whatever port the URL resolved to while
`allow_net("api.example.com:443")` still means that port and no other.

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

Built-ins: `ApproveAll`, `DenyAll`, `StdinApprover`. `DenyAll` is the safe
default for an unattended run that must never take a sensitive action;
`StdinApprover` prompts on the terminal and treats anything other than `y` as a
denial.

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

## Sharing one policy between apps

`Policy` is `serde`-serializable, so io-cli and io-studio read the same format
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
