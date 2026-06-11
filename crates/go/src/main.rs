//! Travsr Phase B — Go semantic analysis.
//!
//! Runs `scip-go index --output {scratch}/index.scip ./...` (cwd = root) inside
//! the ADR-017 sandbox (Standard policy) and returns call/reference edges to the
//! Travsr daemon via the plugin protocol.
//!
//! IMPORTANT: `./...` is required. Passing root as a positional arg causes scip-go
//! to load only `[1/1]` (root package), silently skipping all sub-packages. On a
//! large repo like Kubernetes (2255 packages) that means 0 edges.
//!
//! Install:  go install github.com/sourcegraph/scip-go/cmd/scip-go@latest
//! Register: travsr lang add go

use anyhow::Context as _;
use std::path::Path;
use travsr_core::Language;
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

const TIMEOUT_SECS: u64 = 300;

struct GoPhaseB;

impl Plugin for GoPhaseB {
    fn language(&self) -> Language {
        Language::Go
    }
    fn extensions(&self) -> &[&str] {
        &["go"]
    }
    fn supports_phase_b(&self) -> bool {
        scip_go_available()
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A (Tree-sitter structural parse) is handled by the built-in
        // Go plugin in the core daemon. This binary is Phase B only.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_scip_go(&req.root, req.corpus.as_str()) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("scip-go failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

static SCIP_GO_BIN: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

fn find_scip_go() -> Option<&'static std::path::PathBuf> {
    SCIP_GO_BIN
        .get_or_init(|| {
            // 1. Try PATH first
            if std::process::Command::new("scip-go")
                .arg("--help")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
            {
                return Some(std::path::PathBuf::from("scip-go"));
            }
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
            // 2. travsr-managed install (manual placement in ~/.travsr/bin/)
            let candidate = home.join(".travsr/bin/scip-go");
            if candidate.exists() {
                return Some(candidate);
            }
            // 3. Default GOPATH: `go install ...` lands here when GOPATH is unset
            let candidate = home.join("go/bin/scip-go");
            candidate.exists().then_some(candidate)
        })
        .as_ref()
}

fn scip_go_available() -> bool {
    find_scip_go().is_some()
}

fn run_scip_go(root: &Path, corpus: &str) -> anyhow::Result<InvokeResponse> {
    let bin = find_scip_go().ok_or_else(|| {
        anyhow::anyhow!(
            "scip-go not found — install with: go install github.com/scip-code/scip-go/cmd/scip-go@latest"
        )
    })?;

    // scip-go writes to a file (not stdout); use a temp dir as scratch space.
    let scratch = tempfile::tempdir().context("failed to create temp dir")?;
    let output_path = scratch.path().join("index.scip");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    let mut child = std::process::Command::new(bin)
        .arg("index")
        .arg("--output")
        .arg(&output_path)
        .arg("./...")
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

    travsr_lang_scip_reader::ingest(&output_path, corpus, Language::Go)
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_go=info".parse().unwrap()),
        )
        .init();

    run_plugin(GoPhaseB);
}
