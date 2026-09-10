//! Travsr Phase B: Swift structural analysis.
//!
//! Spawns the pre-built `swift-index-emitter` binary (from
//! `packages/swift-index-emitter`), which uses SwiftSyntax to walk all .swift
//! files and emit a JSON index of definitions and references. The JSON is parsed
//! here and converted into Travsr nodes and edges.
//!
//! Parse-level analysis only: all named declarations are accurate; static/type
//! call sites (UpperCase.method()) are resolved; instance method calls on
//! runtime-typed values are omitted until IndexStore integration is added.
//!
//! Build emitter (required once):
//!   cd packages/swift-index-emitter && swift build -c release
//!
//! Or set env var:
//!   TRAVSR_SWIFT_EMITTER=/path/to/swift-index-emitter
//!
//! ## `cargo build` does NOT build the emitter
//!
//! This crate is a spawner. The analyzer is the SwiftPM package above, and
//! nothing in the Cargo workspace builds it, so rebuilding the workspace after
//! editing `Sources/main.swift` leaves a new spawner driving the previously
//! installed emitter. Its output is still well formed, so the skew is invisible
//! at the result level. `check_emitter_version` makes it visible: it asks the
//! resolved emitter for `--version` once per process, logs the path and version
//! at info, and warns when the version does not match this crate's, or when the
//! emitter is old enough not to know the flag.
//!
//! Emitter location resolution order:
//!   1. $TRAVSR_SWIFT_EMITTER (explicit binary path)
//!   2. <binary-dir>/../../../packages/swift-index-emitter/.build/release/swift-index-emitter (dev/monorepo)
//!   3. <prefix>/bin/travsr-swift-index-emitter (installed)

