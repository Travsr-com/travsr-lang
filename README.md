# travsr-lang

> Phase B language support for [Travsr](https://github.com/Travsr-com/travsr) — deep semantic analysis, installable per-language via npm.

[![CI](https://github.com/Travsr-com/travsr-lang/actions/workflows/ci.yml/badge.svg)](https://github.com/Travsr-com/travsr-lang/actions/workflows/ci.yml)
[![Release](https://github.com/Travsr-com/travsr-lang/actions/workflows/release.yml/badge.svg)](https://github.com/Travsr-com/travsr-lang/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

---

## Background

Travsr builds a graph of your codebase and serves it over MCP so AI agents traverse structure instead of guessing from text chunks. It has two analysis phases:

**Phase A** (built into the core `travsr` binary) — structural parsing via Tree-sitter. Fast, zero external dependencies, always-on. Gives you class, function, method, and import nodes for every supported language.

**Phase B** (this repo) — deep semantic analysis via external LSIF/SCIP tools. Adds call edges, type resolution, cross-module references, and go-to-definition data. Requires an external tool to be installed and runs in a sandboxed subprocess per ADR-017.

Phase B is opt-in per language and per repository. Install only what you need.

---

## Available Language Packages

All 10 external languages have working Phase B support. Install any of them via `travsr lang add <lang>` (see [Installation](#installation)).

| npm Package | Language(s) | Underlying Tool | Sandbox |
|---|---|---|---|
| `@travsr-plugin/go` | Go `.go` | `scip-go` | Standard |
| `@travsr-plugin/python` | Python `.py` | `scip-python` | Standard |
| `@travsr-plugin/ruby` | Ruby `.rb` | `scip-ruby` | Standard |
| `@travsr-plugin/php` | PHP `.php` | `scip-php` | Standard |
| `@travsr-plugin/cpp` | C++ `.cpp .cc .cxx .hpp` | `scip-clang` | Standard |
| `@travsr-plugin/c` | C `.c .h` | `scip-clang` | Standard |
| `@travsr-plugin/java` | Java `.java` | `scip-java` | **RequiresElevated** |
| `@travsr-plugin/kotlin` | Kotlin `.kt` | `scip-java` | **RequiresElevated** |
| `@travsr-plugin/csharp` | C# `.cs` | `scip-dotnet` | **RequiresElevated** |
| `@travsr-plugin/scala` | Scala `.scala` | `scip-scala` | **RequiresElevated** |

> **Built-in languages (not in this repo):** Rust and TypeScript/JavaScript Phase B is compiled into the core `travsr` binary and runs automatically — no additional install needed.

**Sandbox classes:**
- **Standard** — no network access, no dependency downloads. Enabled with a corpus trust grant.
- **RequiresElevated** — build tool (Maven, Gradle, NuGet, sbt) downloads dependencies at analysis time. Requires corpus trust grant **and** explicit PSE sign-off via `travsr lang approve` (ADR-017 Rule 1).

---

## Installation

### 1. Standard languages

```bash
# Install the language package — this installs the travsr-lang-* wrapper binary
# and warns you if the underlying tool is still missing.
travsr lang add go
travsr lang add python
travsr lang add php
travsr lang add ruby
travsr lang add cpp
travsr lang add c
```

`travsr lang add` runs `npm install -g @travsr-plugin/<lang>` automatically if the wrapper is not on PATH. After the npm install, it checks whether the underlying tool (`scip-go`, `scip-python`, etc.) is also present and prints the install command if it is not:

```
✓ @travsr-plugin/go installed.
Warning: scip-go not found on PATH.
  Install it: go install github.com/sourcegraph/scip-go/cmd/scip-go@latest
  Phase B for 'go' will be inactive until scip-go is installed.
✓ 'go' Phase B registered.
```

Install the underlying tool, then run `travsr lang list` to confirm the language reaches `✓ active`.

### 2. RequiresElevated languages (Java, Kotlin, C#, Scala)

These languages need their build toolchain to run at analysis time (network access for Maven, Gradle, NuGet, sbt). PSE approval must be recorded before `travsr lang add` will accept them.

```bash
# 1. Record PSE approval first
travsr lang approve java \
  --approved-by <pse-github-handle> \
  --reason "Maven resolution for acme/backend semantic analysis" \
  --permitted-hosts repo1.maven.org,repo.maven.apache.org,plugins.gradle.org

# 2. Add the language (installs @travsr-plugin/java, warns about scip-java)
travsr lang add java

# 3. Install the underlying tool
#    See: https://github.com/sourcegraph/scip-java/releases

# 4. Activate for a specific repository
travsr lang add java --corpus github.com/acme/backend
```

### 3. Check status of all languages

```bash
travsr lang list
```

```
LANGUAGE     PACKAGE                    SANDBOX    STATUS
--------------------------------------------------------------------------------
typescript   @travsr-plugin/typescript  Standard   ✓ active
javascript   @travsr-plugin/typescript  Standard   ✓ active
rust         rustup component add...    Standard   ✓ active
go           @travsr-plugin/go          Standard   ✓ active
python       @travsr-plugin/python      Standard   wrapper-only  (travsr-lang-python installed, scip-python missing — pip install scip-python)
java         @travsr-plugin/java        Elevated   needs PSE approval (travsr lang approve)
kotlin       @travsr-plugin/kotlin      Elevated   needs PSE approval (travsr lang approve)
scala        @travsr-plugin/scala       Elevated   needs PSE approval (travsr lang approve)
ruby         @travsr-plugin/ruby        Standard   not installed — npm install -g @travsr-plugin/ruby (experimental)
php          @travsr-plugin/php         Standard   not installed — npm install -g @travsr-plugin/php
csharp       @travsr-plugin/csharp      Elevated   needs PSE approval (travsr lang approve)
cpp          @travsr-plugin/cpp         Standard   not installed — npm install -g @travsr-plugin/cpp (requires compile_commands.json)
c            @travsr-plugin/c           Standard   not installed — npm install -g @travsr-plugin/c (requires compile_commands.json)
```

**Three status states:**

| State | Meaning |
|---|---|
| `not-installed` | Neither `travsr-lang-*` wrapper nor the underlying tool is on PATH |
| `wrapper-only` | npm package installed, but the underlying `scip-*` tool is missing — see the hint |
| `✓ active` | Both wrapper and tool on PATH, language registered, sandbox available |

---

## npm Packages

Each language is distributed as an npm package under the `@travsr-plugin` scope. When you run `npm install -g @travsr-plugin/go`, a `postinstall` script:

1. Detects your platform (`process.platform`) and architecture (`process.arch`)
2. Resolves the matching pre-built Rust target triple
3. Downloads the `travsr-lang-go-{target}` binary from the tagged GitHub Release
4. Downloads the corresponding `.sha256` sidecar file
5. Verifies the SHA256 — aborts if the hash does not match
6. Writes the binary to `<package>/bin/travsr-lang-go` with `chmod 0o755`
7. npm's `bin` field wires `travsr-lang-go` onto your PATH

**Supported platforms:**

| OS | Architecture | Target |
|---|---|---|
| macOS | Intel (x64) | `x86_64-apple-darwin` |
| macOS | Apple Silicon (arm64) | `aarch64-apple-darwin` |
| Linux | x86_64 | `x86_64-unknown-linux-gnu` |
| Linux | arm64 | `aarch64-unknown-linux-gnu` |
| Windows | any | Not supported — Phase B unavailable (exits gracefully) |

> The `@travsr-plugin/<lang>` package installs **only the `travsr-lang-*` wrapper**. The underlying indexer (`scip-go`, `scip-python`, etc.) must be installed separately. `travsr lang add` tells you exactly what is missing.

---

## Architecture

### How it works

Each crate in this repo is a minimal Rust binary that speaks the Travsr plugin protocol (length-prefixed JSON over stdin/stdout, defined in `travsr-plugin-protocol`).

When `travsr lang add <lang>` registers a package, the Travsr daemon:

1. Records the binary in `~/.travsr/lang.toml`
2. On each `init` or commit-hook run, resolves the binary via `CatalogResolver` (checks `travsr-lang-<lang>` on PATH)
3. Spawns it as a **sandboxed subprocess** (ADR-017 `SandboxPolicy::Standard` or `Elevated`)
4. Sends `InvokeRequest { root, corpus }` — the binary runs the external tool, parses SCIP output, and returns `InvokeResponse { nodes, edges }`
5. The daemon merges those nodes and edges into the graph, attributed to `corpus`

The sandbox enforces: no network (Standard), read-only repo root, write-only scratch tmpdir, scrubbed environment (`PATH`, `LANG`, `LC_ALL`, `TMPDIR` only), CPU/RAM/wall-clock caps.

### Protocol

All packages share the same wire protocol from `travsr-plugin-sdk`:

```
daemon stdin  → HandshakeRequest → InvokeRequest { root, corpus }
daemon stdout ← HandshakeResponse ← InvokeResponse { nodes, edges }
```

Framing: 4-byte big-endian length prefix + JSON payload. Version incompatibility caught at handshake — daemon refuses a binary whose `protocol_version` it does not support.

### Dependencies

Every package depends only on published crates — no `[patch.crates-io]` or local path hacks:

```toml
[dependencies]
travsr-plugin-sdk = "0.7.0"
travsr-core       = "0.7.0"
```

---

## Release Pipeline

Releases are fully automated. Push a semver tag to trigger cross-platform builds and npm publish:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The release workflow (`.github/workflows/release.yml`) runs three jobs:

1. **`create-release`** — creates the GitHub Release immediately
2. **`build` (4 parallel jobs)** — builds all 10 `travsr-lang-*` binaries for each target, strips them, computes SHA256, uploads 40 files (10 binaries + 10 `.sha256` sidecars) to the release
3. **`publish-npm`** — after all builds complete, bumps each `package.json` version from the git tag and publishes all 10 `@travsr-plugin/<lang>` packages to npm with `--access public`

---

## Adding a Language

Adding Phase B support for a new language requires changes in both this repo and the core travsr repo.

### 1. Create the crate

```bash
mkdir -p crates/<lang>/src
```

**`crates/<lang>/Cargo.toml`:**

```toml
[package]
name = "travsr-lang-<lang>"
description = "Travsr Phase B — <Language> semantic analysis via <tool>"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
rust-version.workspace = true

[[bin]]
name = "travsr-lang-<lang>"
path = "src/main.rs"

[dependencies]
travsr-plugin-sdk        = { workspace = true }
travsr-lang-scip-reader  = { workspace = true }  # if using SCIP output
anyhow                   = { workspace = true }
tracing                  = { workspace = true }
tracing-subscriber       = { workspace = true }
```

**`crates/<lang>/src/main.rs`:**

```rust
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};
use travsr_core::Language;

struct MyLangPhaseB;

impl Plugin for MyLangPhaseB {
    fn language(&self) -> Language { Language::<Variant> }
    fn extensions(&self) -> &[&str] { &["ext"] }
    fn supports_phase_b(&self) -> bool { tool_available() }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A is handled by the core daemon — this binary is Phase B only.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_tool(&req.root, req.corpus.as_str()) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("tool failed: {e}");
                InvokeResponse::default()
            }
        }
    }
}

fn main() {
    tracing_subscriber::fmt().init();
    run_plugin(MyLangPhaseB);
}
```

### 2. Add an npm package

Create `npm/<lang>/package.json` and `npm/<lang>/.gitignore`:

```json
{
  "name": "@travsr-plugin/<lang>",
  "version": "0.1.0",
  "description": "Travsr Phase B — <Language> semantic analysis (travsr-lang-<lang> binary)",
  "license": "MIT",
  "repository": { "type": "git", "url": "https://github.com/Travsr-com/travsr-lang" },
  "scripts": { "postinstall": "node postinstall.js" },
  "bin": { "travsr-lang-<lang>": "./bin/travsr-lang-<lang>" },
  "publishConfig": { "access": "public" },
  "engines": { "node": ">=16" }
}
```

```
# npm/<lang>/.gitignore
bin/
```

The shared `npm/postinstall.js` is copied into each package directory at publish time by the release workflow — no per-package postinstall.js is needed in the source tree.

### 3. Add a catalog entry in the core repo

Open `crates/travsr-plugin-host/src/phase_b/catalog.rs` in the [travsr](https://github.com/Travsr-com/travsr) repo and add a `PhaseBEntry`:

```rust
PhaseBEntry {
    language: "<lang>",
    npm_package: Some("@travsr-plugin/<lang>"),
    command: "<scip-or-lsif-tool>",
    args: &["{root}", "--output", "{output}"],
    output_format: OutputFormat::Scip,        // or Lsif
    sandbox: SandboxRequirement::Standard,    // or RequiresElevated
    install_hint: "npm install -g @travsr-plugin/<lang>",
    underlying_tool_hint: "<how to install the underlying tool>",
    provider_binary: Some("travsr-lang-<lang>"),
    elevated_hosts: &[],  // fill for RequiresElevated
},
```

Add the language binary to the build matrix in `.github/workflows/release.yml` (`BINS` arrays in `build` and `publish-npm` jobs).

### 4. Open a PR

- CI runs `cargo fmt`, `cargo clippy`, `cargo check`, `cargo test`
- Include at least one integration test calling `invoke_phase_b` on a small fixture

---

## Repository Structure

```
travsr-lang/
├── .github/
│   └── workflows/
│       ├── ci.yml         ← fmt + clippy + check + test on every PR
│       └── release.yml    ← cross-platform build + npm publish on v* tags
├── Cargo.toml             ← workspace root (travsr-plugin-sdk = "0.7.0")
├── README.md
├── npm/
│   ├── postinstall.js     ← shared download/SHA256-verify/install script
│   ├── go/                ← @travsr-plugin/go
│   ├── python/            ← @travsr-plugin/python
│   ├── java/              ← @travsr-plugin/java
│   ├── kotlin/            ← @travsr-plugin/kotlin
│   ├── scala/             ← @travsr-plugin/scala
│   ├── ruby/              ← @travsr-plugin/ruby
│   ├── php/               ← @travsr-plugin/php
│   ├── csharp/            ← @travsr-plugin/csharp
│   ├── cpp/               ← @travsr-plugin/cpp
│   └── c/                 ← @travsr-plugin/c
└── crates/
    ├── scip-reader/       ← shared SCIP binary-format ingestion library
    ├── go/                ← Go      (scip-go)      · Standard
    ├── python/            ← Python  (scip-python)  · Standard
    ├── php/               ← PHP     (scip-php)     · Standard
    ├── ruby/              ← Ruby    (scip-ruby)    · Standard
    ├── java/              ← Java    (scip-java)    · RequiresElevated
    ├── kotlin/            ← Kotlin  (scip-java)    · RequiresElevated
    ├── csharp/            ← C#      (scip-dotnet)  · RequiresElevated
    ├── scala/             ← Scala   (scip-scala)   · RequiresElevated
    ├── cpp/               ← C++     (scip-clang)   · Standard
    └── c/                 ← C       (scip-clang)   · Standard
```

> **Note:** Rust and TypeScript/JavaScript Phase B are compiled into the core `travsr` binary — those crates do not live in this repo.

---

## Version Compatibility

| travsr-lang | travsr-plugin-sdk | travsr-core | Protocol version |
|---|---|---|---|
| 0.1.x | 0.7.0 | 0.7.0 | 1 |

The plugin protocol version is checked at handshake. If the daemon and package have incompatible versions, the binary is refused at registration with a clear error — never silently mismatched.

---

## Development

```bash
# Build all packages
cargo build --workspace

# Check all packages
cargo check --workspace

# Run tests
cargo test --workspace

# Format
cargo fmt --all

# Lint
cargo clippy --all-targets -- -D warnings
```

---

## Security

Phase B packages run in an OS-level sandbox enforced by the Travsr daemon (ADR-017):

- **Network:** denied entirely (Standard) or restricted to an explicit host allowlist (Elevated, requires PSE sign-off)
- **Filesystem:** repo root is read-only; a per-invocation scratch tmpdir is the only writable path; all other paths are denied
- **Environment:** only `PATH`, `LANG`, `LC_ALL`, `TMPDIR` are passed in — no secrets, tokens, SSH keys, or cloud credentials
- **Resources:** CPU, RAM, and wall-clock limits enforced; a plugin exceeding the wall-clock cap is killed

If the sandbox mechanism is unavailable on the host (`bwrap` on Linux, `sandbox-exec` on macOS), the language is **disabled entirely** — it never runs unsandboxed as a fallback (ADR-017 Rule 2: fail-closed).

See [ADR-017](https://github.com/Travsr-com/travsr/blob/master/docs/adrs/ADR-017-unified-plugin-sandbox-trust.md) for the full security policy.

---

## License

MIT — see [LICENSE](LICENSE).

Part of the [Travsr](https://travsr.com) project.
