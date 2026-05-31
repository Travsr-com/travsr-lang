//! Travsr Phase B — Python semantic analysis.
//!
//! Runs `scip-python index --project-name project --project-version 0.0.1 {root}`
//! inside the ADR-017 sandbox (Standard policy) and returns call/reference edges
//! to the Travsr daemon via the plugin protocol.
//!
//! Install:  pip install scip-python
//! Register: travsr lang add python

use anyhow::Context as _;
use std::path::Path;
use travsr_core::Language;
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

const TIMEOUT_SECS: u64 = 300;

struct PythonPhaseB;

impl Plugin for PythonPhaseB {
    fn language(&self) -> Language {
        Language::Python
    }
    fn extensions(&self) -> &[&str] {
        &["py", "pyi"]
    }
    fn supports_phase_b(&self) -> bool {
        scip_python_available()
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A (Tree-sitter structural parse) is handled by the built-in
        // Python plugin in the core daemon. This binary is Phase B only.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_scip_python(&req.root) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("scip-python failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

fn scip_python_available() -> bool {
    std::process::Command::new("scip-python")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn run_scip_python(root: &Path) -> anyhow::Result<InvokeResponse> {
    anyhow::ensure!(
        scip_python_available(),
        "scip-python not found on PATH — install with: pip install scip-python"
    );

    // Use a temp dir as CWD so scip-python's default output (index.scip) lands
    // there rather than in the repo root, keeping the workspace clean.
    let scratch = tempfile::tempdir().context("failed to create temp dir")?;
    let output_path = scratch.path().join("index.scip");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    let mut child = std::process::Command::new("scip-python")
        .args([
            "index",
            "--project-name",
            "project",
            "--project-version",
            "0.0.1",
        ])
        .arg(root)
        .current_dir(scratch.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn scip-python")?;

    let status = loop {
        match child.try_wait().context("polling scip-python")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("scip-python timed out after {TIMEOUT_SECS}s");
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
        "scip-python exited with {status}: {stderr_out}"
    );

    let output_size = std::fs::metadata(&output_path).map(|m| m.len()).unwrap_or(0);
    tracing::info!("scip-python produced {output_size} bytes of SCIP output");

    travsr_lang_scip_reader::ingest(&output_path, "", Language::Python)
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_python=info".parse().unwrap()),
        )
        .init();

    run_plugin(PythonPhaseB);
}
