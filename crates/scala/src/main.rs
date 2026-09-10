//! Travsr Phase B: Scala semantic analysis via SemanticDB.
//!
//! SemanticDB is built into the Scala compiler (scalac 2.13+ / Scala 3). This
//! sidecar runs `sbt compile` with SemanticDB enabled across every project in
//! the build (`set every semanticdbEnabled := true`), then parses the resulting
//! `.semanticdb` protobuf files to extract ref/call edges.
//!
//! Why not scip-scala? scip-scala is not published to any accessible registry
//! (Maven Central, Sonatype, or GitHub Releases) and cannot be installed
//! automatically. SemanticDB is a first-class feature of every modern sbt
//! project.
//!
//! ## Sandbox class: RequiresElevated (ADR-017 Rule 1)
//!
//! `sbt compile` resolves dependencies from Maven/sbt plugin repositories.
//!
//! ```text
//! travsr lang approve scala \
//!   --approved-by <pse-handle> \
//!   --reason "sbt dependency resolution for Scala semantic analysis" \
//!   --permitted-hosts repo1.maven.org,repo.maven.apache.org,repo.scala-sbt.org
//! travsr lang install scala
//! ```

use anyhow::Context as _;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use travsr_core::{Edge, EdgeKind, Node, NodeId, ScipRef, VName};
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
    PluginDiagnostic,
};

/// The plugin host watchdogs one Phase B `invoke` at `INVOKE_TIMEOUT_SECS`
/// (travsr-plugin-host `transport.rs`) and SIGKILLs past it, discarding
/// everything the sidecar collected. Every wait in this process has to fit
/// inside that window, or a reportable failure turns into a crash with no index
/// and no reason for it. Same name and same reasoning as the kotlin sidecar,
/// which already solved this, so the two agree.
const HOST_INVOKE_TIMEOUT_SECS: u64 = 300;
/// Kept back from the host's window so that giving up still leaves time to build
/// the response and write it out. A killed invoke reports nothing at all, so the
/// diagnostics and the error string go with it.
const HOST_REPLY_MARGIN_SECS: u64 = 15;
/// Kept back on top of the reply margin for the work that follows the compile:
/// `find_semanticdb_files` walks the whole sbt tree, then every file it found is
/// read and protobuf-parsed (150 of them on the pinned scala-parser-combinators
/// fixture). 60s is the reserve the old 180 + 60 split implied without naming it.
const POST_COMPILE_RESERVE_SECS: u64 = 60;
/// Ceiling on one `sbt compile`, clamped by `attempt_budget_secs` to what is
/// really left of the run's deadline. It is the whole of the compile window
/// today (300 - 15 - 60): a cold build resolving a fresh dependency tree through
/// coursier needs most of that, and the old 180 failed such builds outright
/// under a host ceiling that would have allowed them. Nothing is held back for a
/// second attempt, because a timeout ends the run: `run_sbt_compile` bails on
/// one, so the `!status.success()` retry below is only ever reached on a genuine
/// non-zero exit. The clamp, not this ceiling, is what keeps the run inside the
/// host window.
const COMPILE_TIMEOUT_SECS: u64 = 225;
/// Floor below which an attempt is skipped rather than started. `--server` means
/// every attempt spawns its own sbt JVM and reloads the build before it compiles
/// anything, so with less than this it would expire during the reload and its
/// only effect would be to spend the reserve the scan, the parse and the reply
/// need.
const MIN_ATTEMPT_SECS: u64 = 45;
// #832: enable SemanticDB across *every* project in the build, passed as an sbt
// command rather than an injected `.sbt` setting. A bare `semanticdbEnabled :=
// true` in a root settings file binds only to the root project, and even
// `ThisBuild / semanticdbEnabled := true` does not reach sbt-crossproject
// sub-projects (their per-project default shadows the ThisBuild value;
// verified: `parserCombinatorsJVM / Compile / semanticdbEnabled` stays false).
// So a multi-module build (e.g. scala-parser-combinators, where `root`
// aggregates parserCombinatorsJVM/JS/Native and carries no sources of its own)
// compiled zero `.semanticdb` files. `set every` overrides the default in all
// scopes; enabling it changes `Compile/scalacOptions`, so Zinc recompiles even
// when sources were already built without SemanticDB. sbt auto-selects a
// compatible semanticdb-scalac version per Scala version, so none is pinned.
const SEMANTICDB_ENABLE_CMD: &str = "set every semanticdbEnabled := true";

struct ScalaPhaseB;

impl Plugin for ScalaPhaseB {
    fn language(&self) -> travsr_core::Language {
        travsr_core::Language::Scala
    }
    fn extensions(&self) -> &[&str] {
        &["scala", "sc"]
    }
    fn supports_phase_b(&self) -> bool {
        sbt_available()
    }
    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        ParseResponse::default()
    }
    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        // Collected outside the response so they survive the error path too, as
        // in the php sidecar. The sbt-root caveat below is raised for a layout
        // whose build is then expected to fail, so hanging it off the `Ok` value
        // would drop it in exactly the case it is written for.
        let mut diagnostics = Vec::new();
        let result = run_semanticdb(&req.root, req.corpus.as_str(), &mut diagnostics);
        into_response(result, &req.root, diagnostics)
    }
}

/// Folds a run's outcome and its diagnostics into one response.
fn into_response(
    result: anyhow::Result<InvokeResponse>,
    root: &Path,
    diagnostics: Vec<PluginDiagnostic>,
) -> InvokeResponse {
    let mut resp = match result {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!("semanticdb failed for {}: {e}", root.display());
            InvokeResponse::default()
        }
    };
    resp.diagnostics.extend(diagnostics);
    resp
}

static SBT_BIN: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

fn find_sbt() -> Option<&'static PathBuf> {
    SBT_BIN
        .get_or_init(|| {
            // 1. PATH / ~/.travsr/bin, PATHEXT-aware. A bare `d.join("sbt").exists()`
            // probe finds the POSIX shim sbt ships alongside `sbt.bat` on Windows (a
            // `#!/usr/bin/env bash` script CreateProcess cannot run), the same bug
            // shape as K6/#502. `tool_path` prefers `sbt.bat` there and degrades to
            // the bare name on unix, with no JVM spawn either way.
            if let Some(p) = travsr_core::exec::tool_path("sbt") {
                return Some(p);
            }
            let home = std::env::var_os("HOME").map(PathBuf::from)?;
            // 2. SDKMAN! (sdk install sbt)
            let candidate = home.join(".sdkman/candidates/sbt/current/bin/sbt");
            if candidate.exists() {
                return Some(candidate);
            }
            // 3. Coursier on Linux (cs install sbt)
            let candidate = home.join(".local/share/coursier/bin/sbt");
            if candidate.exists() {
                return Some(candidate);
            }
            // 4. Coursier on macOS
            let candidate = home.join("Library/Application Support/Coursier/bin/sbt");
            candidate.exists().then_some(candidate)
        })
        .as_ref()
}

