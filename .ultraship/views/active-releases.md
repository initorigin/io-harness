<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Active releases

## io-harness 0.48.0

**Execution state:** DEVELOPING
**Release fit:** probable
**Target mode:** published
**Outcome:** Embed this crate and know that *everything* the agent starts is inside the boundary — not only the foreground commands — and that a policy naming three hosts means three hosts to a contained command rather than all of them or none.
**Three execution paths still run at full privilege after 0.47.0, and two of them are not documented as doing so.** `shell_start`'s backgrounded handles are stated as uncontained (`docs/CONTRACT.md:1107`) because `Shell::detached` sets `sandbox: None` (`src/tools/shell.rs:1164`) where the foreground line sets it. The six git built-ins spawn `git` directly (`src/tools/git.rs:540`) and no surface mentions it at all. An agent that cannot write outside the workspace with `exec` can start the same command with `shell_start` and write wherever it likes — which makes the boundary a matter of which tool the model happened to pick. **This release closes both**, so the sentence "a contained run's commands cannot write outside their granted roots" stops carrying an exception list.
**The mode a command needs is resolved before it runs instead of discovered when it fails.** Today one `TaskContract::exec_sandbox` mode covers every spawn. A run granting `ExecMode::ReadOnly` still lets `git_commit` be dispatched, and it fails on a `.git` it cannot write — a failure the model must interpret from an errno. Each tool that spawns now declares the mode it needs, the run resolves that declaration against what the contract granted **before the spawn**, and the two answers meet in the honest direction: a call runs under the *narrower* of what it needs and what it was granted, and a need the grant cannot satisfy is refused up front with a reason naming both. Least privilege per call rather than per run, and a refusal the model can act on rather than an errno it must decode.
**And egress stops being one boolean for a whole run.** `Policy` has carried per-host `Act::Net` rules since 0.8.0; `Policy::permits_any_egress` (`src/policy.rs:616`) flattens them to a single flag because a backend takes one — a network namespace exists or it does not, an SBPL profile says `(allow network*)` or it does not. So a run permitted `api.example.com` gives its contained commands the whole internet. This release routes a contained command's egress through a loopback proxy the run owns: the sandbox permits the proxy and nothing else, the proxy answers every `CONNECT` by asking the run's own `Policy` about that host and port, and every dial — permitted or refused — is recorded. The rule that keeps it honest is 0.47.0's, extended: **a run whose policy names hosts is never given a rung that cannot route to the proxy**, and where no rung on the host can, the run gets the old boolean and says so rather than implying a filter it does not have.



_Canonical sources: products/<id>/execution/active.yaml, products/<id>/releases/<version>.yaml_
