# Changelog

All notable changes to this project will be documented in this file.

## [0.1.0] — 2026-05-31

### Added

**Language crates** — SCIP-based semantic analysis binaries for ten languages:

| Binary | Language |
|---|---|
| `travsr-lang-c` | C |
| `travsr-lang-cpp` | C++ |
| `travsr-lang-csharp` | C# |
| `travsr-lang-go` | Go |
| `travsr-lang-java` | Java |
| `travsr-lang-kotlin` | Kotlin |
| `travsr-lang-php` | PHP |
| `travsr-lang-python` | Python |
| `travsr-lang-ruby` | Ruby |
| `travsr-lang-scala` | Scala |

**SCIP binary ingestion** — `travsr-lang-scip-reader` crate reads `.scip` protobuf output and threads the symbol corpus into the Travsr indexing pipeline.

**npm distribution** — `@travsr-plugin/<lang>` packages for all ten languages. Each package downloads the correct pre-built binary for the host platform/arch on `npm install` via a shared `postinstall.js` script.

**GitHub Actions release workflow** — push a `v*.*.*` tag to:
1. Create a GitHub Release with auto-generated notes.
2. Build and upload binaries for `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, and `aarch64-unknown-linux-gnu`.
3. Publish all `@travsr-plugin/*` packages to npm.

### Changed

- Migrated from local path dependencies to published `travsr-plugin-sdk` and `travsr-core` crates.
- Removed redundant `travsr-lang-rust`, `travsr-lang-typescript`, and `travsr-lang-lsif` crates (replaced by SCIP-native pipeline).
- `travsr-lang-php`: pass `--output` flag explicitly to `scip-php`.

[0.1.0]: https://github.com/Travsr-com/travsr-lang/releases/tag/v0.1.0
