//! Travsr Phase B: Java semantic analysis.
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
//! ## Android Gradle Plugin (AGP) projects
//!
//! scip-java's Gradle plugin configures a project only when it applies the
//! `java` plugin. An Android module applies `com.android.application` or
//! `com.android.library` instead, so on an AGP repo the plugin attaches the
//! SemanticDB javac plugin to nothing, `scipCompileAll` depends on nothing, and
//! the build "succeeds" with an empty index (#904). Both platforms therefore
//! add [`AGP_INIT_SCRIPT_SHIM`] to the Gradle invocation: on Windows it is
//! appended to the init-script travsr renders, on unix it is passed to
//! scip-java as an extra `--init-script` through the build command. The shim
//! reads the paths scip-java's own init-script already publishes as project
//! extra properties, so it has nothing of its own to template and works with
//! both the 0.12.x (`semanticdb`) and 0.13.x (`scip`) plugin generations.
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
    PluginDiagnostic,
};

/// JVM builds (Gradle/Maven) can be slow on a cold dependency cache.
const TIMEOUT_SECS: u64 = 600;

/// Why a Java repo's test sources can be entirely missing from an otherwise
/// healthy SCIP index.
///
/// scip-java analyses with SemanticDB, which is a javac plugin: it only ever
/// sees the sources the build actually hands to javac. If the build skips test
/// compilation then `testCompile` is a no-op, no `.semanticdb` file is written
/// for any test source, the SCIP index carries no Document for it, and no
/// semantic edge can originate in a test file. scip-java exits 0 throughout,
/// so the only visible symptom is a graph that quietly knows nothing about the
/// tests.
const TEST_SCOPE_CAUSE_HINT: &str = "SemanticDB is a javac plugin, so a test \
     source the build never compiles produces no index at all. The usual cause \
     is a build that skips test compilation: `-Dmaven.test.skip=true` (on the \
     command line or in `.mvn/maven.config`), `<maven.test.skip>` or \
     `<skipTests>` in the pom, or `-x compileTestJava` for Gradle.";

