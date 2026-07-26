<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Active releases

## io-harness 0.9.0

**Execution state:** DEVELOPING
**Release fit:** high
**Target mode:** published
**Outcome:** A developer embedding io-harness extends it inside their own binary, without MCP and without forking the crate. They implement the public `Tool` trait for an action their product already knows how to perform — query their database, call their internal API, render their template — register it on the task contract next to the MCP servers, and the model is offered it beside `grep`, `find`, `read_file`, and `write_file`. It is not a privileged back door: the same 0.4.0 policy decides whether it may be called, a refusal is an observation the agent adapts to rather than a failed run, its result is size-capped where it enters the context, every call is in the rusqlite trace, and a 0.5.0 child agent inherits the same set under the same narrowed policy.
The same developer shapes *how* the agent approaches a class of task without touching Rust. They point the contract at a directory of skills — markdown instruction files, optionally with the `name`/`description` frontmatter they already write for other agent tools — and the agent is told what skills exist and loads the body of the one it needs, on demand, through a built-in `read_skill` tool. Instructions, not code: a skill cannot execute anything, and reading one is an ordinary policy-governed file read.
Upgrading from 0.8.1 is a version bump and nothing else. Every existing public item keeps its name and shape; a contract that registers no tools and no skills behaves exactly as it did.


**Constraints** (user-estimate): time One working session, continuous. The owner asked for the harness to be built out without stopping between stages, so 0.9.0 is expected to reach a publishable state in this session rather than across several., budget —, capacity 0.9.0 is the first of four consecutive pillar releases (0.9.0 tool layer, 0.10.0 context and memory, 0.11.0 resilience, 0.12.0 observability and evaluation) the owner asked for in one stretch. Sprawl here is paid for out of the other three, so this release keeps to its stated scope and defers anything that grows it.


_Canonical sources: products/<id>/execution/active.yaml, products/<id>/releases/<version>.yaml_
