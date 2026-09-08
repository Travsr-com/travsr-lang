//! Travsr Phase B: PHP semantic analysis.
//!
//! Runs `scip-php {root}` inside the ADR-017 sandbox (Standard policy) and
//! returns call/reference edges to the Travsr daemon via the plugin protocol.
//!
//! Install:  See https://github.com/sourcegraph/scip-php
//! Register: travsr lang add php

use anyhow::Context as _;
use std::path::Path;
use travsr_core::Language;
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

const TIMEOUT_SECS: u64 = 300;

struct PhpPhaseB;

impl Plugin for PhpPhaseB {
    fn language(&self) -> Language {
        Language::Php
    }
    fn extensions(&self) -> &[&str] {
        &["php", "phtml"]
    }
    fn supports_phase_b(&self) -> bool {
        scip_php_available()
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A (Tree-sitter structural parse) is handled by the built-in
        // PHP plugin in the core daemon. This binary is Phase B only.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_scip_php(&req.root, req.corpus.as_str(), &req.scratch) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("scip-php failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

static SCIP_PHP_BIN: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

fn find_scip_php() -> Option<&'static std::path::PathBuf> {
    SCIP_PHP_BIN
        .get_or_init(|| {
            // 1. Try PATH first
            if std::process::Command::new("scip-php")
                .arg("--help")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
            {
                return Some(std::path::PathBuf::from("scip-php"));
            }
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
            // 2. travsr-managed install (manual placement)
            let candidate = home.join(".travsr/bin/scip-php");
            if candidate.exists() {
                return Some(candidate);
            }
            // 3. Composer global install (~/.composer on macOS, ~/.config/composer on Linux)
            let candidate = home.join(".composer/vendor/bin/scip-php");
            if candidate.exists() {
                return Some(candidate);
            }
            let candidate = home.join(".config/composer/vendor/bin/scip-php");
            candidate.exists().then_some(candidate)
        })
        .as_ref()
}

fn scip_php_available() -> bool {
    find_scip_php().is_some()
}

/// `scratch` is the sandbox-authorized writable directory from the invoke
/// request. Used instead of a self-minted `tempfile::tempdir()` so the path is
/// one the sandbox actually granted, matching every other sidecar.
fn run_scip_php(root: &Path, corpus: &str, scratch: &Path) -> anyhow::Result<InvokeResponse> {
    let bin = find_scip_php().ok_or_else(|| {
        anyhow::anyhow!(
            "scip-php not found. See https://github.com/davidrjenni/scip-php \
             (clone it and run `composer install` inside that checkout, then place \
             its bin/scip-php in ~/.travsr/bin/). Installing it as a project \
             dev-dependency does not work: it requires a vendor/ directory inside \
             its own package dir, which composer does not create for a dependency."
        )
    })?;

    // scip-php takes no positional root and has no `--output`: it indexes
    // `getcwd()` and hardcodes its output to `./index.scip` (bin/scip-php:39,53).
    // travsr passed both anyway and ran it in an empty temp dir, so scip-php set
    // its project root to that dir, failed to read `composer.json` there, and
    // exited 255 on every repo. The `Err` was then swallowed into a default
    // response, so PHP Phase B reported success with zero nodes. Run it in the
    // repo and move its artifact into scratch.
    let produced = root.join("index.scip");
    // Never clobber a file the repo already carries: a pre-existing index.scip
    // is someone's committed artifact, and this function would delete it below.
    anyhow::ensure!(
        !produced.exists(),
        "refusing to run scip-php: {} already exists and would be overwritten",
        produced.display()
    );
    let output_path = scratch.join("index.scip");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    let mut child = std::process::Command::new(bin)
        .current_dir(root)
        // Nothing reads stdout, and scip-php is chatty (PHP deprecation notices
        // from its parser). Piping it unread deadlocks the child once the ~64KB
        // pipe buffer fills, surfacing as a spurious timeout. Same hazard, and
        // the same fix, as travsr-lang#17 in the Ruby sidecar.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn scip-php")?;

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
        match child.try_wait().context("polling scip-php")? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("scip-php timed out after {TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    };

    let stderr_out = stderr_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    anyhow::ensure!(
        status.success(),
        "scip-php exited with {status}: {stderr_out}"
    );

    // Move the artifact out of the repo so the checkout is left as it was found.
    std::fs::rename(&produced, &output_path)
        .or_else(|_| std::fs::copy(&produced, &output_path).map(|_| ()))
        .with_context(|| format!("scip-php wrote no index at {}", produced.display()))?;
    let _ = std::fs::remove_file(&produced);

    let output_size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    tracing::info!("scip-php produced {output_size} bytes of SCIP output");

    travsr_lang_scip_reader::ingest(&output_path, corpus, Language::Php, root)
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_php=info".parse().unwrap()),
        )
        .init();

    run_plugin(PhpPhaseB);
}
