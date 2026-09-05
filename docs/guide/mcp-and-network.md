# MCP and network egress

Point the harness at MCP servers and their tools reach the agent beside the
built-ins, with every outbound connection decided by the same permission layer
that decides paths and binaries.

Because a configured server is the first thing here that can dial an arbitrary
host, the policy has a fourth act — `Act::Net` — beside read, write and exec.

```rust
use io_harness::{run_with, ApproveAll, McpServer, OpenRouter, Policy, Store,
                 TaskContract, Verification};

let contract = TaskContract::workspace(
    "summarise the repo's README into NOTES.md",
    "/path/to/repo",
)
.with_verification(Verification::WorkspaceFileContains {
    file: "NOTES.md".into(),
    needle: "#".into(),
})
.with_mcp([
    McpServer::stdio("files", "my-mcp-file-server"),
    McpServer::http("search", "https://mcp.example.com/mcp"),
]);

let policy = Policy::default()
    .layer("app")
    .allow_read("*")
    .allow_write("*")
    // The stdio server may start; nothing else may be executed.
    .allow_exec("my-mcp-file-server")
    // The HTTP server may be reached; every other host stays denied.
    .allow_net("mcp.example.com");

let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
```

- **Namespaced tools** — a server's tools arrive as `mcp__<server>__<tool>`, so a
  server advertising `write_file` cannot shadow the built-in. Both stay callable
  and distinct.
- **Two transports** — `McpServer::stdio` spawns a child process,
  `McpServer::http` dials a streamable-HTTP endpoint. One session per run, shared
  by the whole [agent tree](composition.md), not one connection per agent.
- **`Act::Net`** — an outbound connection is a policy decision with a target
  (`host` or `host:port`), matched by the same glob matcher as paths and binaries
  and decided by the same deny-first stack: `allow_net`, `deny_net`, `ask_net`. An
  `ask_net` routes to the `Approver` and, if deferred, survives a full process
  restart like any other [deferred approval](durable-runs.md).
- **Your provider still works under deny-all** — the harness contributes its
  configured provider's host as a *named* layer, `provider`, so you need not list
  it. `Policy::explain` attributes the allowance to that layer rather than hiding
  it. An explicit `deny_net` of your own provider still wins, and fails fast as a
  refusal rather than hanging.
- **Two gates per server** — starting a stdio server is an exec check on its
  binary; calling one of its tools is an exec check on the namespaced tool name.
  So a policy can allow a server generally and still deny one of its tools.
- **Contained downward** — a [child agent](composition.md) inherits its parent's
  network rules and can only narrow them; the spawn tool takes `deny_net`
  alongside `deny_write`.
- **Everything is traced** — connects (with transport), tools discovered, each call
  with latency and outcome, and every network verdict with the layer that decided
  it. The MCP conversation is a new table rather than a changed one, so an
  existing store gains it in place and an older binary still reads it.
- **Switched off is not absent** — `enabled = false` on a server, or
  `McpServer { enabled: false, .. }`, means it is never started and contributes no
  tools, while every listing still shows it as configured-and-off. A server that
  vanished from the listing could not be told apart from one that was never
  declared, and nothing could switch it back on. The field defaults to `true`, so
  every file written before 0.70.0 means exactly what it always meant.

Run it live: `cargo run --example mcp_run`.

## The target a rule is matched against (0.71.0)

An `allow_net` pattern is matched against a `host:port` string, not against a
URL. An application that wants to decide about a URL of its own — a preflight
before a run, a settings screen that says whether a configured endpoint is
reachable under the current policy — has to reduce it the same way the harness
does, or its answer is about a different string than the one the run will check.
So the reduction is public, and it is the only thing `io_harness::net` exports:

```rust
use io_harness::net::target;

assert_eq!(target("https://api.example.com/v1").as_deref(), Some("api.example.com:443"));
assert_eq!(target("ws://example.com/socket").as_deref(), Some("example.com:80"));
// An explicit port wins, and userinfo is not part of the host.
assert_eq!(target("https://user:pw@example.com:8443/x").as_deref(), Some("example.com:8443"));
// An IPv6 literal keeps its brackets, which is what makes the `:port` split unambiguous.
assert_eq!(target("https://[::1]:8080/x").as_deref(), Some("[::1]:8080"));
```

The port is always present, filled from the scheme when the URL omits it — 443
for `https` and `wss`, 80 for `http` and `ws` — so a rule that names a port has
something to match and a rule that does not is still matched by
`Policy::explain`'s bare-host form.

