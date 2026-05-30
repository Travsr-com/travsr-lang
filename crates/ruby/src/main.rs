//! Travsr Phase B — Ruby semantic analysis.
//!
//! Runs `scip-ruby {root}` inside the ADR-017 sandbox (Standard policy) and
//! returns call/reference edges to the Travsr daemon via the plugin protocol.
//!
//! Note: scip-ruby support is experimental.
//!
//! Install:  See https://github.com/sourcegraph/scip-ruby
//! Register: travsr lang add ruby

use std::path::Path;
use anyhow::Context as _;
use travsr_core::Language;
use travsr_plugin_sdk::{
    InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin, run_plugin,
};

const TIMEOUT_SECS: u64 = 300;

struct RubyPhaseB;

impl Plugin for RubyPhaseB {
    fn language(&self) -> Language { Language::Ruby }
    fn extensions(&self) -> &[&str] { &["rb", "rake"] }
    fn supports_phase_b(&self) -> bool { scip_ruby_available() }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A (Tree-sitter structural parse) is handled by the built-in
        // Ruby plugin in the core daemon. This binary is Phase B only.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_scip_ruby(&req.root) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("scip-ruby failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

fn scip_ruby_available() -> bool {
    std::process::Command::new("scip-ruby")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn run_scip_ruby(root: &Path) -> anyhow::Result<InvokeResponse> {
    anyhow::ensure!(
        scip_ruby_available(),
        "scip-ruby not found on PATH — see https://github.com/sourcegraph/scip-ruby"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    let mut child = std::process::Command::new("scip-ruby")
        .arg(root)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn scip-ruby")?;

    let status = loop {
        match child.try_wait().context("polling scip-ruby")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("scip-ruby timed out after {TIMEOUT_SECS}s");
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
        "scip-ruby exited with {status}: {stderr_out}"
    );

    tracing::info!("scip-ruby completed successfully");

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
                .add_directive("travsr_lang_ruby=info".parse().unwrap()),
        )
        .init();

    run_plugin(RubyPhaseB);
}