fn sbt_available() -> bool {
    find_sbt().is_some()
}

/// Walk up to `max_depth` levels to find the directory containing `build.sbt`.
fn find_sbt_root(root: &Path, max_depth: usize) -> Option<PathBuf> {
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));
    while let Some((dir, depth)) = queue.pop_front() {
        if dir.join("build.sbt").is_file() {
            return Some(dir);
        }
        if depth < max_depth {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                // Sorted before enqueuing: `read_dir` order decides which of two
                // sibling directories that both hold a `build.sbt` is picked as
                // the root, and the whole index is built from that one choice.
                let mut children: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .filter(|p| {
                        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        !matches!(name, "target" | ".git" | "node_modules" | ".travsr")
                    })
                    .collect();
                children.sort();
                for p in children {
                    queue.push_back((p, depth + 1));
                }
            }
        }
    }
    None
}

/// Walk the whole sbt project collecting `*.semanticdb` files written under a
/// `target/` directory (any depth).
///
/// #832: a multi-module / sbt-crossproject build writes SemanticDB under each
/// sub-project's own `target/` (e.g. `<root>/jvm/target/.../meta-inf/semanticdb`),
/// not only `<root>/target/`, so searching a single top-level `target/` misses
/// every sub-project's output. `.semanticdb` files written by the compiler only
/// ever live under a `target/` dir, so walking source trees too is harmless
/// (and cheap next to a Scala compile).
///
/// Two rules keep the widened walk from picking up someone else's copy:
/// every dot-directory is skipped, and a file is only collected when a `target`
/// component precedes it. Metals/Bloop compile the same sources into
/// `.bloop/<project>/bloop-bsp-clients-classes/.../META-INF/semanticdb/`, which
/// would otherwise be parsed alongside sbt's own output: the same
/// `TextDocument`s twice, from a copy that can lag the source and produce stale
/// `edge_sites` lines.
fn find_semanticdb_files(sbt_root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(sbt_root.to_path_buf());
    while let Some(dir) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                // Skip dot-dirs (.git, .bloop, .metals, .bsp, .idea, .travsr)
                // and dependency caches; everything else may hold a `target/`.
                if !name.starts_with('.') && name != "node_modules" {
                    queue.push_back(p);
                }
            } else if p.extension().and_then(|e| e.to_str()) == Some("semanticdb")
                && under_target_dir(sbt_root, &p)
            {
                found.push(p);
            }
        }
    }
    // `read_dir` hands entries back in filesystem order, so without this the
    // documents are parsed in a different order on every run. That order is
    // load-bearing: a cross-built sbt project compiles the SAME sources into
    // `jvm/target`, `native/target` and `js/target`, and `build_edges` keeps the
    // FIRST definition it sees for a symbol, so whichever variant came back
    // first owns the def node and the `dst` of every cross-file edge flips
    // between runs. Sorting here fixes both the parse order and the node
    // emission order that follows it.
    found.sort();
    found
}

/// True when `file`, relative to `sbt_root`, has a `target` path component.
/// Only the part below the sbt root is examined, so an sbt project that itself
/// sits under a directory called `target` is not mistaken for build output.
fn under_target_dir(sbt_root: &Path, file: &Path) -> bool {
    let rel = file.strip_prefix(sbt_root).unwrap_or(file);
    rel.components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case("target"))
}

