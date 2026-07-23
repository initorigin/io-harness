<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Release history

## io-harness

| Version | Released | Mode | Delivered |
| --- | --- | --- | --- |
| 0.1.0 | 2026-07-23T16:11:56Z | release-ready | A developer embeds the io-harness crate, hands it a task contract to edit one file to a spec, and the harness runs the loop (observe, reason, act, verify, stop) with the filesystem tool and the OpenRouter provider, confirms the file meets the spec with a deterministic check, persists every step to rusqlite, and stops on success or the step cap. |

### 0.1.0 known limitations

- Not published to crates.io. target_mode was published; owner chose to stop at release-ready. Publish is the remaining step (`cargo publish` with CARGO_REGISTRY_TOKEN); crate name `io-harness` confirmed available.
- The end-to-end test uses a mock provider for deterministic offline CI. A real OpenRouter model run was not executed; the live path exists only in examples/edit_file.rs and is unproven against a real model.
- Verification is deterministic substring/exact-match only; no schema or model-judged verification.
- Single file per task, single agent, single provider (OpenRouter). No budgets beyond a step cap, no retry/recovery, no permissions, no human approval — all roadmap.
- No default OpenRouter model; OPENROUTER_MODEL must be set by the caller.


_Canonical sources: products/<id>/releases/<version>.yaml_
