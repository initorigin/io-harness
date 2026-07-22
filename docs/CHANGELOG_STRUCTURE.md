# Changelog Structure

Every [initorigin](https://github.com/initorigin) repo uses this exact structure so
release notes are consistent everywhere. It is
[Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/) with a few
project rules.

## Rules

1. The newest version is at the top. `## [Unreleased]` always sits above the
   released versions.
2. Every released version is a heading: `## [X.Y.Z] - YYYY-MM-DD`.
3. Group entries under these categories, in this order, omitting any that are
   empty in a released version:
   - **Added** — new capabilities
   - **Changed** — changes to existing behavior
   - **Deprecated** — soon-to-be-removed features
   - **Removed** — removed features
   - **Fixed** — bug fixes
   - **Security** — vulnerabilities addressed
4. Entries are user-facing, past-tense-neutral, one line each, and complete
   enough to read as release notes on their own.
5. Reference issues or PRs in parentheses where useful, e.g. `(#12)`.

## Example

```
## [Unreleased]

### Added

- OpenAI provider alongside OpenRouter.

## [0.1.0] - 2026-01-01

### Added

- First working slice: run a single-agent task through the orchestration loop
  with the filesystem tool and the OpenRouter provider, verified end to end.
```

## Why it matters

Release notes are generated verbatim from the matching `## [X.Y.Z]` section (see
[RELEASE_PROCESS.md](RELEASE_PROCESS.md)). Consistent structure in means
consistent notes out.
