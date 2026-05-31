# travsr-lang

> Phase B language support for [Travsr](https://github.com/Travsr-com/travsr) — deep semantic analysis installable per-language.

[![CI](https://github.com/Travsr-com/travsr-lang/actions/workflows/ci.yml/badge.svg)](https://github.com/Travsr-com/travsr-lang/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

---

## Background

Travsr builds a graph of your codebase and serves it over MCP so AI agents traverse structure instead of guessing from text chunks. It has two analysis phases:

**Phase A** (built into the core `travsr` binary) — structural parsing via Tree-sitter. Fast, zero external dependencies, always-on. Gives you class, function, method, and import nodes for every supported language.

**Phase B** (this repo) — deep semantic analysis via external LSIF/SCIP tools. Adds call edges, type resolution, cross-module references, and go-to-definition data. Requires an external tool to be installed and runs in a sandboxed subprocess per ADR-017.

Phase B is opt-in per language and per repository. You install only what you need.

---

## Available Language Packages

| Package | Language(s) | External Tool | Sandbox Class |
|---|---|---|---|
| `travsr-lang-rust` | Rust `.rs` | `rust-analyzer lsif` | Standard |
| `travsr-lang-typescript` | TypeScript `.ts .tsx .mts` / JavaScript `.js .mjs` | `travsr-lsif-ts` | Standard |

**Planned** (contributions welcome — see [Adding a Language](#adding-a-language)):

| Language | Tool | Sandbox Class | Notes |
|---|---|---|---|
| Go | `scip-go` | Standard | `go install github.com/sourcegraph/scip-go/cmd/scip-go@latest` |
| Python | `scip-python` | Standard | `pip install scip-python` |
| PHP | `scip-php` | Standard | [sourcegraph/scip-php](https://github.com/sourcegraph/scip-php) |
| Ruby | `scip-ruby` | Standard | `gem install scip-ruby` (experimental) |
| C / C++ | `scip-clang` | Standard | Requires `compile_commands.json` |
| Java | `scip-java` | **RequiresElevated** | Maven/Gradle network access — PSE sign-off required |
| Kotlin | `scip-java` | **RequiresElevated** | Covered by scip-java |
| C# | `scip-dotnet` | **RequiresElevated** | NuGet restore requires network |
| Scala | `scip-scala` | **RequiresElevated** | sbt network access |

**Sandbox classes:**
- **Standard** — no network access, no dependency downloads. Can be enabled after corpus trust grant only.
- **RequiresElevated** — tool downloads dependencies at analysis time (Maven, Gradle, NuGet, sbt). Requires both corpus trust grant AND explicit PSE approval via `travsr lang approve` (ADR-017 Rule 1).

---

## Installation

### Standard languages (Rust, TypeScript, Go, PHP, Ruby, Python, C/C++)

```bash
# 1. Install the external tool
rustup component add rust-analyzer          # Rust
npm install -g travsr-lsif-ts              # TypeScript / JavaScript

# 2. Register with Travsr
travsr lang add rust
travsr lang add typescript

# 3. Grant trust for the repository you want to analyse
travsr config set plugins.trust.github.com/acme/my-repo true

# 4. Re-index to trigger Phase B
cd /path/to/my-repo
travsr init
```

### RequiresElevated languages (Java, Kotlin, C#, Scala)

These languages need their build toolchain (Maven, Gradle, NuGet, sbt) to run at analysis time, which requires network access. This is governed by `SandboxPolicy::Elevated` in ADR-017 and requires explicit Principal Security Engineer sign-off before activation.

```bash
# 1. Record PSE approval (must be done before travsr lang add)
travsr lang approve java \
  --approved-by <pse-github-handle> \
  --reason "Maven dependency resolution for semantic analysis of acme/backend"

# 2. Install scip-java
#    Download from https://github.com/sourcegraph/scip-java/releases

# 3. Register
travsr lang add java

# 4. Grant corpus trust
travsr config set plugins.trust.github.com/acme/backend true

# 5. Re-index
cd /path/to/backend
travsr init
```

### Check status of all languages

```bash
travsr lang list
```

```
LANGUAGE     COMMAND                SANDBOX    STATUS
------------------------------------------------------------------------
typescript   travsr-lsif-ts         Standard   ✓ active
rust         rust-analyzer          Standard   ✓ active
go           scip-go                Standard   on PATH, not registered (run: travsr lang add go)
python       scip-python            Standard   not installed  hint: pip install scip-python
java         scip-java              Elevated   needs approval (travsr lang approve)
kotlin       scip-java              Elevated   needs approval (travsr lang approve)
csharp       scip-dotnet            Elevated   needs approval (travsr lang approve)
scala        scip-scala             Elevated   needs approval (travsr lang approve)
ruby         scip-ruby              Standard   not installed  hint: gem install scip-ruby (experimental)
php          scip-php               Standard   not installed  hint: Download scip-php from ...
cpp          scip-clang             Standard   not installed  hint: Download scip-clang ...
c            scip-clang             Standard   not installed  hint: Download scip-clang ...
```

---

## Architecture

### How it works

Each package in this repo is a minimal Rust binary that speaks the Travsr plugin protocol (length-prefixed JSON over stdin/stdout, defined in `travsr-plugin-protocol`).

When `travsr lang add <lang>` registers a package, the Travsr daemon:

1. Records the binary in `~/.travsr/lang.toml`
2. On each `init` or commit hook run, spawns the binary as a **sandboxed subprocess** (ADR-017 `SandboxPolicy::Standard` or `Elevated`)
3. Sends a `HandshakeRequest` — the binary replies with its language, version, extensions, and Phase B capability
4. Sends `InvokeRequest { root }` for Phase B analysis — the binary runs the external tool, parses the output, and returns `InvokeResponse { nodes, edges }`
5. The daemon merges those nodes/edges into the graph

The sandbox enforces: no network (Standard), read-only repo root, write-only scratch tmpdir, scrubbed environment (`PATH`, `LANG`, `LC_ALL`, `TMPDIR` only), CPU/RAM/wall-clock caps.

### Protocol

All packages share the same wire protocol from `travsr-plugin-sdk`:

```
daemon stdin  → HandshakeRequest → InvokeRequest
daemon stdout ← HandshakeResponse ← InvokeResponse
```

Framing: 4-byte big-endian length prefix + JSON payload. Version incompatibility caught at handshake — daemon refuses a binary whose `protocol_version` it does not support.

### Dependency

Every package depends only on `travsr-plugin-sdk`:

```toml
[dependencies]
travsr-plugin-sdk = { git = "https://github.com/Travsr-com/travsr", branch = "master" }
```

`travsr-plugin-sdk` re-exports everything a package author needs: the `Plugin` trait, all protocol types (`InvokeRequest`, `InvokeResponse`, `ParseRequest`, `ParseResponse`), core types (`Language`, `Node`, `Edge`, `VName`), and the `run_plugin()` event loop.

---

## Adding a Language

Adding Phase B support for a new language is a three-step process:

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
travsr-plugin-sdk  = { workspace = true }
anyhow             = { workspace = true }
tracing            = { workspace = true }
tracing-subscriber = { workspace = true }
```

**`crates/<lang>/src/main.rs`:**

```rust
use travsr_plugin_sdk::{
    InvokeRequest, InvokeResponse, Language, ParseRequest, ParseResponse,
    Plugin, run_plugin,
};

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
        match run_tool(&req.root) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("tool failed: {e}");
                InvokeResponse::default()
            }
        }
    }
}

fn tool_available() -> bool { /* check PATH */ }

fn run_tool(root: &std::path::Path) -> anyhow::Result<InvokeResponse> {
    // 1. Run the LSIF/SCIP emitter
    // 2. Parse output into Vec<Node> + Vec<Edge>
    // 3. Return InvokeResponse { nodes, edges }
    todo!()
}

fn main() {
    tracing_subscriber::fmt().init();
    run_plugin(MyLangPhaseB);
}
```

### 2. Add to the catalog in the core repo

Open `crates/travsr-plugin-host/src/phase_b/catalog.rs` in the [travsr](https://github.com/Travsr-com/travsr) repo and add an entry:

```rust
PhaseBEntry {
    language: "<lang>",
    command: "<binary-name>",
    args: &["{root}"],
    output_format: OutputFormat::Scip,   // or Lsif
    sandbox: SandboxRequirement::Standard, // or RequiresElevated
    install_hint: "<how to install the tool>",
},
```

### 3. Open a PR

- CI runs `cargo fmt`, `cargo clippy`, `cargo check`, `cargo test`
- Required: at least one integration test that calls `invoke_phase_b` on a small fixture

---

## Repository Structure

```
travsr-lang/
├── .github/
│   └── workflows/
│       └── ci.yml            ← fmt + clippy + check + test on every PR
├── Cargo.toml                ← workspace root
├── README.md
└── crates/
    ├── lsif/                 ← shared LSIF JSON-Lines ingestion library
    ├── rust/                 ← Rust    (rust-analyzer LSIF) · Standard
    ├── typescript/           ← TS/JS   (travsr-lsif-ts LSIF) · Standard
    ├── go/                   ← Go      (scip-go)            · Standard
    ├── python/               ← Python  (scip-python)        · Standard
    ├── php/                  ← PHP     (scip-php)           · Standard
    ├── ruby/                 ← Ruby    (scip-ruby)          · Standard
    ├── java/                 ← Java    (scip-java)          · RequiresElevated
    ├── kotlin/               ← Kotlin  (scip-java)          · RequiresElevated
    ├── csharp/               ← C#      (scip-dotnet)        · RequiresElevated
    ├── scala/                ← Scala   (scip-scala)         · RequiresElevated
    ├── cpp/                  ← C++     (scip-clang)         · Standard
    └── c/                    ← C       (scip-clang)         · Standard
```

> **LSIF vs SCIP:** `rust` and `typescript` emit LSIF, fully ingested by the
> shared `lsif` crate. The `scip-*` packages run their tool under the sandbox
> today; SCIP binary-format ingestion is shared work tracked separately and
> lands once the SCIP reader is in place — until then they return an empty
> `InvokeResponse`.
>
> **Version requirement:** Scala / C / C++ declare `Language::{Scala,Cpp,C}`,
> added to `travsr-core` in `0.6.1`. These packages therefore require
> `travsr-core`/`travsr-plugin-sdk` **≥ 0.6.1** (Phase A grammars for the same
> three languages also ship in core `0.6.1`).

---

## Version Compatibility

| travsr-lang | travsr core | Protocol version |
|---|---|---|
| 0.1.x | 0.5.x | 1 |

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
