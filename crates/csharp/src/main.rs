//! Travsr Phase B — C# semantic analysis.
//!
//! Runs `scip-dotnet index <project> --output {scratch}/index.scip
//!      --working-directory {root}` and returns call/reference edges to the
//! Travsr daemon via the plugin protocol.
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
//! Install: `dotnet tool install --global scip-dotnet`
//! The binary lands in `~/.dotnet/tools/` which may not be on PATH — this
//! sidecar checks that location automatically.

use anyhow::Context as _;
use std::io::Read;
use std::path::{Path, PathBuf};
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
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_scip_dotnet(&req.root, req.corpus.as_str()) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("scip-dotnet failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

static SCIP_DOTNET_BIN: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// Find the scip-dotnet binary: check PATH first, then `~/.dotnet/tools/`.
/// `dotnet tool install --global` places binaries in `~/.dotnet/tools/` which is
/// often not added to PATH on non-interactive shells (CI, daemon invocations).
fn scip_dotnet_binary() -> Option<&'static PathBuf> {
    SCIP_DOTNET_BIN
        .get_or_init(|| {
            if std::process::Command::new("scip-dotnet")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return Some(PathBuf::from("scip-dotnet"));
            }
            let candidate = std::env::var_os("HOME")
                .map(PathBuf::from)?
                .join(".dotnet")
                .join("tools")
                .join("scip-dotnet");
            if candidate.exists() {
                Some(candidate)
            } else {
                None
            }
        })
        .as_ref()
}

fn scip_dotnet_available() -> bool {
    scip_dotnet_binary().is_some()
}

/// Detect the dotnet runtime root for non-standard installations.
///
/// `scip-dotnet` (a .NET global tool) needs `DOTNET_ROOT` pointing at the
/// directory that contains `host/`, `shared/`, and `sdk/`. For Homebrew this is
/// `…/Cellar/dotnet/<ver>/libexec/`. We resolve it by:
///   1. Honouring `DOTNET_ROOT` if already set.
///   2. Canonicalising the `dotnet` symlink on PATH:
///      `.../bin/dotnet` → parent = `bin/` → parent = install root → `libexec/`.
fn dotnet_root() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("DOTNET_ROOT") {
        return Some(PathBuf::from(v));
    }
    let exe = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join("dotnet"))
        .find(|p| p.is_file())?;
    let real = std::fs::canonicalize(&exe).unwrap_or(exe);
    // real = …/Cellar/dotnet/<ver>/bin/dotnet
    // parent       → …/Cellar/dotnet/<ver>/bin
    // parent again → …/Cellar/dotnet/<ver>          (install root)
    // join libexec → …/Cellar/dotnet/<ver>/libexec  (contains host/, sdk/, shared/)
    let libexec = real.parent()?.parent()?.join("libexec");
    if libexec.is_dir() {
        Some(libexec)
    } else {
        // Fallback: just use the bin dir's parent — works for some distro layouts.
        real.parent()
            .and_then(|b| b.parent())
            .map(|p| p.to_path_buf())
    }
}

/// Find the first `.sln` or `.csproj` under `root`, searching up to `depth` levels.
/// Prefers `.sln` (covers the whole solution) over `.csproj`.
/// BFS order ensures shallower files are preferred over deeper ones.
fn find_project_file(root: &Path) -> Option<PathBuf> {
    find_project_file_bfs(root, 5)
}

fn find_project_file_bfs(root: &Path, max_depth: usize) -> Option<PathBuf> {
    // Two-pass BFS: collect .sln first, then .csproj, across all depths.
    let mut sln: Option<PathBuf> = None;
    let mut csproj: Option<PathBuf> = None;
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> = std::collections::VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));

    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let ext = path.extension().and_then(|e| e.to_str());
                if ext == Some("sln") && sln.is_none() {
                    sln = Some(path);
                } else if ext == Some("csproj") && csproj.is_none() {
                    csproj = Some(path);
                }
            } else if path.is_dir() && depth < max_depth {
                queue.push_back((path, depth + 1));
            }
        }
        // Early-exit once we have a solution file — no need to search deeper.
        if sln.is_some() {
            break;
        }
    }
    sln.or(csproj)
}

fn run_scip_dotnet(root: &Path, corpus: &str) -> anyhow::Result<InvokeResponse> {
    let bin = scip_dotnet_binary().ok_or_else(|| {
        anyhow::anyhow!(
            "scip-dotnet not found — install with: dotnet tool install --global scip-dotnet"
        )
    })?;

    let project = find_project_file(root)
        .ok_or_else(|| anyhow::anyhow!("no .sln or .csproj found under {}", root.display()))?;
    tracing::info!(project = %project.display(), "scip-dotnet: indexing project");

    let scratch = tempfile::tempdir().context("failed to create temp dir")?;
    let output_path = scratch.path().join("index.scip");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    let mut cmd = std::process::Command::new(bin);
    cmd.arg("index")
        .arg(&project)
        .arg("--output")
        .arg(&output_path)
        .arg("--working-directory")
        .arg(root)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if let Some(dr) = dotnet_root() {
        tracing::debug!(dotnet_root = %dr.display(), "scip-dotnet: injecting DOTNET_ROOT");
        cmd.env("DOTNET_ROOT", &dr);
    }

    let mut child = cmd.spawn().context("failed to spawn scip-dotnet")?;

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

    travsr_lang_scip_reader::ingest(&output_path, corpus, Language::CSharp)
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_csharp=info".parse().unwrap()),
        )
        .init();

    run_plugin(CsharpPhaseB);
}
