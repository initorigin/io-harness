<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Master roadmap

## io-harness

| Version | Outcome | Detail | Status |
| --- | --- | --- | --- |
| 0.1.0 | A developer embeds the crate, hands it one file-edit task contract, and the harness runs the loop (observe, reason, act, verify, stop) with the filesystem tool and the OpenRouter provider; the verification layer confirms the file meets the spec and the run stops on success or a step cap. | specified | released |
| 0.2.0 | A developer runs a longer, multi-step task and can trust it: step, time, and cost budgets are enforced, failed steps are retried or recovered, and a full trace plus rusqlite state make the run auditable and resumable. Verification is execution-based — the gate compiles and/or runs a test against the produced artifact, so a task cannot pass with a substring stub (0.1.0 live run showed FileContains is trivially gamed; see iterations/US-IO-HARNESS-0.2.0-I01). | specified | released |
| 0.3.0 | A developer runs a task that greps and finds across a repository and edits several files, choosing OpenRouter, Anthropic, or OpenAI as the provider. | outline | planned |
| 0.4.0 | A developer runs an agent on sensitive work with permission boundaries on what it may read, write, and spend, and a human-approval gate that pauses before any sensitive or irreversible action. | hypothesis | planned |
| 0.5.0 | A developer launches a parent agent that spawns and nests sub-agents with shared context, runs generated code in an ephemeral local sandbox, and lets it work unattended for hours (24h+). | hypothesis | planned |
| 0.6.0 | A developer extends the harness through MCP (rmcp), plugins, and skills, and uses the office/document and media tool suite from an agent run. | hypothesis | planned |


_Canonical sources: products/<id>/roadmap.yaml_
