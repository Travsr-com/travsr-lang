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
    PluginDiagnostic,
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
        // Diagnostics are collected outside the response so they survive the
        // error path as well. A run that refuses to start still has to say why:
        // returning a bare default is the silence this is here to end.
        let mut diagnostics = Vec::new();
        let mut resp = match run_scip_php(
            &req.root,
            req.corpus.as_str(),
            &req.scratch,
            &mut diagnostics,
        ) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("scip-php failed for {}: {e}", req.root.display());
                InvokeResponse::default()
            }
        };
        resp.diagnostics.extend(diagnostics);
        resp
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

/// Deletes scip-php's in-repo artifact on every exit path.
///
/// scip-php hardcodes its output to `./index.scip`, so the artifact lands in
/// the checkout. Cleanup used to sit only on the success path, so one timeout
/// or one non-zero exit left the file behind. `remove_file` unlinks a symlink
/// rather than following it, which is what the guard in `run_scip_php` wants
/// here too.
///
/// `Drop` covers a returning function, not a killed process: the plugin host
/// kills a sidecar on a supervisor timeout or on OOM, and the artifact then
/// outlives the run. `run_scip_php` cleans up such a leftover on the next run
/// rather than relying on this.
///
/// While a run is in flight the user's working tree is dirty, because
/// `index.scip` sits in the repo root for the duration. scip-php writes it
/// relative to its own working directory with no way to redirect it, so the
/// file has to be there. A git hook or the file watcher firing mid-index will
/// see it.
struct RepoArtifact(std::path::PathBuf);

