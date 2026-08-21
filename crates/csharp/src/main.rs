//! Travsr Phase B: C# semantic analysis.
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
//! The binary lands in `~/.dotnet/tools/` which may not be on PATH, so this
//! sidecar checks that location automatically.

use anyhow::Context as _;
use std::borrow::Cow;
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

/// Find the scip-dotnet binary. `dotnet tool install --global` places it in
/// `~/.dotnet/tools/`, which is often not on PATH on non-interactive shells (CI,
/// daemon invocations). `travsr_core::exec::tool_path` resolves PATH (PATHEXT-
/// aware on Windows, so it matches `scip-dotnet.exe`) AND `~/.dotnet/tools`, so
/// it finds the tool whether or not that dir is on PATH. The hand-rolled
/// fallback used to check `~/.dotnet/tools/scip-dotnet` with no `.exe` and so
/// always missed on Windows.
fn scip_dotnet_binary() -> Option<&'static PathBuf> {
    SCIP_DOTNET_BIN
        .get_or_init(|| travsr_core::exec::tool_path("scip-dotnet"))
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
    // PATHEXT-aware resolve so `dotnet.exe` is found on Windows (the old
    // `dir.join("dotnet")` never matched there); also checks `~/.dotnet/tools`.
    let exe = travsr_core::exec::tool_path("dotnet")?;
    // canonicalize resolves Homebrew's symlink but adds `\\?\` on Windows, so strip
    // it, or scip-dotnet gets a `\\?\`-prefixed DOTNET_ROOT it cannot parse.
    let real = std::fs::canonicalize(&exe).unwrap_or(exe);
    let real = PathBuf::from(strip_windows_verbatim_prefix(&real.to_string_lossy()).as_ref());
    let dir = real.parent()?;
    // Require an actual `sdk/`: scip-dotnet runs restore/build, and Windows'
    // `C:\Program Files\dotnet` carries `host/` even when SDK-less.
    if dir.join("sdk").is_dir() {
        return Some(dir.to_path_buf());
    }
    // Homebrew macOS: …/Cellar/dotnet/<ver>/bin/dotnet → …/libexec holds the SDK.
    if let Some(libexec) = dir.parent().map(|p| p.join("libexec")) {
        if libexec.join("sdk").is_dir() {
            return Some(libexec);
        }
    }
    // `dotnet` on PATH is a runtime-only host (no SDK): fall back to the per-user
    // dotnet-install default that actually carries an SDK.
    if let Some(d) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".dotnet"))
    {
        if d.join("sdk").is_dir() {
            return Some(d);
        }
    }
    None
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
        // Early-exit once we have a solution file, no need to search deeper.
        if sln.is_some() {
            break;
        }
    }
    sln.or(csproj)
}

/// Strip the Windows extended-length verbatim prefix (`\\?\`, `\\?\UNC\`).
/// `InvokeRequest::root` arrives canonicalized with this prefix on Windows;
/// scip-dotnet passes `--working-directory` into `System.Uri`, which parses the
/// `\\?\` as a UNC authority and throws `UriFormatException`. No-op elsewhere.
fn strip_windows_verbatim_prefix(s: &str) -> Cow<'_, str> {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        // `\\?\UNC\server\share` denotes the UNC path `\\server\share`; keep the
        // leading `\\` rather than degrading it to a bare relative `server\share`.
        Cow::Owned(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        Cow::Borrowed(rest)
    } else {
        Cow::Borrowed(s)
    }
}

/// Terminate a spawned build process and its descendants. On Windows,
/// `Child::kill` terminates only the immediate child (the launcher), leaving the
/// `dotnet`/msbuild grandchildren running; `taskkill /T` kills the whole tree.
fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &child.id().to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = child.kill();
}

fn run_scip_dotnet(root: &Path, corpus: &str) -> anyhow::Result<InvokeResponse> {
    // On Windows, `InvokeRequest::root` arrives canonicalized with the
    // extended-length verbatim prefix (`\\?\D:\...`). scip-dotnet feeds the
    // project / `--working-directory` path into `System.Uri`, which reads the
    // `\\?\` as a UNC authority and aborts the whole index with
    // `UriFormatException: The hostname could not be parsed`. Strip it up front
    // so every path derived from `root` (the project file, the working dir) is
    // a plain drive path. No-op on unix and on already-clean paths.
    let root_buf = PathBuf::from(strip_windows_verbatim_prefix(&root.to_string_lossy()).as_ref());
    let root = root_buf.as_path();

    let bin = scip_dotnet_binary().ok_or_else(|| {
        anyhow::anyhow!(
            "scip-dotnet not found. Install with: dotnet tool install --global scip-dotnet"
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

    // Drain stdout/stderr on their own threads *while* scip-dotnet runs. It shells
    // out to `dotnet restore`/msbuild, which emit far more than a 64 KiB OS pipe
    // buffer holds; reading only after exit deadlocks (the child blocks writing to
    // a full pipe while we block waiting for it to exit). The reader threads finish
    // at EOF, when the child exits or is killed.
    let drain = |stream: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            if let Some(mut s) = stream {
                let _ = s.read_to_string(&mut buf);
            }
            buf
        })
    };
    let out_h = drain(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    );
    let err_h = drain(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    );

    let status = loop {
        match child.try_wait().context("polling scip-dotnet")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                kill_process_tree(&mut child);
                anyhow::bail!("scip-dotnet timed out after {TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    };

    // Join the drain threads so the pipes are fully consumed.
    let _ = out_h.join();
    let stderr_out = err_h.join().unwrap_or_default();

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

#[cfg(test)]
mod tests {
    use super::strip_windows_verbatim_prefix as strip;

    #[test]
    fn strips_verbatim_drive_prefix() {
        assert_eq!(
            strip(r"\\?\D:\com.travsr\repo").as_ref(),
            r"D:\com.travsr\repo"
        );
    }

    #[test]
    fn strips_verbatim_unc_prefix() {
        // `\\?\UNC\server\share` is the verbatim form of `\\server\share`; the
        // leading `\\` must survive, not degrade to a relative `server\share`.
        assert_eq!(
            strip(r"\\?\UNC\server\share\repo").as_ref(),
            r"\\server\share\repo"
        );
    }

    #[test]
    fn no_op_on_plain_paths() {
        assert_eq!(strip(r"D:\repo").as_ref(), r"D:\repo");
        assert_eq!(strip("/home/user/repo").as_ref(), "/home/user/repo");
    }
}
