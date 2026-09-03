# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Fixed

- Swift: the emitter recorded references only from call sites and member accesses, so a type used purely in type position produced zero references. `let x: WBMDAdManager`, a parameter or return type, a generic argument, and a superclass or protocol conformance were all invisible, so a file that only used a type in annotations yielded a definition-only document and `find_references` on the most-used API types returned a confident zero (travsr#830). The visitor now records a reference for every named type in a type position, recursing through optionals, arrays, dictionaries, tuples, `some`/`any` and generic arguments and skipping the language builtins, and the receiver type of a qualified static access (`Enum.constant`, `Type.method()`) is recorded alongside the member. This mirrors the Dart emitter's type-position capture (#14). The CI smoke test now asserts a type-position reference is emitted.

## [0.4.2] - 2026-08-22

Windows Phase B for the JVM and .NET wrappers, which shipped binaries in v0.4.0 but produced nothing when run, plus graph noise-node cleanup. No API or protocol changes (#22).

### Fixed

- Kotlin: `travsr-lang-kotlin` produced zero symbols and zero edges on every Windows machine. Two bugs stacked. `kls_binary()` probed the bare `kotlin-language-server` name with `Path::exists()` and `Command::new`, neither of which resolves the `.cmd` launcher `travsr lang install kotlin` writes on Windows (a bare `Command::new` auto-resolves only `.exe`), so lookup always returned `None` and `run_kls()` failed silently before spawning the server. With the binary found, `path_to_uri` still built `file://` URIs with `format!("file://{}", path.display())`, which on Windows yields `file://D:\repo\Foo.kt`: backslashes and no leading slash before the drive letter, illegal in a URI authority. KLS caught the resulting `URISyntaxException` per call rather than crashing, so the session completed cleanly with nothing in it. Binary lookup now goes through `travsr_core::exec::tool_path` (the shared PATHEXT-aware resolver), and `path_to_uri`/`uri_to_rel` strip the `\\?\` verbatim prefix and normalize symmetrically to `file:///D:/...`.
- Kotlin: `path_to_uri` percent-encodes non-unreserved bytes, so a path containing spaces produces a URI `java.net.URI` accepts instead of throwing per call. `uri_to_rel` case-folds the drive letter so a lowercase drive echoed back by KLS still matches the root, and `percent_decode` decodes into a byte buffer before `from_utf8_lossy` so multi-byte UTF-8 is not mangled and a malformed escape stays literal.
- Kotlin: the managed `~/.travsr/bin` launcher is preferred over PATH on unix as well as Windows, so a stale system-wide KLS cannot shadow the installed version, and the launcher is now checked for the execute bit (`is_runnable_file`) instead of mere existence.
- Java: Phase B on Windows produced zero edges regardless of whether Gradle was installed. The scip-java release travsr ships invokes the build tool by its extensionless name, which a Windows JVM's `ProcessBuilder` cannot run for `.cmd`/`.bat`, so `scip-java index` never drove the build. On Windows the wrapper now drives a Gradle build itself: it extracts scip-java's SemanticDB plugin jars from the launcher (through the JDK's own `jar` tool, since the coursier polyglot layout defeats the Rust `zip` reader's end-of-central-directory scan), runs the repo's `gradlew.bat` with a travsr-generated init script that applies the plugin and redirects build output out of the repo, converts the emitted `.semanticdb` files with `index-semanticdb`, and ingests the result. The unix path is unchanged.
- C#: `travsr-lang-csharp` forwarded the daemon's `\\?\`-prefixed `InvokeRequest::root` straight to scip-dotnet as the project path and `--working-directory`. scip-dotnet builds a `file://` URI from it and `System.Uri` reads the `\\?\` as a UNC authority, aborting the whole index with `UriFormatException` before `dotnet restore` ran. The verbatim prefix is now stripped at the top of `run_scip_dotnet`. `scip_dotnet_binary` and `dotnet_root` also moved to `travsr_core::exec::tool_path`, replacing hand-rolled lookups that never matched a `.exe`; `dotnet_root` requires a real `sdk/` directory and falls back to `~/.dotnet` when the `dotnet` on PATH is a runtime-only host.
- Java, C#, Scala: the wrappers read the child process's piped stdout and stderr only after it exited, so a chatty build filled the OS pipe buffer (64 KiB), the child blocked writing, and the wrapper blocked waiting for it to exit. This surfaced as a multi-minute "slow build" that the daemon watchdog then killed. All three now drain both pipes on dedicated threads while the process runs. The Java driver also drops `--debug`, which filled the buffer in about a second on its own.
- Java, Scala, C#, Kotlin: on timeout the wrappers killed only the direct child, orphaning the Gradle, sbt, dotnet/msbuild and KLS JVM grandchildren while the drain threads waited on them. All four now kill the whole process tree (`taskkill /F /T` on Windows).
- Java, Scala, C#, Kotlin: `strip_windows_verbatim_prefix` degraded a `\\?\UNC\server\share\...` path into a relative one by removing the leading backslashes. It now keeps them and returns `Cow`, avoiding an allocation on the common no-prefix path. Tests that asserted the degraded form were corrected.
- Scala: parameter descriptors (`com/demo/Greeter#greet().(name)`) and anonymous locals (`localN`) leaked into the graph as raw nodes with their own ref and call edges. The scip-reader path already filtered both; the SemanticDB reader did not, so a Scala parameter showed up as a bare `(name)` node where every other language hid it. `is_local_symbol` and `is_parameter_descriptor` join the existing stdlib check in `is_noise_symbol`, applied everywhere `build_edges` previously checked `is_stdlib_symbol`.
- scip-reader: SCIP `Parameter` descriptors (terminal `(name)`) are dropped from both the definition and the reference pass, the same treatment `local N` already got. scip-dotnet emits parameters as definitions where scip-go and scip-java do not, so they surfaced as raw `scip:...(name)` graph nodes.

### Changed

- Scala: removed debug scaffolding from the per-invoke path, a DNS and TCP probe to `repo1.maven.org` and an unbounded dump of the environment, PATH and build stdout to `~/.sbt/scala-wrapper-debug.log`.
- Removed every em-dash from the repository (134 across 28 files: Rust doc comments and inline comments, the Swift and Dart emitter sources, the objc fixtures, `npm/postinstall.js`, the CI and release workflows, and the rpath guard script), per the Travsr-com house style. Each was rewritten in place with a colon, comma, period or parentheses rather than mechanically substituted. Most are comments, but the wrapper "tool not found" messages are user-facing, so their wording changes slightly (for example, `scip-go not found - install with: ...` now reads `scip-go not found. Install with: ...`). No behavior change.
- Java: dropped the isolation-era Gradle workarounds (`--project-dir`, `--no-watch-fs`, the `GRADLE_USER_HOME` temp redirect and the jvmargs tmpdir override) and the `dbg_log` scratch logging, now that this path runs with the user's own privileges. The driver is trimmed to the minimal recipe run from the repo root.

## [0.4.1] - 2026-08-16

### Fixed

- Ruby: `travsr-lang-ruby` now passes the checkout directory (`.`) as scip-ruby's positional input. Without it scip-ruby exited 1 ("You must pass either `-e` or at least one folder or ruby file.") and wrote nothing, so every Ruby symbol carried only a `defines/binding` edge and `find_references` returned pending (travsr-lang#17).
- Objective-C: `travsr-lang-objectivec` loads libclang dynamically at runtime (clang-sys `runtime` feature) instead of link-binding it. The release binary previously baked the build host's Xcode path into `LC_RPATH`, so dyld aborted the process before `main()` on any machine without that exact path, which also stalled the whole repo's Phase B on the host side. libclang is now resolved against the active toolchain (`LIBCLANG_PATH`, then `xcrun`, then well-known dirs) and a missing libclang is a catchable error, not a crash (travsr-lang#17).
- Java: `travsr-lang-scip-reader` now decodes scip-java 0.13.1's structured `Range` sub-messages (proto fields 8 to 11), not just the classic packed int32 arrays at fields 1 and 7. Those ranges previously came back empty, every occurrence fell back to line 1, and the range guard dropped it, so Java Phase B produced definitions but zero call edges. The decoder handles both oneof arms (single-line and multi-line) and fills proto3's zero-elided fields instead of truncating on them. Ruby, Go, Python and TypeScript use the classic packed encoding and were unaffected (#724).

### Added

- A release check (`.github/scripts/check-no-buildhost-rpaths.sh`) that fails the build if the objc emitter regresses to a link-time libclang dependency or a build-host absolute rpath (travsr-lang#17).

## [0.4.0] - 2026-08-15

### Added

- Windows (`x86_64-pc-windows-msvc`) release builds for every wrapper binary, published as `travsr-lang-<lang>-x86_64-pc-windows-msvc.exe` alongside a matching `.exe.sha256` (travsr#588). No previous tag had shipped a Windows asset. `travsr-lang-objectivec` stays macOS-only, since it links libclang and shells out to `xcrun`.

  This release is what makes the assets exist. The CLI half landed in Travsr-com/travsr#704, which already builds the `.exe` name correctly but does not yet claim Windows as available, because no published tag carried the assets when it merged. Adding `x86_64-pc-windows-msvc` to its `WRAPPER_RELEASE_TARGETS` is the remaining step, and this tag is its prerequisite.
- `npm/postinstall.js` resolves `win32`/`x64` to `x86_64-pc-windows-msvc` and handles the `.exe` suffix on both the downloaded asset and the file it writes. Without it `npm i @travsr-plugin/<lang>` on Windows declined to fetch a binary the release now contains.
- A Windows CI job that builds all twelve wrappers, runs the test suite, and rehearses the release packaging, so a Windows-only compile break or an asset-naming mistake surfaces on the PR rather than midway through publishing a tag.
- Dart: references from type positions and typedefs. The emitter previously recorded references only from method invocations, instance creations and prefixed identifiers, so a type used as a parameter, a generic argument, or in an `extends`/`implements`/`with`/`on` clause produced none.
- Objective-C: C functions now carry the `.()` function suffix so they unify with Phase A `fn:<name>` nodes instead of surviving as duplicate term nodes, and clang-synthesized property getters and setters are skipped (#596).

### Changed

- **Licence changed from MIT to Apache-2.0.** The repository previously declared MIT in its manifests but shipped no `LICENSE` file at all; this release adds the licence text and standardises every crate and npm package on Apache-2.0.
- Swift and Objective-C emit `RefCall` edges for bare member accesses, not just calls, and Swift constructor calls target the type symbol directly rather than a synthetic `.init` member, so `find_references` resolves by class name (#449).
- Kotlin, Swift and Scala no longer emit a redundant structural `RefCall` edge alongside the `ScipRef` for the same reference. The daemon wrote both, producing a spurious file-to-callee edge next to the correctly re-homed one.
- The wrapper binary list and the release packaging rules moved into `.github/wrapper-bins.txt` and `.github/scripts/package-wrappers.sh`, shared by the release workflow and the Windows CI job. They were previously three hand-maintained copies of the same list, where a name added to one and missed in another would silently ship a release the installer expects and cannot find.

### Fixed

- `ScipRef.is_call` is now set at all four construction sites, matching the field travsr-core made required in #650. Without it `cargo check --workspace` failed to build.

## [0.3.0] - 2026-07-12

### Added

- Occurrence emission (`ScipRef` records) across the Scala, Kotlin, Swift, and Dart Phase B plugins and the shared scip-reader. This powers all-language `find_references` and `find_pattern` in the travsr CLI (#299).

### Fixed

- Objective-C plugin: bake the libclang RPATH into the binary via `build.rs` so it resolves libclang at runtime instead of relying on a stale or absent library path.

## [0.2.1] - 2026-06-11

### Fixed

- Go plugin: changed `scip-go` invocation from `scip-go --output <f> <root>` to `scip-go index --output <f> ./...` (cwd=root). The old form loaded only the root package, producing zero semantic edges on multi-package repos. `./...` is the standard Go recursive package pattern.
- Java plugin: removed spurious positional `<root>` argument from `scip-java index --output <f>`. scip-java uses cwd for project discovery; the extra argument caused an error on unknown trailing argument.

## [0.2.0] - 2026-06-10

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

## [0.1.0] - 2026-05-31

### Added

**Language crates**: SCIP-based semantic analysis binaries for ten languages:

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

**SCIP binary ingestion**: `travsr-lang-scip-reader` crate reads `.scip` protobuf output and threads the symbol corpus into the Travsr indexing pipeline.

**npm distribution**: `@travsr-plugin/<lang>` packages for all ten languages. Each package downloads the correct pre-built binary for the host platform/arch on `npm install` via a shared `postinstall.js` script.

**GitHub Actions release workflow**: push a `v*.*.*` tag to:
1. Create a GitHub Release with auto-generated notes.
2. Build and upload binaries for `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, and `aarch64-unknown-linux-gnu`.
3. Publish all `@travsr-plugin/*` packages to npm.

### Changed

- Migrated from local path dependencies to published `travsr-plugin-sdk` and `travsr-core` crates.
- Removed redundant `travsr-lang-rust`, `travsr-lang-typescript`, and `travsr-lang-lsif` crates (replaced by SCIP-native pipeline).
- `travsr-lang-php`: pass `--output` flag explicitly to `scip-php`.

[0.4.2]: https://github.com/Travsr-com/travsr-lang/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/Travsr-com/travsr-lang/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/Travsr-com/travsr-lang/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/Travsr-com/travsr-lang/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/Travsr-com/travsr-lang/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Travsr-com/travsr-lang/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Travsr-com/travsr-lang/releases/tag/v0.1.0
