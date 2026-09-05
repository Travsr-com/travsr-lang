//! Travsr Phase B: Ruby semantic analysis.
//!
//! Runs `scip-ruby {root}` inside the ADR-017 sandbox (Standard policy) and
//! returns call/reference edges to the Travsr daemon via the plugin protocol.
//!
//! Note: scip-ruby support is experimental.
//!
//! Install:  See https://github.com/sourcegraph/scip-ruby
//! Register: travsr lang add ruby

use anyhow::Context as _;
use std::path::Path;
use travsr_core::Language;
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

const TIMEOUT_SECS: u64 = 300;

struct RubyPhaseB;

impl Plugin for RubyPhaseB {
    fn language(&self) -> Language {
        Language::Ruby
    }
    fn extensions(&self) -> &[&str] {
        &["rb", "rake"]
    }
    fn supports_phase_b(&self) -> bool {
        scip_ruby_available()
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A (Tree-sitter structural parse) is handled by the built-in
        // Ruby plugin in the core daemon. This binary is Phase B only.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_scip_ruby(&req.root, req.corpus.as_str(), &req.scratch) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("scip-ruby failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

static SCIP_RUBY_BIN: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

fn find_scip_ruby() -> Option<&'static std::path::PathBuf> {
    SCIP_RUBY_BIN
        .get_or_init(|| {
            // 1. Try PATH first
            if std::process::Command::new("scip-ruby")
                .arg("--help")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
            {
                return Some(std::path::PathBuf::from("scip-ruby"));
            }
            // 2. travsr lang install location (GithubBinary → ~/.travsr/bin/)
            let candidate = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)?
                .join(".travsr/bin/scip-ruby");
            candidate.exists().then_some(candidate)
        })
        .as_ref()
}

fn scip_ruby_available() -> bool {
    find_scip_ruby().is_some()
}

fn run_scip_ruby(root: &Path, corpus: &str, scratch: &Path) -> anyhow::Result<InvokeResponse> {
    let bin = find_scip_ruby().ok_or_else(|| {
        anyhow::anyhow!(
            "scip-ruby not found. See https://github.com/sourcegraph/scip-ruby/releases \
             or run: travsr lang install ruby"
        )
    })?;

    let _fallback_scratch;
    let output_dir = if !scratch.as_os_str().is_empty() {
        scratch
    } else {
        _fallback_scratch = tempfile::tempdir().context("failed to create temp dir")?;
        _fallback_scratch.path()
    };
    let output_path = output_dir.join("index.scip");
    // We run scip-ruby with `.current_dir(root)`, so a relative --index-file
    // would land under `root` rather than the scratch dir we read back. The
    // sandbox grant hands us an absolute scratch path in practice; guard it
    // explicitly rather than silently write to the wrong place (review finding 4).
    anyhow::ensure!(
        output_path.is_absolute(),
        "scip-ruby output path must be absolute under current_dir(root): {}",
        output_path.display()
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    // Derive a gem name from the corpus (last path segment) for scip-ruby metadata.
    let gem_name = corpus.split('/').next_back().unwrap_or("gem");

    let mut child = std::process::Command::new(bin)
        .arg("--index-file")
        .arg(&output_path)
        .arg("--gem-metadata")
        .arg(format!("{gem_name}@0.0.0"))
        // travsr-lang#17: scip-ruby requires a positional folder/file to index.
        // Without it scip-ruby exits 1 ("You must pass either `-e` or at least
        // one folder or ruby file.") and writes nothing, so every Ruby symbol
        // ended up with a bare defines/binding edge and no callers/refs. We run
        // with `.current_dir(root)`, so `.` indexes the whole checkout.
        .arg(".")
        .current_dir(root)
        // travsr-lang#17: now that scip-ruby actually indexes the checkout it
        // emits progress/diagnostics. Nothing reads stdout, so discard it:
        // piping it unread would deadlock the child once its ~64KB pipe buffer
        // fills on a large repo, surfacing as a spurious 300s timeout (review
        // finding 4). stderr is piped but drained concurrently below.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn scip-ruby")?;

    // Drain stderr on a reader thread so the child never blocks on a full pipe
    // while we poll for exit.
    let stderr_reader = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            let _ = err.read_to_string(&mut buf);
            buf
        })
    });

    let status = loop {
        match child.try_wait().context("polling scip-ruby")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("scip-ruby timed out after {TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    };

    let stderr_out = stderr_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    anyhow::ensure!(
        status.success(),
        "scip-ruby exited with {status}: {stderr_out}"
    );

    let output_size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    tracing::info!("scip-ruby produced {output_size} bytes of SCIP output");

    travsr_lang_scip_reader::ingest(&output_path, corpus, Language::Ruby, root)
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_ruby=info".parse().unwrap()),
        )
        .init();

    run_plugin(RubyPhaseB);
}