/// Gradle init-script fragment that extends scip-java's SemanticDB plugin to
/// Android Gradle Plugin modules (#904). See the module docs for why.
///
/// It runs after scip-java's own plugin in each project's `afterEvaluate`.
/// A project that applies the `java` plugin is left entirely to scip-java's
/// plugin; for every other project it does two things:
///
/// 1. Puts the repositories declared in `settings.gradle` back onto the
///    project. Both scip-java generations inject `mavenCentral()` and
///    `mavenLocal()` as PROJECT repositories from their own `afterEvaluate`
///    (0.12.x unconditionally; 0.13.1 too, catching only the
///    `InvalidUserCodeException` a `FAIL_ON_PROJECT_REPOS` build throws, and
///    only its later main-branch fix for issue #847 skips the injection when
///    settings declare repositories). Under Gradle's default `PREFER_PROJECT`
///    mode a project that has any repository of its own ignores the settings
///    ones, so `google()` disappears and AGP's own artifacts (`aapt2`, the
///    Android Gradle API) stop resolving. Measured on an AGP 8.13 module: the
///    build failed with "Could not find com.android.tools.build:aapt2" until
///    the settings repositories were re-added. Kept below the `java` check so
///    a plain Java build keeps resolving from exactly where it did before,
///    and done only under `PREFER_PROJECT` (the mode that has the problem),
///    so a `PREFER_SETTINGS` build is not warned at once per project.
///
/// Before either, from `settingsEvaluated` and without importing
/// `RepositoriesMode` (the class exists only from Gradle 6.8, and an import
/// that fails to resolve kills the whole init-script on an older Gradle, for
/// every Gradle repo the shim is added to; the mode is compared by name and
/// set reflectively inside the `try`), a `FAIL_ON_PROJECT_REPOS` build
/// (the Android Studio template default) is relaxed to `PREFER_SETTINGS` for
/// this one indexing build. In that mode scip-java's own injection above
/// throws inside its plugin: 0.13.1 catches that, 0.12.x does not, and the
/// build dies at configuration before any task exists ("Failed to notify
/// project evaluation listener", measured on the AGP 8.13 module with the
/// template's settings). `PREFER_SETTINGS` keeps resolving from exactly the
/// repositories the settings declare and only downgrades the plugin's
/// injection to a warning, so nothing resolves from anywhere it would not
/// have. The user's `settings.gradle` is untouched: the property is changed
/// in memory for this build only.
///
/// 2. Gives every `JavaCompile` task except the release and instrumentation
///    (`AndroidTest`) variants the configuration scip-java's plugin gives a
///    java project's tasks: the javac plugin jar on `compileOnly` and
///    `testCompileOnly` (and on `annotationProcessor` / `testAnnotationProcessor`
///    when that scope has processors, since javac then discovers plugins from
///    the processor path only; the two scopes are independent, so a test-only
///    processor such as Hilt's `testAnnotationProcessor` would otherwise fail
///    the unit-test compile with "plug-in not found" and take the whole Java
///    index with it, measured on the AGP 8.13 fixture), the `-Xplugin`
///    argument, the `--add-exports` fork options when the javac toolchain is
///    JDK 17 or newer (a forked JDK 8 javac rejects them), and the SemanticDB
///    agent when the scip-java generation ships one. The dependency adds are
///    guarded like both scip-java generations guard their own: a build that
///    already resolved `compileOnly` gets a warning instead of "Failed to
///    notify project evaluation listener". `-Xplugin` itself is added at
///    execution time and only when the jar is on the path javac will load
///    plugins from (the processor path when Gradle passes one, the classpath
///    otherwise), so a task the adds never reached, a KMP `jvm` target fed from
///    `jvmCompileOnly` for instance, compiles without the plugin rather than
///    failing with "plug-in not found" and taking the whole build with it; on
///    0.12.x the agent still injects the plugin there. `scipCompileAll` is then made to
///    depend on those tasks, and `scipPrintDependencies` is disabled: on AGP 9
///    that task dies with a `ConcurrentModificationException` while resolving
///    AGP's lazily-registered configurations, and its output (Maven
///    coordinates for cross-repository navigation) is not something travsr
///    reads.
///
/// Task configuration is lazy (`matching` + `configureEach`) on purpose: this
/// `afterEvaluate` callback is registered from the init-script, before the
/// build script applies AGP, so it runs BEFORE AGP's own `afterEvaluate`
/// creates the variant tasks, and an eager `withType(JavaCompile)` here is
/// empty. Unit-test variants ARE included: they are what compiles
/// `src/test/java`, and leaving them out is exactly the test-blind index the
/// `java.test-scope-dark` diagnostic exists to flag (the Maven path runs
/// `test-compile` and scip-java wires `compileTestJava` for the same reason).
/// Release and instrumentation variants are skipped: one build type is enough
/// for an index, every variant compiles the same sources, and a release
/// compile doubles the analysis time of a large app for nothing.
///
/// The paths are read from the project's extra properties (`semanticdbTarget`
/// / `scipTarget`, `javacPluginJar`, `javacAgentPath`) that scip-java's
/// init-script sets on every project, so this fragment needs no templating
/// and no backslash can reach a Groovy string literal.
const AGP_INIT_SCRIPT_SHIM: &str = r#"
// ---- travsr: Android Gradle Plugin support (see travsr-lang-java) ----
import org.gradle.api.JavaVersion
import org.gradle.api.tasks.compile.JavaCompile
// RepositoriesMode is never imported: the class only exists from Gradle 6.8,
// and an unresolvable import fails the whole init-script on an older Gradle
// before any try/catch can run, for every Gradle repo this shim is added to.
// The mode is compared by name and set reflectively, inside the catch.
settingsEvaluated { s ->
  try {
    def mode = s.dependencyResolutionManagement.repositoriesMode
    if (String.valueOf(mode.getOrNull()) == "FAIL_ON_PROJECT_REPOS") {
      def modes = Class.forName("org.gradle.api.initialization.resolve.RepositoriesMode")
      mode.set(Enum.valueOf(modes, "PREFER_SETTINGS"))
    }
  } catch (Throwable ignored) {
  }
}
allprojects { p ->
  p.afterEvaluate {
    if (p.plugins.hasPlugin("java")) { return }
    try {
      def management = p.gradle.settings.dependencyResolutionManagement
      def mode = management.repositoriesMode.getOrNull()
      if (mode == null || String.valueOf(mode) == "PREFER_PROJECT") {
        management.repositories.each { r -> if (p.repositories.findByName(r.name) == null) { p.repositories.add(r) } }
      }
    } catch (Throwable ignored) {
    }
    def targetKey = p.ext.has("semanticdbTarget") ? "semanticdbTarget" : (p.ext.has("scipTarget") ? "scipTarget" : null)
    if (targetKey == null || !p.ext.has("javacPluginJar")) { return }
    def pluginId = targetKey == "semanticdbTarget" ? "semanticdb" : "scip"
    def targetroot = p.ext[targetKey].toString()
    def pluginJar = p.ext["javacPluginJar"].toString()
    def agentJar = p.ext.has("javacAgentPath") ? p.ext["javacAgentPath"].toString() : null
    def sourceroot = p.rootDir.toString()
    try {
      ["compileOnly", "testCompileOnly"].each { conf ->
        if (p.configurations.findByName(conf) != null) { p.dependencies.add(conf, p.files(pluginJar)) }
      }
      ["annotationProcessor", "testAnnotationProcessor"].each { conf ->
        def c = p.configurations.findByName(conf)
        if (c != null && !c.dependencies.isEmpty()) { p.dependencies.add(conf, p.files(pluginJar)) }
      }
    } catch (Throwable e) {
      p.logger.warn("travsr: could not attach the SemanticDB javac plugin to project '" + p.path + "' (" + e.getClass().getSimpleName() + ": " + e.getMessage() + "); its Java sources will not be indexed unless the javac agent is in use")
    }
    def moduleOptions = ["--add-exports", "jdk.compiler/com.sun.tools.javac.api=ALL-UNNAMED",
                         "--add-exports", "jdk.compiler/com.sun.tools.javac.code=ALL-UNNAMED",
                         "--add-exports", "jdk.compiler/com.sun.tools.javac.model=ALL-UNNAMED",
                         "--add-exports", "jdk.compiler/com.sun.tools.javac.tree=ALL-UNNAMED",
                         "--add-exports", "jdk.compiler/com.sun.tools.javac.util=ALL-UNNAMED"]
    def selected = p.tasks.withType(JavaCompile).matching { t -> !(t.name ==~ /.*(AndroidTest|Release).*/) }
    selected.configureEach { t ->
      t.options.fork = true
      t.options.incremental = false
      // -Xplugin is decided at execution time, when the task's inputs are
      // resolved: javac loads plugins from -processorpath when Gradle passes
      // one (a non-empty annotation processor path) and from the classpath
      // otherwise, and the jar only reached those through the configurations
      // above. A task fed from other configurations (a KMP jvm target, or a
      // project whose adds failed) gets no -Xplugin instead of a javac that
      // dies with "plug-in not found" and takes the whole build with it.
      t.doFirst {
        def jarPath = new File(pluginJar).canonicalPath
        def carries = { fc -> fc != null && fc.files.any { it.canonicalPath == jarPath } }
        def apPath = t.options.annotationProcessorPath
        def reachable = (apPath != null && !apPath.isEmpty()) ? carries(apPath) : carries(t.classpath)
        def args = t.options.compilerArgs
        if (reachable && !args.any { it.toString().startsWith("-Xplugin:" + pluginId) }) {
          args.add("-Xplugin:" + pluginId + " -targetroot:" + targetroot + " -sourceroot:" + sourceroot + " -randomtimestamp=" + System.nanoTime())
        }
      }
      def jvmArgs = new ArrayList<String>(t.options.forkOptions.jvmArgs ?: [])
      def javacVersion = null
      try {
        def compiler = t.javaCompiler.getOrNull()
        if (compiler != null) { javacVersion = compiler.metadata.languageVersion.asInt() }
      } catch (Throwable ignored) {
      }
      if (javacVersion == null) { javacVersion = JavaVersion.current().majorVersion.toInteger() }
      if (javacVersion >= 17) { jvmArgs.addAll(moduleOptions) }
      if (agentJar != null) {
        jvmArgs.addAll(["-javaagent:" + agentJar, "-Dsemanticdb.pluginpath=" + pluginJar, "-Dsemanticdb.sourceroot=" + sourceroot, "-Dsemanticdb.targetroot=" + targetroot])
      }
      t.options.forkOptions.jvmArgs = jvmArgs
    }
    if (p.tasks.findByName("scipCompileAll") != null) { p.tasks.named("scipCompileAll") { it.dependsOn(selected) } }
    if (p.tasks.findByName("scipPrintDependencies") != null) { p.tasks.named("scipPrintDependencies") { it.enabled = false } }
  }
}
"#;

