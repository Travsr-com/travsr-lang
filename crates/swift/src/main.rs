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
use travsr_core::{Edge, EdgeKind, Language, Node, NodeId, VName};
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

const TIMEOUT_SECS: u64 = 300;

// ── Emitter discovery ─────────────────────────────────────────────────────────

fn emitter_path() -> Option<PathBuf> {
    // 1. Explicit env var override.
    if let Ok(p) = std::env::var("TRAVSR_SWIFT_EMITTER") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }

    if let Ok(exe) = std::env::current_exe() {
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
        if let Some(path) = dev {
            if path.exists() {
                return Some(path);
            }
        }

        // 3. Installed: /usr/local/bin/travsr-swift-index-emitter
        let installed = exe
            .parent()
            .map(|bin| bin.join("travsr-swift-index-emitter"));
        if let Some(path) = installed {
            if path.exists() {
                return Some(path);
            }
        }
    }

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
        emitter_path().is_some()
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_swift_emitter(&req.root, req.corpus.as_str()) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("swift emitter failed for {}: {e}", req.root.display());
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
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

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
                anyhow::bail!("swift emitter timed out after {TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    };

    let mut stderr_buf = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr_buf);
    }
    tracing::debug!("swift emitter stderr: {stderr_buf}");

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
    if bytes.is_empty() {
        return Ok(InvokeResponse::default());
    }

    let root: serde_json::Value =
        serde_json::from_slice(&bytes).context("parsing emitter JSON")?;

    let docs = root["documents"].as_array().context("missing 'documents'")?;
    let lang_str = Language::Swift.as_str();

    // Pass 1: build symbol → NodeId map from all definitions.
    let mut def_ids: std::collections::HashMap<String, NodeId> =
        std::collections::HashMap::new();
    let mut nodes: Vec<Node> = Vec::new();

    for doc in docs {
        let path = doc["path"].as_str().unwrap_or("");
        let defs = match doc["definitions"].as_array() {
            Some(a) => a,
            None => continue,
        };
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
            nodes.push(Node::new(vname, kind).with_line(line));
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
        let file_id =
            VName::new(corpus, "", path, lang_str, format!("file:{path}")).id();

        for r in refs {
            let sym = r["symbol"].as_str().unwrap_or("");
            if sym.is_empty() {
                continue;
            }
            if let Some(&dst_id) = def_ids.get(sym) {
                edges.push(Edge::new(file_id, dst_id, EdgeKind::RefCall));
            }
        }
    }

    tracing::info!(
        nodes = nodes.len(),
        edges = edges.len(),
        "swift emitter ingestion complete"
    );

    Ok(InvokeResponse { nodes, edges })
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
