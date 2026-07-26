# Capabilities — IO Harness

> A production-grade Rust agent harness that runs AI agents from a typed task contract to a verified result.

This document lists what IO Harness does. It is the human-readable companion to
the product definition in `.ultraship/products/`. It grows with each release;
items not yet shipped are roadmap and are called out in release notes and the
[CHANGELOG](../CHANGELOG.md).

## What it is

The shared engine every initorigin app (io-cli, io-studio) and io-eval build on.

- **Type:** Rust library crate
- **Stack:** Rust · cargo · tokio · rusqlite · rmcp · own HTTP+SSE provider client

## The twelve pillars

A complete harness holds all twelve. Each row says which release delivered it, or
which release will. A pillar is *shipped* when a release contract accepted it —
not when a release merely touched it.

| Pillar | Status | Release |
| --- | --- | --- |
| **Task contract** — goal, constraints, expected output, success criteria | shipped | 0.1.0 |
| **Orchestration loop** — observe, reason, act, check, stop | shipped | 0.1.0 |
| **Verification layer** — execution-based gates confirm the task is done | shipped | 0.2.0, hardened 0.8.1 |
| **Stop conditions and budgets** — cap steps, time, tokens, retries | shipped | 0.2.0, tree-wide 0.5.0 |
| **Permissions and guardrails** — what the agent may read, write, exec, dial | shipped | 0.4.0, network 0.8.0 |
| **Human approval layer** — review before sensitive or irreversible actions | shipped | 0.4.0, durable 0.7.0 |
| **Tool layer** — narrow, typed actions the agent invokes | shipped | 0.3.0, 0.8.0, completed 0.9.0 |
| **Context construction** — feed the model only relevant, current, trusted info | shipped | 0.10.0 |
| **State and memory** — progress and decisions within a run; durable recall across runs | shipped | 0.2.0, completed 0.10.0 |
| **Recovery and retry** — retries, fallbacks, replanning, escalation | partial — provider retry and escalation only | 0.2.0, completed 0.11.0 |
| **Observability and tracing** — prompts, decisions, tool calls, cost, outcomes | partial — durable trace, no live stream | 0.2.0, completed 0.12.0 |
| **Evaluation layer** — success, reliability, safety, latency, cost across cases | pending — no replay, no outcome record | 0.12.0 |

## Beyond the pillars

- **Providers** — OpenRouter, Anthropic, OpenAI (own HTTP+SSE client) · 0.1.0, 0.3.0
- **Agent composition** — spawn and nest many agents with shared context and one budget · 0.5.0
- **Ephemeral code-exec sandboxes** — OS-native per platform over a portable floor · 0.6.0
- **Long-running autonomous tasks** — 24h+ unattended, crash-resumable · 0.7.0
- **Extensibility** — MCP client, stdio and streamable HTTP · 0.8.0; in-process `Tool` implementations and markdown skills · 0.9.0
- **Built-in tools** — filesystem, grep, find over a policy-scoped workspace · 0.1.0, 0.3.0
- **Office and document tools** — Word/Excel/PowerPoint/PDF create and edit, PDF watermark, PDF form fill, OCR, barcode/QR · planned 0.13.0
- **Media and git** — image and video passthrough where the model accepts it; real repository work · planned 0.14.0

## Status

Released through 0.10.0, live on crates.io. Ten of the twelve pillars are shipped;
0.11.0 (recovery and retry) and 0.12.0 (observability and evaluation) close the
remaining two. Ordering rationale is recorded in
`.ultraship/iterations/US-IO-HARNESS-0.9.0-I01.yaml`; the roadmap is canonical.
