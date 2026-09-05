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
/// directory that contains `host/`, `shared/`, and `sdk/`; without it, it looks
/// only at the hardcoded default `/usr/local/share/dotnet` and fails to launch
/// on a Homebrew or user-local install.
///
/// Resolution order: (1) honour `DOTNET_ROOT` if already set; (2) canonicalise a
/// `dotnet` launcher — from PATH, or from a well-known install location when PATH
/// does not carry one — and map it to its runtime root
/// ([`dotnet_root_from_binary`]); (3) fall back to the well-known runtime roots
/// directly ([`well_known_dotnet_roots`]).
///
/// The non-PATH locations in steps 2 and 3 are the load-bearing part. The daemon
/// usually injects `DOTNET_ROOT` for the sidecar (the plugin host resolves it),
/// but a daemon launched with a minimal PATH — no Homebrew `/opt/homebrew/bin` —
/// leaves the host unable to find `dotnet`, so nothing is injected and the
/// sidecar must resolve the runtime itself or `scip-dotnet` fails to launch and
/// the C# lane produces no reference edges.
fn dotnet_root() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("DOTNET_ROOT") {
        return Some(PathBuf::from(v));
    }
    // PATH first: `tool_path` is PATHEXT-aware, so it finds `dotnet.exe` on
    // Windows (a bare `dir.join("dotnet")` never did) and also checks
    // `~/.dotnet/tools`. Map the launcher to an install root that carries a real
    // `sdk/` (see `dotnet_root_from_binary`).
    if let Some(exe) = travsr_core::exec::tool_path("dotnet") {
        if let Some(root) = dotnet_root_from_binary(&exe) {
            return Some(root);
        }
    }
    // `dotnet` is not on PATH (a daemon launched with a minimal PATH, no
    // Homebrew `/opt/homebrew/bin`) or the PATH launcher is runtime-only: try the
    // well-known launcher locations such a PATH omits, then probe the install
    // roots directly. Each is still confirmed by an `sdk/`, so a runtime-only
    // host is never mistaken for a usable root.
    for exe in well_known_dotnet_binaries() {
        if exe.is_file() {
            if let Some(root) = dotnet_root_from_binary(&exe) {
                return Some(root);
            }
        }
    }
    well_known_dotnet_roots()
        .into_iter()
        .find(|d| d.join("sdk").is_dir())
}

/// The dotnet install root a `dotnet` launcher belongs to, requiring an actual
/// `sdk/`.
///
/// scip-dotnet runs `restore`/`build`, so a runtime-only host is not enough:
/// Windows' `C:\Program Files\dotnet` carries `host/` even when SDK-less, which
/// is why the marker is `sdk/`, not `host/`. Two real layouts carry it:
///   - **Homebrew macOS:** `<root>/bin/dotnet` (a symlink into the Cellar), SDK
///     under the sibling `<root>/libexec`.
///   - **Official installer:** the launcher sits in the install root itself
///     (`<root>/dotnet`, with `<root>/sdk`).
fn dotnet_root_from_binary(exe: &Path) -> Option<PathBuf> {
    // canonicalize resolves Homebrew's symlink but adds `\\?\` on Windows; strip
    // it, or scip-dotnet gets a `\\?\`-prefixed DOTNET_ROOT it cannot parse.
    let real = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let real = PathBuf::from(strip_windows_verbatim_prefix(&real.to_string_lossy()).as_ref());
    let dir = real.parent()?;
    if dir.join("sdk").is_dir() {
        return Some(dir.to_path_buf());
    }
    // Homebrew: `…/bin/dotnet` → the SDK is in the sibling `…/libexec`.
    if let Some(libexec) = dir.parent().map(|p| p.join("libexec")) {
        if libexec.join("sdk").is_dir() {
            return Some(libexec);
        }
    }
    // A runtime-only host: fall back to the per-user dotnet-install root that
    // actually carries an SDK.
    if let Some(home) = user_home() {
        let d = home.join(".dotnet");
        if d.join("sdk").is_dir() {
            return Some(d);
        }
    }
    None
}

