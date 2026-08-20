//! Travsr Phase B — Java semantic analysis.
//!
//! On unix, runs `scip-java index --output {scratch}/index.scip {root}` and
//! returns call/reference edges to the Travsr daemon via the plugin protocol.
//!
//! ## Windows: travsr drives the build itself
//!
//! The scip-java release travsr ships invokes the build tool (`gradlew`, `mvn`)
//! by its extensionless name, which a Windows JVM's `ProcessBuilder` cannot run
//! for `.cmd`/`.bat`, so `scip-java index` produces zero edges on Windows
//! regardless of whether Gradle/Maven are installed. On Windows this wrapper
//! therefore drives the build directly for a Gradle project:
//!
//!   1. Extract scip-java's own SemanticDB plugin jars from the launcher.
//!   2. Run the repo's `gradlew.bat` with a travsr-generated init-script (forward
//!      slashes, no backslash-escaping pitfalls) that applies the SemanticDB
//!      plugin and redirects all build output out of the repo.
//!   3. Convert the emitted `.semanticdb` files to SCIP with scip-java's
//!      `index-semanticdb` subcommand (present in the 0.12.x line travsr pins on
//!      Windows).
//!   4. Ingest the SCIP index via `travsr_lang_scip_reader::ingest`.
//!
//! ## Sandbox class: RequiresElevated (ADR-017 Rule 1)
//!
//! scip-java drives Maven/Gradle, which resolve dependencies from the network
//! at analysis time. It therefore runs under `SandboxPolicy::Elevated` and the
//! Travsr daemon refuses to spawn it until a Principal Security Engineer has
//! recorded an approval with an explicit host allowlist:
//!
//! ```text
//! travsr lang approve java \
//!   --approved-by <pse-handle> \
//!   --reason "Maven/Gradle dependency resolution" \
//!   --permitted-hosts repo1.maven.org,repo.maven.apache.org,plugins.gradle.org
//! travsr lang add java
//! ```
//!
//! Install: download scip-java from https://github.com/sourcegraph/scip-java/releases

use anyhow::Context as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use travsr_core::Language;
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

/// JVM builds (Gradle/Maven) can be slow on a cold dependency cache.
const TIMEOUT_SECS: u64 = 600;

struct JavaPhaseB;

