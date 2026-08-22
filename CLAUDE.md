# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Phase B language plugins for Travsr (Java, Go, Python, C#, and more), thin wrapper binaries
around external SCIP/LSIF emitters, distributed via npm. Rust workspace `crates/*`,
rust-version 1.75. **Requires a sibling `travsr` checkout at `../travsr`**: `[patch.crates-io]`
points `travsr-core`, `travsr-plugin-protocol`, and `travsr-plugin-sdk` at `../travsr/crates/...`.
Nothing builds without it. (The README understates the crate count and claims no patch table;
the Cargo.toml is authoritative.)

## Commands

```bash
cargo check --workspace
cargo test --workspace
cargo test --workspace --exclude travsr-lang-objc   # what Windows CI runs (objc needs libclang/xcrun)
cargo build -p travsr-lang-go                       # one plugin
cargo test -p travsr-lang-objc
cargo clippy --all-targets -- -D warnings           # exact CI flags
cargo fmt --all -- --check
```

Non-Rust emitters in `packages/`:

```bash
cd packages/swift-index-emitter && swift build -c release
cd packages/dart-scip-emitter && dart pub get && dart compile exe bin/emit.dart -o bin/travsr-dart-index-emitter
```

Tests are inline `#[cfg(test)]` modules; there is no snapshot/golden mechanism. `fixtures/`
holds real mini-projects (csproj, sbt, Xcode sources) for running a wrapper by hand.

## Architecture

A Phase B plugin is a small binary implementing `travsr_plugin_sdk::Plugin`:
`language()`, `extensions()`, `supports_phase_b()`, a no-op `parse()` (Phase A is tree-sitter in
the core daemon), and `invoke_phase_b(&InvokeRequest{root, corpus}) -> InvokeResponse{nodes, edges}`,
with `main()` calling `run_plugin(...)`. Wire protocol: 4-byte BE length prefix + JSON over
stdin/stdout, version-checked handshake. The core daemon resolves `travsr-lang-<lang>` on PATH,
records it in `~/.travsr/lang.toml`, and spawns it sandboxed per ADR-017 (java/kotlin/csharp/scala
are RequiresElevated and need `travsr lang approve`).

Crates: `scip-reader` (shared SCIP ingestion lib) plus one `travsr-lang-<x>` wrapper per language,
each shelling out to its emitter (scip-go, scip-java, scip-dotnet, scip-clang, pyright,
SemanticDB/sbt, the bundled swift/dart emitters, etc.). The objc crate's binary is named
`travsr-lang-objectivec` and is macOS-only.

Distribution: npm tarballs under `npm/<lang>/` contain **no binary**; `postinstall.js` maps
platform to target triple, downloads the GitHub release asset plus `.sha256`, verifies, and
writes `bin/travsr-lang-<lang>`. Release workflow (tag `vX.Y.Z`) builds every bin listed in
`.github/wrapper-bins.txt` for 5 targets (aarch64-linux via `cross`), packages with
`.github/scripts/package-wrappers.sh`, then publishes each npm package.

Adding a language: `crates/<lang>` with `[[bin]] name = "travsr-lang-<lang>"`, an
`npm/<lang>/package.json`, entries in `.github/wrapper-bins.txt` and the release LANGS array,
and a `PhaseBEntry` in the core repo's `crates/travsr-plugin-host/src/phase_b/catalog.rs`.

## Gotchas

- **Expected red CI**: CI clones `Travsr-com/travsr@master`; a plugin change that needs a
  not-yet-merged travsr-core/SDK API fails with unresolved-import errors until the paired
  travsr PR merges.
- Release asset names are a cross-repo contract with `wrapper_asset_name` in the core repo's
  `crates/travsr-cli/src/install.rs`; the Windows CI job rehearses packaging and rejects
  extensionless assets.
- `.gitattributes` pins `*.sh` and `.github/wrapper-bins.txt` to LF; CRLF breaks the Windows
  CI job's Git Bash steps.
- objc is deliberately absent from `wrapper-bins.txt` (built by macOS-only jobs), and
  `check-no-buildhost-rpaths.sh` fails the release if a libclang rpath leaks into the binary.
