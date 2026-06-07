//! Travsr Phase B — Dart semantic analysis.
//!
//! Spawns the `dart-scip-emitter` Dart script, which uses `package:analyzer`
//! to walk all .dart files and emit a JSON index of definitions and references.
//! The JSON is parsed here and converted into Travsr nodes and edges.
//!
//! Install emitter:
//!   cd packages/dart-scip-emitter && dart pub get
//!
//! Register plugin:
//!   travsr lang add dart
//!
//! Emitter location resolution order:
//!   1. $TRAVSR_DART_EMITTER env var (explicit path to emit.dart)
//!   2. <binary-dir>/../../packages/dart-scip-emitter/bin/emit.dart (dev/monorepo)
//!   3. <binary-dir>/../share/travsr-lang-dart/emit.dart (installed)

use anyhow::Context as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use travsr_core::{Edge, EdgeKind, Language, Node, NodeId, VName};
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

const TIMEOUT_SECS: u64 = 300;

// ── Tool availability ─────────────────────────────────────────────────────────

static DART_AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn dart_available() -> bool {
    *DART_AVAILABLE.get_or_init(|| {
        std::process::Command::new("dart")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    })
}

fn emitter_path() -> Option<PathBuf> {
    // 1. Explicit env var override.
    if let Ok(p) = std::env::var("TRAVSR_DART_EMITTER") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }

    // 2. Relative to this binary — works in dev (cargo run) and monorepo CI.
    if let Ok(exe) = std::env::current_exe() {
        // target/debug/travsr-lang-dart → ../../packages/dart-scip-emitter/bin/emit.dart
        let dev = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(|root| {
                root.join("packages")
                    .join("dart-scip-emitter")
                    .join("bin")
                    .join("emit.dart")
            });
        if let Some(path) = dev {
            if path.exists() {
                return Some(path);
            }
        }

        // 3. Installed path: /usr/local/lib/travsr-lang-dart/emit.dart
        let installed = exe
            .parent()
            .and_then(|p| p.parent())
            .map(|prefix| {
                prefix
                    .join("share")
                    .join("travsr-lang-dart")
                    .join("emit.dart")
            });
        if let Some(path) = installed {
            if path.exists() {
                return Some(path);
            }
        }
    }

    None
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
        dart_available() && emitter_path().is_some()
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_dart_emitter(&req.root, req.corpus.as_str()) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("dart emitter failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

// ── Emitter invocation ────────────────────────────────────────────────────────

fn run_dart_emitter(root: &Path, corpus: &str) -> anyhow::Result<InvokeResponse> {
    let emitter = emitter_path().context(
        "dart-scip-emitter not found — set $TRAVSR_DART_EMITTER or run \
         `cd packages/dart-scip-emitter && dart pub get`",
    )?;
    anyhow::ensure!(dart_available(), "dart not found on PATH");

    let scratch = tempfile::tempdir().context("failed to create temp dir")?;
    let output_path = scratch.path().join("index.json");
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    let mut child = std::process::Command::new("dart")
        .arg("run")
        .arg(&emitter)
        .arg(root)
        .arg(&output_path)
        .current_dir(emitter.parent().and_then(|p| p.parent()).unwrap_or(root))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn dart emitter")?;

    let status = loop {
        match child.try_wait().context("polling dart emitter")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("dart emitter timed out after {TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    };

    let mut stderr_buf = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr_buf);
    }
    tracing::debug!("dart emitter stderr: {stderr_buf}");

    anyhow::ensure!(
        status.success(),
        "dart emitter exited with {status}: {stderr_buf}"
    );

    parse_emitter_output(&output_path, corpus)
}

// ── JSON parsing ──────────────────────────────────────────────────────────────

fn parse_emitter_output(
    json_path: &Path,
    corpus: &str,
) -> anyhow::Result<InvokeResponse> {
    let bytes = std::fs::read(json_path)
        .with_context(|| format!("reading emitter output {}", json_path.display()))?;
    if bytes.is_empty() {
        return Ok(InvokeResponse::default());
    }

    let root: serde_json::Value =
        serde_json::from_slice(&bytes).context("parsing emitter JSON")?;

    let docs = root["documents"].as_array().context("missing 'documents'")?;
    let lang_str = Language::Dart.as_str();

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

    // Pass 2: resolve references → emit ref/call edges.
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
        "dart emitter ingestion complete"
    );

    Ok(InvokeResponse { nodes, edges })
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
