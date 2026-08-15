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

/// Point clang-sys's runtime loader at the active toolchain.
///
/// clang-sys's `load_manually` honors `LIBCLANG_PATH`; we set it from the
/// resolved toolchain dir when the user has not set it explicitly. Called once
/// from `main()` before any thread is spawned, so this process-global env
/// mutation is race-free (travsr-lang#17, review finding 3 — `set_var` races
/// with concurrent `getenv` and is `unsafe` from edition 2024 onward).
#[cfg(target_os = "macos")]
fn init_libclang_env() {
    if std::env::var_os("LIBCLANG_PATH").is_none() {
        if let Some(dir) = active_libclang_dir() {
            std::env::set_var("LIBCLANG_PATH", dir);
        }
    }
}

/// The process-wide `libclang` handle, loaded once via clang-sys's runtime loader.
///
/// travsr-lang#17: the binary no longer link-binds libclang, so it must be
/// loaded at runtime before the first FFI call. clang-sys keeps its handle in
/// *thread-local* storage (`clang_sys::load` populates only the calling
/// thread), but the FFI calls in `visitor::build_index` run on a *spawned*
/// thread. So we load the thread-independent `SharedLibrary` once here and hand
/// the `Arc` to whichever thread performs FFI via `clang_sys::set_library`
/// (review finding 1). A missing libclang returns a clean `Err` the caller
/// reports as a warning, instead of a dyld crash that truncated the plugin pipe
/// and stalled the whole repo's Phase B.
#[cfg(target_os = "macos")]
fn shared_libclang() -> anyhow::Result<std::sync::Arc<clang_sys::SharedLibrary>> {
    use std::sync::{Arc, OnceLock};
    static LIB: OnceLock<Result<Arc<clang_sys::SharedLibrary>, String>> = OnceLock::new();
    LIB.get_or_init(|| {
        clang_sys::load_manually()
            .map(Arc::new)
            .map_err(|e| e.to_string())
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
    shared_libclang().is_ok()
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

    // travsr-lang#17: dlopen libclang before any FFI. clang-sys stores the
    // handle in thread-local storage, so it must be installed on the visitor
    // thread itself (review finding 1) — loading it here would leave the spawned
    // thread's TLS empty and panic on its first `clang_*` call.
    let lib = shared_libclang()?;

    // Build the SCIP index via the libclang visitor. The visitor is synchronous;
    // the timeout guards against pathological translation units.
    let index = std::thread::scope(|s| {
        let handle = s.spawn(move || {
            clang_sys::set_library(Some(lib));
            visitor::build_index(root, corpus, files)
        });
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

    // Resolve the toolchain's libclang dir into LIBCLANG_PATH before any thread
    // is spawned (travsr-lang#17, review finding 3).
    #[cfg(target_os = "macos")]
    init_libclang_env();

    run_plugin(ObjcPhaseB);
}

// ── Tests ───────────────────────────────────────────────────────────────────
//
// travsr-lang#17, review finding 2: every line of the libclang load path is
// behind `#[cfg(target_os = "macos")]`, so it is only ever compiled — let alone
// run — on macOS. This test drives the full `run_emitter` path (including the
// spawned visitor thread that performs the FFI) over a real `.m` fixture and
// asserts a nonzero symbol count. It is the guard that would have caught the
// original cross-thread bug, where libclang was loaded on the calling thread
// but the visitor thread's TLS was empty, yielding a silent zero-symbol no-op.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn run_emitter_produces_symbols_over_real_source() {
        // libclang is required to exercise this path. If the toolchain has no
        // libclang (e.g. no Xcode Command Line Tools), there is nothing to
        // guard, so skip rather than fail. CI's objc-macos job has Xcode, so
        // the assertion runs there and permanently guards the load path.
        init_libclang_env();
        if !libclang_available() {
            eprintln!("skipping: libclang not available on this host");
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        // Self-contained ObjC: a root class with a method that message-sends to
        // another. No system imports, so parsing does not depend on the SDK
        // headers resolving, only on libclang loading and running.
        std::fs::write(
            root.join("greeter.m"),
            "@interface Greeter\n\
             - (void)greet;\n\
             - (void)run;\n\
             @end\n\
             @implementation Greeter\n\
             - (void)greet {}\n\
             - (void)run { [self greet]; }\n\
             @end\n",
        )
        .expect("write fixture");

        let scratch = root.join("scratch");
        std::fs::create_dir_all(&scratch).expect("scratch dir");

        let resp = run_emitter(root, "test/objc", &scratch, None).expect("emitter runs");

        // The core guard: the visitor thread actually called into libclang and
        // produced symbols. Under the cross-thread bug this would be empty.
        assert!(
            !resp.nodes.is_empty(),
            "objc emitter produced zero symbols over a real .m file — the \
             libclang load path is broken (likely loaded on the wrong thread)"
        );
    }
}
