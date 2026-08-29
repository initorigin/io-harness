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
[mcp__files__delete_everything refused] (rule mcp__files__delete_*) — the policy forbids calling this tool
```

A denied **host**, or a configured server that will not start, stops the run
before anything happens, with `Error::Refused { act: "net", .. }` or
`Error::Mcp { server, reason }` — because unlike a single tool call, there is no
useful way for the agent to work around a capability it was told it had.

## The limit, stated plainly

The harness governs the connections **it** opens. A stdio MCP server is a separate
process: the harness decides whether it may start and which of its tools may be
called, but once running it dials whatever it likes. Isolating a server's own
egress would need OS-level containment, which the harness does not build. The
[execution sandbox](sandbox.md) contains the subprocesses a verification gate
runs; it is not applied to a configured MCP server.

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
