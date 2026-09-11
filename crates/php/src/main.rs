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
        let result = run_scip_php(
            &req.root,
            req.corpus.as_str(),
            &req.scratch,
            &mut diagnostics,
        );
        into_response(result, &req.root, diagnostics)
    }
}

/// Folds a run's outcome and its diagnostics into one response.
///
/// Split out of `invoke_phase_b` so the rule it carries can be exercised on its
/// own. `run_scip_php` looks for a scip-php install before it reaches any of its
/// guards, so driving `invoke_phase_b` to an error path meant faking an install,
/// and faking one meant setting `HOME` from a test: a process-global mutation in
/// a parallel test binary, racing a `OnceLock` another test may already have
/// filled.
fn into_response(
    result: anyhow::Result<InvokeResponse>,
    root: &Path,
    diagnostics: Vec<PluginDiagnostic>,
) -> InvokeResponse {
    let mut resp = match result {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!("scip-php failed for {}: {e}", root.display());
            InvokeResponse::default()
        }
    };
    resp.diagnostics.extend(diagnostics);
    resp
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

/// Disposes of scip-php's in-repo artifact on every exit path, then writes the
/// user's saved bytes back over it.
///
/// scip-php hardcodes its output to `./index.scip`, so the artifact lands in
/// the checkout. Cleanup used to sit only on the success path, so one timeout
/// or one non-zero exit left the file behind.
///
/// Which of the two runs is decided by whether the user is owed anything, and
/// the split is load-bearing rather than cosmetic.
///
/// Owed nothing, so our own output is all that is at the path: unlink it, and
/// if the unlink is denied truncate it instead. Unsandboxed the remove takes
/// the file and the tree is left as it was found. Sandboxed the repo root is a
/// read-only mount with one writable bind layered over `index.scip` itself
/// (ADR-017 Rule 1), so only this file's contents are ours, the file survives
/// the remove, and truncating it to zero leaves precisely the zero-byte stub
/// the plugin host created before the run.
///
/// Owed a restore: truncate in place and write the bytes back, and never
/// unlink. On Windows the sidecar holds `GENERIC_ALL` on `index.scip`, which
/// includes DELETE, but only `GENERIC_READ` on the repo root
/// (travsr-plugin-host `sandbox/windows.rs`). The delete therefore succeeds and
/// the recreate, which needs `FILE_ADD_FILE` on the parent directory, does not:
/// unlinking on this path is a one-way door that loses the user's file. A
/// truncate and a write ask nothing of the parent directory on any of the three
/// platforms, so this branch is correct everywhere and the ordering hazard the
/// other branch has cannot arise.
///
/// `Drop` covers a returning function, not a killed process: on a supervisor
/// timeout or OOM the plugin host SIGKILLs the sidecar, neither step runs, and
/// the saved copy dies with the scratch dir. `preserve_existing_artifact`
/// spells out what that costs the user.
///
/// While a run is in flight the user's working tree is dirty, because
/// `index.scip` sits in the repo root for the duration. scip-php writes it
/// relative to its own working directory with no way to redirect it, so the
/// file has to be there. A git hook or the file watcher firing mid-index will
/// see it.
struct RepoArtifact {
    produced: std::path::PathBuf,
    /// The copy of the user's file in scratch, when the run found one to save.
    saved: Option<std::path::PathBuf>,
}

impl Drop for RepoArtifact {
    fn drop(&mut self) {
        let Some(saved) = &self.saved else {
            // Nothing owed. Take the file if we may, and empty it if we may not.
            let _ = std::fs::remove_file(&self.produced);
            if std::fs::symlink_metadata(&self.produced).is_ok() {
                let _ = std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&self.produced);
            }
            return;
        };
        // A restore is owed, so the file is opened and overwritten, never
        // unlinked: see the note on Windows above. Truncating on open disposes
        // of our own output in the same syscall that begins putting the user's
        // back, so there is no window where either could delete the other.
        //
        // `create` only as a second attempt, never as the first: the file is
        // normally still there (the sandbox denies the rename below into
        // scratch, so the copy fallback leaves the source in place), and opening
        // it directly asks nothing of the read-only parent directory. An
        // unsandboxed run whose rename did move the file out is the only case
        // that needs the create, and there the directory is writable anyway.
        let dst = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.produced)
            .or_else(|_| std::fs::File::create(&self.produced));
        // Open and stream rather than `fs::copy`: contents are the only thing
        // this process may change at this path, and `fs::copy` also carries the
        // source's permissions over to the destination.
        if let (Ok(mut src), Ok(mut dst)) = (std::fs::File::open(saved), dst) {
            let _ = std::io::copy(&mut src, &mut dst);
        }
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