impl Drop for RepoArtifact {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Last `max_bytes` bytes of `s`, on a char boundary, prefixed with an elision
/// marker when truncated. scip-php is chatty (PHP deprecation notices from its
/// parser), and its stderr rides out on an error string.
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

/// Clears whatever sits at scip-php's hardcoded output path before a run, or
/// refuses when that path is not a plain file.
///
/// A file already sitting there when a run starts is, in practice, this
/// sidecar's own leftover: scip-php hardcodes that name, and the run that wrote
/// it was killed by the plugin host (supervisor timeout, OOM) before
/// `RepoArtifact` could clean up.
///
/// Deleting it and carrying on is the deliberate choice over refusing. Refusing
/// protects nothing that a successful run does not overwrite anyway, and it
/// turns one killed run into a PHP Phase B that reports zero nodes on every
/// later run until a human finds the file, with no message reaching the person
/// who could act. The delete is reported, so it is never silent.
///
/// Only a plain file is deleted. `symlink_metadata`, not `exists()`: the latter
/// follows symlinks and is false for a dangling one. A symlink or a directory
/// here can point outside the repo, so that case still refuses, now with a
/// diagnostic saying so.
fn clear_leftover_artifact(
    produced: &Path,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(produced) {
        Err(_) => Ok(()),
        Ok(meta) if meta.is_file() => {
            std::fs::remove_file(produced)
                .with_context(|| format!("removing the leftover {}", produced.display()))?;
            diagnostics.push(PluginDiagnostic::warning(
                "php.stale-index-removed",
                format!(
                    "Deleted a leftover {} before indexing. An earlier PHP index run was \
                     stopped before it could clean it up.",
                    produced.display()
                ),
            ));
            Ok(())
        }
        Ok(_) => {
            diagnostics.push(PluginDiagnostic::warning(
                "php.index-path-blocked",
                format!(
                    "PHP was not indexed: {} is a link or a folder, and the indexer has to \
                     write its output to that name. Move or delete it, then index again.",
                    produced.display()
                ),
            ));
            anyhow::bail!(
                "refusing to run scip-php: {} is a link or a directory",
                produced.display()
            )
        }
    }
}

/// `scratch` is the sandbox-authorized writable directory from the invoke
/// request, used so the artifact lands somewhere the sandbox actually granted.
/// It is `#[serde(default)]` on the wire, so an empty value falls back to a
/// tempdir rather than joining onto a relative path that would resolve against
/// the process CWD, exactly as the ruby and c sidecars do.
fn run_scip_php(
    root: &Path,
    corpus: &str,
    scratch: &Path,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> anyhow::Result<InvokeResponse> {
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
    clear_leftover_artifact(&produced, diagnostics)?;

    let _fallback_scratch;
    let output_dir = if !scratch.as_os_str().is_empty() {
        scratch
    } else {
        _fallback_scratch = tempfile::tempdir().context("failed to create temp dir")?;
        _fallback_scratch.path()
    };
    let output_path = output_dir.join("index.scip");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);

    // Armed before the spawn, so the artifact is removed however this returns.
    let _artifact = RepoArtifact(produced.clone());

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
    let mut stderr_reader = child.stderr.take().map(|mut err| {
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
                // Reap it: a killed child left unwaited stays a zombie for the
                // lifetime of this sidecar process.
                let _ = child.wait();
                // Drop the reader handle, do not join it. `read_to_string`
                // returns at EOF, and EOF needs every holder of the pipe's
                // write end to be gone. A grandchild that inherited it outlives
                // the kill, so joining here could block forever and turn a
                // bounded timeout into a hang. Dropping detaches the thread.
                drop(stderr_reader.take());
                anyhow::bail!("scip-php timed out after {TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    };

    let stderr_out = stderr_reader
        .take()
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    anyhow::ensure!(
        status.success(),
        "scip-php exited with {status}: {}",
        tail(&stderr_out, 4000)
    );

    // Move the artifact out of the repo so the checkout is left as it was found.
    // `RepoArtifact` removes whatever is still there afterwards, including the
    // source of a `copy` fallback.
    std::fs::rename(&produced, &output_path)
        .or_else(|_| std::fs::copy(&produced, &output_path).map(|_| ()))
        .with_context(|| format!("scip-php wrote no index at {}", produced.display()))?;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn diag_codes(diagnostics: &[PluginDiagnostic]) -> Vec<&str> {
        diagnostics.iter().map(|d| d.code.as_str()).collect()
    }

    /// The guard used to refuse forever on any pre-existing `index.scip`. Since
    /// `RepoArtifact::drop` does not run when the plugin host SIGKILLs the
    /// sidecar, one killed run wedged PHP Phase B at zero nodes until a human
    /// found the file. A plain file is this sidecar's own leftover, so it is
    /// removed and the run continues.
    #[test]
    fn stale_regular_index_is_removed_and_reported_not_refused() {
        let repo = tempfile::tempdir().expect("tempdir");
        let produced = repo.path().join("index.scip");
        std::fs::write(&produced, b"leftover from a killed run").expect("write leftover");

        let mut diagnostics = Vec::new();
        clear_leftover_artifact(&produced, &mut diagnostics).expect("stale file must not refuse");

        assert!(
            std::fs::symlink_metadata(&produced).is_err(),
            "the leftover must be gone so scip-php can write its own"
        );
        assert_eq!(diag_codes(&diagnostics), ["php.stale-index-removed"]);
    }

    /// A symlink can point outside the repo, so it is refused rather than
    /// deleted or written through. `symlink_metadata`, not `exists()`: the
    /// latter follows the link and is false for a dangling one, which is how a
    /// link previously slipped past the guard.
    #[cfg(unix)]
    #[test]
    fn symlink_at_the_index_path_is_refused_and_left_unfollowed() {
        let repo = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        let target = outside.path().join("somebody-elses.scip");
        std::fs::write(&target, b"not ours to touch").expect("write target");
        let produced = repo.path().join("index.scip");
        std::os::unix::fs::symlink(&target, &produced).expect("symlink");

        let mut diagnostics = Vec::new();
        let err = clear_leftover_artifact(&produced, &mut diagnostics);

        assert!(err.is_err(), "a symlink must refuse the run");
        let meta = std::fs::symlink_metadata(&produced).expect("the symlink must still be there");
        assert!(meta.file_type().is_symlink(), "it must still be a symlink");
        assert!(
            target.exists(),
            "the link target must not be written through"
        );
        assert_eq!(diag_codes(&diagnostics), ["php.index-path-blocked"]);
    }

    /// Same refusal for a directory: `remove_file` would fail on it anyway, and
    /// the point is that the user hears why instead of getting a silent
    /// zero-node run.
    #[test]
    fn directory_at_the_index_path_is_refused_and_left_in_place() {
        let repo = tempfile::tempdir().expect("tempdir");
        let produced = repo.path().join("index.scip");
        std::fs::create_dir(&produced).expect("mkdir");

        let mut diagnostics = Vec::new();
        let err = clear_leftover_artifact(&produced, &mut diagnostics);

        assert!(err.is_err(), "a directory must refuse the run");
        assert!(produced.is_dir(), "the directory must still be there");
        assert_eq!(diag_codes(&diagnostics), ["php.index-path-blocked"]);
    }

    /// The ordinary case. A diagnostic is a caveat on the result, so a clean
    /// repo must not emit one: routine chatter on every successful index is
    /// exactly what the channel is not for.
    #[test]
    fn a_clean_repo_produces_no_diagnostic() {
        let repo = tempfile::tempdir().expect("tempdir");
        let produced = repo.path().join("index.scip");

        let mut diagnostics = Vec::new();
        clear_leftover_artifact(&produced, &mut diagnostics).expect("nothing there, nothing to do");

        assert!(diagnostics.is_empty(), "got {:?}", diag_codes(&diagnostics));
    }

    /// `invoke_phase_b` used to return a bare `InvokeResponse::default()` on
    /// error, which threw away everything the run had to say. The refusal above
    /// happens on the error path, so it is the case that proves diagnostics are
    /// collected outside the `Result` and survive it.
    ///
    /// Mutates `HOME` because `find_scip_php` runs before the guard and caches
    /// its answer in a `OnceLock`, so a fake install is the only way to reach
    /// the guard. Nothing else in this binary reads `HOME`. Real scip-php is
    /// never executed: the guard bails before the spawn.
    #[cfg(unix)]
    #[test]
    fn a_refusal_reaches_the_response_instead_of_a_bare_default() {
        let home = tempfile::tempdir().expect("tempdir");
        let bin_dir = home.path().join(".travsr/bin");
        std::fs::create_dir_all(&bin_dir).expect("create fake bin dir");
        std::fs::write(bin_dir.join("scip-php"), b"").expect("write fake scip-php");
        std::env::set_var("HOME", home.path());
        assert!(
            find_scip_php().is_some(),
            "test setup: the fake install must be discoverable"
        );

        let repo = tempfile::tempdir().expect("tempdir");
        // A directory, so the guard refuses and returns before spawning.
        std::fs::create_dir(repo.path().join("index.scip")).expect("mkdir");

        let resp = PhpPhaseB.invoke_phase_b(&InvokeRequest {
            root: repo.path().to_path_buf(),
            corpus: "testcorpus".to_string(),
            scratch: std::path::PathBuf::new(),
            files: None,
        });

        assert_eq!(diag_codes(&resp.diagnostics), ["php.index-path-blocked"]);
    }
}
