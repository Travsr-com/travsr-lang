//! Travsr Phase B: Dart semantic analysis.
//!
//! Spawns the pre-built `travsr-dart-index-emitter` binary (compiled from
//! `packages/dart-scip-emitter/bin/emit.dart` via `dart compile exe`) and
//! returns call/reference edges to the Travsr daemon via the plugin protocol.
//!
//! Emitter location resolution order:
//!   1. $TRAVSR_DART_EMITTER
//!   2. packages/dart-scip-emitter/bin/emit-native (dev)
//!   3. <prefix>/bin/travsr-dart-index-emitter (installed)
//!
//! ## `cargo build` does NOT build the emitter
//!
//! This crate is a spawner. The analyzer is the Dart package above, built with
//! `cd packages/dart-scip-emitter && dart compile exe bin/emit.dart -o
//! bin/emit-native`, and nothing in the Cargo workspace builds it. Rebuilding
//! the workspace after editing `bin/emit.dart` therefore leaves a new spawner
//! driving the previously installed emitter, whose output is still well formed,
//! so the skew is invisible at the result level. `check_emitter_version` makes
//! it visible: it asks the resolved emitter for `--version` once per process,
//! logs the path and version at info, and warns when the version does not match
//! this crate's, or when the emitter is old enough not to know the flag.

use anyhow::Context as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use travsr_core::{Edge, EdgeKind, Language, Node, NodeId, VName};
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
    PluginDiagnostic,
};

const TIMEOUT_SECS: u64 = 300;
/// Budget for the `--version` handshake. Short on purpose: it answers in
/// milliseconds when it answers at all, and it must never be able to consume
/// the invoke budget the host watchdogs at TIMEOUT_SECS.
const VERSION_PROBE_SECS: u64 = 10;
/// Cap on how much of the probe's stdout is read. A version line is one line.
const VERSION_LINE_MAX_BYTES: u64 = 256;

// ── Emitter discovery ─────────────────────────────────────────────────────────

fn emitter_path() -> Option<PathBuf> {
    // 1. Explicit env var override.
    if let Ok(p) = std::env::var("TRAVSR_DART_EMITTER") {
        let path = PathBuf::from(&p);
        tracing::debug!(
            path = %path.display(),
            exists = path.exists(),
            "emitter_path[1]: $TRAVSR_DART_EMITTER"
        );
        if path.exists() {
            return Some(path);
        }
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(err) => {
            tracing::debug!("emitter_path: current_exe() failed: {err}");
            return None;
        }
    };
    tracing::debug!(exe = %exe.display(), "emitter_path: current_exe");

    // 2. Dev/monorepo: target/{debug|release}/travsr-lang-dart
    //    → ../../packages/dart-scip-emitter/bin/emit-native (pre-compiled AOT binary)
    let dev = exe
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|root| {
            root.join("packages")
                .join("dart-scip-emitter")
                .join("bin")
                .join("emit-native")
        });
    if let Some(ref path) = dev {
        tracing::debug!(
            path = %path.display(),
            exists = path.exists(),
            "emitter_path[2]: dev monorepo path"
        );
        if path.exists() {
            return Some(path.clone());
        }
    }

    // 3. Installed: <prefix>/bin/travsr-dart-index-emitter (sibling of sidecar binary)
    let installed = exe
        .parent()
        .map(|bin| bin.join("travsr-dart-index-emitter"));
    if let Some(ref path) = installed {
        tracing::debug!(
            path = %path.display(),
            exists = path.exists(),
            "emitter_path[3]: installed sibling path"
        );
        if path.exists() {
            return Some(path.clone());
        }
    }

    tracing::debug!("emitter_path: not found at any location");
    None
}

// ── Dart SDK discovery ────────────────────────────────────────────────────────

