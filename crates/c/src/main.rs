//! Travsr Phase B — C semantic analysis.
//!
//! Runs `scip-clang --compdb-path {root}/compile_commands.json --output
//! {scratch}/index.scip` and returns call/reference edges to the Travsr daemon
//! via the plugin protocol. C and C++ share the scip-clang indexer; this binary
//! claims the C file extensions.
//!
//! ## Sandbox class: Standard (ADR-017 Rule 1)
//!
//! scip-clang reads the compilation database (`compile_commands.json`) and the
//! source tree; it does not download dependencies, so it runs under
//! `SandboxPolicy::Standard` (no network). It only needs a corpus trust grant:
//!
//! ```text
//! travsr lang add c
//! travsr config set plugins.trust.<corpus> true
//! ```
//!
//! Requires `compile_commands.json` at the repo root (generate with CMake
//! `-DCMAKE_EXPORT_COMPILE_COMMANDS=ON`, Bear, or `compiledb`).
//! Install: see https://github.com/sourcegraph/scip-clang

use anyhow::Context as _;
use std::path::Path;
use travsr_core::Language;
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

const TIMEOUT_SECS: u64 = 300;

struct CPhaseB;

impl Plugin for CPhaseB {
    fn language(&self) -> Language {
        Language::C
    }
    fn extensions(&self) -> &[&str] {
        &["c", "h"]
    }
    fn supports_phase_b(&self) -> bool {
        scip_clang_available()
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A (Tree-sitter structural parse) is handled by the core daemon.
        // This binary is Phase B only.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_scip_clang(&req.root, req.corpus.as_str()) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("scip-clang failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

static SCIP_CLANG_AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn scip_clang_available() -> bool {
    *SCIP_CLANG_AVAILABLE.get_or_init(|| {
        std::process::Command::new("scip-clang")
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    })
}

fn run_scip_clang(root: &Path, corpus: &str) -> anyhow::Result<InvokeResponse> {
    anyhow::ensure!(
        scip_clang_available(),
        "scip-clang not found on PATH — see https://github.com/sourcegraph/scip-clang"
    );

    let compdb = root.join("compile_commands.json");
    if !compdb.exists() {
        tracing::info!(
            "no compile_commands.json in {} — skipping C Phase B (generate with \
             CMAKE_EXPORT_COMPILE_COMMANDS=ON or Bear)",
            root.display()
        );
        return Ok(InvokeResponse::default());
    }

    let scratch = tempfile::tempdir().context("failed to create temp dir")?;
    let output_path = scratch.path().join("index.scip");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    let mut child = std::process::Command::new("scip-clang")
        .arg("--compdb-path")
        .arg(&compdb)
        .arg("--index-output-path")
        .arg(&output_path)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn scip-clang")?;

    let status = loop {
        match child.try_wait().context("polling scip-clang")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("scip-clang timed out after {TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    };

    let mut stderr_out = String::new();
    if let Some(mut err) = child.stderr.take() {
        use std::io::Read;
        let _ = err.read_to_string(&mut stderr_out);
    }

    anyhow::ensure!(
        status.success(),
        "scip-clang exited with {status}: {stderr_out}"
    );

    let output_size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    tracing::info!("scip-clang produced {output_size} bytes of SCIP output");

    travsr_lang_scip_reader::ingest(&output_path, corpus, Language::C)
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_c=info".parse().unwrap()),
        )
        .init();

    run_plugin(CPhaseB);
}
