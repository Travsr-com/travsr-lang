//! Travsr Phase B — C# semantic analysis.
//!
//! Runs `scip-dotnet {root} --output {scratch}/index.scip` and returns
//! call/reference edges to the Travsr daemon via the plugin protocol.
//!
//! ## Sandbox class: RequiresElevated (ADR-017 Rule 1)
//!
//! scip-dotnet performs a NuGet restore, which downloads packages from the
//! network at analysis time. It therefore runs under `SandboxPolicy::Elevated`
//! and the daemon refuses to spawn it until a Principal Security Engineer has
//! recorded an approval with an explicit host allowlist:
//!
//! ```text
//! travsr lang approve csharp \
//!   --approved-by <pse-handle> \
//!   --reason "NuGet restore for C# semantic analysis" \
//!   --permitted-hosts api.nuget.org,www.nuget.org
//! travsr lang add csharp
//! ```
//!
//! Install: see https://github.com/sourcegraph/scip-dotnet

use anyhow::Context as _;
use std::path::Path;
use travsr_core::Language;
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

/// NuGet restore plus a full Roslyn pass can be slow on a cold cache.
const TIMEOUT_SECS: u64 = 600;

struct CsharpPhaseB;

impl Plugin for CsharpPhaseB {
    fn language(&self) -> Language {
        Language::CSharp
    }
    fn extensions(&self) -> &[&str] {
        &["cs"]
    }
    fn supports_phase_b(&self) -> bool {
        scip_dotnet_available()
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A (Tree-sitter structural parse) is handled by the built-in
        // C# plugin in the core daemon. This binary is Phase B only.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_scip_dotnet(&req.root) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("scip-dotnet failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

fn scip_dotnet_available() -> bool {
    std::process::Command::new("scip-dotnet")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn run_scip_dotnet(root: &Path) -> anyhow::Result<InvokeResponse> {
    anyhow::ensure!(
        scip_dotnet_available(),
        "scip-dotnet not found on PATH — see https://github.com/sourcegraph/scip-dotnet"
    );

    let scratch = tempfile::tempdir().context("failed to create temp dir")?;
    let output_path = scratch.path().join("index.scip");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    let mut child = std::process::Command::new("scip-dotnet")
        .arg(root)
        .arg("--output")
        .arg(&output_path)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn scip-dotnet")?;

    let status = loop {
        match child.try_wait().context("polling scip-dotnet")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("scip-dotnet timed out after {TIMEOUT_SECS}s");
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
        "scip-dotnet exited with {status}: {stderr_out}"
    );

    let output_size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    tracing::info!("scip-dotnet produced {output_size} bytes of SCIP output");

    travsr_lang_scip_reader::ingest(&output_path, "", Language::CSharp)
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_csharp=info".parse().unwrap()),
        )
        .init();

    run_plugin(CsharpPhaseB);
}