**`None` is a refusal, not "nothing to check".** This is the whole of what a
reimplementation gets wrong. `target` answers `None` for a URL with no `://`, an
empty authority (`https://`), an empty host or an empty port
(`https://host:/x`), and any scheme that opens no connection a policy could
govern — `file:`, `data:`, anything outside `http`/`https`/`ws`/`wss`. An
unrecognised scheme is a refusal rather than a pass-through precisely because "I
did not recognise this" and "this is harmless" are not the same statement. So
there is one correct shape for consuming it:

```rust,ignore
match target(url) {
    Some(t) => policy_allows(&t),
    None => false, // NOT `true`, and NOT "skip the check"
}
```

A caller that reads `None` as "no target, so nothing to decide" reports
*permitted* for exactly the URLs the runtime refuses.

0.71.0 also removed a case where the shape was right and the value was not:
`target("https://user@/x")` used to return `Some(":443")` after dropping the
userinfo, a hostless target that a permissive policy matches and allows. An
empty host is no host, so it is `None` now.

**A backslash ends the authority (0.74.0)**, as it does in the WHATWG URL parser
and in Chrome's GURL for exactly these four schemes:

```rust
use io_harness::net::target;

assert_eq!(target("http://127.0.0.1:11434\\@example.com/v1").as_deref(),
           Some("127.0.0.1:11434"));
```

Without it the backslash left the userinfo split an `@` to find, so that URL was
*checked* as `example.com:80` and would have been *dialled* at `127.0.0.1:11434`.
A reimplementation that splits the authority on `/`, `?` and `#` alone has that
same gap, which is the reason this is stated rather than left to the source.

## The local-address floor (0.74.0)

**A target the policy allows is refused anyway when it is local.** Every net
decision in this crate up to 0.73.0 was a hostname glob, so `Policy::permissive()`
handed the model this host's own admin ports, the private network around it and
the cloud instance-metadata service — and no rule an operator wrote was wrong for
letting it, because none of those is a hostname anybody thinks to deny. The floor
sits *under* the policy: it can refuse what your rules allowed, and it cannot
permit what your rules denied. A target already denied is not even resolved.

What is refused, quoted the way a refusal quotes it:

| Range | Why |
| --- | --- |
| `127.0.0.0/8`, `::1` | loopback — the whole `/8`, not just `127.0.0.1` |
| `0.0.0.0/8`, `::` | "this network"; a `connect()` to it reaches this host |
| `169.254.0.0/16`, `fe80::/10` | link-local |
| `10/8`, `172.16/12`, `192.168/16` | RFC 1918 private networks |
| `100.64.0.0/10` | carrier-grade NAT (RFC 6598) — not RFC 1918, so it needed naming separately |
| `fc00::/7` | unique-local, of which `fd00::/8` is the half anything real uses |
| `169.254.169.254`, `100.100.100.200` | cloud instance metadata, named so the refusal says "metadata" |
| `localhost`, `localhost.localdomain`, `*.localhost`, `*.local` | names reserved to this machine or this link (RFC 6761, RFC 6762) |
| `metadata.google.internal`, `metadata.goog` | metadata by name, refused before any resolver is consulted |

Both IPv6 spellings of an IPv4 address are reduced first — `::ffff:127.0.0.1` and
the deprecated `::127.0.0.1` land on the same rules as `127.0.0.1` — and a
short-form host that only a resolver expands (`2130706433`, `127.1`) is graded
after it is resolved rather than before.

**`IO_HARNESS_ALLOW_LOCAL_ADDRESSES=1` is the only way off it**, and the local
model runtime is what it is for: `Compatible::ollama` points at
`http://localhost:11434/v1`, which is the whole point of that run and is refused
without it. `1` and `true` lift the floor; anything else, including the variable
being absent, leaves it in place, and the value is re-read per connection rather
than latched at the first one. The metadata hostnames and the two metadata
addresses stay refused even when it is set — no local model runtime answers on
one.

**It is an environment variable and there is deliberately no `io.toml` key beside
it.** A config key that widens is a key a cloned repository could set, and
`[policy]` is accepted from a workspace file on the rule that such a file may
narrow and never widen. The environment of a process that has already started is
the one thing a hostile repository cannot write, so the widening lives there and
nowhere else. An embedder that wants it per-run sets the variable around the run
rather than looking for a key.

