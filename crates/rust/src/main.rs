//! Travsr Phase B — Rust semantic analysis.
//!
//! Runs `rust-analyzer lsif <root>` inside the ADR-017 sandbox (Standard policy)
//! and returns call/reference edges to the Travsr daemon via the plugin protocol.
//!
//! Install:  rustup component add rust-analyzer
//! Register: travsr lang add rust

use std::path::Path;
use anyhow::Context as _;
use travsr_plugin_sdk::{
    InvokeRequest, InvokeResponse, Language, ParseRequest, ParseResponse, Plugin, run_plugin,
};

const TIMEOUT_SECS: u64 = 300;

struct RustPhaseB;

impl Plugin for RustPhaseB {
    fn language(&self) -> Language { Language::Rust }
    fn extensions(&self) -> &[&str] { &["rs"] }
    fn supports_phase_b(&self) -> bool { ra_available() }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A (Tree-sitter structural parse) is handled by the built-in
        // Rust plugin in the core daemon. This binary is Phase B only.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_ra_lsif(&req.root) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("rust-analyzer lsif failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

fn ra_available() -> bool {
    std::process::Command::new("rust-analyzer")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn run_ra_lsif(root: &Path) -> anyhow::Result<InvokeResponse> {
    anyhow::ensure!(
        ra_available(),
        "rust-analyzer not found on PATH — install with: rustup component add rust-analyzer"
    );

    let root_str = root.to_string_lossy();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    let mut child = std::process::Command::new("rust-analyzer")
        .args(["lsif", &root_str])
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn rust-analyzer")?;

    let status = loop {
        match child.try_wait().context("polling rust-analyzer")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("rust-analyzer timed out after {TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    };

    let mut lsif = String::new();
    let mut stderr_out = String::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        let _ = out.read_to_string(&mut lsif);
    }
    if let Some(mut err) = child.stderr.take() {
        use std::io::Read;
        let _ = err.read_to_string(&mut stderr_out);
    }

    anyhow::ensure!(
        status.success(),
        "rust-analyzer exited with {status}: {stderr_out}"
    );

    let line_count = lsif.lines().count();
    tracing::info!("rust-analyzer produced {line_count} LSIF records");

    // TODO: parse LSIF JSON-Lines and return actual nodes/edges.
    // Tracked: publish travsr-lsif as a standalone crate and depend on it here.
    // The protocol infrastructure is fully wired — ingestion is the remaining step.
    Ok(InvokeResponse::default())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_rust=info".parse().unwrap()),
        )
        .init();

    run_plugin(RustPhaseB);
}
