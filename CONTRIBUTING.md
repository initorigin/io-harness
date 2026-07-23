# Contributing to IO Harness

Thanks for your interest. This guide keeps contributions consistent across every
[initorigin](https://github.com/initorigin) repo.

## Ground rules

- Be respectful — see [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
- By contributing you agree your work is licensed under Apache-2.0.
- Keep changes small and focused. One concern per PR.

## Branch and PR flow

- The integration branch is **`develop`**. `main` holds released versions only.
- Branch from `develop` using **`feat/<version>`** (for example `feat/0.1.0`).
- Open a **pull request into `develop`**. Do not push directly to `develop` or
  `main`. A maintainer reviews and merges.
- When a version is ready to release, a **`develop` → `main`** PR is opened; that
  merge is the release. See [docs/RELEASE_PROCESS.md](docs/RELEASE_PROCESS.md).

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<optional scope>): <subject>
```

Types: `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `build`, `ci`,
`chore`. Keep the subject under ~50 characters and in the imperative mood. Do not
add signatures, "Co-Authored-By", or generated-by trailers.

## Changelog is required

Every user-facing change updates [CHANGELOG.md](CHANGELOG.md) under
`## [Unreleased]`, in the correct category, following
[docs/CHANGELOG_STRUCTURE.md](docs/CHANGELOG_STRUCTURE.md). Release notes are
generated from the CHANGELOG, so an entry that is unclear ships as unclear notes.

## Before you open a PR

- Code is formatted and lints clean (`cargo fmt` and `cargo clippy`).
- Tests pass where they exist.
- The CHANGELOG is updated.
- The PR description follows the template.