/// The Android Gradle Plugin's own words for "I cannot find the SDK", as they
/// appear in a failed build's output. `SDK location not found` is the
/// no-`ANDROID_HOME`/no-`local.properties` case; the others are an SDK that is
/// there but lacks the platform or build tools the project asks for, or that
/// AGP tried and failed to complete itself.
const ANDROID_SDK_FAILURE_MARKERS: &[&str] = &[
    "SDK location not found",
    "Failed to find Platform SDK with path",
    "Failed to find target with hash string",
    "Failed to find Build Tools revision",
    "Failed to install the following Android SDK packages",
];

/// A structured diagnostic naming the Android SDK when a failed Gradle build's
/// output says the SDK is what was missing (#904).
///
/// Without this the failure reached the user as "produced no symbols", the
/// same shape as every other zero-node run, and the AGP message that named the
/// actual cause sat in a sidecar stderr line nobody sees at default verbosity.
/// The host persists warning diagnostics into `travsr status`, so the SDK is
/// named where the user looks.
fn android_sdk_diagnostic(build_output: &str) -> Option<PluginDiagnostic> {
    let marker = ANDROID_SDK_FAILURE_MARKERS
        .iter()
        .find(|m| build_output.contains(**m))?;
    Some(PluginDiagnostic::warning(
        "java.android-sdk-missing",
        format!(
            "the Android SDK this Android Gradle Plugin build needs was not found \
             (Gradle said: \"{marker}\"), so the build could not configure and no Java \
             symbols or call edges were produced. Point ANDROID_HOME at an installed SDK \
             that has the platform and build-tools the project asks for, then re-run \
             `travsr init --semantic --force`."
        ),
    ))
}

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
            Ok(mut resp) => {
                travsr_lang_scip_reader::warn_if_test_scope_dark(
                    Language::Java,
                    req.files.as_deref(),
                    &mut resp,
                    TEST_SCOPE_CAUSE_HINT,
                );
                resp
            }
            Err(e) => {
                let detail = format!("{e:#}");
                tracing::warn!("scip-java failed for {}: {detail}", req.root.display());
                // A build that failed for want of the Android SDK says so in a
                // structured diagnostic the host keeps, rather than only in the
                // stderr line above (#904).
                InvokeResponse {
                    diagnostics: android_sdk_diagnostic(&detail).into_iter().collect(),
                    ..InvokeResponse::default()
                }
            }
        }
    }
}