/// Saves the contents of whatever the user already has at scip-php's hardcoded
/// output path, or refuses when that path is not a plain file.
///
/// scip-php has no `--output`: it writes `./index.scip` in its working
/// directory, and that directory has to be the repo root for it to find
/// `composer.json`. Indexing therefore writes into the user's checkout, and a
/// file already sitting at that name is a file indexing overwrites. Refusing
/// the run instead is not the answer, and for the reason it looks like one: a
/// successful run clobbers that file either way, so refuse-vs-delete was never
/// the axis. The bytes are copied out and `RepoArtifact` puts them back.
///
/// The save is by CONTENT, into `saved` under the sandbox-authorized scratch
/// directory, because contents are the only thing this process may change here.
/// ADR-017 Rule 1 binds the repo root read-only and layers a single writable
/// bind over `index.scip` itself (travsr-plugin-host `sandbox/linux.rs`, for the
/// `RepoWrite::File("index.scip")` grant; macOS Seatbelt grants the same one
/// literal, and php is Standard policy, so it is sandboxed there too). Creating,
/// renaming or unlinking a name inside the repo root needs write permission on
/// the root, which is never granted: both the move-aside that stood here and
/// the delete before it took EROFS and bailed the run through `?`, so php Phase
/// B returned nothing on every sandboxed Linux run.
///
/// What this does NOT cover: the plugin host SIGKILLs the sidecar on a
/// supervisor timeout or OOM, `Drop` never runs, and the saved copy goes with
/// the scratch directory the host then deletes. The user's `index.scip`
/// contents are lost in that case. It is not a regression and it cannot be
/// fixed from in here, because scip-php overwrites that path in place with no
/// way to redirect it, so the contents are gone the moment the run starts
/// whatever this function does.
///
/// Only a plain file is read. `symlink_metadata`, not `exists()`: the latter
/// follows symlinks and is false for a dangling one. A symlink or a directory
/// here can point outside the repo, so that case refuses with a diagnostic
/// saying so, and is neither followed, read, nor written through.
///
/// Returns whether a restore is owed once the run is over.
fn preserve_existing_artifact(
    produced: &Path,
    saved: &Path,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> anyhow::Result<bool> {
    match std::fs::symlink_metadata(produced) {
        // Nothing at the path: an unsandboxed run on a clean checkout.
        Err(_) => Ok(false),
        Ok(meta) if meta.is_file() => {
            // A plain file being here says nothing about the user. On a
            // sandboxed run the plugin host pre-creates `index.scip` as a
            // zero-byte stub before this process starts, because bwrap needs
            // the source of its writable bind to exist, so presence is
            // guaranteed and only the length carries information: non-empty is
            // the user's file, empty is the host's stub. That is what made the
            // old refuse-or-delete-on-presence logic wrong.
            if meta.len() == 0 {
                return Ok(false);
            }
            std::fs::copy(produced, saved).with_context(|| {
                format!(
                    "saving the contents of {} to {}",
                    produced.display(),
                    saved.display()
                )
            })?;
            Ok(true)
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
/// the process CWD, exactly as the ruby and c sidecars do. It is also validated
/// before use, for the reason spelled out at that fallback.
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

    // The field is validated, not trusted, because it can name a path that was
    // never mapped into this process. The plugin host fills it with the HOST
    // path of its tempdir (travsr-plugin-host `transport.rs`,
    // `Sidecar::invoke_phase_b`), and the Linux sandbox bind-mounts that
    // directory at the fixed path `/travsr-scratch`, over a fresh tmpfs root
    // with `/tmp` unbound (`sandbox/linux.rs`). The name handed to us therefore
    // does not exist in our own mount namespace and every write through it
    // fails. macOS and Windows grant the host path itself, so there it is real
    // and it is used.
    //
    // The fallback is not a degraded mode on Linux, it is the path that has
    // been working: `tempfile::tempdir` resolves `TMPDIR`, which that same
    // sandbox sets to `/travsr-scratch`.
    let _fallback_scratch;
    let output_dir = if !scratch.as_os_str().is_empty() && scratch.is_dir() {
        scratch
    } else {
        if !scratch.as_os_str().is_empty() {
            // Never silent: this line is the only signal that the granted
            // scratch dir was unusable and something else got written to.
            tracing::debug!(
                "invoke scratch dir {} is not a directory this process can see, \
                 falling back to TMPDIR",
                scratch.display()
            );
        }
        _fallback_scratch = tempfile::tempdir().context("failed to create temp dir")?;
        _fallback_scratch.path()
    };
    let output_path = output_dir.join("index.scip");

    // The user's copy is saved in scratch, so it has to be classified after the
    // scratch directory is settled. `_fallback_scratch` is declared above the
    // guard below and so drops after it: the restore reads the saved copy back
    // out of that tempdir from inside the guard's `Drop`.
    let saved = output_dir.join("index.scip.orig");
    let restore = preserve_existing_artifact(&produced, &saved, diagnostics)?;

    // Armed the moment there is anything to undo, not just before the spawn:
    // every return from here on owes the user their file back.
    let _artifact = RepoArtifact {
        produced: produced.clone(),
        saved: restore.then_some(saved),
    };

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
    // The rename fails across the sandbox's read-only repo root, hence the copy
    // fallback; `RepoArtifact` then disposes of whatever is still at the source.
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

    /// The defect this whole path exists for: indexing used to destroy a file in
    /// the user's checkout. scip-php has no `--output`, so its artifact lands on
    /// a name the user may already be using. The bytes are saved to scratch and
    /// written back, so a run that returns leaves the tree as it found it.
    #[test]
    fn a_pre_existing_index_has_its_bytes_restored_after_the_run() {
        let repo = tempfile::tempdir().expect("tempdir");
        let scratch = tempfile::tempdir().expect("tempdir");
        let produced = repo.path().join("index.scip");
        let saved = scratch.path().join("index.scip.orig");
        std::fs::write(&produced, b"the user's own scip-php output").expect("write the original");

        let mut diagnostics = Vec::new();
        let restore = preserve_existing_artifact(&produced, &saved, &mut diagnostics)
            .expect("a plain file must not refuse the run");

        assert!(restore, "the run owes these bytes back");
        assert!(
            diagnostics.is_empty(),
            "saving a file and putting it back is not a caveat on the result: {:?}",
            diag_codes(&diagnostics)
        );

        // What the run then does with the path: scip-php overwrites it in place.
        std::fs::write(&produced, b"our index").expect("write our artifact");
        drop(RepoArtifact {
            produced: produced.clone(),
            saved: Some(saved.clone()),
        });

        assert_eq!(
            std::fs::read(&produced).expect("the original must be back"),
            b"the user's own scip-php output",
            "the disposal has to run before the restore, not after it"
        );
    }

    /// The restore must overwrite the file, never delete and recreate it. On
    /// Windows the sidecar holds `GENERIC_ALL` on `index.scip` but only
    /// `GENERIC_READ` on the repo root, so the delete succeeds and the recreate
    /// is denied for want of `FILE_ADD_FILE` on the directory: the user's file
    /// would be gone. Pinning the inode across the guard catches a reordering
    /// back to delete-then-create, which no assertion on the bytes can see.
    #[cfg(unix)]
    #[test]
    fn the_restore_path_never_unlinks_the_file() {
        use std::os::unix::fs::MetadataExt as _;

        let repo = tempfile::tempdir().expect("tempdir");
        let scratch = tempfile::tempdir().expect("tempdir");
        let produced = repo.path().join("index.scip");
        let saved = scratch.path().join("index.scip.orig");
        std::fs::write(&produced, b"the user's own scip-php output").expect("write the original");

        let mut diagnostics = Vec::new();
        assert!(
            preserve_existing_artifact(&produced, &saved, &mut diagnostics)
                .expect("a plain file must not refuse the run")
        );

        // scip-php overwrites the path in place, which is what the sandbox
        // grants it, so the file the guard finds is the same one throughout.
        std::fs::write(&produced, b"our index").expect("write our artifact");
        let before = std::fs::metadata(&produced).expect("stat before").ino();

        drop(RepoArtifact {
            produced: produced.clone(),
            saved: Some(saved.clone()),
        });

        let after = std::fs::metadata(&produced).expect("stat after").ino();
        assert_eq!(
            before, after,
            "the restore must open and overwrite the file, not replace it"
        );
        assert_eq!(
            std::fs::read(&produced).expect("the original must be back"),
            b"the user's own scip-php output"
        );
    }

    /// On a sandboxed run the plugin host pre-creates `index.scip` as a
    /// zero-byte stub, so a plain file is there on every run and presence alone
    /// says nothing. Only a non-empty file is the user's, and saving the stub
    /// would restore it over our own output on the way out.
    #[test]
    fn a_zero_byte_stub_owes_nothing_and_is_not_saved() {
        let repo = tempfile::tempdir().expect("tempdir");
        let scratch = tempfile::tempdir().expect("tempdir");
        let produced = repo.path().join("index.scip");
        let saved = scratch.path().join("index.scip.orig");
        std::fs::write(&produced, b"").expect("write the host's stub");

        let mut diagnostics = Vec::new();
        let restore = preserve_existing_artifact(&produced, &saved, &mut diagnostics)
            .expect("the host's own stub must not refuse the run");

        assert!(
            !restore,
            "an empty file is the host's stub, not the user's data"
        );
        assert!(
            std::fs::symlink_metadata(&saved).is_err(),
            "nothing to save, so nothing may be written to scratch"
        );
        assert!(diagnostics.is_empty(), "got {:?}", diag_codes(&diagnostics));
    }

    /// The sandboxed disposal path: the repo root is a read-only mount, so the
    /// unlink is denied and only the file's contents are writable. A read-only
    /// parent directory reproduces that shape. The file has to end up as the
    /// same zero-byte stub the host created, not as our leftover index.
    #[cfg(unix)]
    #[test]
    fn a_denied_unlink_leaves_a_zero_byte_stub_behind() {
        use std::os::unix::fs::PermissionsExt as _;

        let repo = tempfile::tempdir().expect("tempdir");
        let produced = repo.path().join("index.scip");
        std::fs::write(&produced, b"our index").expect("write our artifact");
        std::fs::set_permissions(repo.path(), std::fs::Permissions::from_mode(0o555))
            .expect("make the repo root read-only");

        // Root ignores the directory mode, so the denial cannot be reproduced
        // and there is nothing here to pin.
        let probe = repo.path().join("index.scip.probe");
        let denied = std::fs::File::create(&probe).is_err();
        if denied {
            drop(RepoArtifact {
                produced: produced.clone(),
                saved: None,
            });

            let meta = std::fs::symlink_metadata(&produced).expect("the file must still be there");
            assert_eq!(meta.len(), 0, "our output must be truncated away");
        } else {
            let _ = std::fs::remove_file(&probe);
        }

        std::fs::set_permissions(repo.path(), std::fs::Permissions::from_mode(0o755))
            .expect("restore permissions so the tempdir can clean itself up");
    }

    /// A symlink can point outside the repo, so it is refused rather than read,
    /// truncated or written through. `symlink_metadata`, not `exists()`: the
    /// latter follows the link and is false for a dangling one, which is how a
    /// link previously slipped past the guard.
    #[cfg(unix)]
    #[test]
    fn symlink_at_the_index_path_is_refused_and_left_unfollowed() {
        let repo = tempfile::tempdir().expect("tempdir");
        let scratch = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        let target = outside.path().join("somebody-elses.scip");
        std::fs::write(&target, b"not ours to touch").expect("write target");
        let produced = repo.path().join("index.scip");
        let saved = scratch.path().join("index.scip.orig");
        std::os::unix::fs::symlink(&target, &produced).expect("symlink");

        let mut diagnostics = Vec::new();
        let err = preserve_existing_artifact(&produced, &saved, &mut diagnostics);

        assert!(err.is_err(), "a symlink must refuse the run");
        let meta = std::fs::symlink_metadata(&produced).expect("the symlink must still be there");
        assert!(meta.file_type().is_symlink(), "it must still be a symlink");
        assert!(
            std::fs::symlink_metadata(&saved).is_err(),
            "the link must not be read through into scratch either"
        );
        assert_eq!(
            std::fs::read(&target).expect("the link target must still be there"),
            b"not ours to touch",
            "the link target must not be written through"
        );
        assert_eq!(diag_codes(&diagnostics), ["php.index-path-blocked"]);
    }

    /// Same refusal for a directory: the run cannot write its output to that
    /// name, and the point is that the user hears why instead of getting a
    /// silent zero-node run.
    #[test]
    fn directory_at_the_index_path_is_refused_and_left_in_place() {
        let repo = tempfile::tempdir().expect("tempdir");
        let scratch = tempfile::tempdir().expect("tempdir");
        let produced = repo.path().join("index.scip");
        let saved = scratch.path().join("index.scip.orig");
        std::fs::create_dir(&produced).expect("mkdir");

        let mut diagnostics = Vec::new();
        let err = preserve_existing_artifact(&produced, &saved, &mut diagnostics);

        assert!(err.is_err(), "a directory must refuse the run");
        assert!(produced.is_dir(), "the directory must still be there");
        assert_eq!(diag_codes(&diagnostics), ["php.index-path-blocked"]);
    }

    /// The ordinary case. A diagnostic is a caveat on the result, so a clean
    /// repo must not emit one: routine chatter on every successful index is
    /// exactly what the channel is not for. Nothing is owed back either, so the
    /// guard must not restore a file that was never there.
    #[test]
    fn a_clean_repo_produces_no_diagnostic() {
        let repo = tempfile::tempdir().expect("tempdir");
        let scratch = tempfile::tempdir().expect("tempdir");
        let produced = repo.path().join("index.scip");
        let saved = scratch.path().join("index.scip.orig");

        let mut diagnostics = Vec::new();
        let restore = preserve_existing_artifact(&produced, &saved, &mut diagnostics)
            .expect("nothing there, nothing to do");

        assert!(!restore, "nothing was saved, so nothing is owed back");
        assert!(diagnostics.is_empty(), "got {:?}", diag_codes(&diagnostics));
    }

    /// The response used to be a bare `InvokeResponse::default()` on error,
    /// which threw away everything the run had to say. The refusal above happens
    /// on the error path, so it is the case that proves diagnostics are
    /// collected outside the `Result` and survive it.
    ///
    /// Driven through `into_response` rather than `invoke_phase_b` so no
    /// scip-php install has to be faked: faking one meant setting `HOME`, which
    /// is process-global state in a parallel test binary and races the
    /// `OnceLock` inside `find_scip_php`.
    #[test]
    fn a_refusal_reaches_the_response_instead_of_a_bare_default() {
        let repo = tempfile::tempdir().expect("tempdir");
        let scratch = tempfile::tempdir().expect("tempdir");
        let produced = repo.path().join("index.scip");
        let saved = scratch.path().join("index.scip.orig");
        // A directory, so the real guard refuses and its diagnostic is the one
        // that has to survive the error.
        std::fs::create_dir(&produced).expect("mkdir");

        let mut diagnostics = Vec::new();
        let result = preserve_existing_artifact(&produced, &saved, &mut diagnostics)
            .map(|_| InvokeResponse::default());
        assert!(
            result.is_err(),
            "test setup: the guard must refuse a directory"
        );

        let resp = into_response(result, repo.path(), diagnostics);

        assert_eq!(diag_codes(&resp.diagnostics), ["php.index-path-blocked"]);
    }
}
