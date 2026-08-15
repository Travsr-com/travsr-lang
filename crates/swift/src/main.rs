//! Travsr Phase B — Swift structural analysis.
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
};

const TIMEOUT_SECS: u64 = 300;

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

// ── Emitter invocation ────────────────────────────────────────────────────────

fn run_swift_emitter(root: &Path, corpus: &str) -> anyhow::Result<InvokeResponse> {
    let emitter = emitter_path().context(
        "swift-index-emitter not found — run \
         `cd packages/swift-index-emitter && swift build -c release` \
         or set $TRAVSR_SWIFT_EMITTER",
    )?;

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

    let status = loop {
        match child.try_wait().context("polling swift emitter")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("swift emitter timed out after {TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    };

    let mut stderr_buf = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr_buf);
    }

    tracing::debug!(exit_code = %status, "run_swift_emitter: subprocess exited");
    if !stderr_buf.is_empty() {
        tracing::debug!("run_swift_emitter stderr:\n{stderr_buf}");
    }

    anyhow::ensure!(
        status.success(),
        "swift emitter exited with {status}: {stderr_buf}"
    );

    parse_emitter_output(&output_path, corpus)
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
        tracing::debug!("parse_emitter_output: output file is empty — returning default");
        return Ok(InvokeResponse::default());
    }

    let root: serde_json::Value = serde_json::from_slice(&bytes).context("parsing emitter JSON")?;

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
    // and find_references works — the emitter already gives us each ref's line.
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
                            // is_call (#650): no call/non-call signal available
                            // here; preserve prior behavior / wire default.
                            is_call: true,
                        });
                    } else {
                        edges.push(Edge::new(file_id, dst_id, EdgeKind::RefCall));
                    }
                } else {
                    tracing::debug!(
                        sym,
                        "parse_emitter_output: ref symbol not in def_ids — skipped"
                    );
                }
            }
        }

        // Inheritance / protocol conformance → IsImplementation edges (child → parent).
        // Absent in JSON produced by older emitter versions — silently skipped.
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
                            "parse_emitter_output: inheritance parent not in def_ids — skipped"
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
        parse_emitter_output(&path, "testcorpus").expect("parse")
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
        // recorded for find_references only and yields no caller edge.
        assert!(resp.refs[0].is_call, "swift call-site ref must set is_call");
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
