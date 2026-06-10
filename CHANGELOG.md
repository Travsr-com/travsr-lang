# Changelog

All notable changes to this project will be documented in this file.

## [0.2.0] — 2026-06-10

### Added

- Swift Phase B plugin: structural analysis via the bundled `swift-index-emitter` (SwiftSyntax). Produces call edges, type references, and inheritance edges. Available as `@travsr-plugin/swift`.
- Dart Phase B plugin: semantic analysis via the bundled `dart-scip-emitter`. Available as `@travsr-plugin/dart`. The npm package installs share files to `~/.travsr/share/` and runs `dart pub get` on first install.
- Scala Phase B plugin: semantic edges via SemanticDB and `sbt compile`. Available as `@travsr-plugin/scala`.
- C# Phase B plugin: semantic edges via `scip-dotnet`. Available as `@travsr-plugin/csharp`.
- Kotlin Phase B plugin: semantic edges via `kotlin-language-server` LSP client. Available as `@travsr-plugin/kotlin`.
- One-command install for Swift and Dart: `travsr lang install swift` and `travsr lang install dart` now resolve the emitter path automatically.

### Fixed

- PATH fallback for Phase B binary resolution: all 7 SCIP-based plugins (C, C++, Go, Java, PHP, Python, Ruby) now correctly probe the system PATH when the binary is not found at the default install location.
- Scala: `sbt` binary PATH fallback aligned with the same probe logic.
- Python plugin: improved install hint when `scip-python` is not found.
- PATH probe exit code handling: non-zero exit from a probe no longer causes a spurious error.

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

[0.2.0]: https://github.com/Travsr-com/travsr-lang/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Travsr-com/travsr-lang/releases/tag/v0.1.0
