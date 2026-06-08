//! Travsr Phase B — Dart semantic analysis.
//!
//! Runs `dart run emit.dart <root> <output>` inside the ADR-017 sandbox
//! (Standard policy + dart toolchain grants) and returns call/reference edges
//! to the Travsr daemon via the plugin protocol.
//!
//! The emitter uses `package:analyzer` and requires:
//!   - `dart` on PATH (Dart SDK — read-only, already under /opt/homebrew)
//!   - `~/.pub-cache/` readable (dart package cache)
//!   - The emitter script + .dart_tool/ readable
//!
//! Both sandbox grants are added by `toolchain_access("dart")` in travsr's
//! `crates/travsr-plugin-host/src/sandbox/toolchain.rs`.
//!
//! Install emitter (one-time):
//!   cd packages/dart-scip-emitter && dart pub get
//!
//! Emitter location resolution order:
//!   1. $TRAVSR_DART_EMITTER (explicit path to emit.dart)
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
        let result = std::process::Command::new("dart")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok();
        tracing::debug!(available = result, "dart_available: dart --version check");
        result
    })
}

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
    //    → ../../packages/dart-scip-emitter/bin/emit.dart
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

    // 3. Installed: <prefix>/share/travsr-lang-dart/emit.dart
    //    e.g. ~/.nvm/.../share/travsr-lang-dart/emit.dart
    let installed = exe
        .parent()
        .and_then(|p| p.parent())
        .map(|prefix| {
            prefix
                .join("share")
                .join("travsr-lang-dart")
                .join("emit.dart")
        });
    if let Some(ref path) = installed {
        tracing::debug!(
            path = %path.display(),
            exists = path.exists(),
            "emitter_path[3]: installed prefix path"
        );
        if path.exists() {
            return Some(path.clone());
        }
    }

    tracing::debug!("emitter_path: not found at any location");
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
        let dart = dart_available();
        let emitter = emitter_path();
        let supported = dart && emitter.is_some();
        tracing::debug!(
            dart_available = dart,
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

// ── Emitter invocation ────────────────────────────────────────────────────────

fn run_dart_emitter(root: &Path, corpus: &str) -> anyhow::Result<InvokeResponse> {
    let emitter = emitter_path().context(
        "dart-scip-emitter not found — install with: \
         cd packages/dart-scip-emitter && dart pub get  \
         (then copy emit.dart + .dart_tool/ to <prefix>/share/travsr-lang-dart/)",
    )?;
    anyhow::ensure!(dart_available(), "dart not found on PATH");

    let scratch = tempfile::tempdir().context("failed to create temp dir")?;
    let output_path = scratch.path().join("index.json");
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    // current_dir must be the package root so `dart run` can find pubspec.yaml
    // and .dart_tool/package_config.json.
    // Layout A (installed): <share>/travsr-lang-dart/emit.dart → parent IS root
    // Layout B (dev):       packages/dart-scip-emitter/bin/emit.dart → parent.parent() IS root
    let pkg_root = emitter
        .parent()
        .and_then(|p| {
            if p.join("pubspec.yaml").exists() {
                Some(p) // installed: emit.dart sits directly in the package root
            } else {
                p.parent() // dev: emit.dart lives in bin/, go up one more level
            }
        })
        .unwrap_or(root);

    tracing::debug!(
        emitter = %emitter.display(),
        root = %root.display(),
        pkg_root = %pkg_root.display(),
        output = %output_path.display(),
        "run_dart_emitter: launching 'dart run emit.dart'"
    );

    // Log pubspec.yaml and .dart_tool presence — common failure indicators.
    let pubspec = pkg_root.join("pubspec.yaml");
    let dart_tool = pkg_root.join(".dart_tool").join("package_config.json");
    tracing::debug!(
        pubspec_exists = pubspec.exists(),
        dart_tool_exists = dart_tool.exists(),
        "run_dart_emitter: package root sanity check"
    );

    let mut child = std::process::Command::new("dart")
        .arg("run")
        .arg(&emitter)
        .arg(root)
        .arg(&output_path)
        .current_dir(pkg_root)
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

    tracing::debug!(exit_code = %status, "run_dart_emitter: subprocess exited");
    if !stderr_buf.is_empty() {
        tracing::debug!("run_dart_emitter stderr:\n{stderr_buf}");
    }

    anyhow::ensure!(
        status.success(),
        "dart emitter exited with {status}: {stderr_buf}"
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

    let root_val: serde_json::Value =
        serde_json::from_slice(&bytes).context("parsing emitter JSON")?;

    let docs = root_val["documents"]
        .as_array()
        .context("missing 'documents'")?;

    tracing::debug!(doc_count = docs.len(), "parse_emitter_output: documents found");

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
        tracing::debug!(path, def_count = defs.len(), "parse_emitter_output: document defs");
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
        let file_vname = VName::new(corpus, "", path, lang_str, format!("file:{path}"));
        let file_id = file_vname.id();

        tracing::debug!(path, ref_count = refs.len(), "parse_emitter_output: document refs");
        for r in refs {
            let sym = r["symbol"].as_str().unwrap_or("");
            if sym.is_empty() {
                continue;
            }
            if let Some(&dst_id) = def_ids.get(sym) {
                edges.push(Edge::new(file_id, dst_id, EdgeKind::RefCall));
            } else {
                tracing::debug!(sym, "parse_emitter_output: ref symbol not in def_ids — skipped");
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