static SCIP_JAVA_BIN: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

fn find_scip_java() -> Option<&'static std::path::PathBuf> {
    // Resolve through the shared PATHEXT-aware resolver: it checks PATH and the
    // toolchain-managed dirs (including ~/.travsr/bin, where `travsr lang install
    // java` writes the launcher) and, on Windows, finds `scip-java.cmd`, the
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
            "scip-java not found. Download from https://github.com/sourcegraph/scip-java/releases \
             and place in ~/.travsr/bin/scip-java"
        )
    })?;

    // scip-java writes a SCIP index file (not stdout); use a temp dir as scratch.
    let scratch = tempfile::tempdir().context("failed to create temp dir")?;
    let output_path = scratch.path().join("index.scip");

    // Maven: ask for `test-compile` explicitly instead of letting scip-java run
    // the project's default lifecycle.
    //
    // Two things go wrong with the default. It runs far past what SemanticDB
    // needs, so a goal that has nothing to do with indexing can fail the whole
    // run: on the pinned JSON-java fixture it reached maven-gpg-plugin and died
    // with `Cannot run program "gpg"`, producing an empty index and a Phase B
    // that knew nothing about the repo. And a project that skips test
    // compilation (`-Dmaven.test.skip=true` in `.mvn/maven.config`, a common
    // way to make such a build pass) never compiles its tests, so SemanticDB,
    // being a javac plugin, sees none of them and no call site in any test file
    // reaches the graph.
    //
    // `test-compile` is the earliest phase that compiles BOTH source roots and
    // it runs no tests, no javadoc, no signing and no deploy. Measured on that
    // fixture: the default fails outright, `test-compile` succeeds and the index
    // gains 59 test files.
    //
    // There is deliberately no fallback to the default lifecycle when
    // `test-compile` fails. The default is a superset of `test-compile`, so it
    // cannot succeed where `test-compile` failed on the test sources, which is
    // the only case a fallback would be for. What it would do is run the rest of
    // the project's own lifecycle inside the sandbox: its tests, and any verify,
    // signing or deploy plugin, all repo-controlled. That is the exact
    // invocation this call was written to stop making. When a repo's test scope
    // does not compile, the `test-scope-dark` diagnostic from the SCIP ingest
    // says so.
    //
    // `clean` and `--batch-mode` have to be passed here because a build command
    // given to scip-java REPLACES its default one, it does not extend it:
    // `IndexCommand.finalBuildCommand` returns the default
    // (`--batch-mode clean verify -DskipTests`) only when the build command is
    // empty. So anything the default supplied and this still needs must be
    // repeated.
    //
    // `clean` is the load-bearing one. scip-java always passes
    // `-Dmaven.compiler.useIncrementalCompilation=false`, which selects
    // maven-compiler-plugin's stale-source check, so only sources newer than
    // their class files recompile. This sidecar runs on the developer's live
    // working tree, where an already-built `target/classes` is the normal case,
    // not the edge case. Without `clean` maven then logs "Nothing to compile",
    // javac never runs, SemanticDB writes no `.semanticdb`, scip-java still
    // exits 0, and Phase B reports success with zero nodes.
    //
    // `--batch-mode` because the sandbox gives maven no terminal: interactive
    // mode colours the output with escape codes and can stop on a prompt that
    // nothing will ever answer.
    //
    // The leading `--` is required. Without it scip-java's own parser claims
    // `--batch-mode` and exits with "no such option"; `--` ends its options so
    // the rest is passed through to maven.
    //
    // `verify` and `-DskipTests` are deliberately not restored: dropping
    // `verify` is the whole point of naming a phase here, and `test-compile`
    // runs no tests, so skipping them is moot.
    let build_system = detect_build_system(root);
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("index").arg("--output").arg(&output_path);
    // Name the build tool instead of letting scip-java guess. It refuses to
    // guess when a repo carries markers for both (a stray `gradlew` next to a
    // `pom.xml` is enough): "Multiple build tools detected", exit 1, zero
    // symbols (#835). `detect_build_system` already made that call, and the
    // Gradle-specific arguments below are only right if scip-java runs Gradle.
    if let Some(name) = build_system.as_ref().map(BuildSystem::scip_java_name) {
        cmd.args(["--build-tool", name]);
    }
    let maven = matches!(build_system, Some(BuildSystem::Maven));
    if maven {
        // `-Dmaven.clean.failOnError=false` is what lets `clean` run inside the
        // sandbox. The host grants `target/` as a bind over a read-only repo
        // root, so maven-clean-plugin can delete everything INSIDE target/ but
        // not the `target` directory itself, and by default that one failure is
        // fatal: "Failed to delete /repo/target", BUILD FAILURE, no index.
        //
        // Measured on Linux (bwrap, arm64, maven 3.9): with the flag, clean
        // still clears the contents, the undeletable directory degrades to a
        // WARNING, javac runs and the build succeeds. Contents are all `clean`
        // was ever needed for here: scip-java passes
        // `-Dmaven.compiler.useIncrementalCompilation=false`, which selects the
        // stale-source check, and clearing the classes is what makes every
        // source stale again.
        cmd.args([
            "--",
            "--batch-mode",
            "-Dmaven.clean.failOnError=false",
            "clean",
            "test-compile",
        ]);
    } else if matches!(build_system, Some(BuildSystem::Gradle)) {
        // Gradle: add the AGP shim as a second init-script (#904). scip-java
        // keeps its own `--init-script`, `-P` and `-D` arguments whatever the
        // build command says; the command only replaces the task list, so the
        // tasks scip-java would have run are repeated here. Gradle runs
        // init-scripts in command-line order, so the shim's `afterEvaluate`
        // callbacks register after the plugin's and see its extra properties.
        let shim = scratch.path().join("travsr-agp-init.gradle");
        std::fs::write(&shim, AGP_INIT_SCRIPT_SHIM)
            .context("failed to write the AGP init-script")?;
        cmd.arg("--").arg("--init-script").arg(&shim);
        cmd.args(GRADLE_INDEX_TASKS);
    }
    cmd.current_dir(root);
    run_to_completion(cmd, "scip-java")?;

    let output_size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    tracing::info!("scip-java produced {output_size} bytes of SCIP output");

    travsr_lang_scip_reader::ingest(&output_path, corpus, Language::Java, root)
}

