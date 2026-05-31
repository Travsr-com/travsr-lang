//! Travsr Phase B — Scala semantic analysis.
//!
//! Runs `scip-scala {root} --output {scratch}/index.scip` and returns
//! call/reference edges to the Travsr daemon via the plugin protocol.
//!
//! ## Sandbox class: RequiresElevated (ADR-017 Rule 1)
//!
//! scip-scala drives sbt, which resolves dependencies from the network at
//! analysis time. It therefore runs under `SandboxPolicy::Elevated` and the
//! daemon refuses to spawn it until a Principal Security Engineer has recorded
//! an approval with an explicit host allowlist:
//!
//! ```text
//! travsr lang approve scala \
//!   --approved-by <pse-handle> \
//!   --reason "sbt dependency resolution for Scala semantic analysis" \
//!   --permitted-hosts repo1.maven.org,repo.maven.apache.org,repo.scala-sbt.org
//! travsr lang add scala
//! ```
//!
//! Install: see https://github.com/sourcegraph/scip-scala

use anyhow::Context as _;
use std::path::Path;
use travsr_core::Language;
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

/// sbt dependency resolution plus a full Scala compile can be slow when cold.
const TIMEOUT_SECS: u64 = 600;

struct ScalaPhaseB;

impl Plugin for ScalaPhaseB {
    fn language(&self) -> Language {
        Language::Scala
    }
    fn extensions(&self) -> &[&str] {
        &["scala", "sc"]
    }
    fn supports_phase_b(&self) -> bool {
        scip_scala_available()
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A (Tree-sitter structural parse) is handled by the core daemon.
        // This binary is Phase B only.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_scip_scala(&req.root, req.corpus.as_str()) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("scip-scala failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

static SCIP_SCALA_AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn scip_scala_available() -> bool {
    *SCIP_SCALA_AVAILABLE.get_or_init(|| {
        std::process::Command::new("scip-scala")
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    })
}

fn run_scip_scala(root: &Path, corpus: &str) -> anyhow::Result<InvokeResponse> {
    anyhow::ensure!(
        scip_scala_available(),
        "scip-scala not found on PATH — see https://github.com/sourcegraph/scip-scala"
    );

    let scratch = tempfile::tempdir().context("failed to create temp dir")?;
    let output_path = scratch.path().join("index.scip");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    let mut child = std::process::Command::new("scip-scala")
        .arg(root)
        .arg("--output")
        .arg(&output_path)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn scip-scala")?;

    let status = loop {
        match child.try_wait().context("polling scip-scala")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("scip-scala timed out after {TIMEOUT_SECS}s");
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
        "scip-scala exited with {status}: {stderr_out}"
    );

    let output_size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    tracing::info!("scip-scala produced {output_size} bytes of SCIP output");

    travsr_lang_scip_reader::ingest(&output_path, corpus, Language::Scala)
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_scala=info".parse().unwrap()),
        )
        .init();

    run_plugin(ScalaPhaseB);
}
