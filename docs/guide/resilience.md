# Resilience

A run survives more than a crash: provider failures are typed and retried on
their own terms, a second provider can stand behind the first, and an agent that
stops getting anywhere is stopped rather than left to spend its whole step budget.

The failures this covers are the ones a single `String` error cannot tell apart.
A 429 and a wrong API key are not the same value and are not retried equally
eagerly. A server that accepts a connection and then stops writing is caught by a
request deadline instead of hanging the run forever. A response that arrives and
cannot be read is `Malformed` rather than "the model chose not to call a tool".
An escalation records whether the failure was one another attempt could have
survived, so a caller can tell a transient escalation from a terminal one instead
of re-running both.

## Failures you can branch on

```rust
use io_harness::{Error, ProviderErrorKind};

match harness_result {
    Err(Error::Provider { kind: ProviderErrorKind::Auth, .. }) => // your key
    Err(Error::Provider { kind: ProviderErrorKind::RateLimited, retry_after, .. }) => // wait
    Err(Error::Provider { kind, .. }) if kind.is_retryable() => // transient
    _ => {}
}
```

`Transport`, `Timeout`, `RateLimited`, `Server` and `Malformed` are retryable;
`Auth` and `Request` are not, because re-sending a request the server read and
refused is two failures instead of one. Retries wait, doubling to a ceiling and
honouring the server's `Retry-After` above that ceiling — and never sleeping past
the run's own time budget.

The status-to-kind mapping lives in one place, so OpenRouter, OpenAI and
Anthropic cannot drift apart on what a 503 means.

## A second provider behind the first

```rust
use io_harness::provider::Fallback;
use io_harness::{Anthropic, OpenRouter};

let provider = Fallback::new(OpenRouter::from_env()?, Anthropic::from_env()?);
let result = io_harness::run(&contract, &provider, &store).await?;
```

`Fallback` is itself a `Provider`, so no entry point changes, and it nests for
three — `Fallback::new(a, Fallback::new(b, c))`. It falls through only on a
failure another provider might not have: it asks the same question
`ProviderErrorKind::is_retryable` answers for a retry, because a failure about
the request itself, or about your own configuration, will happen again at the
next vendor. Both hosts are authorized against the
[egress policy](mcp-and-network.md) before the first step — a fallback is not a
way around it.

Run it live: `cargo run --example fallback_live`.

## An agent that stops getting anywhere

```rust
use io_harness::StallPolicy;

let contract = contract.with_stall_policy(StallPolicy { window: 3, max_replans: 1 });
```

A stall needs both halves: `window` consecutive steps that changed nothing in the
workspace **and** repeated a tool call the window already saw. One alone is not
enough — a run reading four different files is working, not stuck. On the first
stall the agent is told what it already tried and given one chance to change
approach; if it stalls again the run ends as `RunOutcome::Stalled` instead of
spending the rest of its step budget. `window: 0` switches it off.

This is a failure that was measured, not imagined: the 0.10.0 evidence records a
live run re-reading the same four files for sixteen consecutive turns before
hitting its step cap.

Run it live: `cargo run --example stall_live`.

## The limits, stated plainly

**Fallback does not promise equivalence.** Falling over swaps the model mid-run,
and the harness cannot see whether the replacement is as capable or even the same
size. The provider that answered is recorded per step so you can tell.

**The request deadline defaults to 600 seconds** — long enough that no realistic
single completion reaches it, since a full 8192-token stream at a sluggish 15
tokens/second is about nine minutes. A hung socket is caught by any finite
deadline, so the default is deliberately generous. You can name it and replace it
— `with_timeout` on any of the three providers, and the default is public as
`REQUEST_TIMEOUT`:

```rust
use std::time::Duration;
use io_harness::{OpenRouter, REQUEST_TIMEOUT};

// Slower model: raise it. Impatient caller: lower it. Rebuilds the HTTP client,
// so call it before handing the provider to a run.
let provider = OpenRouter::from_env()?.with_timeout(Duration::from_secs(1_800));
assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(600));   // the default it replaces
```

`Anthropic::with_timeout` and `OpenAi::with_timeout` are the same call, and each
provider module re-exports `REQUEST_TIMEOUT` beside its own type.
`McpServer::with_timeout` sets the equivalent deadline for a configured
[MCP server](mcp-and-network.md).

**Stall detection is a heuristic on a signal, not a proof.** It reads whether a
write changed the file's bytes and whether a tool call repeated. An agent that
makes real progress the harness cannot see — thinking, or work in a tool whose
effects are outside the workspace — could in principle be flagged, which is why
the window is configurable and can be disabled.

**A retry is not a queue.** Honouring `Retry-After` is not the same as modelling a
provider's rate limit, and nothing here tracks a provider's health across runs: a
provider that failed the last run starts the next one trusted.

## See also

- [MCP and network egress](mcp-and-network.md) — the egress policy every
  provider host, fallback included, is authorized against
- [Durable runs](durable-runs.md) — the other half of surviving a long run:
  checkpoints and resume
- [Context and memory](context-and-memory.md) — the token budget a retry may not
  sleep past
- [Observability and replay](observability.md) — the per-step provider record and
  the stall rows
- [The public contract](../CONTRACT.md)
- [README](../../README.md)