/// The user's home directory, on every platform this ships to.
///
/// `HOME` alone is not enough: Windows sets `USERPROFILE` and normally leaves
/// `HOME` unset, so every `~/.dotnet` candidate below silently vanished there —
/// which meant the whole off-PATH fallback resolved to nothing on Windows, not
/// merely to less. Scoped to this file's dotnet lookups rather than made a
/// general helper, matching `phase_b_dart`'s precedent in the main repo.
fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// The machine-wide dotnet install directories on Windows.
///
/// `dotnet_root_from_binary` already reasons about `C:\Program Files\dotnet`
/// carrying an SDK-less `host/`, yet no candidate list named that path, so on
/// Windows this fallback covered only a per-user install. `ProgramFiles` /
/// `ProgramFiles(x86)` are honoured first so a non-default system drive or a
/// 32-bit install still resolves; the literals are the fallback for when the
/// variables are unset. The `sdk/` requirement at both call sites is what keeps
/// a runtime-only root from being picked.
///
/// Empty off Windows, where these paths do not exist and probing them would only
/// cost a stat.
fn windows_dotnet_roots() -> Vec<PathBuf> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(var) {
            out.push(PathBuf::from(base).join("dotnet"));
        }
    }
    for literal in [r"C:\Program Files\dotnet", r"C:\Program Files (x86)\dotnet"] {
        let p = PathBuf::from(literal);
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// `dotnet` launcher locations off PATH to try when PATH carries no `dotnet`:
/// the well-known installs a sandboxed or GUI-launched PATH omits — Homebrew's
/// version-independent `opt/` symlink on Apple silicon and Intel, the official
/// installer directory (launcher in the install root), the Windows machine-wide
/// install, and a user-local `~/.dotnet`. PATH itself is already covered by
/// `tool_path`.
fn well_known_dotnet_binaries() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = [
        "/opt/homebrew/opt/dotnet/bin/dotnet",
        "/usr/local/opt/dotnet/bin/dotnet",
        "/usr/local/share/dotnet/dotnet",
        "/usr/share/dotnet/dotnet",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    out.extend(
        windows_dotnet_roots()
            .into_iter()
            .map(|r| r.join("dotnet.exe")),
    );
    if let Some(home) = user_home() {
        out.push(home.join(".dotnet").join("dotnet"));
    }
    out
}

/// Install roots to probe directly when no `dotnet` launcher is reachable at
/// all. Confirmed by `sdk/` at the call site, consistent with
/// `dotnet_root_from_binary`.
fn well_known_dotnet_roots() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = [
        "/opt/homebrew/opt/dotnet/libexec",
        "/usr/local/opt/dotnet/libexec",
        "/usr/local/share/dotnet",
        "/usr/share/dotnet",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    out.extend(windows_dotnet_roots());
    if let Some(home) = user_home() {
        out.push(home.join(".dotnet"));
    }
    out
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

    travsr_lang_scip_reader::ingest(&output_path, corpus, Language::CSharp, root)
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
    use super::*;

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

    /// Homebrew layout: `<root>/bin/dotnet` with the SDK in `<root>/libexec`.
    /// This is the case the daemon-sandbox PATH gap made unreachable. The marker
    /// is `sdk/`, not `host/`: scip-dotnet needs the SDK to restore/build.
    #[test]
    fn resolves_homebrew_layout_from_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("libexec/sdk")).unwrap();
        std::fs::write(root.join("bin/dotnet"), b"#!/bin/sh\n").unwrap();

        let got = dotnet_root_from_binary(&root.join("bin/dotnet")).unwrap();
        // `dotnet_root_from_binary` strips the Windows `\\?\` verbatim prefix
        // that `canonicalize` adds, so strip `want` the same way (a no-op on
        // unix) or the two never match on Windows.
        let canon = std::fs::canonicalize(root.join("libexec")).unwrap();
        let want = PathBuf::from(strip(canon.to_string_lossy().as_ref()).as_ref());
        assert_eq!(got, want, "must point at the libexec SDK root");
    }

    /// Official-installer layout: the `dotnet` launcher sits directly in the
    /// install root beside `sdk/`.
    #[test]
    fn resolves_official_installer_layout_from_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("sdk")).unwrap();
        std::fs::write(root.join("dotnet"), b"#!/bin/sh\n").unwrap();

        let got = dotnet_root_from_binary(&root.join("dotnet")).unwrap();
        // Strip the `\\?\` prefix from `want` to match the function (see above).
        let canon = std::fs::canonicalize(root).unwrap();
        let want = PathBuf::from(strip(canon.to_string_lossy().as_ref()).as_ref());
        assert_eq!(
            got, want,
            "launcher in the root resolves to the root itself"
        );
    }

    /// The Homebrew fallbacks the sandbox PATH omits are actually in the search.
    #[test]
    fn candidates_cover_the_sandbox_path_gap() {
        assert!(well_known_dotnet_binaries()
            .iter()
            .any(|p| p.ends_with("opt/homebrew/opt/dotnet/bin/dotnet")));
        assert!(well_known_dotnet_roots()
            .iter()
            .any(|p| p.ends_with("opt/homebrew/opt/dotnet/libexec")));
    }

    /// The candidate lists were POSIX-only apart from `~/.dotnet`, so on Windows
    /// the machine-wide install — the one `dotnet_root_from_binary`'s own
    /// docstring reasons about — was never probed.
    #[test]
    #[cfg_attr(not(windows), ignore = "windows-only install locations")]
    fn candidates_cover_the_windows_machine_wide_install() {
        assert!(
            well_known_dotnet_roots()
                .iter()
                .any(|p| p.ends_with("dotnet") && p.to_string_lossy().contains("Program Files")),
            "the machine-wide Program Files root must be probed"
        );
        assert!(
            well_known_dotnet_binaries()
                .iter()
                .any(|p| p.ends_with("dotnet.exe")),
            "and its launcher must be tried"
        );
    }

    /// Windows sets `USERPROFILE` and leaves `HOME` unset, so keying the
    /// per-user candidate on `HOME` alone dropped it entirely there.
    #[test]
    fn user_home_falls_back_to_userprofile() {
        // Both unset is the only case that may yield None; otherwise either
        // variable must resolve. Read whatever this environment provides rather
        // than mutating process env, which is not safe across parallel tests.
        let has_home = std::env::var_os("HOME").is_some();
        let has_profile = std::env::var_os("USERPROFILE").is_some();
        assert_eq!(
            user_home().is_some(),
            has_home || has_profile,
            "user_home must honour USERPROFILE as well as HOME"
        );
    }
}