// ── Windows: travsr-driven build ───────────────────────────────────────────

/// Windows Phase B: travsr drives the build (see module docs) rather than
/// scip-java's broken build orchestration.
fn run_windows(req: &InvokeRequest) -> anyhow::Result<InvokeResponse> {
    // `InvokeRequest::root` arrives canonicalized with the extended-length
    // verbatim prefix (`\\?\D:\...`) on Windows. gradlew.bat and the Gradle
    // init-script cannot use a verbatim path, so strip it up front.
    let root = PathBuf::from(strip_windows_verbatim_prefix(&req.root.to_string_lossy()).as_ref());
    let root = root.as_path();

    let scip_java = find_scip_java()
        .ok_or_else(|| anyhow::anyhow!("scip-java not found. Run `travsr lang install java`"))?;
    let launcher = scip_java_launcher_jar(scip_java);
    let java = java_exe().context("no `java` found (set JAVA_HOME or put java on PATH)")?;
    let jar =
        jar_exe().context("no `jar` found (need a JDK, not just a JRE, on JAVA_HOME/PATH)")?;

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
            "no Gradle or Maven build file found under {}, cannot build for \
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

    travsr_lang_scip_reader::ingest(&output_path, req.corpus.as_str(), Language::Java, root)
}

/// Which build tool drives a Java project. Gradle wins when both are present:
/// a Gradle wrapper/build script is a stronger signal than a bare `pom.xml`.
#[derive(Debug, PartialEq, Eq)]
enum BuildSystem {
    Gradle,
    Maven,
}

