# Public Contract — IO Harness

The public contract is the surface other code and users depend on. Changes to it
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) and are
recorded in the [CHANGELOG](../CHANGELOG.md).

## Surface

A versioned Rust crate API following SemVer. No network API in v0.1; the daemon/IPC contract is deferred.

## Compatibility

- **MAJOR** — breaking change to the surface above.
- **MINOR** — backward-compatible additions.
- **PATCH** — backward-compatible fixes.

Pre-1.0.0, minor versions may break; breaking changes are always called out in
the CHANGELOG under the relevant version.

## Stability

Pre-release. The contract is not yet stable and may change between 0.x versions.
Each change is documented in the CHANGELOG and the release notes.