/// `InvokeRequest::root` arrives on Windows already canonicalized with the
/// extended-length verbatim prefix, e.g. `\\?\D:\repo`, not a plain drive
/// path (same shape as the Kotlin wrapper's `strip_windows_verbatim_prefix`,
/// see its doc comment for the general mechanism). Left unstripped and handed
/// to `Command::current_dir`, sbt's own `bootServerSocket` (which even
/// `--server` mode runs, to bind the IPC socket other clients could still
/// attach to) calls `Path.toRealPath()` on the CWD to derive the socket's
/// identity, and `WindowsLinkSupport.getRealPath` throws
/// `AccessDeniedException` on a `\\?\`-prefixed path under the sandbox (empty
/// stdout, no compile ever attempted). A no-op when the prefix isn't present.
fn strip_windows_verbatim_prefix(s: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        // `\\?\UNC\server\share` denotes the UNC path `\\server\share`; keep the
        // leading `\\` rather than degrading it to a bare relative `server\share`.
        std::borrow::Cow::Owned(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        std::borrow::Cow::Borrowed(rest)
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Terminate a spawned build process and its descendants. On Windows,
/// `Child::kill` terminates only the immediate child (the launcher), leaving the
/// sbt/JVM grandchildren running; `taskkill /T` kills the whole tree.
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

fn run_semanticdb(
    root: &Path,
    corpus: &str,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> anyhow::Result<InvokeResponse> {
    // One deadline for the whole run, not a fixed budget per attempt. The host
    // keeps nothing from an invoke it kills, so both compiles, the SemanticDB
    // scan, the parse and the reply have to fit inside its window together, and
    // two independent budgets cannot express that: 180 + 60 could overrun it (a
    // non-zero exit at 170s followed by a full 60s fallback is 230s before the
    // scan even starts) while also underspending it, since a fast failure at 20s
    // still left the fallback only 60s of a window with 200s free.
    let deadline = Instant::now()
        + Duration::from_secs(
            HOST_INVOKE_TIMEOUT_SECS
                .saturating_sub(HOST_REPLY_MARGIN_SECS)
                .saturating_sub(POST_COMPILE_RESERVE_SECS),
        );
    let root_str = root.display().to_string();
    let root_stripped = strip_windows_verbatim_prefix(&root_str);
    let root = Path::new(root_stripped.as_ref());
    let sbt_bin = find_sbt().ok_or_else(|| {
        anyhow::anyhow!(
            "sbt not found. Install via SDKMAN! (sdk install sbt), \
             Coursier (cs install sbt), or Homebrew (brew install sbt)"
        )
    })?;

    let sbt_root = find_sbt_root(root, 4)
        .ok_or_else(|| anyhow::anyhow!("no build.sbt found under {}", root.display()))?;
    tracing::info!(sbt_root = %sbt_root.display(), "found sbt project root");
    if let Some(d) = build_root_not_granted(root, &sbt_root) {
        diagnostics.push(d);
    }

    // Compile the Test configuration too. `compile` alone builds only the
    // Compile config, so no test source ever gets a `.semanticdb` and every
    // caller that lives in a test is invisible: on scala-parser-combinators all
    // 78 files were `src/main` and `references parseAll` returned 0 of its 4
    // real callers, every one of them a test. Enabling the SemanticDB *setting*
    // in every scope (#832) does not compile test sources, it only means they
    // would carry SemanticDB if something built them.
    let budget = attempt_budget_secs(
        deadline.saturating_duration_since(Instant::now()),
        COMPILE_TIMEOUT_SECS,
        MIN_ATTEMPT_SECS,
    )
    .ok_or_else(|| {
        anyhow::anyhow!(
            "under {MIN_ATTEMPT_SECS}s of the host's {HOST_INVOKE_TIMEOUT_SECS}s invoke window \
             was left by the time sbt could be started"
        )
    })?;
    let (mut status, mut stdout_out, mut stderr_out) =
        run_sbt_compile(&sbt_root, sbt_bin, budget, true)?;
    if !status.success() {
        // A repo whose tests do not compile must still get its main-scope
        // graph rather than nothing: sbt runs the commands in sequence and
        // aborts at the first failure, so a broken test tree would otherwise
        // take `compile` down with it. Retry without the test scope, on what is
        // left of the shared deadline rather than a fresh fixed budget.
        match attempt_budget_secs(
            deadline.saturating_duration_since(Instant::now()),
            COMPILE_TIMEOUT_SECS,
            MIN_ATTEMPT_SECS,
        ) {
            Some(budget) => {
                tracing::warn!(
                    "sbt compile including Test scope failed, retrying with the Compile \
                     scope only for {budget}s; test-source references will be missing:\n{}",
                    tail(&stderr_out, 4000)
                );
                (status, stdout_out, stderr_out) =
                    run_sbt_compile(&sbt_root, sbt_bin, budget, false)?;
            }
            // Reporting the first failure is worth more than starting a retry
            // that cannot finish: an attempt that runs into the host's watchdog
            // loses the whole response, this diagnostic included.
            None => tracing::warn!(
                "sbt compile including Test scope failed and under {MIN_ATTEMPT_SECS}s of the \
                 host's {HOST_INVOKE_TIMEOUT_SECS}s window is left, so the Compile-scope retry \
                 is skipped and the failure is reported as is:\n{}",
                tail(&stderr_out, 4000)
            ),
        }
    }
    anyhow::ensure!(
        status.success(),
        "sbt compile exited with {status}:\n{}",
        tail(&stderr_out, 4000)
    );

    let semanticdb_files = find_semanticdb_files(&sbt_root);
    tracing::info!("found {} .semanticdb files", semanticdb_files.len());

    // #832: a compile that succeeds but produces no SemanticDB used to fail
    // silently as an empty graph with a misdirecting "unbuildable project"
    // remedy downstream. Surface the sbt output so the real cause (SemanticDB
    // not enabled for the compiled sub-projects, nothing recompiled, etc.) is
    // recoverable from the log.
    if semanticdb_files.is_empty() {
        tracing::warn!(
            sbt_root = %sbt_root.display(),
            "sbt compile succeeded but produced no .semanticdb files. \
             SemanticDB may not be enabled for the sub-projects that were \
             compiled, or nothing was recompiled. sbt stdout tail:\n{}\n\
             sbt stderr tail:\n{}",
            tail(&stdout_out, 4000),
            tail(&stderr_out, 4000),
        );
    }

    let mut all_docs = Vec::new();
    for path in &semanticdb_files {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        all_docs.extend(parse_text_documents(&bytes));
    }

    // #813: `sbt_root`, not `root`. SemanticDB `TextDocument.uri`s are relative
    // to sbt's sourceroot, which is the sbt project base directory, and
    // `find_sbt_root` allows that to be a nested directory. Joining them onto
    // the repo root would not find the file, and the column would silently be
    // `None` for every reference in such a build.
    Ok(build_edges(&all_docs, corpus, &sbt_root))
}

/// This attempt's sbt budget in seconds: `ceiling_secs`, clamped to what is left
/// of the run's overall deadline. `None` when under `floor_secs` remains, which
/// is the caller's signal to skip the attempt rather than start one that would
/// expire before sbt has reloaded the build and take the reply reserve with it.
///
/// Pure so the arithmetic is testable without an sbt install: the clock lives at
/// the call site.
fn attempt_budget_secs(remaining: Duration, ceiling_secs: u64, floor_secs: u64) -> Option<u64> {
    let remaining = remaining.as_secs();
    (remaining >= floor_secs).then_some(remaining.min(ceiling_secs))
}

/// Caveat for the layout where `find_sbt_root` resolves below the repo root.
///
/// The host's repo-write grants for scala are all anchored at the repo root
/// (travsr-plugin-host `sandbox/toolchain.rs`, `repo_write_subpaths("scala")`:
/// `target`, `project/target`, `project/project/target`, `js|jvm|native/target`,
/// each joined to the repo root). `find_sbt_root` BFSes DOWN up to four levels
/// and `run_sbt_compile` then runs with `current_dir(sbt_root)`, so a build.sbt
/// below the repo root makes sbt write `<sbt_root>/target/`, which no grant
/// covers. On Linux the repo root is a bwrap `--ro-bind`, so those writes take
/// EROFS and the user gets sbt's stderr tail with nothing pointing at travsr's
/// own sandbox as the cause.
///
/// Widening the grants is a security-policy change in the other repo and needs
/// an ADR amendment, so this only makes the cause visible. `None` for the common
/// layout where the two paths agree: a caveat, not routine chatter.
fn build_root_not_granted(repo_root: &Path, sbt_root: &Path) -> Option<PluginDiagnostic> {
    if sbt_root == repo_root {
        return None;
    }
    Some(PluginDiagnostic::warning(
        "scala.build-root-not-granted",
        format!(
            "Scala indexing is expected to fail on Linux for this layout: build.sbt is at {}, \
             not at the repo root {}, so sbt writes its build output under a directory the \
             travsr sandbox does not grant (the grants cover target/ and project/target/ \
             relative to the repo root only) and the writes are denied.",
            sbt_root.display(),
            repo_root.display()
        ),
    ))
}

/// Last `max_bytes` bytes of `s`, on a char boundary, prefixed with an elision
/// marker when truncated. Used to bound sbt output echoed into a log line.
fn tail(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("...(truncated)...\n{}", &s[start..])
}

/// `with_tests` also compiles the Test configuration, so test sources get
/// SemanticDB output and their call sites enter the graph. Retried as `false` by
/// the caller when a repo's tests do not compile.
///
/// `budget_secs` is this attempt's own budget, started here. The caller derives
/// it from one deadline shared by the whole run (`attempt_budget_secs`), so the
/// retry gets what is genuinely left instead of a fixed budget that could push
/// the run past the host's watchdog, and is skipped outright when too little is
/// left for it to finish. A timeout here bails rather than returning a status,
/// so it ends the run: the caller's retry is reached only on a non-zero exit.
fn run_sbt_compile(
    sbt_root: &Path,
    sbt_bin: &Path,
    budget_secs: u64,
    with_tests: bool,
) -> anyhow::Result<(std::process::ExitStatus, String, String)> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(budget_secs);
    // #S3: plain `sbt compile` uses sbt 2.x's thin-client/background-server
    // split (`sbtn`), which talks to the server over a loopback socket. Windows
    // AppContainer blocks loopback for a sandboxed process unless separately
    // exempted via NetworkIsolationSetAppContainerConfig (a systemwide,
    // admin-only setting travsr has no business changing per invoke). The
    // client's connect fails silently and it returns a false "success" in
    // under a second with no compile ever run. `--server` runs sbt itself in
    // the foreground as one process (no client/server IPC at all), so it works
    // the same whether sandboxed or not; confirmed identical real compiles
    // (real elapsed time, `.semanticdb` output) with and without the sandbox.
    let mut child = std::process::Command::new(sbt_bin)
        .args(if with_tests {
            &["--server", SEMANTICDB_ENABLE_CMD, "compile", "Test/compile"][..]
        } else {
            &["--server", SEMANTICDB_ENABLE_CMD, "compile"][..]
        })
        .current_dir(sbt_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn sbt")?;

    // Drain stdout/stderr on their own threads *while* sbt runs. sbt compile emits
    // far more output than an OS pipe buffer holds; reading only after exit
    // deadlocks (sbt blocks writing to a full pipe while we block waiting for it to
    // exit). The reader threads finish at EOF, when sbt exits or is killed.
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
        match child.try_wait().context("polling sbt")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                kill_process_tree(&mut child);
                anyhow::bail!("sbt compile timed out after {budget_secs}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(500)),
        }
    };

    // Join the stdout drain thread so its pipe is fully consumed (the drain is
    // what prevents the pipe-buffer deadlock). #832: keep its content too so a
    // zero-SemanticDB outcome can surface the sbt output instead of failing
    // silently; sbt reports compile progress and most errors on stdout.
    let stdout_out = out_h.join().unwrap_or_default();
    let stderr_out = err_h.join().unwrap_or_default();
    Ok((status, stdout_out, stderr_out))
}

// ── Minimal SemanticDB protobuf parser ───────────────────────────────────────
// SemanticDB file = serialised TextDocuments proto (wire format).
// We only decode fields needed for ref/call edge extraction.

#[derive(Debug, Default)]
struct TextDocument {
    uri: String,
    symbols: Vec<SymbolInfo>,
    occurrences: Vec<Occurrence>,
}

#[derive(Debug)]
struct SymbolInfo {
    symbol: String,
    kind: u32,
}

#[derive(Debug)]
struct Occurrence {
    start_line: u32,
    // #813: SemanticDB Range field 2, the occurrence's start character. Kept so
    // the reference can carry a column where the encoding cannot change it.
    start_char: u32,
    end_line: u32,
    symbol: String,
    role: u32, // 1 = REFERENCE, 2 = DEFINITION
    // #299 F3: whether this occurrence actually carried a Range message.
    // SemanticDB ranges are 0-based, so a genuine line-1 occurrence and an
    // absent range both decode to start_line == 0, so this bit disambiguates
    // them so a range-less occurrence is never emitted as a phantom `path:1`.
    has_range: bool,
}

fn read_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let b = *data.get(*pos)?;
        *pos += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

fn skip_field(data: &[u8], pos: &mut usize, wire_type: u64) {
    match wire_type {
        0 => {
            read_varint(data, pos);
        }
        1 => *pos = pos.saturating_add(8).min(data.len()),
        2 => {
            if let Some(len) = read_varint(data, pos) {
                *pos = pos.saturating_add(len as usize).min(data.len());
            }
        }
        5 => *pos = pos.saturating_add(4).min(data.len()),
        _ => *pos = data.len(), // unknown wire type, stop parsing
    }
}

/// Parse start_line (field 1), start_character (field 2) and end_line (field 3)
/// from a SemanticDB Range message.
fn parse_range_lines(data: &[u8]) -> (u32, u32, u32) {
    let mut pos = 0;
    let mut start_line = 0u32;
    let mut start_char = 0u32;
    let mut end_line = 0u32;
    while pos < data.len() {
        let Some(tag) = read_varint(data, &mut pos) else {
            break;
        };
        let field = (tag >> 3) as u32;
        let wtype = tag & 7;
        if wtype == 0 {
            let val = read_varint(data, &mut pos).unwrap_or(0);
            match field {
                1 => start_line = val as u32,
                2 => start_char = val as u32,
                3 => end_line = val as u32,
                _ => {}
            }
        } else {
            skip_field(data, &mut pos, wtype);
        }
    }
    (start_line, start_char, end_line)
}

fn parse_occurrence(data: &[u8]) -> Occurrence {
    let mut pos = 0;
    let mut occ = Occurrence {
        start_line: 0,
        start_char: 0,
        end_line: 0,
        symbol: String::new(),
        role: 0,
        has_range: false,
    };
    while pos < data.len() {
        let Some(tag) = read_varint(data, &mut pos) else {
            break;
        };
        let field = (tag >> 3) as u32;
        let wtype = tag & 7;
        match (field, wtype) {
            (1, 2) => {
                // range: extract both start_line and end_line
                let len = read_varint(data, &mut pos).unwrap_or(0) as usize;
                let end = pos.saturating_add(len).min(data.len());
                let (sl, sc, el) = parse_range_lines(&data[pos..end]);
                occ.start_line = sl;
                occ.start_char = sc;
                occ.end_line = if el > 0 { el } else { sl }; // single-line: end == start
                occ.has_range = true;
                pos = end;
            }
            (2, 2) => {
                // symbol string
                let len = read_varint(data, &mut pos).unwrap_or(0) as usize;
                let end = pos.saturating_add(len).min(data.len());
                if let Ok(s) = std::str::from_utf8(&data[pos..end]) {
                    occ.symbol = s.to_string();
                }
                pos = end;
            }
            (3, 0) => {
                // role
                occ.role = read_varint(data, &mut pos).unwrap_or(0) as u32;
            }
            (_, wtype) => skip_field(data, &mut pos, wtype),
        }
    }
    occ
}

fn parse_sym_info(data: &[u8]) -> SymbolInfo {
    let mut pos = 0;
    let mut info = SymbolInfo {
        symbol: String::new(),
        kind: 0,
    };
    while pos < data.len() {
        let Some(tag) = read_varint(data, &mut pos) else {
            break;
        };
        let field = (tag >> 3) as u32;
        let wtype = tag & 7;
        match (field, wtype) {
            (1, 2) => {
                let len = read_varint(data, &mut pos).unwrap_or(0) as usize;
                let end = pos.saturating_add(len).min(data.len());
                if let Ok(s) = std::str::from_utf8(&data[pos..end]) {
                    info.symbol = s.to_string();
                }
                pos = end;
            }
            (3, 0) => {
                info.kind = read_varint(data, &mut pos).unwrap_or(0) as u32;
            }
            (_, wtype) => skip_field(data, &mut pos, wtype),
        }
    }
    info
}

fn parse_text_document(data: &[u8]) -> TextDocument {
    let mut pos = 0;
    let mut doc = TextDocument::default();
    while pos < data.len() {
        let Some(tag) = read_varint(data, &mut pos) else {
            break;
        };
        let field = (tag >> 3) as u32;
        let wtype = tag & 7;
        match (field, wtype) {
            (2, 2) => {
                // uri
                let len = read_varint(data, &mut pos).unwrap_or(0) as usize;
                let end = pos.saturating_add(len).min(data.len());
                if let Ok(s) = std::str::from_utf8(&data[pos..end]) {
                    doc.uri = s.to_string();
                }
                pos = end;
            }
            (5, 2) => {
                // symbols (repeated SymbolInformation)
                let len = read_varint(data, &mut pos).unwrap_or(0) as usize;
                let end = pos.saturating_add(len).min(data.len());
                doc.symbols.push(parse_sym_info(&data[pos..end]));
                pos = end;
            }
            (6, 2) => {
                // occurrences (repeated SymbolOccurrence)
                let len = read_varint(data, &mut pos).unwrap_or(0) as usize;
                let end = pos.saturating_add(len).min(data.len());
                doc.occurrences.push(parse_occurrence(&data[pos..end]));
                pos = end;
            }
            (_, wtype) => skip_field(data, &mut pos, wtype),
        }
    }
    doc
}

fn parse_text_documents(data: &[u8]) -> Vec<TextDocument> {
    let mut pos = 0;
    let mut docs = Vec::new();
    while pos < data.len() {
        let Some(tag) = read_varint(data, &mut pos) else {
            break;
        };
        let field = (tag >> 3) as u32;
        let wtype = tag & 7;
        match (field, wtype) {
            (1, 2) => {
                // documents (repeated TextDocument)
                let len = read_varint(data, &mut pos).unwrap_or(0) as usize;
                let end = pos.saturating_add(len).min(data.len());
                docs.push(parse_text_document(&data[pos..end]));
                pos = end;
            }
            (_, wtype) => skip_field(data, &mut pos, wtype),
        }
    }
    docs
}

// ── Edge construction ─────────────────────────────────────────────────────────

// SemanticDB Kind enum values (scalameta/semanticdb.proto)
const KIND_FIELD: u32 = 2;
const KIND_METHOD: u32 = 3;
const KIND_OBJECT: u32 = 10;
const KIND_PACKAGE_OBJECT: u32 = 12;
const KIND_CLASS: u32 = 13;
const KIND_TRAIT: u32 = 14;
const KIND_MACRO: u32 = 22;

fn is_container_kind(kind: u32) -> bool {
    matches!(
        kind,
        KIND_FIELD
            | KIND_METHOD
            | KIND_OBJECT
            | KIND_PACKAGE_OBJECT
            | KIND_CLASS
            | KIND_TRAIT
            | KIND_MACRO
    )
}

fn kind_str(kind: u32) -> &'static str {
    match kind {
        KIND_FIELD => "field",
        KIND_METHOD | KIND_MACRO => "method",
        KIND_OBJECT | KIND_PACKAGE_OBJECT => "object",
        KIND_CLASS => "class",
        KIND_TRAIT => "trait",
        _ => "sym",
    }
}

/// A symbol whose fully-qualified name sits under a standard-library namespace.
///
/// Only meaningful together with the set of symbols this repo defines: see
/// [`is_noise_symbol`]. A SemanticDB symbol carries no package or provenance
/// prefix, so the namespace alone cannot say whether `scala/util/…` is the
/// standard library or the repo's own code.
fn is_stdlib_symbol(symbol: &str) -> bool {
    symbol.starts_with("scala/")
        || symbol.starts_with("java/")
        || symbol.starts_with("javax/")
        || symbol.starts_with("_root_/")
        || symbol.starts_with("android/")
}

/// A SemanticDB anonymous local (`local0`, `local12`): intra-method noise with
/// no navigable identity, like SCIP's `local N`. Dropped so it never surfaces
/// as a graph node.
fn is_local_symbol(symbol: &str) -> bool {
    symbol
        .strip_prefix("local")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// A parameter descriptor (`…greet().(name)`, terminal `(name)` ending in `)`).
/// Signature-local noise the scip-reader path already drops; mirror it here so
/// scala matches the other languages and never emits a raw parameter node.
fn is_parameter_descriptor(symbol: &str) -> bool {
    symbol.ends_with(')')
}

/// Symbols that must not become graph nodes or edge endpoints: anonymous
/// locals, parameter descriptors, and *external* standard-library symbols.
///
/// `defined` is every symbol carrying a `SymbolInformation` entry in some parsed
/// `TextDocument`, which is exactly the set this repo defines. A SCIP symbol
/// carries a package moniker that separates the stdlib from the repo's own code;
/// a SemanticDB symbol does not, it is a bare fully-qualified name. So the
/// namespace prefix can only be trusted for a symbol the repo does not define
/// itself. Without that gate, a repo published under `scala.*` has 100% of its
/// own symbols classified as stdlib and indexes to zero nodes: on
/// scala-parser-combinators all 3460 non-local symbols were dropped, `def_ids`
/// came out empty, and Phase B reported success with an empty graph.
fn is_noise_symbol(symbol: &str, defined: &HashSet<&str>) -> bool {
    is_local_symbol(symbol)
        || is_parameter_descriptor(symbol)
        || (!defined.contains(symbol) && is_stdlib_symbol(symbol))
}

fn sdb_vname(symbol: &str, path: &str, corpus: &str) -> VName {
    VName::new(corpus, "", path, "scala", format!("sdb:{symbol}"))
}

/// `source_root` is what a `TextDocument.uri` is relative to (sbt's sourceroot,
/// i.e. the sbt project base directory), used only to read source for the
/// occurrence column. A wrong or unreadable root costs columns, never edges.
fn build_edges(docs: &[TextDocument], corpus: &str, source_root: &Path) -> InvokeResponse {
    // #813: one read per referenced file, shared across all documents.
    let mut src_cache = travsr_lang_scip_reader::SourceCache::new();
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    // #299 S1: occurrence records (path:line) so the daemon can populate
    // edge_sites and answer find_references. Without these, only structural
    // ref/call edges exist and find_references returns 0.
    let mut refs: Vec<ScipRef> = Vec::new();

    // Pre-pass: global symbol → definition NodeId, so a reference in one file to a
    // symbol defined in another resolves to the correct callee node (the per-doc
    // edge builder below keys dst on the *reference's* uri, which is wrong across
    // files; refs use this map instead).
    // Every symbol this repo defines, so `is_noise_symbol` can tell the repo's
    // own `scala/…` code from the actual standard library.
    let defined: HashSet<&str> = docs
        .iter()
        .flat_map(|d| d.symbols.iter())
        .map(|s| s.symbol.as_str())
        .collect();

    let mut def_ids: HashMap<String, NodeId> = HashMap::new();
    for doc in docs {
        for sym in &doc.symbols {
            if is_noise_symbol(&sym.symbol, &defined) {
                continue;
            }
            def_ids
                .entry(sym.symbol.clone())
                .or_insert_with(|| sdb_vname(&sym.symbol, &doc.uri, corpus).id());
        }
    }

    for doc in docs {
        let uri = &doc.uri;

        // symbol → kind map
        let kind_map: HashMap<&str, u32> = doc
            .symbols
            .iter()
            .map(|s| (s.symbol.as_str(), s.kind))
            .collect();

        // DEF occurrences for container symbols, sorted by start_line ascending
        let mut def_containers: Vec<(u32, &str)> = doc
            .occurrences
            .iter()
            .filter(|o| o.role == 2) // DEFINITION
            .filter(|o| {
                let kind = kind_map.get(o.symbol.as_str()).copied().unwrap_or(0);
                is_container_kind(kind)
            })
            .map(|o| (o.start_line, o.symbol.as_str()))
            .collect();
        def_containers.sort_by_key(|(line, _)| *line);

        // Emit a node for every user-defined symbol
        for sym in &doc.symbols {
            if is_noise_symbol(&sym.symbol, &defined) {
                continue;
            }
            let vname = sdb_vname(&sym.symbol, uri, corpus);
            // #299 F3: only trust a DEFINITION occurrence that carried a real
            // range. A range-less def would decode to start_line 0 → a bogus
            // line-0 span that can wrongly enclose later occurrences; emit the
            // node without a line instead so it is never a false enclosing span.
            let def_occ = doc
                .occurrences
                .iter()
                .find(|o| o.role == 2 && o.symbol == sym.symbol && o.has_range);
            let mut node = Node::new(vname, kind_str(sym.kind));
            if let Some(o) = def_occ {
                node = node
                    .with_line(o.start_line + 1)
                    .with_end_line(o.end_line + 1);
            }
            nodes.push(node);
        }

        // ref/call edges: for each user REFERENCE find enclosing DEF container
        for occ in &doc.occurrences {
            if occ.role != 1 {
                continue; // only REFERENCEs
            }
            if is_noise_symbol(&occ.symbol, &defined) {
                continue;
            }
            // #299 F3: a range-less reference occurrence has no real position;
            // skip it so it is never recorded as a phantom `path:1` site (and
            // its enclosing-container lookup below is meaningless without a line).
            if !occ.has_range {
                continue;
            }

            // #299 S1 + R6: emit an occurrence record for this reference so the
            // daemon attributes it to its enclosing function (positional span
            // lookup in write_scip_attributed_batch) and records an edge_sites
            // row. A ScipRef, when produced, SUPERSEDES the structural enclosing
            // edge below: emitting both creates a spurious second ref/call caller
            // because the structural edge derives its src from the def-container
            // heuristic, not the positional span, so the two rows differ under
            // ON CONFLICT(src, dst, kind). The structural edge also mis-keyed its
            // dst on the *reference's* uri instead of the definition's (#597),
            // dangling for every cross-file reference; routing resolved refs
            // solely through the ScipRef (callee_id keyed on the def uri via
            // def_ids) removes that dangling too.
            if let Some(&callee_id) = def_ids.get(&occ.symbol) {
                refs.push(ScipRef {
                    caller_path: uri.clone(),
                    caller_line: occ.start_line + 1,
                    callee_id,
                    // is_call (#650): no call/non-call signal available here;
                    // preserve prior behavior / wire default (default_true).
                    is_call: true,
                    // #813: the occurrence's own column, kept only where the
                    // encoding cannot change its meaning (SemanticDB counts
                    // UTF-16 code units, but this does not have to trust that);
                    // `None` otherwise and the daemon name-searches as before.
                    caller_col: src_cache.col(
                        &source_root.join(uri),
                        occ.start_line + 1,
                        occ.start_char as i32,
                    ),
                });
                continue;
            }

            // Fallback only when the callee has no in-corpus definition (no
            // ScipRef, no def node): a best-effort enclosing → reference edge.
            // It has no resolvable callee node and is dropped by the daemon's
            // fail-closed callee gate; kept minimal to avoid a wider behavioral
            // change than R6 requires.
            let Some((_, enc_sym)) = def_containers
                .iter()
                .rev()
                .find(|(line, _)| *line <= occ.start_line)
            else {
                continue;
            };

            if *enc_sym == occ.symbol {
                continue; // skip self-ref
            }

            let src_vname = sdb_vname(enc_sym, uri, corpus);
            let dst_vname = sdb_vname(&occ.symbol, uri, corpus);
            edges.push(Edge::new(src_vname.id(), dst_vname.id(), EdgeKind::RefCall));
        }
    }

    let edge_count = edges.len();
    let node_count = nodes.len();
    tracing::info!(
        nodes = node_count,
        edges = edge_count,
        "semanticdb: built graph"
    );

    InvokeResponse {
        diagnostics: Vec::new(),
        nodes,
        edges,
        refs,
        unresolved_calls: Vec::new(),
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_scala=info".parse().unwrap()),
        )
        .init();

    run_plugin(ScalaPhaseB);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three shapes the clamp has to get right: ceiling binds, remaining
    /// binds, and the floor refuses.
    #[test]
    fn attempt_budget_clamps_to_the_smaller_of_ceiling_and_remaining() {
        assert_eq!(
            attempt_budget_secs(Duration::from_secs(300), 225, 45),
            Some(225)
        );
        assert_eq!(
            attempt_budget_secs(Duration::from_secs(80), 225, 45),
            Some(80)
        );
        assert_eq!(attempt_budget_secs(Duration::from_secs(44), 225, 45), None);
        // The floor is inclusive: exactly enough is enough.
        assert_eq!(
            attempt_budget_secs(Duration::from_secs(45), 225, 45),
            Some(45)
        );
        assert_eq!(attempt_budget_secs(Duration::ZERO, 225, 45), None);
    }

    /// The property the shared deadline exists for: whatever the first attempt
    /// spends, the retry can only get what is left, so the two together never
    /// exceed the compile window. The 180 + 60 pair this replaced could total
    /// 230s before the SemanticDB scan had even started.
    #[test]
    fn a_retry_can_only_have_what_the_first_attempt_left() {
        let window = HOST_INVOKE_TIMEOUT_SECS - HOST_REPLY_MARGIN_SECS - POST_COMPILE_RESERVE_SECS;
        for spent in 0..=window {
            let retry = attempt_budget_secs(
                Duration::from_secs(window - spent),
                COMPILE_TIMEOUT_SECS,
                MIN_ATTEMPT_SECS,
            )
            .unwrap_or(0);
            assert!(spent + retry <= window, "spent {spent}, retry {retry}");
        }
    }

    /// A cold coursier resolve needs more than the 180s this replaced, and the
    /// host's 300s kill less both reserves is the only real ceiling.
    #[test]
    fn compile_ceiling_fills_the_window_without_overrunning_it() {
        assert_eq!(
            COMPILE_TIMEOUT_SECS,
            HOST_INVOKE_TIMEOUT_SECS - HOST_REPLY_MARGIN_SECS - POST_COMPILE_RESERVE_SECS
        );
    }

    #[test]
    fn sbt_root_at_the_repo_root_raises_no_caveat() {
        let root = Path::new("/repo");
        assert!(build_root_not_granted(root, root).is_none());
    }

    #[test]
    fn sbt_root_below_the_repo_root_warns_with_both_paths() {
        let d = build_root_not_granted(Path::new("/repo"), Path::new("/repo/backend/svc"))
            .expect("a nested build.sbt is outside every repo-root-anchored grant");
        assert_eq!(d.code, "scala.build-root-not-granted");
        assert!(d.message.contains("/repo/backend/svc"), "{}", d.message);
        assert!(d.message.contains("/repo"), "{}", d.message);
    }

    /// The caveat is raised for a layout whose sbt build is then expected to
    /// fail, so it only earns its keep if it survives the `Err` path.
    #[test]
    fn diagnostics_survive_the_error_path() {
        let diagnostics = vec![
            build_root_not_granted(Path::new("/repo"), Path::new("/repo/sub"))
                .expect("nested root"),
        ];
        let resp = into_response(
            Err(anyhow::anyhow!("sbt compile exited with 1")),
            Path::new("/repo"),
            diagnostics,
        );
        assert_eq!(
            resp.diagnostics
                .iter()
                .map(|d| d.code.as_str())
                .collect::<Vec<_>>(),
            ["scala.build-root-not-granted"]
        );
    }

    #[test]
    fn strip_verbatim_prefix_handles_drive_unc_and_plain() {
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\D:\repo").as_ref(),
            r"D:\repo"
        );
        // UNC verbatim form keeps its leading `\\`, not a relative `server\share`.
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\UNC\server\share\repo").as_ref(),
            r"\\server\share\repo"
        );
        assert_eq!(
            strip_windows_verbatim_prefix(r"D:\repo").as_ref(),
            r"D:\repo"
        );
        assert_eq!(
            strip_windows_verbatim_prefix("/home/u/repo").as_ref(),
            "/home/u/repo"
        );
    }

    fn touch(root: &Path, rel: &str) -> PathBuf {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&p, b"").expect("write file");
        p
    }

    // #832: sub-project `target/` output is collected at any depth, while the
    // copies Metals/Bloop keep under dot-directories are not.
    #[test]
    fn find_semanticdb_files_collects_subproject_target_and_skips_tool_caches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        let jvm = touch(
            root,
            "jvm/target/scala-2.13/meta/META-INF/semanticdb/A.scala.semanticdb",
        );
        let native = touch(
            root,
            "native/target/scala-2.13/meta/META-INF/semanticdb/B.scala.semanticdb",
        );
        // Same sources, compiled by Bloop/Metals; must not be picked up.
        touch(
            root,
            ".bloop/jvm/bloop-bsp-clients-classes/classes-Metals/META-INF/semanticdb/A.scala.semanticdb",
        );
        touch(root, ".git/objects/stale/A.scala.semanticdb");
        touch(root, ".metals/readonly/C.scala.semanticdb");
        // Not build output: a `.semanticdb` sitting in a source tree.
        touch(root, "jvm/src/main/scala/D.scala.semanticdb");

        // Asserted in order, not sorted first: the caller keeps the first
        // definition it sees for a symbol, so a cross-built project needs this
        // list to come back the same way every run. `jvm` sorts before `native`.
        assert_eq!(find_semanticdb_files(root), vec![jvm, native]);
    }

    #[test]
    fn tail_truncates_on_a_char_boundary() {
        // Under the limit: returned verbatim, no marker.
        assert_eq!(tail("abc", 8), "abc");
        assert_eq!(tail("abcdefgh", 8), "abcdefgh");

        let out = tail("abcdefghij", 4);
        assert_eq!(out, "...(truncated)...\nghij");

        // A cut landing mid-character advances to the next boundary rather
        // than slicing a multi-byte char in half.
        let s = "aa\u{00e9}bb"; // 6 bytes: 'a' 'a' 0xC3 0xA9 'b' 'b'
        let out = tail(s, 3);
        assert_eq!(out, "...(truncated)...\nbb");
    }

    fn sym(symbol: &str, kind: u32) -> SymbolInfo {
        SymbolInfo {
            symbol: symbol.to_string(),
            kind,
        }
    }

    fn occ(symbol: &str, line: u32, role: u32) -> Occurrence {
        Occurrence {
            start_char: 0,
            start_line: line,
            end_line: line,
            symbol: symbol.to_string(),
            role,
            has_range: true,
        }
    }

    // R6 + #597: a cross-file reference to a defined symbol is carried solely by
    // a ScipRef whose callee_id is keyed on the DEFINITION's uri; no structural
    // RefCall edge (which would mis-key its dst on the reference's uri and
    // duplicate the positional edge) is emitted.
    #[test]
    fn resolved_cross_file_ref_emits_scipref_only() {
        let docs = vec![
            TextDocument {
                uri: "A.scala".to_string(),
                symbols: vec![
                    sym("a/Caller#", KIND_CLASS),
                    sym("a/Caller#call().", KIND_METHOD),
                ],
                occurrences: vec![
                    occ("a/Caller#", 0, 2),
                    occ("a/Caller#call().", 1, 2),
                    // reference to a symbol defined in B.scala
                    occ("b/Callee#target().", 3, 1),
                ],
            },
            TextDocument {
                uri: "B.scala".to_string(),
                symbols: vec![
                    sym("b/Callee#", KIND_CLASS),
                    sym("b/Callee#target().", KIND_METHOD),
                ],
                occurrences: vec![occ("b/Callee#", 0, 2), occ("b/Callee#target().", 1, 2)],
            },
        ];

        let resp = build_edges(&docs, "testcorpus", Path::new("/nonexistent"));

        // exactly one ScipRef, callee keyed on the DEFINITION uri (B.scala)
        let callee_id = sdb_vname("b/Callee#target().", "B.scala", "testcorpus").id();
        assert_eq!(resp.refs.len(), 1);
        assert_eq!(resp.refs[0].caller_path, "A.scala");
        assert_eq!(resp.refs[0].caller_line, 4); // 3 + 1
        assert_eq!(resp.refs[0].callee_id, callee_id);
        // no structural RefCall edge for the resolved ref (supersede)
        assert!(!resp.edges.iter().any(|e| e.kind == EdgeKind::RefCall));
    }

    // Fallback: a reference to a symbol with no in-corpus definition still emits
    // the best-effort enclosing → reference edge (dropped fail-closed downstream)
    // and no ScipRef.
    #[test]
    fn unresolved_ref_emits_fallback_edge_only() {
        let docs = vec![TextDocument {
            uri: "A.scala".to_string(),
            symbols: vec![
                sym("a/Caller#", KIND_CLASS),
                sym("a/Caller#call().", KIND_METHOD),
            ],
            occurrences: vec![
                occ("a/Caller#", 0, 2),
                occ("a/Caller#call().", 1, 2),
                // reference to an undefined symbol (not in def_ids)
                occ("z/Unknown#gone().", 5, 1),
            ],
        }];

        let resp = build_edges(&docs, "testcorpus", Path::new("/nonexistent"));

        assert!(resp.refs.is_empty());
        let src = sdb_vname("a/Caller#call().", "A.scala", "testcorpus").id();
        let dst = sdb_vname("z/Unknown#gone().", "A.scala", "testcorpus").id();
        assert!(resp
            .edges
            .iter()
            .any(|e| e.src == src && e.dst == dst && e.kind == EdgeKind::RefCall));
    }

    // SemanticDB symbols carry no package/provenance prefix, so a namespace
    // prefix alone cannot tell the stdlib from a repo that publishes under that
    // same namespace. scala-parser-combinators lives in `scala.util.parsing.*`,
    // so every one of its 3460 symbols was classified stdlib and dropped: an
    // empty `def_ids`, no nodes, and a Phase B "success" with an empty graph.
    #[test]
    fn repo_defined_stdlib_namespace_symbol_is_kept() {
        let own = "scala/util/parsing/combinator/Parsers#phrase().";
        let external = "scala/Predef#println().";
        let defined: HashSet<&str> = [own].into_iter().collect();

        assert!(
            !is_noise_symbol(own, &defined),
            "a `scala/...` symbol this repo defines is the repo's own code"
        );
        assert!(
            is_noise_symbol(external, &defined),
            "a `scala/...` symbol the repo does not define is the real stdlib"
        );
        // The other two classes are unaffected by the `defined` set.
        assert!(is_noise_symbol("local0", &defined));
        assert!(is_noise_symbol("com/demo/Greeter#greet().(name)", &defined));
    }

    #[test]
    fn noise_symbol_classification() {
        assert!(is_local_symbol("local0"));
        assert!(is_local_symbol("local42"));
        assert!(!is_local_symbol("locally")); // not a `local<digits>` symbol
        assert!(!is_local_symbol("a/Caller#call()."));
        assert!(is_parameter_descriptor("com/demo/Greeter#greet().(name)"));
        assert!(!is_parameter_descriptor("com/demo/Greeter#greet()."));
        assert!(!is_parameter_descriptor("com/demo/Greeter#"));
    }

    // A parameter descriptor and an anonymous local must not become nodes, refs,
    // or edges, mirroring the scip-reader filtering so scala stops leaking raw
    // `(name)` / `localN` symbols into the graph.
    #[test]
    fn parameter_and_local_symbols_are_dropped() {
        let docs = vec![TextDocument {
            uri: "Greeter.scala".to_string(),
            symbols: vec![
                sym("com/demo/Greeter#", KIND_CLASS),
                sym("com/demo/Greeter#greet().", KIND_METHOD),
                sym("com/demo/Greeter#greet().(name)", KIND_FIELD),
                sym("local0", KIND_FIELD),
            ],
            occurrences: vec![
                occ("com/demo/Greeter#", 0, 2),
                occ("com/demo/Greeter#greet().", 1, 2),
                // references to the parameter and to an anonymous local
                occ("com/demo/Greeter#greet().(name)", 1, 1),
                occ("local0", 1, 1),
            ],
        }];

        let resp = build_edges(&docs, "testcorpus", Path::new("/nonexistent"));

        // Only the class and the method are nodes; no parameter / local node.
        assert_eq!(resp.nodes.len(), 2);
        assert!(resp.nodes.iter().all(
            |n| !n.vname.signature.contains("(name)") && !n.vname.signature.contains("local0")
        ));
        // No ref or edge points at the dropped symbols.
        assert!(resp.refs.is_empty());
        assert!(resp.edges.iter().all(|e| e.kind != EdgeKind::RefCall));
    }
}