/// Locate the Dart SDK root, so the emitter can be told where it lives.
///
/// The emitter is AOT-compiled, so package:analyzer's default SDK detection
/// resolves relative to `Platform.resolvedExecutable`, which is the emitter's
/// own install directory and not an SDK. `emit.dart:78` reads `DART_SDK` and
/// hands it to `AnalysisContextCollection` as `sdkPath`; unset or empty falls
/// back to that broken auto-detect (`emit.dart:82`), which leaves Dart Phase B
/// empty. A wrong path would be passed straight through, so this returns `Some`
/// only for a directory that validates as an SDK root.
///
/// The validation marker and the search order match travsr's own direct-spawn
/// path in `crates/travsr-analysis/src/phase_b_dart.rs`.
fn detect_dart_sdk() -> Option<PathBuf> {
    let is_sdk_root = |sdk: &Path| sdk.join("lib/_internal/allowed_experiments.json").exists();

    // An explicit override wins, but only when it validates.
    if let Ok(p) = std::env::var("DART_SDK") {
        let path = PathBuf::from(&p);
        if !p.is_empty() && is_sdk_root(&path) {
            return Some(path);
        }
    }

    let dart_name = format!("dart{}", std::env::consts::EXE_SUFFIX);
    let path_var = std::env::var_os("PATH")?;
    let mut first_dart: Option<PathBuf> = None;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(&dart_name);
        if !candidate.exists() {
            continue;
        }
        if first_dart.is_none() {
            first_dart = Some(candidate.clone());
        }
        // `<sdk>/bin/dart` gives `<sdk>`. Canonicalize first to follow symlinks
        // (Homebrew: /opt/homebrew/bin/dart -> .../dart-sdk/<ver>/libexec/bin/dart).
        let real = std::fs::canonicalize(&candidate).unwrap_or(candidate);
        if let Some(sdk) = real.parent().and_then(|p| p.parent()) {
            if is_sdk_root(sdk) {
                return Some(sdk.to_path_buf());
            }
        }
    }

    // Version-manager shims (asdf / mise / volta) are shell scripts rather than
    // symlinks into `<sdk>/bin/dart`, so the parent walk above lands on the shim
    // directory and never validates. Ask the `dart` tool itself instead.
    if let Some(dart) = first_dart {
        if let Some(sdk) = sdk_root_via_dart(&dart) {
            if is_sdk_root(&sdk) {
                return Some(sdk);
            }
        }
    }

    tracing::warn!(
        "Dart SDK not found on PATH; the emitter will fall back to the SDK \
         auto-detection that does not work for an AOT binary. Set DART_SDK to \
         the SDK root (the directory containing lib/_internal) to enable \
         cross-file Dart analysis."
    );
    None
}

