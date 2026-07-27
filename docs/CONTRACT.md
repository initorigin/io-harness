# Public Contract — IO Harness

The public contract is the surface other code and users depend on. Changes to it
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) and are
recorded in the [CHANGELOG](../CHANGELOG.md).

## Surface

A versioned Rust crate API following SemVer. No network API in v0.1; the daemon/IPC contract is deferred.

Since 0.14.0 part of that surface sits behind cargo features: `documents`, and
the per-format `xlsx`, `docx`, `pptx`, `pdf` and `barcode` it turns on together.
`default = []`, so the default build is the surface described above and enabling
a feature only adds to it — the `tools::documents` modules, the twelve document
tool-name constants, and `barcode::Decoded`. `Workspace::read_bytes`,
`Workspace::write_bytes` and `Verification::DocumentContains` are present in
every build; without the features, `DocumentContains` returns a typed error
naming the missing feature rather than the variant disappearing.

## Compatibility

- **MAJOR** — breaking change to the surface above.
- **MINOR** — backward-compatible additions.
- **PATCH** — backward-compatible fixes.

Pre-1.0.0, minor versions may break; breaking changes are always called out in
the CHANGELOG under the relevant version.

## Stability

Pre-release. The contract is not yet stable and may change between 0.x versions.
Each change is documented in the CHANGELOG and the release notes.