**A refusal names the floor as its layer** — `local-address floor` — so a trace
tells "your rules refused this" apart from "the floor underneath your rules
refused this", and the message carries the address that decided, the reason and
the key that would restore it. A host that resolves to a mix of permitted and
refused addresses is refused whole, and a host that resolves to nothing is refused
too: "nothing came back" is not "nothing objected".

**Where the check is made differs by call site, and the difference is not
cosmetic.** A floor that graded only names is one
`http://169.254.169.254.nip.io/` walks through — `nip.io` answers
`<anything>.<ip>` with that address — so the floor resolves. What that resolution
is worth depends on what dials afterwards:

| Call site | What it does | What that leaves open |
| --- | --- | --- |
| the HTTP MCP transport, the egress proxy | resolve once, grade, dial exactly the graded addresses | nothing — check and dial are the same answer |
| a provider endpoint | resolve once, grade, dial exactly the graded addresses (0.80.0) | nothing for a name. Until 0.80.0 the `Provider` owned its own client and resolved the name a second time to dial, so a name that answered with a local address only on the second ask reached it. Each built-in provider now holds a client pinned to the addresses its endpoint graded at, built once and reused, so the check and the dial are one answer. A **named** local endpoint — `http://localhost:11434/v1` for a local model — is refused unless `IO_HARNESS_ALLOW_LOCAL_ADDRESSES=1` is set, which is what a run already required and what direct use of a `Provider` did not |
| `browser_navigate` | graded by name only | a *name* that resolves onto a local address. Chrome resolves each URL itself, and pinning a navigation to an address breaks SNI and certificate validation. `browser_navigate("http://169.254.169.254.nip.io/")` under a policy that allows every host reaches cloud metadata |

The way to close the browser row is to route it through the run's egress proxy,
which already resolves once and dials what it graded. That wiring is not in this
release, and this table is here rather than a claim that the floor covers
everything.

## Asking whether a server actually answers

A policy preflight tells you whether a server is *permitted* to start. It cannot
tell you whether it works: a wrong command and an unreachable host both pass it.
`probe_mcp` starts one configured server, reports what happened, and shuts it down
again.

```rust
use io_harness::{probe_mcp, McpProbe, McpServer, Policy};

let server = McpServer::stdio("files", "my-mcp-file-server");
match probe_mcp(&server, &policy).await {
    McpProbe::Answered { tools } => println!("{} tools", tools.len()),
    McpProbe::Refused { rule, layer, .. } => println!("policy said no: {rule:?} in {layer:?}"),
    McpProbe::NotStarted { reason } => println!("the command is wrong: {reason}"),
    McpProbe::Unreachable { reason } => println!("the host is not there: {reason}"),
    McpProbe::TimedOut { secs } => println!("no answer within {secs}s"),
    McpProbe::Disabled => println!("switched off; nothing was started"),
    _ => {}
}
```

Those are four different problems with four different fixes, which is the whole
reason the probe reports them apart rather than as one failure. It is bounded by
the server's own `timeout_secs` — **including the handshake**, which the run loop's
own connect does not bound — so a server that accepts a connection and then says
nothing is reported as timed out rather than hanging the caller. A disabled server
answers `Disabled` without being started.

## What a refusal looks like

An out-of-policy **tool call** is an observation the model can adapt to, not a
crashed run — the same treatment a refused path already gets:

```text
[exec refused] mcp__files__delete_everything (rule mcp__files__delete_*) — the policy forbids this; carry on without it
```

Since 0.70.0 the call goes through the same approval gate a write does, so a
policy whose `exec` effect is `Ask` — which `Policy::default()`'s is — asks the
approver instead of refusing. An approver that denies produces the observation
above; one that defers pauses the run with a pending row, and the run resumes
through `resume_with_decision` exactly as a deferred write does.

A refused **server** stops the run before anything happens — because unlike a
single tool call, there is no useful way for the agent to work around a
capability it was told it had. Which error you get is decided by which check said
no, and the two are not interchangeable:

| What happened | Error |
| --- | --- |
| the policy refused a stdio server's binary | `Error::Refused { act: "exec", target: <command>, .. }` |
| the policy refused a remote server's host | `Error::Refused { act: "net", target: <host:port>, .. }` |
| the policy **allowed** the server, and then it would not spawn, the handshake failed, or its tools could not be listed | `Error::Mcp { server, reason }` |

`Error::Mcp` is the far side of the policy line rather than the refusal itself.
A caller mapping errors on the refusal path wants `Error::Refused` — that is the
one case the check exists for, and matching `Error::Mcp` for it catches a broken
command instead of a boundary decision.