/// Ask the `dart` tool where its own SDK lives, for the shim case above.
///
/// No `dart` flag prints the SDK path, so this runs a one-line program that
/// prints the `<sdk>` of `Platform.resolvedExecutable` (`<sdk>/bin/dart`). A
/// shim `exec`s the real binary, so `resolvedExecutable` is the true SDK binary
/// and not the shim script. Any failure returns `None`, so a broken shim
/// degrades to the warning above rather than to an invalid `sdkPath`.
fn sdk_root_via_dart(dart: &Path) -> Option<PathBuf> {
    let scratch = tempfile::tempdir().ok()?;
    let prog = scratch.path().join("travsr_sdk_probe.dart");
    std::fs::write(
        &prog,
        "import 'dart:io';\n\
         void main() => print(File(Platform.resolvedExecutable).parent.parent.path);\n",
    )
    .ok()?;
    let out = std::process::Command::new(dart).arg(&prog).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

// ── Plugin ────────────────────────────────────────────────────────────────────

struct DartPhaseB;

impl Plugin for DartPhaseB {
    fn language(&self) -> Language {
        Language::Dart
    }

    fn extensions(&self) -> &[&str] {
        &["dart"]
    }

    fn supports_phase_b(&self) -> bool {
        let emitter = emitter_path();
        let supported = emitter.is_some();
        tracing::debug!(
            emitter = ?emitter,
            supports_phase_b = supported,
            "DartPhaseB::supports_phase_b"
        );
        supported
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        tracing::debug!(root = %req.root.display(), corpus = %req.corpus, "DartPhaseB::invoke_phase_b");
        match run_dart_emitter(&req.root, req.corpus.as_str()) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("dart emitter failed for {}: {e:#}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

// ── Emitter version handshake ─────────────────────────────────────────────────

/// Ask the resolved emitter what it is, then log it and warn if it is not the
/// build this sidecar expects.
///
/// The trap this closes: `cargo build --release` at the repo root does NOT
/// build `packages/dart-scip-emitter`. A developer who rebuilds the workspace
/// therefore gets a new Rust spawner talking to whatever emitter binary is
/// already installed at `~/.travsr/bin/travsr-dart-index-emitter`, with no
/// signal that the pair is skewed. A stale emitter emits well-formed output,
/// so the only symptom is results that quietly do not reflect the source.
///
/// Two signals, both WARN and never fatal (a version-skewed emitter still
/// produces a usable index, and refusing to run would take Phase B away from
/// someone whose only problem is a missing rebuild):
///   1. The emitter does not understand `--version` at all. It predates this
///      handshake, so it is definitely older than this sidecar.
///   2. It reports a version other than this sidecar's own package version.
///
/// Runs once per resolved emitter path and build time, so a repeat invoke does
/// not re-spawn it, and a rebuild is probed again.
fn check_emitter_version(emitter: &Path) -> Option<PluginDiagnostic> {
    // Caches the *verdict*, not just the fact of having run: the probe stays
    // once per emitter, but a repeat invoke still gets the diagnostic to attach
    // to its own response.
    //
    // Keyed on the emitter path AND its build time, not process-global:
    // `emitter_path()` resolves per invoke and can legitimately change
    // mid-process ($TRAVSR_DART_EMITTER, or a dev build appearing while the daemon
    // keeps this sidecar warm). An unkeyed cache then returned the first
    // emitter's verdict, naming a binary that is no longer the one being run.
    //
    // The build time is in the key because rebuilding in place, over the same
    // path, is the usual way an emitter changes. Keyed on the path alone, a
    // developer who rebuilt and reinstalled kept being told about a skew that
    // was already fixed, and a newly stale emitter kept being reported clean,
    // for as long as the daemon held this sidecar warm.
    #[allow(clippy::type_complexity)]
    static CHECKED: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<(PathBuf, Option<u64>), Option<PluginDiagnostic>>,
        >,
    > = std::sync::OnceLock::new();

    let mtime = std::fs::metadata(emitter)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let key = (emitter.to_path_buf(), mtime);

    let cache = CHECKED.get_or_init(Default::default);
    if let Ok(map) = cache.lock() {
        if let Some(cached) = map.get(&key) {
            return cached.clone();
        }
    }

    let reported = probe_version(emitter);

    // Always say which binary actually ran. Benchmarking against an emitter you
    // did not build is the failure this exists to prevent, and the path plus the
    // build identity is what makes that visible.
    tracing::info!(
        emitter = %emitter.display(),
        version = reported.as_deref().unwrap_or("unknown"),
        mtime_unix = mtime,
        sidecar_version = env!("CARGO_PKG_VERSION"),
        "dart emitter resolved"
    );

    let expected = env!("CARGO_PKG_VERSION");
    let diagnostic = match &reported {
        None => Some(PluginDiagnostic::warning(
            "emitter.version-unsupported",
            format!(
                "the dart index emitter at {} does not support `--version`, so it predates \
                 this sidecar (v{expected}) and its output may not reflect the current emitter \
                 source. `cargo build` does not rebuild it.",
                emitter.display()
            ),
        )),
        Some(line) if version_field(line) != expected => Some(PluginDiagnostic::warning(
            "emitter.version-mismatch",
            format!(
                "the dart index emitter at {} reports {line}, but this sidecar is v{expected}. \
                 Its output may not reflect the current emitter source; `cargo build` does not \
                 rebuild it.",
                emitter.display()
            ),
        )),
        Some(_) => None,
    };
    match reported {
        None => tracing::warn!(
            emitter = %emitter.display(),
            "dart emitter does not support `--version`, so it predates this \
             sidecar (v{expected}) and its output may not reflect the current \
             emitter source. `cargo build` does not rebuild it: run \
             `cd packages/dart-scip-emitter && dart compile exe bin/emit.dart -o bin/emit-native` and reinstall it to \
             ~/.travsr/bin/travsr-dart-index-emitter."
        ),
        Some(line) if version_field(&line) != expected => tracing::warn!(
            emitter = %emitter.display(),
            reported = %line,
            expected = %expected,
            "dart emitter version does not match this sidecar. Rebuild it with \
             `cd packages/dart-scip-emitter && dart compile exe bin/emit.dart -o bin/emit-native` and reinstall it to \
             ~/.travsr/bin/travsr-dart-index-emitter."
        ),
        Some(_) => {}
    }
    if let Ok(mut map) = cache.lock() {
        map.insert(key, diagnostic.clone());
    }
    diagnostic
}

/// The version field of a `--version` line: `"dart-index-emitter 0.4.2"` -> `"0.4.2"`.
///
/// Compared exactly. A `ends_with` test made `10.4.2` compare equal to `0.4.2`,
/// so the first release past a single-digit major would have silently stopped
/// reporting skew.
///
/// Only the first line is read. The emitter prints the version line and nothing
/// else, but taking the last token of the whole output made any extra line a
/// bogus version and a skew warning that was not real. (CI's handshake pipes
/// through `tail -n 1` instead, because there it is build output that precedes
/// the line; here the emitter's stderr is already discarded.)
fn version_field(line: &str) -> &str {
    let first = line.lines().next().unwrap_or(line).trim();
    first.split_whitespace().last().unwrap_or(first)
}

/// Run `<emitter> --version` under its own short deadline and return the
/// trimmed line it printed.
///
/// `Command::output()` waits for exit with no bound and reads both pipes
/// unbounded, so an emitter that hangs on startup held this sidecar until the
/// plugin host SIGKILLed it at 300s. The language was then lost to a timeout
/// that gave no hint a version probe caused it. A version line is one short
/// line, so the read is capped too.
fn probe_version(emitter: &Path) -> Option<String> {
    let mut child = std::process::Command::new(emitter)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let mut reader = child.stdout.take().map(|out| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = out.take(VERSION_LINE_MAX_BYTES).read_to_string(&mut buf);
            buf
        })
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(VERSION_PROBE_SECS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.take().and_then(|h| h.join().ok());
                tracing::warn!(
                    emitter = %emitter.display(),
                    "`--version` did not answer within {VERSION_PROBE_SECS}s; treating this \
                     emitter as one that does not support the flag"
                );
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };

    let out = reader
        .take()
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    if !status.success() {
        return None;
    }
    let line = out.trim().to_string();
    (!line.is_empty()).then_some(line)
}

// ── Emitter invocation ────────────────────────────────────────────────────────

fn run_dart_emitter(root: &Path, corpus: &str) -> anyhow::Result<InvokeResponse> {
    let emitter = emitter_path().context(
        "travsr-dart-index-emitter not found. \
         set $TRAVSR_DART_EMITTER or run `travsr lang install dart`",
    )?;
    let version_diagnostic = check_emitter_version(&emitter);

    let scratch = tempfile::tempdir().context("failed to create temp dir")?;
    let output_path = scratch.path().join("index.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    tracing::debug!(
        emitter = %emitter.display(),
        root = %root.display(),
        output = %output_path.display(),
        "run_dart_emitter: launching travsr-dart-index-emitter"
    );

    let mut command = std::process::Command::new(&emitter);
    command
        .arg(root)
        .arg(&output_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    // Point the emitter at the real SDK. It reads `DART_SDK` and passes it to
    // the analyzer as `sdkPath` (`emit.dart:78`); without it the AOT binary
    // auto-detects relative to its own path, fails to read the SDK, and Dart
    // Phase B comes back empty. Set only when detection validated an SDK root:
    // an empty value is ignored by the emitter, but a wrong one is not.
    if let Some(sdk) = detect_dart_sdk() {
        tracing::debug!(sdk = %sdk.display(), "run_dart_emitter: DART_SDK");
        command.env("DART_SDK", sdk);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {}", emitter.display()))?;

    // Drain stderr on a reader thread so the emitter never blocks on a full pipe
    // while we poll for exit. It is piped but was read only after the loop, so
    // an emitter writing more than a pipe buffer (~64KB) of diagnostics blocked
    // forever and burned the whole invoke budget as a spurious timeout. Same
    // hazard, and the same fix, as in the PHP sidecar.
    let mut stderr_reader = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = err.read_to_string(&mut buf);
            buf
        })
    });

    let status = loop {
        match child.try_wait().context("polling dart emitter")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_reader.take().and_then(|h| h.join().ok());
                anyhow::bail!("dart-index-emitter timed out after {TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    };

    let stderr_buf = stderr_reader
        .take()
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    tracing::debug!(exit_code = %status, "run_dart_emitter: subprocess exited");
    if !stderr_buf.is_empty() {
        tracing::debug!("run_dart_emitter stderr:\n{stderr_buf}");
    }

    anyhow::ensure!(
        status.success(),
        "dart emitter exited with {status}: {stderr_buf}"
    );

    // A version-skewed emitter still produces a usable index, so this rides out
    // on the response rather than failing the run. Attaching it here is the point:
    // the host only echoes sidecar stderr when a run yields zero nodes, and a
    // stale emitter yields plenty, just not of the current source.
    let mut resp = parse_emitter_output(&output_path, corpus)?;
    resp.diagnostics.extend(version_diagnostic);
    Ok(resp)
}

// ── JSON parsing ──────────────────────────────────────────────────────────────

fn parse_emitter_output(json_path: &Path, corpus: &str) -> anyhow::Result<InvokeResponse> {
    let bytes = std::fs::read(json_path)
        .with_context(|| format!("reading emitter output {}", json_path.display()))?;

    tracing::debug!(
        path = %json_path.display(),
        bytes = bytes.len(),
        "parse_emitter_output: read output file"
    );

    if bytes.is_empty() {
        tracing::debug!("parse_emitter_output: output file is empty, returning default");
        return Ok(InvokeResponse::default());
    }

    let root_val: serde_json::Value =
        serde_json::from_slice(&bytes).context("parsing emitter JSON")?;

    let docs = root_val["documents"]
        .as_array()
        .context("missing 'documents'")?;

    tracing::debug!(
        doc_count = docs.len(),
        "parse_emitter_output: documents found"
    );

    let lang_str = Language::Dart.as_str();

    // Pass 1: build symbol → NodeId map from all definitions.
    let mut def_ids: std::collections::HashMap<String, NodeId> = std::collections::HashMap::new();
    let mut nodes: Vec<Node> = Vec::new();

    for doc in docs {
        let path = doc["path"].as_str().unwrap_or("");
        let defs = match doc["definitions"].as_array() {
            Some(a) => a,
            None => continue,
        };
        tracing::debug!(
            path,
            def_count = defs.len(),
            "parse_emitter_output: document defs"
        );
        for d in defs {
            let sym = d["symbol"].as_str().unwrap_or("");
            let kind = d["kind"].as_str().unwrap_or("definition");
            let line = d["line"].as_u64().unwrap_or(0) as u32;
            if sym.is_empty() {
                continue;
            }
            let vname = VName::new(corpus, "", path, lang_str, sym);
            let node_id = vname.id();
            def_ids.insert(sym.to_string(), node_id);
            let mut node = Node::new(vname, kind).with_line(line);
            if let Some(el) = d["end_line"].as_u64() {
                node = node.with_end_line(el as u32);
            }
            nodes.push(node);
        }
    }

    // Pass 2: resolve references → emit RefCall edges.
    let mut edges: Vec<Edge> = Vec::new();

    for doc in docs {
        let path = doc["path"].as_str().unwrap_or("");
        let refs = match doc["references"].as_array() {
            Some(a) => a,
            None => continue,
        };
        let file_id = VName::new(corpus, "", path, lang_str, "file").id();

        tracing::debug!(
            path,
            ref_count = refs.len(),
            "parse_emitter_output: document refs"
        );
        for r in refs {
            let sym = r["symbol"].as_str().unwrap_or("");
            if sym.is_empty() {
                continue;
            }
            if let Some(&dst_id) = def_ids.get(sym) {
                edges.push(Edge::new(file_id, dst_id, EdgeKind::RefCall));
            } else {
                tracing::debug!(
                    sym,
                    "parse_emitter_output: ref symbol not in def_ids, skipped"
                );
            }
        }
    }

    tracing::info!(
        nodes = nodes.len(),
        edges = edges.len(),
        "dart emitter ingestion complete"
    );

    Ok(InvokeResponse {
        diagnostics: Vec::new(),
        nodes,
        edges,
        ..Default::default()
    })
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_dart=info".parse().unwrap()),
        )
        .init();

    run_plugin(DartPhaseB);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The last token of the LAST line used to be taken, so an emitter that
    /// printed anything after its version line reported a bogus version and a
    /// skew that was not real.
    #[test]
    fn version_field_reads_only_the_first_line() {
        assert_eq!(version_field("dart-index-emitter 0.4.2"), "0.4.2");
        assert_eq!(
            version_field("dart-index-emitter 0.4.2\nwarning: something else"),
            "0.4.2"
        );
    }
}