impl BuildSystem {
    /// The name scip-java's `--build-tool` flag takes for this build system.
    /// Matched case-insensitively by both the 0.12.x and 0.13.x lines.
    fn scip_java_name(&self) -> &'static str {
        match self {
            BuildSystem::Gradle => "gradle",
            BuildSystem::Maven => "maven",
        }
    }
}

/// The Gradle tasks scip-java runs by default (its `GradleBuildTool`), repeated
/// wherever travsr supplies the build command itself: `clean` so every source
/// recompiles and emits `.semanticdb` fresh, then the plugin's
/// `scipPrintDependencies` and `scipCompileAll`.
const GRADLE_INDEX_TASKS: [&str; 3] = ["clean", "scipPrintDependencies", "scipCompileAll"];

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
        .args(GRADLE_INDEX_TASKS);
    run_to_completion(cmd, "gradle")?;

    Ok(targetroot)
}

/// Render the SemanticDB init-script. All paths are emitted with forward slashes
/// and no verbatim prefix: a backslash in a Groovy string literal is an escape,
/// which is exactly what breaks scip-java's own generated init-script on Windows.
/// `buildDir` is redirected out of the repo so the build needs no repo-write
/// grant (the sandbox binds the repo read-only). The AGP shim is appended so
/// Android modules are indexed too (#904, see [`AGP_INIT_SCRIPT_SHIM`]).
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
    ) + AGP_INIT_SCRIPT_SHIM
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
/// JDK's own `jar` tool (the same java.util.zip that runs the launcher), which
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
        .context(
            "scip-java_2.13 payload jar not found in the launcher. Is this a 0.12.x scip-java?",
        )?
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
    // Put the build in its own process group so the timeout path can signal the
    // whole tree. `Child::kill` reaches only the launcher; the JVM it starts, and
    // the `mvn`/`gradle` JVM under that, are what actually keep running.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
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
                kill_process_tree(&mut child);
                // Reap it: `Child::drop` does not wait, so bailing straight out
                // left a zombie behind. The drain threads are deliberately NOT
                // joined here. A grandchild that outlived the tree kill still
                // holds the write end of its pipe, and joining would then block
                // this sidecar until the host's own watchdog fired, turning one
                // language's clean failure into a crash that discards the whole
                // invoke. A detached thread on an abandoned process is a bounded
                // leak for the rest of this invocation; a wedged sidecar is not.
                let _ = child.wait();
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
    let src = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    const MAX: usize = 4000;
    if src.len() <= MAX {
        return src.to_string();
    }
    // Slicing a `&str` at a raw byte offset panics when the offset lands inside a
    // multibyte character, which any build printing a non-ASCII path or a
    // localized JVM message can produce. Walk forward to the next boundary.
    let mut start = src.len() - MAX;
    while start < src.len() && !src.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &src[start..])
}

