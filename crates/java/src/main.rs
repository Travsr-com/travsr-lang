//! Travsr Phase B — Java semantic analysis.
//!
//! Runs `scip-java index --output {scratch}/index.scip {root}` and returns
//! call/reference edges to the Travsr daemon via the plugin protocol.
//!
//! ## Sandbox class: RequiresElevated (ADR-017 Rule 1)
//!
//! scip-java drives Maven/Gradle, which resolve dependencies from the network
//! at analysis time. It therefore runs under `SandboxPolicy::Elevated` and the
//! Travsr daemon refuses to spawn it until a Principal Security Engineer has
//! recorded an approval with an explicit host allowlist:
//!
//! ```text
//! travsr lang approve java \
//!   --approved-by <pse-handle> \
//!   --reason "Maven/Gradle dependency resolution" \
//!   --permitted-hosts repo1.maven.org,repo.maven.apache.org,plugins.gradle.org
//! travsr lang add java
//! ```
//!
//! Install: download scip-java from https://github.com/sourcegraph/scip-java/releases

use anyhow::Context as _;
use std::path::Path;
use travsr_core::Language;
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

/// JVM builds (Gradle/Maven) can be slow on a cold dependency cache.
const TIMEOUT_SECS: u64 = 600;

struct JavaPhaseB;

impl Plugin for JavaPhaseB {
    fn language(&self) -> Language {
        Language::Java
    }
    fn extensions(&self) -> &[&str] {
        &["java"]
    }
    fn supports_phase_b(&self) -> bool {
        scip_java_available()
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A (Tree-sitter structural parse) is handled by the built-in
        // Java plugin in the core daemon. This binary is Phase B only.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_scip_java(&req.root, req.corpus.as_str()) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("scip-java failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

static SCIP_JAVA_AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn scip_java_available() -> bool {
    *SCIP_JAVA_AVAILABLE.get_or_init(|| {
        std::process::Command::new("scip-java")
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    })
}

fn run_scip_java(root: &Path, corpus: &str) -> anyhow::Result<InvokeResponse> {
    anyhow::ensure!(
        scip_java_available(),
        "scip-java not found on PATH — download from https://github.com/sourcegraph/scip-java/releases"
    );

    // scip-java writes a SCIP index file (not stdout); use a temp dir as scratch.
    let scratch = tempfile::tempdir().context("failed to create temp dir")?;
    let output_path = scratch.path().join("index.scip");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    let mut child = std::process::Command::new("scip-java")
        .arg("index")
        .arg("--output")
        .arg(&output_path)
        .arg(root)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn scip-java")?;

    let status = loop {
        match child.try_wait().context("polling scip-java")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("scip-java timed out after {TIMEOUT_SECS}s");
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
        "scip-java exited with {status}: {stderr_out}"
    );

    let output_size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    tracing::info!("scip-java produced {output_size} bytes of SCIP output");

    travsr_lang_scip_reader::ingest(&output_path, corpus, Language::Java)
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_java=info".parse().unwrap()),
        )
        .init();

    run_plugin(JavaPhaseB);
}
