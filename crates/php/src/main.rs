//! Travsr Phase B — PHP semantic analysis.
//!
//! Runs `scip-php {root}` inside the ADR-017 sandbox (Standard policy) and
//! returns call/reference edges to the Travsr daemon via the plugin protocol.
//!
//! Install:  See https://github.com/sourcegraph/scip-php
//! Register: travsr lang add php

use anyhow::Context as _;
use std::path::Path;
use travsr_core::Language;
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

const TIMEOUT_SECS: u64 = 300;

struct PhpPhaseB;

impl Plugin for PhpPhaseB {
    fn language(&self) -> Language {
        Language::Php
    }
    fn extensions(&self) -> &[&str] {
        &["php", "phtml"]
    }
    fn supports_phase_b(&self) -> bool {
        scip_php_available()
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A (Tree-sitter structural parse) is handled by the built-in
        // PHP plugin in the core daemon. This binary is Phase B only.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_scip_php(&req.root, req.corpus.as_str()) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("scip-php failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

static SCIP_PHP_AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn scip_php_available() -> bool {
    *SCIP_PHP_AVAILABLE.get_or_init(|| {
        std::process::Command::new("scip-php")
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    })
}

fn run_scip_php(root: &Path, corpus: &str) -> anyhow::Result<InvokeResponse> {
    anyhow::ensure!(
        scip_php_available(),
        "scip-php not found on PATH — see https://github.com/sourcegraph/scip-php"
    );

    let scratch = tempfile::tempdir().context("failed to create temp dir")?;
    let output_path = scratch.path().join("index.scip");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    let mut child = std::process::Command::new("scip-php")
        .arg(root)
        .arg("--output")
        .arg(&output_path)
        .current_dir(scratch.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn scip-php")?;

    let status = loop {
        match child.try_wait().context("polling scip-php")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("scip-php timed out after {TIMEOUT_SECS}s");
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
        "scip-php exited with {status}: {stderr_out}"
    );

    let output_size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    tracing::info!("scip-php produced {output_size} bytes of SCIP output");

    travsr_lang_scip_reader::ingest(&output_path, corpus, Language::Php)
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_php=info".parse().unwrap()),
        )
        .init();

    run_plugin(PhpPhaseB);
}