impl Plugin for JavaPhaseB {
    fn language(&self) -> Language {
        Language::Java
    }
    fn extensions(&self) -> &[&str] {
        &["java"]
    }
    fn supports_phase_b(&self) -> bool {
        scip_java_available()
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        // Phase A (Tree-sitter structural parse) is handled by the built-in
        // Java plugin in the core daemon. This binary is Phase B only.
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        let result = if cfg!(windows) {
            run_windows(req)
        } else {
            run_scip_java(&req.root, req.corpus.as_str())
        };
        match result {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("scip-java failed for {}: {e:#}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

static SCIP_JAVA_BIN: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

fn find_scip_java() -> Option<&'static std::path::PathBuf> {
    // Resolve through the shared PATHEXT-aware resolver: it checks PATH and the
    // toolchain-managed dirs (including ~/.travsr/bin, where `travsr lang install
    // java` writes the launcher) and, on Windows, finds `scip-java.cmd` — the
    // generated `java -jar <asset>` launcher, since scip-java has no native
    // Windows exe. The old hand-rolled lookup checked only the extensionless
    // name: `Command::new("scip-java")` auto-resolves `.exe` but never `.cmd`,
    // and `Path::exists()` on `~/.travsr/bin/scip-java` matched the bare
    // (non-runnable) coursier jar-launcher, so both branches missed the real
    // runnable form and Phase B produced zero edges on Windows.
    SCIP_JAVA_BIN
        .get_or_init(|| travsr_core::exec::tool_path("scip-java"))
        .as_ref()
}

fn scip_java_available() -> bool {
    find_scip_java().is_some()
}

/// Unix path: scip-java's own `index` orchestration works because the shipped
/// release launches `gradlew`/`mvn` as valid unix scripts.
fn run_scip_java(root: &Path, corpus: &str) -> anyhow::Result<InvokeResponse> {
    let bin = find_scip_java().ok_or_else(|| {
        anyhow::anyhow!(
            "scip-java not found — download from https://github.com/sourcegraph/scip-java/releases \
             and place in ~/.travsr/bin/scip-java"
        )
    })?;

    // scip-java writes a SCIP index file (not stdout); use a temp dir as scratch.
    let scratch = tempfile::tempdir().context("failed to create temp dir")?;
    let output_path = scratch.path().join("index.scip");

    let mut cmd = std::process::Command::new(bin);
    cmd.arg("index")
        .arg("--output")
        .arg(&output_path)
        .current_dir(root);
    run_to_completion(cmd, "scip-java")?;

    let output_size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    tracing::info!("scip-java produced {output_size} bytes of SCIP output");

    travsr_lang_scip_reader::ingest(&output_path, corpus, Language::Java)
}

// ── Windows: travsr-driven build ───────────────────────────────────────────

/// Windows Phase B: travsr drives the build (see module docs) rather than
/// scip-java's broken build orchestration.
fn run_windows(req: &InvokeRequest) -> anyhow::Result<InvokeResponse> {
    // `InvokeRequest::root` arrives canonicalized with the extended-length
    // verbatim prefix (`\\?\D:\...`) on Windows. gradlew.bat and the Gradle
    // init-script cannot use a verbatim path, so strip it up front.
    let root = PathBuf::from(strip_windows_verbatim_prefix(&req.root.to_string_lossy()));
    let root = root.as_path();

    let scip_java = find_scip_java()
        .ok_or_else(|| anyhow::anyhow!("scip-java not found — run `travsr lang install java`"))?;
    let launcher = scip_java_launcher_jar(scip_java);
    let java = java_exe().context("no `java` found (set JAVA_HOME or put java on PATH)")?;
    let jar = jar_exe().context("no `jar` found (need a JDK, not just a JRE, on JAVA_HOME/PATH)")?;

    // Everything travsr writes goes under the sandbox-authorized scratch dir; on
    // Windows the sandbox forces TEMP/TMP there too, so a `tempdir()` fallback
    // (older daemons that send an empty scratch) lands in the same granted area.
    let scratch_owned;
    let scratch: &Path = if req.scratch.as_os_str().is_empty() {
        scratch_owned = tempfile::tempdir().context("failed to create temp dir")?;
        scratch_owned.path()
    } else {
        req.scratch.as_path()
    };

    let targetroot = match detect_build_system(root) {
        Some(BuildSystem::Gradle) => build_gradle(root, scratch, &launcher, &jar)?,
        Some(BuildSystem::Maven) => anyhow::bail!(
            "Maven-based Java projects are not yet supported for semantic analysis \
             on Windows; convert the project to Gradle, or run on macOS/Linux"
        ),
        None => anyhow::bail!(
            "no Gradle or Maven build file found under {} — cannot build for \
             semantic analysis",
            root.display()
        ),
    };

    // Convert the emitted SemanticDB files to a single SCIP index.
    let output_path = scratch.join("index.scip");
    let mut cmd = std::process::Command::new(&java);
    cmd.arg("-jar")
        .arg(&launcher)
        .arg("index-semanticdb")
        .arg("--output")
        .arg(&output_path)
        .arg(&targetroot)
        .current_dir(root);
    run_to_completion(cmd, "scip-java index-semanticdb")?;

    let output_size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    tracing::info!("scip-java produced {output_size} bytes of SCIP output");

    travsr_lang_scip_reader::ingest(&output_path, req.corpus.as_str(), Language::Java)
}

/// Which build tool drives a Java project. Gradle wins when both are present:
/// a Gradle wrapper/build script is a stronger signal than a bare `pom.xml`.
#[derive(Debug, PartialEq, Eq)]
enum BuildSystem {
    Gradle,
    Maven,
}

fn detect_build_system(root: &Path) -> Option<BuildSystem> {
    let has_gradle = [
        "gradlew.bat",
        "gradlew",
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
    ]
    .iter()
    .any(|f| root.join(f).exists());
    if has_gradle {
        return Some(BuildSystem::Gradle);
    }
    if root.join("pom.xml").exists() {
        return Some(BuildSystem::Maven);
    }
    None
}

/// The three SemanticDB jars scip-java bundles, extracted from the launcher.
struct GradlePluginJars {
    gradle_plugin: PathBuf,
    semanticdb_plugin: PathBuf,
    semanticdb_agent: PathBuf,
}

/// Build a Gradle project with the SemanticDB plugin and return the targetroot
/// directory holding the emitted `.semanticdb` files.
fn build_gradle(
    root: &Path,
    scratch: &Path,
    launcher: &Path,
    jar: &Path,
) -> anyhow::Result<PathBuf> {
    let jars = extract_gradle_plugin_jars(jar, launcher, &scratch.join("scip-java-plugins"))
        .context("failed to extract scip-java SemanticDB plugin jars")?;

    let targetroot = scratch.join("semanticdb-targetroot");
    let build_dir = scratch.join("gradle-build");

    let init_script = scratch.join("travsr-semanticdb-init.gradle");
    std::fs::write(
        &init_script,
        render_gradle_init_script(&jars, &targetroot, &build_dir),
    )
    .context("failed to write Gradle init-script")?;

    // Prefer the repo's own wrapper; fall back to a `gradle` on PATH. Rust's
    // std spawns a `.bat` through cmd.exe with strict arg escaping.
    let gradlew = root.join("gradlew.bat");
    let mut cmd = if gradlew.is_file() {
        std::process::Command::new(&gradlew)
    } else {
        let gradle = travsr_core::exec::tool_path("gradle")
            .context("no gradlew.bat in the project and no `gradle` on PATH")?;
        std::process::Command::new(gradle)
    };
    // Run from the project root (this analyzer runs with the user's own
    // privileges, so there is no read-only-repo constraint to work around) with a
    // one-shot `--no-daemon` build: apply the SemanticDB init-script, `clean` so
    // sources recompile and emit `.semanticdb` fresh, then the plugin's
    // `scipPrintDependencies` + `scipCompileAll` tasks. `--console=plain` keeps
    // the output free of progress-bar control codes. The init-script redirects the
    // build output out of the repo, so nothing is written back into the sources.
    cmd.current_dir(root)
        .arg("--no-daemon")
        .arg("--console=plain")
        .arg("--init-script")
        .arg(&init_script)
        .arg("clean")
        .arg("scipPrintDependencies")
        .arg("scipCompileAll");
    run_to_completion(cmd, "gradle")?;

    Ok(targetroot)
}

/// Render the SemanticDB init-script. All paths are emitted with forward slashes
/// and no verbatim prefix — a backslash in a Groovy string literal is an escape,
/// which is exactly what breaks scip-java's own generated init-script on Windows.
/// `buildDir` is redirected out of the repo so the build needs no repo-write
/// grant (the sandbox binds the repo read-only).
fn render_gradle_init_script(
    jars: &GradlePluginJars,
    targetroot: &Path,
    build_dir: &Path,
) -> String {
    let gradle_plugin = to_gradle_path(&jars.gradle_plugin);
    let semanticdb_plugin = to_gradle_path(&jars.semanticdb_plugin);
    let semanticdb_agent = to_gradle_path(&jars.semanticdb_agent);
    let target = to_gradle_path(targetroot);
    let build = to_gradle_path(build_dir);
    format!(
        r#"initscript {{
  dependencies {{
    classpath(files("{gradle_plugin}"))
  }}
}}
import com.sourcegraph.gradle.semanticdb.SemanticdbGradlePlugin
allprojects {{
  layout.buildDirectory.set(new File("{build}/" + project.name))
  project.ext["semanticdbTarget"] = "{target}"
  project.ext["javacPluginJar"] = "{semanticdb_plugin}"
  project.ext["dependenciesOut"] = "{target}/dependencies.txt"
  project.ext["javacAgentPath"] = "{semanticdb_agent}"
  apply plugin: SemanticdbGradlePlugin
}}
"#
    )
}

/// A path as a Groovy string-literal value: verbatim prefix stripped, backslashes
/// turned into forward slashes (Gradle accepts forward slashes on Windows).
fn to_gradle_path(p: &Path) -> String {
    strip_windows_verbatim_prefix(&p.to_string_lossy()).replace('\\', "/")
}

const PLUGIN_JAR_NAMES: [&str; 3] = [
    "gradle-plugin.jar",
    "semanticdb-plugin.jar",
    "semanticdb-agent.jar",
];

/// Extract scip-java's `gradle-plugin.jar`, `semanticdb-plugin.jar` and
/// `semanticdb-agent.jar` from the launcher into `dest`. They live at the root of
/// the nested `coursier/bootstrap/launcher/jars/scip-java_2.13-<ver>.jar` inside
/// the launcher.
///
/// The launcher is a coursier polyglot jar (a shell preamble in front of a zip
/// whose payload jars are STORED uncompressed). That layout defeats the Rust
/// `zip` reader's end-of-central-directory scan, so extraction goes through the
/// JDK's own `jar` tool — the same java.util.zip that runs the launcher — which
/// reads it correctly and supports selective entry extraction.
fn extract_gradle_plugin_jars(
    jar: &Path,
    launcher: &Path,
    dest: &Path,
) -> anyhow::Result<GradlePluginJars> {
    std::fs::create_dir_all(dest).context("create plugin-jar dir")?;

    // Discover the versioned payload jar name (`jar tf` lists archive entries).
    let listing = capture_stdout(
        std::process::Command::new(jar).arg("tf").arg(launcher),
        "jar tf",
    )?;
    let nested = listing
        .lines()
        .map(str::trim)
        .find(|n| {
            n.starts_with("coursier/bootstrap/launcher/jars/scip-java_2.13-") && n.ends_with(".jar")
        })
        .context("scip-java_2.13 payload jar not found in the launcher — is this a 0.12.x scip-java?")?
        .to_string();

    // `jar` extracts into the current directory, so run each step with cwd=dest.
    // Step 1: pull the payload jar out of the launcher.
    run_to_completion(
        {
            let mut c = std::process::Command::new(jar);
            c.arg("xf").arg(launcher).arg(&nested).current_dir(dest);
            c
        },
        "jar extract payload",
    )?;
    let nested_path = dest.join(&nested);

    // Step 2: pull the three plugin jars out of the payload jar (they sit at its
    // root, so they land directly in `dest`).
    run_to_completion(
        {
            let mut c = std::process::Command::new(jar);
            c.arg("xf").arg(&nested_path);
            for name in PLUGIN_JAR_NAMES {
                c.arg(name);
            }
            c.current_dir(dest);
            c
        },
        "jar extract plugins",
    )?;

    let jars = GradlePluginJars {
        gradle_plugin: dest.join("gradle-plugin.jar"),
        semanticdb_plugin: dest.join("semanticdb-plugin.jar"),
        semanticdb_agent: dest.join("semanticdb-agent.jar"),
    };
    anyhow::ensure!(
        jars.gradle_plugin.is_file()
            && jars.semanticdb_plugin.is_file()
            && jars.semanticdb_agent.is_file(),
        "scip-java plugin jars missing after extraction under {}",
        dest.display()
    );
    Ok(jars)
}

/// Spawn `cmd`, capture stdout, and fail with stderr if it exits non-zero.
fn capture_stdout(cmd: &mut std::process::Command, what: &str) -> anyhow::Result<String> {
    let out = cmd
        .stderr(std::process::Stdio::piped())
        .output()
        .with_context(|| format!("failed to spawn {what}"))?;
    anyhow::ensure!(
        out.status.success(),
        "{what} exited with {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The runnable scip-java payload jar. On Windows `tool_path` resolves the
/// `scip-java.cmd` launcher (a `java -jar <jar>` shim); the jar itself is the
/// sibling extensionless `scip-java`. On unix the resolved path already is the
/// jar/launcher.
fn scip_java_launcher_jar(resolved: &Path) -> PathBuf {
    match resolved.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat") => {
            resolved.with_extension("")
        }
        _ => resolved.to_path_buf(),
    }
}

/// Resolve the `java` executable: JAVA_HOME/bin first (the sandbox grants execute
/// there and forwards JAVA_HOME), then PATH.
fn java_exe() -> Option<PathBuf> {
    if let Some(java_home) = std::env::var_os("JAVA_HOME") {
        let exe = if cfg!(windows) { "java.exe" } else { "java" };
        let candidate = PathBuf::from(java_home).join("bin").join(exe);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    travsr_core::exec::tool_path("java")
}

/// Resolve the JDK `jar` tool: JAVA_HOME/bin first, then PATH.
fn jar_exe() -> Option<PathBuf> {
    if let Some(java_home) = std::env::var_os("JAVA_HOME") {
        let exe = if cfg!(windows) { "jar.exe" } else { "jar" };
        let candidate = PathBuf::from(java_home).join("bin").join(exe);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    travsr_core::exec::tool_path("jar")
}

/// Spawn `cmd`, wait up to `TIMEOUT_SECS`, and fail with captured output if it
/// exits non-zero. Shared by every subprocess this wrapper runs.
///
/// stdout and stderr are drained on their own threads *while* the process runs,
/// not after it exits. A Gradle build emits far more than an OS pipe buffer holds
/// (tens of KB), so reading only after exit deadlocks: the child blocks writing to
/// a full pipe while we block waiting for it to exit. The reader threads run to
/// EOF, which the child reaching exit (or being killed) produces by closing its
/// write ends.
fn run_to_completion(mut cmd: std::process::Command, what: &str) -> anyhow::Result<()> {
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {what}"))?;

    let drain = |stream: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            if let Some(mut s) = stream {
                let _ = s.read_to_string(&mut buf);
            }
            buf
        })
    };
    let out_h = drain(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    );
    let err_h = drain(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);
    let status = loop {
        match child
            .try_wait()
            .with_context(|| format!("polling {what}"))?
        {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("{what} timed out after {TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    };

    let stdout_out = out_h.join().unwrap_or_default();
    let stderr_out = err_h.join().unwrap_or_default();
    anyhow::ensure!(
        status.success(),
        "{what} exited with {status}:\n{}",
        // Gradle prints the real failure to stdout under --console=plain; include a
        // tail of both so the cause survives without dumping megabytes.
        tail_lines(&stderr_out, &stdout_out)
    );
    Ok(())
}

/// The last chunk of a subprocess's output for an error message: prefer stderr,
/// fall back to stdout, and cap the length so a chatty build can't bloat the log.
fn tail_lines(stderr: &str, stdout: &str) -> String {
    let src = if stderr.trim().is_empty() { stdout } else { stderr };
    const MAX: usize = 4000;
    if src.len() <= MAX {
        src.to_string()
    } else {
        format!("…{}", &src[src.len() - MAX..])
    }
}

/// Strip the Windows extended-length verbatim prefix (`\\?\`, `\\?\UNC\`).
/// `InvokeRequest::root` arrives canonicalized with this prefix on Windows;
/// gradlew and the Gradle init-script cannot use a verbatim path. No-op elsewhere.
fn strip_windows_verbatim_prefix(s: &str) -> &str {
    s.strip_prefix(r"\\?\UNC\")
        .or_else(|| s.strip_prefix(r"\\?\"))
        .unwrap_or(s)
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_java=info".parse().unwrap()),
        )
        .init();

    run_plugin(JavaPhaseB);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_verbatim_drive_prefix() {
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\D:\com.travsr\repo"),
            r"D:\com.travsr\repo"
        );
    }

    #[test]
    fn strips_verbatim_unc_prefix() {
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\UNC\server\share\repo"),
            r"server\share\repo"
        );
    }

    #[test]
    fn strip_is_noop_on_plain_paths() {
        assert_eq!(strip_windows_verbatim_prefix(r"D:\repo"), r"D:\repo");
        assert_eq!(
            strip_windows_verbatim_prefix("/home/u/repo"),
            "/home/u/repo"
        );
    }

    #[test]
    fn gradle_path_uses_forward_slashes_and_strips_verbatim() {
        assert_eq!(
            to_gradle_path(Path::new(r"\\?\D:\com.travsr\testing\javarepo\build")),
            "D:/com.travsr/testing/javarepo/build"
        );
        // A path with no backslashes and no prefix is unchanged.
        assert_eq!(to_gradle_path(Path::new("/tmp/x/y")), "/tmp/x/y");
    }

    #[test]
    fn init_script_is_valid_groovy_shape() {
        let jars = GradlePluginJars {
            gradle_plugin: PathBuf::from(r"C:\scratch\scip-java-plugins\gradle-plugin.jar"),
            semanticdb_plugin: PathBuf::from(r"C:\scratch\scip-java-plugins\semanticdb-plugin.jar"),
            semanticdb_agent: PathBuf::from(r"C:\scratch\scip-java-plugins\semanticdb-agent.jar"),
        };
        let script = render_gradle_init_script(
            &jars,
            Path::new(r"C:\scratch\semanticdb-targetroot"),
            Path::new(r"C:\scratch\gradle-build"),
        );
        // No backslash may survive into a Groovy string literal (the escaping bug).
        assert!(
            !script.contains('\\'),
            "init-script must not contain backslashes:\n{script}"
        );
        // The plugin is referenced and applied.
        assert!(
            script.contains("classpath(files(\"C:/scratch/scip-java-plugins/gradle-plugin.jar\"))")
        );
        assert!(script.contains("apply plugin: SemanticdbGradlePlugin"));
        assert!(script.contains("import com.sourcegraph.gradle.semanticdb.SemanticdbGradlePlugin"));
        // Build output is redirected out of the repo.
        assert!(script.contains("layout.buildDirectory.set"));
        assert!(script
            .contains(r#"project.ext["semanticdbTarget"] = "C:/scratch/semanticdb-targetroot""#));
    }

    #[test]
    fn launcher_jar_drops_cmd_extension() {
        assert_eq!(
            scip_java_launcher_jar(Path::new(r"C:\Users\me\.travsr\bin\scip-java.cmd")),
            PathBuf::from(r"C:\Users\me\.travsr\bin\scip-java")
        );
        assert_eq!(
            scip_java_launcher_jar(Path::new("/home/me/.travsr/bin/scip-java")),
            PathBuf::from("/home/me/.travsr/bin/scip-java")
        );
    }

    #[test]
    fn build_system_detection() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_build_system(dir.path()), None);

        std::fs::write(dir.path().join("pom.xml"), "<project/>").unwrap();
        assert_eq!(detect_build_system(dir.path()), Some(BuildSystem::Maven));

        // Gradle wins when both are present.
        std::fs::write(dir.path().join("build.gradle"), "").unwrap();
        assert_eq!(detect_build_system(dir.path()), Some(BuildSystem::Gradle));
    }
}
