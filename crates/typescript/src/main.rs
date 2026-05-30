//! Travsr Phase B — TypeScript/JavaScript semantic analysis.
//!
//! Runs `travsr-lsif-ts --project <tsconfig>` inside the ADR-017 sandbox
//! (Standard policy) and returns call/reference edges to the Travsr daemon.
//!
//! Install:  npm install -g travsr-lsif-ts
//! Register: travsr lang add typescript

use std::path::Path;
use anyhow::Context as _;
use travsr_core::Language;
use travsr_plugin_sdk::{
    InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin, run_plugin,
};
use travsr_lsif;

const TIMEOUT_SECS: u64 = 60;

struct TypeScriptPhaseB;

impl Plugin for TypeScriptPhaseB {
    fn language(&self) -> Language { Language::TypeScript }
    fn extensions(&self) -> &[&str] { &["ts", "tsx", "mts", "cts", "js", "mjs"] }
    fn supports_phase_b(&self) -> bool { lsif_ts_available() }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A handled by the built-in TypeScript plugin in the core daemon.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        let tsconfig = req.root.join("tsconfig.json");
        if !tsconfig.exists() {
            tracing::info!(
                "no tsconfig.json in {} — skipping TypeScript Phase B",
                req.root.display()
            );
            return InvokeResponse::default();
        }
        match run_lsif_ts(&tsconfig) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("travsr-lsif-ts failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

fn lsif_ts_available() -> bool {
    std::process::Command::new("travsr-lsif-ts")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn run_lsif_ts(tsconfig: &Path) -> anyhow::Result<InvokeResponse> {
    anyhow::ensure!(
        lsif_ts_available(),
        "travsr-lsif-ts not found on PATH — install with: npm install -g travsr-lsif-ts"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    let mut child = std::process::Command::new("travsr-lsif-ts")
        .arg("--project")
        .arg(tsconfig)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn travsr-lsif-ts")?;

    let status = loop {
        match child.try_wait().context("polling travsr-lsif-ts")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("travsr-lsif-ts timed out after {TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(100)),
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
        "travsr-lsif-ts exited with {status}: {stderr_out}"
    );

    let line_count = lsif.lines().count();
    tracing::info!("travsr-lsif-ts produced {line_count} LSIF records");

    Ok(travsr_lsif::ingest(&lsif, "", Language::TypeScript)
        .unwrap_or_else(|e| {
            tracing::warn!("LSIF ingest error: {e}");
            InvokeResponse::default()
        }))
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_typescript=info".parse().unwrap()),
        )
        .init();

    run_plugin(TypeScriptPhaseB);
}
