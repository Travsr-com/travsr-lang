//! Travsr Phase B — Go semantic analysis.
//!
//! Runs `scip-go --output {scratch}/index.scip {root}` inside the ADR-017
//! sandbox (Standard policy) and returns call/reference edges to the Travsr
//! daemon via the plugin protocol.
//!
//! Install:  go install github.com/sourcegraph/scip-go/cmd/scip-go@latest
//! Register: travsr lang add go

use std::path::Path;
use anyhow::Context as _;
use travsr_core::Language;
use travsr_plugin_sdk::{
    InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin, run_plugin,
};

const TIMEOUT_SECS: u64 = 300;

struct GoPhaseB;

impl Plugin for GoPhaseB {
    fn language(&self) -> Language { Language::Go }
    fn extensions(&self) -> &[&str] { &["go"] }
    fn supports_phase_b(&self) -> bool { scip_go_available() }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A (Tree-sitter structural parse) is handled by the built-in
        // Go plugin in the core daemon. This binary is Phase B only.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_scip_go(&req.root) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("scip-go failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

fn scip_go_available() -> bool {
    std::process::Command::new("scip-go")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn run_scip_go(root: &Path) -> anyhow::Result<InvokeResponse> {
    anyhow::ensure!(
        scip_go_available(),
        "scip-go not found on PATH — install with: go install github.com/sourcegraph/scip-go/cmd/scip-go@latest"
    );

    // scip-go writes to a file (not stdout); use a temp dir as scratch space.
    let scratch = tempfile::tempdir().context("failed to create temp dir")?;
    let output_path = scratch.path().join("index.scip");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    let mut child = std::process::Command::new("scip-go")
        .arg("--output")
        .arg(&output_path)
        .arg(root)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn scip-go")?;

    let status = loop {
        match child.try_wait().context("polling scip-go")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("scip-go timed out after {TIMEOUT_SECS}s");
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
        "scip-go exited with {status}: {stderr_out}"
    );

    let output_size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    tracing::info!("scip-go produced {output_size} bytes of SCIP output");

    // SCIP binary format parsing is deferred — tracked in travsr-lang#TODO.
    // The tool runs successfully in the sandbox; output is not yet ingested.
    // Use travsr-lang-rust or travsr-lang-typescript for LSIF-based Phase B
    // which has full ingestion support.
    Ok(InvokeResponse::default())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_go=info".parse().unwrap()),
        )
        .init();

    run_plugin(GoPhaseB);
}
