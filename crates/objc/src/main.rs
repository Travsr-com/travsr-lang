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

// ── libclang availability ─────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn libclang_available() -> bool {
    // On macOS, libclang is provided by Xcode. xcrun confirms that the active
    // Xcode toolchain is installed; if xcrun succeeds, libclang is present.
    std::process::Command::new("xcrun")
        .args(["--find", "clang"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
