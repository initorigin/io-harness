# Release Process — IO Harness

This is the same across every [initorigin](https://github.com/initorigin) repo so
releases look and work the same everywhere.

## Branches

- **`develop`** — integration branch. All feature work merges here first.
- **`feat/<version>`** — work branch for a version (for example `feat/0.1.0`),
  cut from `develop`, merged back into `develop` by PR.
- **`main`** — released versions only. Nothing lands here except a
  `develop` → `main` release PR.

## Versioning

[Semantic Versioning](https://semver.org/spec/v2.0.0.html): `MAJOR.MINOR.PATCH`.
Pre-1.0.0, minor versions may include breaking changes; they are always noted in
the CHANGELOG. The public surface governed by this is defined in
[CONTRACT.md](CONTRACT.md).

## Steps to cut a release

1. On `develop`, confirm the work for the version is complete and the
   `## [Unreleased]` section of [CHANGELOG.md](../CHANGELOG.md) is accurate.
2. Rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD` and open a fresh
   `## [Unreleased]` block above it, following
   [CHANGELOG_STRUCTURE.md](CHANGELOG_STRUCTURE.md).
3. Open a **`develop` → `main`** pull request titled `release: vX.Y.Z`.
4. After review and merge, tag `main` as `vX.Y.Z`.
5. Create the **GitHub Release** for `vX.Y.Z`. Its notes are taken **verbatim
   from the `## [X.Y.Z]` section of the CHANGELOG** — do not rewrite them.
6. Record the release in the UltraShip release record for this product.

## Why the CHANGELOG drives release notes

One source, no drift: what contributors write during development becomes the
release notes with no re-authoring. That only works if CHANGELOG entries are
user-facing and complete, so treat every entry as release copy.
