//! Travsr Phase B — Objective-C semantic analysis via libclang.
//!
//! Uses libclang directly (via `clang-sys`) to parse `.m` / `.mm` / `.h` files,
//! builds a SCIP protobuf index in memory, writes it to the sandbox-granted
//! scratch dir, and ingests it via the shared `travsr-lang-scip-reader`.
//!
//! No external tool is required — the emitter is self-contained as long as
//! Xcode Command Line Tools are installed (macOS only; returns `false` from
//! `supports_phase_b` on other platforms).
//!
//! ## Sandbox class: Standard (ADR-017 Rule 1)
//!
//! Reads source files and an optional `compile_commands.json`; no network.
//!
//! Install:  `travsr lang install objectivec`
//! Register: `travsr lang add objectivec`

#[cfg(target_os = "macos")]
mod compdb;
mod diag;
#[cfg(target_os = "macos")]
mod symbol;
#[cfg(target_os = "macos")]
mod visitor;

use travsr_core::Language;
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

#[cfg(target_os = "macos")]
use anyhow::Context as _;
#[cfg(target_os = "macos")]
use std::path::Path;

#[cfg(target_os = "macos")]
const TIMEOUT_SECS: u64 = 300;

struct ObjcPhaseB;

impl Plugin for ObjcPhaseB {
    fn language(&self) -> Language {
        Language::ObjectiveC
    }

    fn extensions(&self) -> &[&str] {
        &["m", "mm"]
    }

    fn supports_phase_b(&self) -> bool {
        libclang_available()
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A (Tree-sitter structural parse) is handled by the core daemon.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        #[cfg(not(target_os = "macos"))]
        let _ = req;

        #[cfg(target_os = "macos")]
        match run_emitter(
            &req.root,
            req.corpus.as_str(),
            &req.scratch,
            req.files.as_deref(),
        ) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("objc emitter failed for {}: {e:#}", req.root.display());
                InvokeResponse::default()
            }
        }
        #[cfg(not(target_os = "macos"))]
        InvokeResponse::default()
    }
}

// ── libclang availability & runtime loading (travsr-lang#17) ───────────────────

/// Directory containing this machine's active `libclang.dylib`.
///
/// Resolved at runtime — never from the build host. Order: `LIBCLANG_PATH`,
/// then the active toolchain via `xcrun --find clang` (works for both Command
/// Line Tools and a full Xcode.app, wherever installed), then well-known
/// fallbacks.
#[cfg(target_os = "macos")]
fn active_libclang_dir() -> Option<std::path::PathBuf> {
    // 1. Explicit override — a file path or a directory.
    if let Some(p) = std::env::var_os("LIBCLANG_PATH") {
        let raw = std::path::PathBuf::from(p);
        let dir = if raw.is_file() {
            raw.parent().map(Path::to_path_buf)
        } else {
            Some(raw)
        };
        if let Some(dir) = dir {
            if dir.join("libclang.dylib").exists() {
                return Some(dir);
            }
        }
    }

    // 2. Active toolchain via xcrun: <toolchain>/usr/bin/clang → .../usr/lib.
    if let Ok(out) = std::process::Command::new("xcrun")
        .args(["--find", "clang"])
        .output()
    {
        if out.status.success() {
            if let Ok(s) = std::str::from_utf8(&out.stdout) {
                let lib = Path::new(s.trim())
                    .parent() // usr/bin
                    .and_then(Path::parent) // usr
                    .map(|usr| usr.join("lib"));
                if let Some(lib) = lib {
                    if lib.join("libclang.dylib").exists() {
                        return Some(lib);
                    }
                }
            }
        }
    }

    // 3. Well-known fallbacks (full Xcode, CLT, Homebrew LLVM).
    for candidate in [
        "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib",
        "/Library/Developer/CommandLineTools/usr/lib",
        "/opt/homebrew/opt/llvm/lib",
        "/usr/local/opt/llvm/lib",
    ] {
        let dir = std::path::PathBuf::from(candidate);
        if dir.join("libclang.dylib").exists() {
            return Some(dir);
        }
    }

    None
}

/// Load libclang once, dynamically, against this machine's toolchain.
///
/// travsr-lang#17: the binary no longer link-binds libclang, so the first FFI
/// call would panic ("a `libclang` function was called before the library was
/// loaded") unless we `clang_sys::load()` first. A missing libclang returns a
/// clean `Err` the caller reports as a warning, instead of a dyld crash that
/// truncated the plugin pipe and stalled the whole repo's Phase B.
#[cfg(target_os = "macos")]
fn ensure_libclang_loaded() -> anyhow::Result<()> {
    use std::sync::OnceLock;
    static LOADED: OnceLock<Result<(), String>> = OnceLock::new();
    LOADED
        .get_or_init(|| {
            // clang-sys's runtime loader honors LIBCLANG_PATH; point it at the
            // active toolchain when the user has not set it explicitly.
            if std::env::var_os("LIBCLANG_PATH").is_none() {
                if let Some(dir) = active_libclang_dir() {
                    std::env::set_var("LIBCLANG_PATH", dir);
                }
            }
            clang_sys::load().map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|e| {
            anyhow::anyhow!(
                "libclang could not be loaded: {e}. Install the Xcode Command Line \
                 Tools (`xcode-select --install`) or set LIBCLANG_PATH to a directory \
                 containing libclang.dylib."
            )
        })
}

#[cfg(target_os = "macos")]
fn libclang_available() -> bool {
    ensure_libclang_loaded().is_ok()
}

#[cfg(not(target_os = "macos"))]
fn libclang_available() -> bool {
    false
}

// ── Emitter invocation ────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn run_emitter(
    root: &Path,
    corpus: &str,
    scratch: &Path,
    files: Option<&[String]>,
) -> anyhow::Result<InvokeResponse> {
    let _fallback_scratch;
    let output_path = if !scratch.as_os_str().is_empty() {
        scratch.join("index.scip")
    } else {
        _fallback_scratch = tempfile::tempdir().context("failed to create temp dir")?;
        _fallback_scratch.path().join("index.scip")
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    // travsr-lang#17: dlopen libclang before any FFI. Once loaded on this thread
    // the shared handle is visible to the visitor thread spawned below.
    ensure_libclang_loaded()?;

    // Build the SCIP index via the libclang visitor. The visitor is synchronous;
    // the timeout guards against pathological translation units.
    let index = std::thread::scope(|s| {
        let handle = s.spawn(|| visitor::build_index(root, corpus, files));
        loop {
            if handle.is_finished() {
                return handle
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("visitor thread panicked")));
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("objc visitor timed out after {TIMEOUT_SECS}s");
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    })?;

    // Serialize SCIP protobuf.
    use protobuf::Message as _;
    let bytes = index
        .write_to_bytes()
        .context("serializing SCIP index to protobuf")?;

    if bytes.is_empty() {
        tracing::info!(
            "empty SCIP index — no ObjC symbols found under {}",
            root.display()
        );
        return Ok(InvokeResponse::default());
    }

    std::fs::write(&output_path, &bytes).context("writing index.scip")?;
    tracing::info!(
        bytes = bytes.len(),
        path = %output_path.display(),
        "objc-index-emitter: wrote SCIP index"
    );

    travsr_lang_scip_reader::ingest(&output_path, corpus, Language::ObjectiveC)
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_objc=info".parse().unwrap()),
        )
        .init();

    run_plugin(ObjcPhaseB);
}