use anyhow::Context as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use travsr_core::{Edge, EdgeKind, Language, Node, NodeId, ScipRef, VName};
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
    if let Ok(p) = std::env::var("TRAVSR_SWIFT_EMITTER") {
        let path = PathBuf::from(&p);
        tracing::debug!(
            path = %path.display(),
            exists = path.exists(),
            "emitter_path[1]: $TRAVSR_SWIFT_EMITTER"
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

    // 2. Dev/monorepo: target/{debug|release}/travsr-lang-swift
    //    → ../../packages/swift-index-emitter/.build/release/swift-index-emitter
    let dev = exe
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|root| {
            root.join("packages")
                .join("swift-index-emitter")
                .join(".build")
                .join("release")
                .join("swift-index-emitter")
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

    // 3. Installed: <prefix>/bin/travsr-swift-index-emitter (sibling of sidecar binary)
    let installed = exe
        .parent()
        .map(|bin| bin.join("travsr-swift-index-emitter"));
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

// ── Plugin ────────────────────────────────────────────────────────────────────

struct SwiftPhaseB;

impl Plugin for SwiftPhaseB {
    fn language(&self) -> Language {
        Language::Swift
    }

    fn extensions(&self) -> &[&str] {
        &["swift"]
    }

    fn supports_phase_b(&self) -> bool {
        let emitter = emitter_path();
        let supported = emitter.is_some();
        tracing::debug!(
            emitter = ?emitter,
            supports_phase_b = supported,
            "SwiftPhaseB::supports_phase_b"
        );
        supported
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        tracing::debug!(root = %req.root.display(), corpus = %req.corpus, "SwiftPhaseB::invoke_phase_b");
        match run_swift_emitter(&req.root, req.corpus.as_str()) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("swift emitter failed for {}: {e:#}", req.root.display());
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
/// build `packages/swift-index-emitter`. A developer who rebuilds the workspace
/// therefore gets a new Rust spawner talking to whatever emitter binary is
/// already installed at `~/.travsr/bin/travsr-swift-index-emitter`, with no
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
/// Runs once per resolved emitter path, so a repeat invoke does not re-spawn it.
fn check_emitter_version(emitter: &Path) -> Option<PluginDiagnostic> {
    // Caches the *verdict*, not just the fact of having run: the probe stays
    // once per emitter, but a repeat invoke still gets the diagnostic to attach
    // to its own response.
    //
    // Keyed on the emitter path, not process-global: `emitter_path()` resolves
    // per invoke and can legitimately change mid-process ($TRAVSR_SWIFT_EMITTER,
    // or a dev build appearing while the daemon keeps this sidecar warm). An
    // unkeyed cache then returned the first emitter's verdict, naming a binary
    // that is no longer the one being run.
    #[allow(clippy::type_complexity)]
    static CHECKED: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, Option<PluginDiagnostic>>>,
    > = std::sync::OnceLock::new();
    let cache = CHECKED.get_or_init(Default::default);
    if let Ok(map) = cache.lock() {
        if let Some(cached) = map.get(emitter) {
            return cached.clone();
        }
    }

    let mtime = std::fs::metadata(emitter)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());

    let reported = probe_version(emitter);

    // Always say which binary actually ran. Benchmarking against an emitter you
    // did not build is the failure this exists to prevent, and the path plus the
    // build identity is what makes that visible.
    tracing::info!(
        emitter = %emitter.display(),
        version = reported.as_deref().unwrap_or("unknown"),
        mtime_unix = mtime,
        sidecar_version = env!("CARGO_PKG_VERSION"),
        "swift emitter resolved"
    );

    let expected = env!("CARGO_PKG_VERSION");
    let diagnostic = match &reported {
        None => Some(PluginDiagnostic::warning(
            "emitter.version-unsupported",
            format!(
                "the swift index emitter at {} does not support `--version`, so it predates \
                 this sidecar (v{expected}) and its output may not reflect the current emitter \
                 source. `cargo build` does not rebuild it.",
                emitter.display()
            ),
        )),
        Some(line) if version_field(line) != expected => Some(PluginDiagnostic::warning(
            "emitter.version-mismatch",
            format!(
                "the swift index emitter at {} reports {line}, but this sidecar is v{expected}. \
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
            "swift emitter does not support `--version`, so it predates this \
             sidecar (v{expected}) and its output may not reflect the current \
             emitter source. `cargo build` does not rebuild it: run \
             `cd packages/swift-index-emitter && swift build -c release` and reinstall it to \
             ~/.travsr/bin/travsr-swift-index-emitter."
        ),
        Some(line) if version_field(&line) != expected => tracing::warn!(
            emitter = %emitter.display(),
            reported = %line,
            expected = %expected,
            "swift emitter version does not match this sidecar. Rebuild it with \
             `cd packages/swift-index-emitter && swift build -c release` and reinstall it to \
             ~/.travsr/bin/travsr-swift-index-emitter."
        ),
        Some(_) => {}
    }
    if let Ok(mut map) = cache.lock() {
        map.insert(emitter.to_path_buf(), diagnostic.clone());
    }
    diagnostic
}

/// The version field of a `--version` line: `"swift-index-emitter 0.4.2"` -> `"0.4.2"`.
///
/// Compared exactly. A `ends_with` test made `10.4.2` compare equal to `0.4.2`,
/// so the first release past a single-digit major would have silently stopped
/// reporting skew.
fn version_field(line: &str) -> &str {
    line.split_whitespace().last().unwrap_or(line)
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

fn run_swift_emitter(root: &Path, corpus: &str) -> anyhow::Result<InvokeResponse> {
    let emitter = emitter_path().context(
        "swift-index-emitter not found. Run \
         `cd packages/swift-index-emitter && swift build -c release` \
         or set $TRAVSR_SWIFT_EMITTER",
    )?;
    let version_diagnostic = check_emitter_version(&emitter);

    let scratch = tempfile::tempdir().context("failed to create temp dir")?;
    let output_path = scratch.path().join("index.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    tracing::debug!(
        emitter = %emitter.display(),
        root = %root.display(),
        output = %output_path.display(),
        "run_swift_emitter: launching swift-index-emitter"
    );

    let mut child = std::process::Command::new(&emitter)
        .arg(root)
        .arg(&output_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
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
        match child.try_wait().context("polling swift emitter")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_reader.take().and_then(|h| h.join().ok());
                anyhow::bail!("swift emitter timed out after {TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    };

    let stderr_buf = stderr_reader
        .take()
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    tracing::debug!(exit_code = %status, "run_swift_emitter: subprocess exited");
    if !stderr_buf.is_empty() {
        tracing::debug!("run_swift_emitter stderr:\n{stderr_buf}");
    }

    anyhow::ensure!(
        status.success(),
        "swift emitter exited with {status}: {stderr_buf}"
    );

    // A version-skewed emitter still produces a usable index, so this rides out
    // on the response rather than failing the run. Attaching it here is the point:
    // the host only echoes sidecar stderr when a run yields zero nodes, and a
    // stale emitter yields plenty, just not of the current source.
    let mut resp = parse_emitter_output(&output_path, corpus, root)?;
    resp.diagnostics.extend(version_diagnostic);
    Ok(resp)
}

// ── JSON parsing ──────────────────────────────────────────────────────────────

fn parse_emitter_output(
    json_path: &Path,
    corpus: &str,
    repo_root: &Path,
) -> anyhow::Result<InvokeResponse> {
    // #813: one read per referenced file, shared across all documents.
    let mut src_cache = travsr_lang_scip_reader::SourceCache::new();
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

    let root: serde_json::Value = serde_json::from_slice(&bytes).context("parsing emitter JSON")?;
    // #813: the emitter declares what its `col` counts. An older emitter that
    // declares nothing stays on the conservative encoding-agnostic path.
    let col_unit = travsr_lang_scip_reader::ColUnit::parse(root["col_unit"].as_str());

    let docs = root["documents"]
        .as_array()
        .context("missing 'documents'")?;

    tracing::debug!(
        doc_count = docs.len(),
        "parse_emitter_output: documents found"
    );

    let lang_str = Language::Swift.as_str();

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

    // Pass 2: resolve references → RefCall edges; inheritances → IsImplementation edges.
    let mut edges: Vec<Edge> = Vec::new();
    // #299 S1: occurrence records (path:line) so the daemon populates edge_sites
    // and find_references works, since the emitter already gives us each ref's line.
    let mut refs_out: Vec<ScipRef> = Vec::new();

    for doc in docs {
        let path = doc["path"].as_str().unwrap_or("");
        let file_id = VName::new(corpus, "", path, lang_str, "file").id();

        // Call-site references → RefCall edges (file node → definition node).
        if let Some(refs) = doc["references"].as_array() {
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
                    // R6: when the reference carries a line, emit only the
                    // ScipRef. The daemon's write_scip_attributed_batch re-homes
                    // it to the enclosing function and records the ref/call edge
                    // plus an edge_site. Also emitting the file-granular edge
                    // would add a spurious `file -> callee` duplicate: edges are
                    // keyed ON CONFLICT(src, dst, kind), and the file src differs
                    // from the enclosing-fn src, so both rows survive. The
                    // file-granular edge is kept ONLY as a fallback for a
                    // line-less reference, where no ScipRef is possible.
                    // Emitter lines are 1-based (definitions store them as-is).
                    if let Some(line) = r["line"].as_u64() {
                        refs_out.push(ScipRef {
                            caller_path: path.to_string(),
                            caller_line: line as u32,
                            callee_id: dst_id,
                            // is_call (#650, #830): the emitter marks a
                            // type-position use (annotation, parameter or
                            // return type, generic argument, conformance, the
                            // receiver of a qualified access) with
                            // `"is_call": false` so it records an occurrence
                            // for find_references without becoming a ref/call
                            // edge. The key is written only for those, so a
                            // call site and any JSON from an emitter built
                            // before the field existed both read `true`.
                            is_call: r["is_call"].as_bool().unwrap_or(true),
                            // #813: the emitter reports a column and declares its
                            // unit, so this converts rather than guessing. An
                            // emitter too old to declare one falls back to the
                            // encoding-agnostic window, and anything unresolvable
                            // stays `None` for the daemon to name-search.
                            caller_col: r["col"].as_i64().and_then(|c| {
                                src_cache.col_in(
                                    &repo_root.join(path),
                                    line as u32,
                                    i32::try_from(c).ok()?,
                                    col_unit,
                                )
                            }),
                        });
                    } else {
                        edges.push(Edge::new(file_id, dst_id, EdgeKind::RefCall));
                    }
                } else {
                    tracing::debug!(
                        sym,
                        "parse_emitter_output: ref symbol not in def_ids, skipped"
                    );
                }
            }
        }

        // Inheritance / protocol conformance → IsImplementation edges (child → parent).
        // Absent in JSON produced by older emitter versions, silently skipped.
        if let Some(inhs) = doc["inheritances"].as_array() {
            tracing::debug!(
                path,
                inh_count = inhs.len(),
                "parse_emitter_output: document inheritances"
            );
            for inh in inhs {
                let child_sym = inh["child"].as_str().unwrap_or("");
                let parent_sym = inh["parent"].as_str().unwrap_or("");
                if child_sym.is_empty() || parent_sym.is_empty() {
                    continue;
                }
                match (def_ids.get(child_sym), def_ids.get(parent_sym)) {
                    (Some(&child_id), Some(&parent_id)) => {
                        edges.push(Edge::new(child_id, parent_id, EdgeKind::IsImplementation));
                    }
                    _ => {
                        tracing::debug!(
                            child_sym,
                            parent_sym,
                            "parse_emitter_output: inheritance parent not in def_ids, skipped"
                        );
                    }
                }
            }
        }
    }

    tracing::info!(
        nodes = nodes.len(),
        edges = edges.len(),
        "swift emitter ingestion complete"
    );

    Ok(InvokeResponse {
        diagnostics: Vec::new(),
        nodes,
        edges,
        refs: refs_out,
        unresolved_calls: Vec::new(),
    })
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_swift=info".parse().unwrap()),
        )
        .init();

    run_plugin(SwiftPhaseB);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> InvokeResponse {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.json");
        std::fs::write(&path, json).expect("write canned JSON");
        parse_emitter_output(&path, "testcorpus", Path::new("/nonexistent")).expect("parse")
    }

    /// #813: the emitter declares `col_unit: utf8`, so a column past a
    /// non-ASCII prefix must be carried through as the byte column rather than
    /// abstaining the way an undeclared unit does. Uses a real source file,
    /// since the conversion reads it: the other tests pass `/nonexistent` and
    /// therefore never exercise this path.
    #[test]
    fn declared_utf8_col_survives_a_non_ascii_prefix() {
        let repo = tempfile::tempdir().expect("tempdir");
        // `let s = "caf<e-acute>"; svc.charge()` - `charge` starts at byte 21
        // but at UTF-16 code unit 20, so a reader that guessed the unit would
        // land one character off, and one that abstained would drop it.
        let line = "let s = \"caf\u{e9}\"; svc.charge()";
        assert_eq!(line.find("charge"), Some(21));
        std::fs::write(repo.path().join("Caller.swift"), format!("{line}\n")).expect("write");

        let dir = tempfile::tempdir().expect("tempdir");
        let json_path = dir.path().join("out.json");
        std::fs::write(
            &json_path,
            r#"{"version":1,"col_unit":"utf8","documents":[
                {"path":"Svc.swift","definitions":[
                    {"symbol":"swift::Svc.charge","kind":"function","line":1,"end_line":3}
                ],"references":[],"inheritances":[]},
                {"path":"Caller.swift","definitions":[],
                 "references":[{"symbol":"swift::Svc.charge","line":1,"col":21}],
                 "inheritances":[]}
            ]}"#,
        )
        .expect("write canned JSON");

        let resp = parse_emitter_output(&json_path, "testcorpus", repo.path()).expect("parse");
        assert_eq!(resp.refs.len(), 1);
        assert_eq!(resp.refs[0].caller_line, 1);
        assert_eq!(resp.refs[0].caller_col, Some(21));
    }

    /// An emitter binary predating #813 declares no unit and reports no column,
    /// so nothing is derived and the daemon name-searches the line as before.
    #[test]
    fn output_without_col_leaves_caller_col_unset() {
        let resp = parse(
            r#"{"version":1,"documents":[
                {"path":"ClassA.swift","definitions":[
                    {"symbol":"swift::ClassA","kind":"class","line":1,"end_line":10}
                ],"references":[],"inheritances":[]},
                {"path":"ClassB.swift","definitions":[],
                 "references":[{"symbol":"swift::ClassA","line":7}],"inheritances":[]}
            ]}"#,
        );
        assert_eq!(resp.refs.len(), 1);
        assert_eq!(resp.refs[0].caller_col, None);
    }

    fn node_id(path: &str, sym: &str) -> NodeId {
        VName::new("testcorpus", "", path, Language::Swift.as_str(), sym).id()
    }

    #[test]
    fn constructor_call_resolves_to_class_node() {
        // #449: the emitter targets the type itself (`swift::ClassA`), not a
        // synthetic `.init` member, so `find_references("ClassA")` must see
        // constructor call sites directly, regardless of whether the type
        // declares an explicit initializer.
        let resp = parse(
            r#"{"version":1,"documents":[
                {"path":"ClassA.swift","definitions":[
                    {"symbol":"swift::ClassA","kind":"class","line":1,"end_line":10},
                    {"symbol":"swift::ClassA.init","kind":"function","line":2,"end_line":4}
                ],"references":[],"inheritances":[]},
                {"path":"ClassB.swift","definitions":[],
                 "references":[{"symbol":"swift::ClassA","line":7}],"inheritances":[]}
            ]}"#,
        );
        let class_id = node_id("ClassA.swift", "swift::ClassA");
        // R6: a ranged reference is carried solely by the ScipRef (which the
        // daemon attributes to the enclosing function). No file-granular
        // RefCall edge is emitted, so no spurious `file -> class` duplicate.
        assert!(!resp.edges.iter().any(|e| e.kind == EdgeKind::RefCall));
        assert_eq!(resp.refs.len(), 1);
        assert_eq!(resp.refs[0].callee_id, class_id);
        assert_eq!(resp.refs[0].caller_path, "ClassB.swift");
        assert_eq!(resp.refs[0].caller_line, 7);
        // travsr-lang#17: the ref must be flagged as a call so the daemon derives
        // a call edge (get_callers / blast radius). A ref with is_call=false is
        // recorded for find_references only and yields no caller edge. The
        // emitter omits `is_call` on a call site, so this also pins the default.
        assert!(resp.refs[0].is_call, "swift call-site ref must set is_call");
    }

    // #830: a type-position use carries `"is_call": false` and must reach the
    // daemon as an occurrence-only ScipRef. Without this the emitter's new
    // annotation / parameter / return / generic / conformance references would
    // each become a `ref/call` edge, and `get_callers(ClassA)` would list every
    // declaration site that merely names the type as a caller.
    #[test]
    fn type_position_ref_is_not_a_call() {
        let resp = parse(
            r#"{"version":1,"documents":[
                {"path":"ClassA.swift","definitions":[
                    {"symbol":"swift::ClassA","kind":"class","line":1,"end_line":10}
                ],"references":[],"inheritances":[]},
                {"path":"ClassB.swift","definitions":[],
                 "references":[
                    {"symbol":"swift::ClassA","line":7,"is_call":false},
                    {"symbol":"swift::ClassA","line":9}
                 ],"inheritances":[]}
            ]}"#,
        );
        assert_eq!(resp.refs.len(), 2);
        // The annotation at line 7 is an occurrence only.
        assert_eq!(resp.refs[0].caller_line, 7);
        assert!(
            !resp.refs[0].is_call,
            "type-position ref must not be a call"
        );
        // The call site at line 9 omits the key and keeps the `true` default,
        // which is also what JSON from a pre-#830 emitter binary looks like.
        assert_eq!(resp.refs[1].caller_line, 9);
        assert!(
            resp.refs[1].is_call,
            "call-site ref must default to is_call"
        );
    }

    #[test]
    fn lineless_ref_emits_fallback_file_edge() {
        // R6: a reference with no line cannot become a ScipRef, so the
        // file-granular RefCall edge survives as the only fallback.
        let resp = parse(
            r#"{"version":1,"documents":[
                {"path":"ClassA.swift","definitions":[
                    {"symbol":"swift::ClassA","kind":"class","line":1,"end_line":10}
                ],"references":[],"inheritances":[]},
                {"path":"ClassB.swift","definitions":[],
                 "references":[{"symbol":"swift::ClassA"}],"inheritances":[]}
            ]}"#,
        );
        let class_id = node_id("ClassA.swift", "swift::ClassA");
        let file_id = VName::new(
            "testcorpus",
            "",
            "ClassB.swift",
            Language::Swift.as_str(),
            "file",
        )
        .id();
        assert!(resp
            .edges
            .iter()
            .any(|e| e.src == file_id && e.dst == class_id && e.kind == EdgeKind::RefCall));
        assert!(resp.refs.is_empty());
    }

    #[test]
    fn dotted_static_access_resolves_to_field_node() {
        let resp = parse(
            r#"{"version":1,"documents":[
                {"path":"ClassC.swift","definitions":[
                    {"symbol":"swift::ClassC","kind":"class","line":1,"end_line":8},
                    {"symbol":"swift::ClassC.shared","kind":"field","line":2,"end_line":2}
                ],"references":[],"inheritances":[]},
                {"path":"Caller.swift","definitions":[],
                 "references":[{"symbol":"swift::ClassC.shared","line":4}],"inheritances":[]}
            ]}"#,
        );
        let shared_id = node_id("ClassC.swift", "swift::ClassC.shared");
        // R6: ranged ref → ScipRef only, no file-granular RefCall duplicate.
        assert!(!resp.edges.iter().any(|e| e.kind == EdgeKind::RefCall));
        assert_eq!(resp.refs.len(), 1);
        assert_eq!(resp.refs[0].callee_id, shared_id);
    }

    #[test]
    fn unknown_ref_symbol_is_skipped() {
        let resp = parse(
            r#"{"version":1,"documents":[
                {"path":"A.swift","definitions":[
                    {"symbol":"swift::A","kind":"class","line":1,"end_line":2}
                ],"references":[{"symbol":"swift::Nowhere.method","line":2}],"inheritances":[]}
            ]}"#,
        );
        assert!(resp.edges.is_empty());
        assert!(resp.refs.is_empty());
    }
}