/// Strip the Windows extended-length verbatim prefix (`\\?\`, `\\?\UNC\`).
/// `InvokeRequest::root` arrives canonicalized with this prefix on Windows;
/// gradlew and the Gradle init-script cannot use a verbatim path. No-op elsewhere.
fn strip_windows_verbatim_prefix(s: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        // `\\?\UNC\server\share` denotes the UNC path `\\server\share`; keep the
        // leading `\\` rather than degrading it to a bare relative `server\share`.
        std::borrow::Cow::Owned(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        std::borrow::Cow::Borrowed(rest)
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Terminate a spawned build process and its descendants. `Child::kill`
/// terminates only the immediate child (the gradlew/mvn launcher), leaving the
/// Gradle/JVM grandchildren running: on Windows `taskkill /T` kills the tree,
/// and on unix `run_to_completion` gives the child its own process group, so a
/// negative pid signals every descendant that has not left it.
///
/// PRECONDITION (unix): `child` MUST have been spawned with
/// `process_group(0)`. `run_to_completion` is the only spawner here and does
/// set it. Without it the child stays in THIS process's group, `-child.id()`
/// names that group, and the `kill -9` below takes the sidecar down with the
/// build it was trying to stop. Any new caller must set it too.
fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &child.id().to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        // `kill(2)` needs either libc or nix, neither of which this crate
        // depends on, so shell out the same way the Windows branch does.
        let _ = std::process::Command::new("kill")
            .args(["-9", &format!("-{}", child.id())])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = child.kill();
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_java=info".parse().unwrap())
                // The shared SCIP ingest crate is a different tracing target, so
                // without this its own diagnostics (empty index, test-blind
                // index) are filtered out and never reach stderr.
                .add_directive("travsr_lang_scip_reader=info".parse().unwrap()),
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
            strip_windows_verbatim_prefix(r"\\?\D:\com.travsr\repo").as_ref(),
            r"D:\com.travsr\repo"
        );
    }

    #[test]
    fn strips_verbatim_unc_prefix() {
        // `\\?\UNC\server\share` is the verbatim form of `\\server\share`; the
        // leading `\\` must survive, not degrade to a relative `server\share`.
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\UNC\server\share\repo").as_ref(),
            r"\\server\share\repo"
        );
    }

    #[test]
    fn strip_is_noop_on_plain_paths() {
        assert_eq!(
            strip_windows_verbatim_prefix(r"D:\repo").as_ref(),
            r"D:\repo"
        );
        assert_eq!(
            strip_windows_verbatim_prefix("/home/u/repo").as_ref(),
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

    // #904: the rendered Windows init-script carries the AGP shim, and the shim
    // is what an Android module needs from it: the javac plugin attached to the
    // module's own `JavaCompile` tasks, `scipCompileAll` made to depend on
    // them, and `scipPrintDependencies` (which cannot survive AGP 9) disabled.
    // Groovy-literal hygiene is asserted on the whole script, shim included.
    #[test]
    fn init_script_carries_the_agp_shim() {
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
        assert!(
            !script.contains('\\'),
            "no backslash may reach Groovy:\n{script}"
        );
        // The plugin is applied before the shim, so the shim's afterEvaluate
        // runs after the plugin's and finds `scipCompileAll`.
        let plugin_at = script
            .find("apply plugin: SemanticdbGradlePlugin")
            .expect("plugin");
        let shim_at = script.find("Android Gradle Plugin support").expect("shim");
        assert!(
            plugin_at < shim_at,
            "shim must follow the plugin application"
        );
        for needle in [
            r#"if (p.plugins.hasPlugin("java")) { return }"#,
            "p.tasks.withType(JavaCompile)",
            r#"p.tasks.named("scipCompileAll") { it.dependsOn(selected) }"#,
            "settingsEvaluated",
            r#"String.valueOf(mode.getOrNull()) == "FAIL_ON_PROJECT_REPOS""#,
            r#"Enum.valueOf(modes, "PREFER_SETTINGS")"#,
            r#"["compileOnly", "testCompileOnly"]"#,
            r#"["annotationProcessor", "testAnnotationProcessor"]"#,
            "if (javacVersion >= 17) { jvmArgs.addAll(moduleOptions) }",
            "t.doFirst {",
            "def reachable = ",
            r#"p.tasks.named("scipPrintDependencies") { it.enabled = false }"#,
            "-javaagent:",
            "management.repositories.each",
        ] {
            assert!(
                script.contains(needle),
                "init-script is missing `{needle}`:\n{script}"
            );
        }
    }

    // The shim never templates a path of its own: everything it needs comes
    // from the extra properties scip-java's init-script sets, for either plugin
    // generation. That is what lets one static fragment serve both the Windows
    // (0.12.x, `semanticdb`) and unix (0.13.x, `scip`) invocations.
    #[test]
    fn agp_shim_reads_both_plugin_generations_from_extra_properties() {
        for key in [
            "semanticdbTarget",
            "scipTarget",
            "javacPluginJar",
            "javacAgentPath",
        ] {
            assert!(
                AGP_INIT_SCRIPT_SHIM.contains(&format!("\"{key}\"")),
                "shim must read `{key}`"
            );
        }
        assert!(AGP_INIT_SCRIPT_SHIM.contains(r#""semanticdbTarget" ? "semanticdb" : "scip""#));
        assert!(!AGP_INIT_SCRIPT_SHIM.contains('\\'));
        // Release and instrumentation variants are left out, one build type is
        // enough; unit-test variants stay in, since they compile src/test/java.
        assert!(AGP_INIT_SCRIPT_SHIM.contains("AndroidTest|Release"));
        assert!(!AGP_INIT_SCRIPT_SHIM.contains("UnitTest"));
        // Plain Java builds are left to scip-java's plugin entirely: the
        // settings-repository re-add sits below the java check.
        let java_check = AGP_INIT_SCRIPT_SHIM
            .find(r#"hasPlugin("java")"#)
            .expect("java check");
        let repo_readd = AGP_INIT_SCRIPT_SHIM
            .find("management.repositories.each")
            .expect("repo re-add");
        assert!(
            java_check < repo_readd,
            "repository re-add must follow the java check"
        );
    }

    #[test]
    fn scip_java_build_tool_names_match_detection() {
        assert_eq!(BuildSystem::Gradle.scip_java_name(), "gradle");
        assert_eq!(BuildSystem::Maven.scip_java_name(), "maven");
        assert_eq!(
            GRADLE_INDEX_TASKS,
            ["clean", "scipPrintDependencies", "scipCompileAll"]
        );
    }

    // #904: a failed AGP build that names the SDK yields a diagnostic naming the
    // SDK; any other failure yields none and stays a plain stderr line.
    #[test]
    fn android_sdk_diagnostic_fires_only_on_agp_sdk_failures() {
        let missing = "gradle exited with exit code: 1:\nFAILURE: Build failed with an exception.\n\
                       * What went wrong:\nSDK location not found. Define a valid SDK location with an \
                       ANDROID_HOME environment variable or by setting the sdk.dir path in your \
                       project's local properties file at 'C:\\repo\\local.properties'.";
        let d = android_sdk_diagnostic(missing).expect("SDK failure must produce a diagnostic");
        assert_eq!(d.code, "java.android-sdk-missing");
        assert!(d.message.contains("ANDROID_HOME"), "{}", d.message);
        assert!(
            d.message.contains("SDK location not found"),
            "{}",
            d.message
        );

        let platform = "Failed to find Platform SDK with path: platforms;android-36";
        assert!(android_sdk_diagnostic(platform).is_some());

        let unrelated = "gradle exited with exit code: 1:\nerror: cannot find symbol Foo";
        assert!(android_sdk_diagnostic(unrelated).is_none());
        assert!(android_sdk_diagnostic("").is_none());
    }
}