## Serving this crate's own tools over MCP (0.78.0)

Everything above is this crate as an MCP **client**. Behind the `mcp-server`
feature it also serves: another harness spawns this one, speaks MCP to it on
stdio, and calls `grep`, `read_file`, `edit_file`, `exec` and the rest — through
this crate's policy rather than its own.

That is the point of it, and it is worth saying plainly. A tool is a few lines of
process spawning; a deny-first layered policy, an approval tier, a three-OS
sandbox and a durable journal are not. What is being lent is the boundary, not the
tool.

```rust
use io_harness::{serve_mcp, McpServerConfig, Policy};

serve_mcp(
    McpServerConfig::new(".", "runs.db")
        .with_policy(Policy::default().allow_read("src/**")),
).await?;
```

`serve_mcp_with` takes an `Approver` where `serve_mcp` uses `DenyAll`.

### What a served call goes through

The same dispatch a model's call goes through — so the policy gate decides it, a
`policy_events` row records the decision, the journal opens and closes an attempt
for it, and it is announced on the `Observer` channel. A served session opens a
store and starts one run, so afterwards it is readable with `Attach` and
`Store::events_since` exactly as any run is.

### An asking rule refuses

There is no human at the far end of a pipe. The default approver is `DenyAll`, so
a rule whose effect is `Ask` comes back as a refusal carrying this crate's own
words rather than blocking on somebody who is not there. Paired with the default
`Policy::default()` — reads allowed, writes and execs asking, egress denied — that
means reads work and every mutation refuses until an operator names it. An
operator who wants a different answer supplies their own `Approver`; that is a
decision they take, not one a client can take for them.

### What is not served

`ask_question`, `ask_questions`, `propose_plan`, `spawn`, `send_message`,
`read_messages`, `read_skill`, `remember`, `forget` and `todo_write`, named in
`MCP_SERVER_UNSERVED`. Most need something a served session has not got — a
person to answer, a plan gate to decide, children to talk to, or a server-side
document a remote caller should not be handed. Offering one and refusing every
call to it would be a worse answer than not offering it.

**The last three are excluded because they are ungated, not because they are
useless.** `remember`, `forget` and `todo_write` write to the harness's own store
rather than to the workspace, so the policy has no path to check and deliberately
does not see them; their only boundary is the plan gate, which a served session
also does not have. Durable memory is recalled into a run's context and a served
session shares its memory key with any run over the same root, so serving them
would let a client with no policy grant plant text that reaches every later run
over that workspace.

The served set is written out by name and a test pins it, so a tool added in a
later release fails that test until somebody decides which side it belongs on.
Deriving it as "the catalogue minus the unserved list" would have made a new
built-in servable silently, which is the opposite of the guarantee.

## The limit, stated plainly

The harness governs the connections **it** opens. A stdio MCP server is a separate
process: the harness decides whether it may start and which of its tools may be
called, but once running it dials whatever it likes. Isolating a server's own
egress would need OS-level containment, which the harness does not build. The
[execution sandbox](sandbox.md) contains the subprocesses a verification gate
runs; it is not applied to a configured MCP server.

**And the same sentence is true with the roles swapped.** When this crate is the
server, it governs the calls it is asked to make and nothing about the process
asking. The client is not authenticated — a stdio pipe has no identity — so
whoever can spawn the server can call every tool the policy allows. The boundary
being lent is the policy, not an access-control list, and the operator who starts
the server is the one who decides what is inside it.

**Stdio only.** There is no HTTP listener in this release: that would be a bind
address, an auth story and a session manager, none of which this is arguing for.

**Tools only.** No resources, no prompts, no sampling, no roots, no elicitation.
The `initialize` handshake advertises the `tools` capability and nothing else.

**Not a proxy.** This crate's own client can reach other people's MCP servers, and
this server does not re-export them. Two policies in one path would make it
unclear which one refused.

## See also

- [Permissions and approval](permissions.md) — the layer stack `Act::Net` joins,
  and how a deferred `ask_net` is answered
- [Tools and skills](tools-and-skills.md) — the in-process half of the same
  extension surface, and the same boundary stated for it
- [Agent composition](composition.md) — how a child inherits and narrows network
  rules
- [Durable runs](durable-runs.md) — approvals that survive a process restart
- [Execution sandbox](sandbox.md) — what OS-level containment the crate does have,
  and where it applies
- [Observability and replay](observability.md) — the MCP and network rows in the
  trace
- [The public contract](../CONTRACT.md)
- [README](../../README.md)
