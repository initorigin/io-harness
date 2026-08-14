<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Active releases

## io-harness 0.54.0

**Execution state:** DEVELOPING
**Release fit:** probable
**Target mode:** published
**Outcome:** Ask a session for something that needs three files read, and the reads are already running while the model is still finishing its sentence. A turn that reads then answers gets its reads back sooner by the width of the rest of the completion — the tokens after the last tool call, which on a model that narrates its plan is most of the message.
**Today the crate throws the early half away on purpose, and the comment saying so is in the source.** Both provider wires reassemble tool-call arguments fragment by fragment as they arrive (`src/provider/anthropic.rs:508` and `src/provider/openai_wire.rs:511`, each a `BTreeMap<u64, (String, String)>` of index to name and joined JSON), and both refuse to hand a fragment onward: `text_delta` at `src/provider/anthropic.rs:484-490` says an `input_json_delta` "is not renderable and is not safe to act on half-parsed, and the accumulator owns reassembling those." That is right about a *fragment*. It is not right about a *finished* call, and the difference between the two is one successful `serde_json::from_str`. `Provider::complete_streaming` (`src/provider/mod.rs:1303`) then has no way to say it either: its sink is `&(dyn Fn(&str) + Send + Sync)` — assistant text, and nothing else. A tool call reaches the run loop only inside the finished `CompletionResponse`.
**So the wait is real and it is nobody's fault.** 0.41.0 already overlaps read-only calls with each other (`src/run.rs:9189`, `read_batch`); what it cannot do is start any of them before the model has stopped talking. This release moves the starting line, and moves nothing else: the calls that overlap are the same calls, run by the same `ReadWork::run` (`src/run.rs:8987`), folded back through the same slot-by-index path, into the same trace.



_Canonical sources: products/<id>/execution/active.yaml, products/<id>/releases/<version>.yaml_
