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

## Capabilities

- **Task contract** — goal, constraints, expected output, success criteria
- **Context construction** — feed the model only relevant, current, trusted info
- **Tool layer** — narrow, typed actions the agent invokes
- **Orchestration loop** — observe, reason, act, check, stop
- **State and memory** — progress, intermediate results, decisions (rusqlite)
- **Verification layer** — tests, schemas, read-backs confirm the task is done
- **Permissions and guardrails** — what the agent may access, change, send, spend
- **Recovery and retry** — retries, fallbacks, replanning, escalation
- **Stop conditions and budgets** — cap steps, time, cost, retries, risky actions
- **Observability and tracing** — record prompts, decisions, tool calls, cost
- **Human approval layer** — review before sensitive or irreversible actions
- **Providers** — OpenRouter first, then Anthropic and OpenAI (own HTTP+SSE client)
- **Agent composition** — spawn and nest many agents (100+) with shared context
- **Long-running autonomous tasks** — 24h+ with no user input
- **Ephemeral local code-exec sandboxes** — write, run, capture, destroy
- **Built-in tools** — filesystem, git, grep, find
- **Office and document tools** — Word/Excel/PowerPoint/PDF create/edit/delete, PDF watermark, PDF form fill, OCR, barcode/QR read and generate
- **Media** — image and video passthrough when the model supports it
- **Extensibility** — MCP (rmcp), plugins, skills

## Status

Pre-release. Product is BRAINSTORMED; v0.1 targets a single-agent task through the orchestration loop with the filesystem tool and the OpenRouter provider, verified end to end.
