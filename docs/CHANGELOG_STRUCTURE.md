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
   - **Breaking changes** — see rule 6; first, because it is what a reader
     upgrading across the version needs before anything else
   - **Added** — new capabilities
   - **Changed** — changes to existing behavior
   - **Deprecated** — soon-to-be-removed features
   - **Removed** — removed features
   - **Fixed** — bug fixes
   - **Security** — vulnerabilities addressed
4. Entries are user-facing, past-tense-neutral, one line each, and complete
   enough to read as release notes on their own.
5. Reference issues or PRs in parentheses where useful, e.g. `(#12)`.
6. **A break is marked, and a marked break carries a migration note.** This crate
   is pre-1.0 and stays pre-1.0, so a break ships as a minor bump — which SemVer
   permits and which Cargo already treats as incompatible. What a consumer relies
   on is therefore not that a minor never breaks, but that when it breaks the
   changelog says so and says what to write instead.
   - The marker is the literal token `**BREAKING`, at the start of the entry. Use
     the bare `**BREAKING**` for a source break, or qualify it —
     `**BREAKING (behaviour)**`, `**BREAKING (trace)**`, `**BREAKING (MSRV)**` —
     when nothing stops compiling but something a caller could depend on changed.
   - Every marked entry must contain the literal token `Migration:` (rendered
     `*Migration:*`) followed by **what to write instead**: the old call on one
     side and the new call on the other, in a fenced snippet where the shapes
     differ enough to need one. "The API changed" is not a migration note.
   - When there is genuinely nothing to write — an item removed with no
     replacement, or a behaviour change with no opt-out — say that in the note,
     in those words, and say what the reader should do instead. An honest "there
     is no opt-out; start a new run rather than resuming" is a migration note. A
     vague one is not.
   - A break counts as a break whether or not it was intended as one: a removed
     or renamed item, a changed signature, a changed trait method set, a new
     variant on a public enum (which breaks an exhaustive `match`), a new public
     field on a struct (which breaks a struct literal), a raised MSRV, a changed
     default, or a behaviour a caller could have been depending on.
7. `tests/changelog.rs` enforces rule 6: it parses this repo's `CHANGELOG.md`,
   finds every entry carrying the marker, and fails naming the versions of any
   that have no migration note. Its own negative control is a fixture entry that
   is marked and unmigrated, so the checker cannot pass by matching nothing. Run
   it with `cargo test --test changelog`.

## Example

````
## [Unreleased]

### Breaking changes

- **BREAKING** — `Store::record_step` is removed; `Store::record` replaces it.
  *Migration:*

  ```rust
  // before
  store.record_step(run_id, step, "wrote src/hello.rs", "ok")?;
  // after
  store.record(run_id, &StepRecord::new(step, "wrote src/hello.rs", "ok"))?;
  ```

### Added

- OpenAI provider alongside OpenRouter.

## [0.1.0] - 2026-01-01

### Added

- First working slice: run a single-agent task through the orchestration loop
  with the filesystem tool and the OpenRouter provider, verified end to end.
````

## Why it matters

Release notes are generated verbatim from the matching `## [X.Y.Z]` section (see
[RELEASE_PROCESS.md](RELEASE_PROCESS.md)). Consistent structure in means
consistent notes out.
