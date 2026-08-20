//! Travsr Phase B — Kotlin semantic analysis via kotlin-language-server (KLS).
//!
//! Instead of wrapping Maven/Gradle directly, this sidecar drives KLS over
//! stdio using LSP.  KLS auto-detects the build system, resolves the
//! classpath, and answers full symbol + reference queries — regardless of
//! whether the project uses Maven or Gradle.
//!
//! ## Protocol flow
//!
//! 1. Spawn `kotlin-language-server` with `current_dir = project_root`
//! 2. LSP `initialize` / `initialized` handshake
//! 3. Drain `$/progress` notifications until KLS finishes indexing
//! 4. `textDocument/didOpen` + `textDocument/documentSymbol` per `.kt` file
//! 5. `textDocument/references` per defined symbol (using `selectionRange.start`)
//! 6. Emit each reference location as a `ScipRef`; the daemon's positional
//!    lookup re-homes it to the enclosing function (or a file node when none
//!    exists) and records the `ref/call` edge
//! 7. `shutdown` + `exit`
//!
//! ## Install KLS
//!
//! Download `server.zip` from github.com/fwcd/kotlin-language-server/releases
//! and place the `bin/kotlin-language-server` wrapper at
//! `~/.travsr/bin/kotlin-language-server`.
//!
//! ## Register
//!
//! ```text
//! travsr lang approve kotlin --approved-by <pse> \
//!   --reason "KLS semantic analysis — build-system agnostic" \
//!   --permitted-hosts repo1.maven.org,plugins.gradle.org
//! travsr lang add kotlin
//! ```

use anyhow::Context as _;
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use travsr_core::{Edge, Language, Node, ScipRef, VName};
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

const TIMEOUT_SECS: u64 = 600;
// #299 F12: cold Gradle/Maven dependency resolution routinely exceeds 30s, after
// which references ran against a half-built index and silently under-counted.
// Scale to the overall session budget and warn on timeout instead of proceeding
// silently. Overridable via TRAVSR_KLS_PROGRESS_WAIT_SECS for very large repos.
const PROGRESS_WAIT_SECS: u64 = 180;
const MAX_REFS_PER_SYMBOL: usize = 500;

// ── Binary lookup ─────────────────────────────────────────────────────────────

static KLS_BIN: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// Resolve the installed `kotlin-language-server` launcher.
///
/// `travsr lang install kotlin` writes a platform-appropriate launcher into
/// `~/.travsr/bin`: a `#!/bin/sh` script on unix, a `.cmd` on Windows (see
/// `install_zip_binary` in travsr-cli). A bare `Path::exists()`/`Command::new`
/// check on the extensionless name never finds the Windows `.cmd` — Windows
/// only auto-resolves `.exe` implicitly, not `.cmd`/`.bat` — so this silently
/// treated KLS as absent on every Windows machine, on both installed paths
/// (~/.travsr/bin and PATH), and Phase B degraded to a clean 0-node response
/// with no diagnostic. `travsr_core::exec::tool_path` is the shared,
/// PATHEXT-aware resolver travsr's own CLI (`analyzer_command_present`) and
/// sandbox layer already use for exactly this problem.
fn kls_binary() -> Option<PathBuf> {
    KLS_BIN
        .get_or_init(|| {
            managed_kls().or_else(|| travsr_core::exec::tool_path("kotlin-language-server"))
        })
        .clone()
}

/// Prefer travsr's own managed install in `~/.travsr/bin` over a possibly-stale
/// `kotlin-language-server` on `PATH`. `tool_path` searches `PATH` first, so
/// without this a system KLS would shadow the exact launcher
/// `travsr lang install kotlin` wrote (a `.cmd` on Windows, a shell script on
/// unix). Returns `None` when no managed launcher is present, falling back to
/// the PATHEXT-aware `tool_path` search.
fn managed_kls() -> Option<PathBuf> {
    let dir = travsr_home()?.join(".travsr").join("bin");
    let candidates: &[&str] = if cfg!(windows) {
        &[
            "kotlin-language-server.cmd",
            "kotlin-language-server.bat",
            "kotlin-language-server.exe",
        ]
    } else {
        &["kotlin-language-server"]
    };
    candidates
        .iter()
        .map(|name| dir.join(name))
        .find(|p| p.is_file())
}

/// The user's home directory, matching `travsr_core::exec`'s own resolution
/// (`USERPROFILE` first on Windows, `HOME` first elsewhere).
fn travsr_home() -> Option<PathBuf> {
    let (primary, secondary) = if cfg!(windows) {
        ("USERPROFILE", "HOME")
    } else {
        ("HOME", "USERPROFILE")
    };
    std::env::var_os(primary)
        .or_else(|| std::env::var_os(secondary))
        .map(PathBuf::from)
}

// ── Plugin ────────────────────────────────────────────────────────────────────

struct KotlinPhaseB;

impl Plugin for KotlinPhaseB {
    fn language(&self) -> Language {
        Language::Kotlin
    }
    fn extensions(&self) -> &[&str] {
        &["kt", "kts"]
    }
    fn supports_phase_b(&self) -> bool {
        kls_binary().is_some()
    }

    fn parse(&self, _req: &ParseRequest) -> ParseResponse {
        ParseResponse::default()
    }

    fn invoke_phase_b(&self, req: &InvokeRequest) -> InvokeResponse {
        match run_kls(&req.root, req.corpus.as_str()) {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("KLS phase B failed for {}: {e:#}", req.root.display());
                InvokeResponse::default()
            }
        }
    }
}

// ── LSP data types ────────────────────────────────────────────────────────────

#[derive(Clone)]
struct LspPos {
    line: u64,
    character: u64,
}

#[derive(Clone)]
struct LspRange {
    start: LspPos,
    end: LspPos,
}

#[derive(Clone)]
struct DocSym {
    name: String,
    kind: u64,
    range: LspRange,
    sel_range: LspRange,
    /// Dot-separated container path (e.g. `"Greeter"` for a method inside class Greeter).
    container: String,
}

impl DocSym {
    fn signature(&self) -> String {
        let prefix = kind_sig_prefix(self.kind);
        if self.container.is_empty() {
            format!("{}:{}", prefix, self.name)
        } else {
            format!("{}:{}.{}", prefix, self.container, self.name)
        }
    }

    fn kind_str(&self) -> &'static str {
        kind_to_str(self.kind)
    }
}

fn kind_sig_prefix(k: u64) -> &'static str {
    match k {
        5 => "class",
        6 | 9 => "method",
        10 => "enum",
        11 => "interface",
        12 => "fn",
        13 => "var",
        14 => "const",
        _ => "sym",
    }
}

fn kind_to_str(k: u64) -> &'static str {
    match k {
        5 => "class",
        6 => "method",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        _ => "symbol",
    }
}

// ── LSP framing ───────────────────────────────────────────────────────────────

fn read_lsp_msg<R: BufRead>(r: &mut R) -> anyhow::Result<Value> {
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        r.read_line(&mut line).context("read LSP header line")?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse().context("parse Content-Length")?;
        }
    }
    anyhow::ensure!(content_length > 0, "LSP message with zero content-length");
    let mut body = vec![0u8; content_length];
    r.read_exact(&mut body).context("read LSP body")?;
    serde_json::from_slice(&body).context("parse LSP JSON body")
}

// ── LSP session ───────────────────────────────────────────────────────────────

struct LspSession {
    child: Child,
    stdin: BufWriter<std::process::ChildStdin>,
    recv: mpsc::Receiver<anyhow::Result<Value>>,
    inbox: VecDeque<Value>,
    next_id: u64,
}

impl LspSession {
    fn new(mut child: Child) -> anyhow::Result<Self> {
        let stdin = child.stdin.take().context("child stdin not piped")?;
        let stdout = child.stdout.take().context("child stdout not piped")?;

        let (tx, recv) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_lsp_msg(&mut reader) {
                    Ok(msg) => {
                        if tx.send(Ok(msg)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        });

        Ok(LspSession {
            child,
            stdin: BufWriter::new(stdin),
            recv,
            inbox: VecDeque::new(),
            next_id: 1,
        })
    }

    fn write_msg(&mut self, msg: &Value) -> anyhow::Result<()> {
        let body = serde_json::to_string(msg).context("serialize LSP message")?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body)
            .context("write LSP message")?;
        self.stdin.flush().context("flush LSP stdin")
    }

    fn notify(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        self.write_msg(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))
    }

    /// Receive one message with deadline; returns `None` on timeout.
    fn recv_one(&mut self, deadline: Instant) -> anyhow::Result<Option<Value>> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        match self.recv.recv_timeout(remaining) {
            Ok(Ok(msg)) => Ok(Some(msg)),
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("KLS reader thread disconnected unexpectedly")
            }
        }
    }

    /// Send a request and return its `result` field (buffers notifications).
    fn request(&mut self, method: &str, params: Value, deadline: Instant) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_msg(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;

        loop {
            // Check inbox for an already-buffered response.
            let pos = self
                .inbox
                .iter()
                .position(|m| m.get("id").and_then(|v| v.as_u64()) == Some(id));
            if let Some(i) = pos {
                let msg = self
                    .inbox
                    .remove(i)
                    .ok_or_else(|| anyhow::anyhow!("inbox removal failed"))?;
                return extract_result(method, msg);
            }

            let msg = self
                .recv_one(deadline)?
                .with_context(|| format!("timeout waiting for LSP '{method}' id={id}"))?;

            if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                return extract_result(method, msg);
            }
            self.inbox.push_back(msg);
        }
    }

    /// Drain `$/progress` notifications until begin+end pair or timeout.
    /// Wait for KLS indexing to finish. Returns `Ok(true)` when the project's
    /// progress notifications ended cleanly, `Ok(false)` when the wait timed out
    /// (KLS still indexing) so the caller can warn — references gathered against a
    /// half-built index would silently under-count (#299 F12).
    fn wait_for_progress_end(&mut self, timeout: Duration) -> anyhow::Result<bool> {
        let deadline = Instant::now() + timeout;
        let mut active: i32 = 0;
        let mut any_begin = false;

        loop {
            match self.recv_one(deadline)? {
                None => {
                    tracing::debug!("progress wait timed out (active={active}), proceeding");
                    return Ok(false);
                }
                Some(msg) => {
                    if is_progress_begin(&msg) {
                        active += 1;
                        any_begin = true;
                        tracing::debug!(
                            "KLS progress begin (active={active}): {}",
                            msg["params"]["value"]["title"].as_str().unwrap_or("?")
                        );
                    } else if is_progress_end(&msg) {
                        active = (active - 1).max(0);
                        tracing::debug!("KLS progress end (active={active})");
                        if any_begin && active == 0 {
                            return Ok(true);
                        }
                    } else {
                        self.inbox.push_back(msg);
                    }
                }
            }
        }
    }

    fn shutdown(&mut self, deadline: Instant) {
        let _ = self.request("shutdown", json!(null), deadline);
        let _ = self.notify("exit", json!(null));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn extract_result(method: &str, msg: Value) -> anyhow::Result<Value> {
    if let Some(err) = msg.get("error") {
        anyhow::bail!("LSP error from '{method}': {err}");
    }
    Ok(msg["result"].clone())
}

fn is_progress_begin(msg: &Value) -> bool {
    msg.get("method").and_then(|m| m.as_str()) == Some("$/progress")
        && msg["params"]["value"]["kind"].as_str() == Some("begin")
}

fn is_progress_end(msg: &Value) -> bool {
    msg.get("method").and_then(|m| m.as_str()) == Some("$/progress")
        && msg["params"]["value"]["kind"].as_str() == Some("end")
}

// ── URI / path helpers ────────────────────────────────────────────────────────

/// `path.display()` on Windows yields backslashes and no leading slash before
/// the drive letter (e.g. `D:\repo\Foo.kt`). `file://` + that string is not a
/// valid URI — `java.net.URI.create` on the KLS side rejects it with
/// `Illegal character in authority`, since a bare backslash and an
/// unescaped drive-letter colon right after `//` are both illegal there. KLS
/// swallows the exception per-call (logs it, does not crash), so every
/// `initialize`/`didOpen`/`documentSymbol` request against a malformed URI
/// silently produced nothing — no error propagated back to this wrapper, no
/// symbols, no references, on every Windows machine. Rewriting to forward
/// slashes plus a leading `/` before the drive letter (`file:///D:/repo/Foo.kt`)
/// is the standard `file://` form for Windows paths and is what KLS's own
/// `documentSymbol`/`publishDiagnostics` responses come back as. A no-op on
/// unix, where `path.display()` already starts with `/`.
/// Strip the Windows extended-length verbatim prefix (`\\?\`, `\\?\UNC\`).
/// `repo_root`/`InvokeRequest::root` on Windows come through canonicalized —
/// confirmed live via a debug probe: `root` arrives as
/// `\\?\D:\com.travsr\testing\kotlinrepo`, not a plain drive path — so every
/// path built from it (and every path this wrapper walks under it) carries
/// the prefix too. Left unstripped, `path_to_uri` turned it into
/// `file:////?/D:/...` (four slashes, a literal `?`), which KLS's `initialize`
/// rejects as an invalid `rootUri`, failing the whole session before a single
/// file is ever opened. A no-op string when the prefix isn't present (unix,
/// or a caller-constructed path that was never canonicalized).
fn strip_windows_verbatim_prefix(s: &str) -> Cow<'_, str> {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        // `\\?\UNC\server\share` denotes the UNC path `\\server\share`; keep the
        // leading `\\` rather than degrading it to a bare relative `server\share`.
        Cow::Owned(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        Cow::Borrowed(rest)
    } else {
        Cow::Borrowed(s)
    }
}

fn path_to_uri(path: &Path) -> String {
    let raw = path.display().to_string();
    let mut s = strip_windows_verbatim_prefix(&raw).replace('\\', "/");
    if starts_with_drive_letter(&s) {
        s.insert(0, '/');
    }
    // Percent-encode so a path with spaces (e.g. `C:\Users\First Last\repo`) or
    // other non-URI bytes produces a URI `java.net.URI` accepts; KLS otherwise
    // throws `URISyntaxException` per call and the session completes empty. Path
    // separators and the drive colon stay literal.
    format!("file://{}", percent_encode_path(&s))
}

/// Percent-encode a file-URI path body: every byte except RFC 3986 unreserved
/// characters and the path-structural `/` and `:` becomes `%XX` (uppercase hex).
/// Non-ASCII UTF-8 bytes are encoded too, so the result is pure ASCII.
fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Inverse of `path_to_uri`: strips the same verbatim prefix from `root`
/// (so it matches the un-prefixed form baked into every URI `path_to_uri`
/// produced) and the leading `/` before a Windows drive letter, so
/// `Path::new` parses `/D:/repo/Foo.kt` as the drive-rooted path
/// `D:/repo/Foo.kt` — not a root-relative path with `D:` as an ordinary
/// segment, which `strip_prefix(root)` would never match. A no-op on unix.
fn uri_to_rel(root: &Path, uri: &str) -> Option<String> {
    let path_str = uri.strip_prefix("file://")?;
    let decoded = percent_decode(path_str);
    let decoded = decoded
        .strip_prefix('/')
        .filter(|rest| starts_with_drive_letter(rest))
        .unwrap_or(&decoded);
    // Case-fold the drive letter: KLS may echo a lowercase drive (`file:///d:/…`)
    // while `root` was sent with an uppercase one, and `strip_prefix` is
    // exact-case — without this every location silently misses (nodes=0).
    let decoded = normalize_drive_letter(decoded);
    let p = Path::new(decoded.as_ref());

    let root_raw = root.display().to_string();
    let root_stripped = strip_windows_verbatim_prefix(&root_raw).replace('\\', "/");
    let root_stripped = normalize_drive_letter(&root_stripped);

    p.strip_prefix(Path::new(root_stripped.as_ref()))
        .ok()
        .map(|rel| rel.to_string_lossy().into_owned())
}

fn starts_with_drive_letter(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Upper-case a leading Windows drive letter (`d:/…` → `D:/…`) so two spellings
/// of the same drive compare equal; a no-op on paths without a drive letter.
fn normalize_drive_letter(s: &str) -> Cow<'_, str> {
    if starts_with_drive_letter(s) && s.as_bytes()[0].is_ascii_lowercase() {
        let mut owned = s.to_string();
        owned[..1].make_ascii_uppercase();
        Cow::Owned(owned)
    } else {
        Cow::Borrowed(s)
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // A `%XX` escape decodes to one byte; accumulate raw bytes and interpret
        // the whole buffer as UTF-8 at the end so a multi-byte sequence like
        // `%C3%A9` becomes `é` rather than two mojibake chars.
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ── Parse helpers ─────────────────────────────────────────────────────────────

fn parse_pos(v: &Value) -> LspPos {
    LspPos {
        line: v["line"].as_u64().unwrap_or(0),
        character: v["character"].as_u64().unwrap_or(0),
    }
}

fn parse_range(v: &Value) -> LspRange {
    LspRange {
        start: parse_pos(&v["start"]),
        end: parse_pos(&v["end"]),
    }
}

/// Recursively flatten hierarchical `DocumentSymbol[]` into a flat `Vec<DocSym>`.
fn flatten_doc_syms(arr: &[Value], container: &str, out: &mut Vec<DocSym>) {
    for sym in arr {
        let name = sym["name"].as_str().unwrap_or("?").to_string();
        let kind = sym["kind"].as_u64().unwrap_or(0);

        // DocumentSymbol has selectionRange; SymbolInformation has location.
        let (range, sel_range) = if sym.get("selectionRange").is_some() {
            (
                parse_range(&sym["range"]),
                parse_range(&sym["selectionRange"]),
            )
        } else {
            let r = parse_range(&sym["location"]["range"]);
            (r.clone(), r)
        };

        out.push(DocSym {
            name: name.clone(),
            kind,
            range,
            sel_range,
            container: container.to_string(),
        });

        let child_container = if container.is_empty() {
            name.clone()
        } else {
            format!("{}.{}", container, name)
        };

        if let Some(children) = sym["children"].as_array() {
            flatten_doc_syms(children, &child_container, out);
        }
    }
}

// ── File walker ───────────────────────────────────────────────────────────────

fn collect_kt_files(root: &Path) -> Vec<(PathBuf, String)> {
    let mut result = Vec::new();
    collect_kt_recursive(root, root, &mut result);
    result
}

fn collect_kt_recursive(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == "target" || name_str == "build" || name_str.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_kt_recursive(root, &path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("kt") | Some("kts")
        ) {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push((path.clone(), rel.to_string_lossy().into_owned()));
            }
        }
    }
}

// ── Main orchestrator ─────────────────────────────────────────────────────────

fn run_kls(root: &Path, corpus: &str) -> anyhow::Result<InvokeResponse> {
    let kls = kls_binary().context(
        "kotlin-language-server not found — install at ~/.travsr/bin/kotlin-language-server",
    )?;

    let child = Command::new(&kls)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn kotlin-language-server")?;

    let mut session = LspSession::new(child)?;
    let deadline = Instant::now() + Duration::from_secs(TIMEOUT_SECS);
    let root_uri = path_to_uri(root);

    // 1. Initialize
    session.request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "documentSymbol": {
                        "hierarchicalDocumentSymbolSupport": true
                    }
                },
                "window": {
                    "workDoneProgress": true
                }
            }
        }),
        deadline,
    )?;

    // 2. Notify initialized — triggers KLS to start indexing the project
    session.notify("initialized", json!({}))?;

    // 3. Wait for KLS to finish indexing (Maven/Gradle dep resolution happens here)
    let progress_wait_secs = std::env::var("TRAVSR_KLS_PROGRESS_WAIT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(PROGRESS_WAIT_SECS);
    tracing::info!("waiting for KLS to index {} …", root.display());
    if session.wait_for_progress_end(Duration::from_secs(progress_wait_secs))? {
        tracing::info!("KLS ready");
    } else {
        // #299 F12: timed out while KLS was still indexing. documentSymbol /
        // references below run against an incomplete index, so the reference
        // counts may under-report. Surface it as a warning (not a silent debug)
        // and let the user raise TRAVSR_KLS_PROGRESS_WAIT_SECS for cold Gradle builds.
        tracing::warn!(
            "KLS did not finish indexing within {progress_wait_secs}s; \
             reference results may be incomplete for a cold/large Gradle project"
        );
    }

    // 4. Collect .kt files
    let kt_files = collect_kt_files(root);
    if kt_files.is_empty() {
        tracing::warn!("no .kt files found in {}", root.display());
        session.shutdown(deadline);
        return Ok(InvokeResponse::default());
    }

    // 5. Open each file + collect symbols via documentSymbol
    let mut sym_map: HashMap<String, Vec<DocSym>> = HashMap::new(); // uri → symbols
    let mut rel_map: HashMap<String, String> = HashMap::new(); // uri → rel_path

    for (abs_path, rel_path) in &kt_files {
        let uri = path_to_uri(abs_path);
        let text = std::fs::read_to_string(abs_path).unwrap_or_default();

        session.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "kotlin",
                    "version": 1,
                    "text": text
                }
            }),
        )?;

        let result = session.request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri } }),
            deadline,
        )?;

        let mut syms = Vec::new();
        if let Some(arr) = result.as_array() {
            flatten_doc_syms(arr, "", &mut syms);
        }
        tracing::debug!("{}: {} symbols", rel_path, syms.len());
        sym_map.insert(uri.clone(), syms);
        rel_map.insert(uri, rel_path.clone());
    }

    // 6. Build nodes + ref/call edges
    let mut nodes: Vec<Node> = Vec::new();
    // R6: no structural RefCall edges are built here — every reference carries
    // a ScipRef instead (see the loop below).
    let edges: Vec<Edge> = Vec::new();
    // #299 S1: occurrence records (path:line) so the daemon populates edge_sites
    // and find_references works. The LSP already hands us each reference location;
    // record it as a ScipRef instead of discarding the line into an edge.
    let mut refs: Vec<ScipRef> = Vec::new();

    // Collect all (uri, sym) pairs first to avoid borrowing issues
    let all_syms: Vec<(String, DocSym)> = sym_map
        .iter()
        .flat_map(|(uri, syms)| syms.iter().map(move |s| (uri.clone(), s.clone())))
        .collect();

    for (uri, sym) in &all_syms {
        let rel_path = match rel_map.get(uri.as_str()) {
            Some(r) => r.as_str(),
            None => continue,
        };

        let def_vname = VName::new(corpus, "", rel_path, "kotlin", sym.signature());
        let def_id = def_vname.id();
        nodes.push(
            Node::new(def_vname, sym.kind_str())
                .with_line(sym.sel_range.start.line.saturating_add(1) as u32)
                .with_end_line(sym.range.end.line.saturating_add(1) as u32),
        );

        let refs_val = match session.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": {
                    "line": sym.sel_range.start.line,
                    "character": sym.sel_range.start.character
                },
                "context": { "includeDeclaration": false }
            }),
            deadline,
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("references failed for {}: {e}", sym.signature());
                continue;
            }
        };

        if let Some(locs) = refs_val.as_array() {
            for loc in locs.iter().take(MAX_REFS_PER_SYMBOL) {
                let ref_uri = match loc["uri"].as_str() {
                    Some(u) => u,
                    None => continue,
                };
                let ref_line = loc["range"]["start"]["line"].as_u64().unwrap_or(0);

                let ref_rel = match uri_to_rel(root, ref_uri) {
                    Some(r) => r,
                    None => continue,
                };

                // R6 (mirrors swift/scala on this branch): every reference from
                // `textDocument/references` carries a real line, so a ScipRef is
                // always produced here — there is no line-less fallback case for
                // KLS. Emitting a structural edge from our own `find_enclosing`
                // symbol-range heuristic alongside it would always be redundant,
                // and the two are computed independently (KLS documentSymbol
                // ranges here vs. the daemon's function/method-only span table
                // in `write_scip_attributed_batch`), so they are not guaranteed
                // to agree — confirmed producing real spurious duplicate callers
                // on `travsr-test-fixtures/kotlin` (e.g. `class:Cat`, `class:Dog`,
                // `sym:Zoo.animals` all also duplicated as `file`-attributed
                // edges into `class:Animal`). The daemon's positional lookup
                // re-homes the ScipRef to the enclosing function, or a file node
                // when no enclosing function span exists.
                refs.push(ScipRef {
                    caller_path: ref_rel.clone(),
                    caller_line: (ref_line as u32).saturating_add(1),
                    callee_id: def_id,
                    // is_call (#650): no call/non-call signal available here;
                    // preserve prior behavior / wire default (default_true).
                    is_call: true,
                });
            }
        }
    }

    tracing::info!("KLS phase B: {} nodes, {} edges", nodes.len(), edges.len());

    session.shutdown(deadline);
    Ok(InvokeResponse {
        nodes,
        edges,
        refs,
        unresolved_calls: Vec::new(),
    })
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("travsr_lang_kotlin=info".parse().unwrap()),
        )
        .init();

    run_plugin(KotlinPhaseB);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression test for the Windows `file://` URI bug: `path_to_uri` used to
    // emit `file://D:\repo\Foo.kt` (backslashes, no leading slash before the
    // drive letter), which `java.net.URI.create` rejects with "Illegal
    // character in authority" on every LSP call — KLS swallowed the exception
    // per-request and silently returned nothing, so `documentSymbol` /
    // `references` never produced results on Windows even with a perfectly
    // valid project. Verified against a live `kotlin-language-server` process:
    // the pre-fix URI shape reproduced the exact exception; the post-fix shape
    // round-trips cleanly and KLS's own responses come back in this same form.
    #[cfg(windows)]
    #[test]
    fn path_to_uri_windows_drive_path_is_a_valid_file_uri() {
        let uri = path_to_uri(Path::new(r"D:\repo\Foo.kt"));
        assert_eq!(uri, "file:///D:/repo/Foo.kt");
    }

    #[cfg(windows)]
    #[test]
    fn uri_to_rel_windows_round_trips_through_path_to_uri() {
        let root = Path::new(r"D:\repo");
        let uri = path_to_uri(Path::new(r"D:\repo\src\Foo.kt"));
        assert_eq!(uri, "file:///D:/repo/src/Foo.kt");
        assert_eq!(uri_to_rel(root, &uri).as_deref(), Some("src/Foo.kt"));
    }

    // Regression test for the extended-length verbatim prefix: `InvokeRequest::root`
    // arrives as `\\?\D:\...` on Windows (confirmed live via a debug probe against
    // the real sandboxed indexer, not a synthetic case). Without stripping it,
    // `path_to_uri` produced `file:////?/D:/...` — KLS's `initialize` rejected this
    // `rootUri` outright, failing the whole session before any file was opened, so
    // even the prior (bare drive-letter) URI fix alone was not sufficient on a real
    // repo root.
    #[cfg(windows)]
    #[test]
    fn path_to_uri_strips_windows_verbatim_prefix() {
        let uri = path_to_uri(Path::new(r"\\?\D:\repo\Foo.kt"));
        assert_eq!(uri, "file:///D:/repo/Foo.kt");
    }

    #[cfg(windows)]
    #[test]
    fn uri_to_rel_strips_verbatim_prefix_from_root() {
        // `root` still carries the verbatim prefix (as it does in production);
        // `uri` is what `path_to_uri` actually produced for a file under it —
        // already prefix-free, matching what KLS itself echoes back.
        let root = Path::new(r"\\?\D:\repo");
        let uri = "file:///D:/repo/src/Foo.kt";
        assert_eq!(uri_to_rel(root, uri).as_deref(), Some("src/Foo.kt"));
    }

    #[cfg(unix)]
    #[test]
    fn path_to_uri_unix_absolute_path_unchanged() {
        let uri = path_to_uri(Path::new("/home/user/repo/Foo.kt"));
        assert_eq!(uri, "file:///home/user/repo/Foo.kt");
    }

    #[cfg(unix)]
    #[test]
    fn uri_to_rel_unix_round_trips_through_path_to_uri() {
        let root = Path::new("/home/user/repo");
        let uri = path_to_uri(Path::new("/home/user/repo/src/Foo.kt"));
        assert_eq!(uri_to_rel(root, &uri).as_deref(), Some("src/Foo.kt"));
    }

    // A user-profile path with a space (`C:\Users\First Last\…`, the most common
    // Windows layout) must percent-encode to a URI `java.net.URI` accepts, and
    // still round-trip back to the correct relative path.
    #[cfg(windows)]
    #[test]
    fn path_to_uri_percent_encodes_spaces_and_round_trips() {
        let root = Path::new(r"D:\Users\First Last\repo");
        let uri = path_to_uri(Path::new(r"D:\Users\First Last\repo\src\Foo.kt"));
        assert_eq!(uri, "file:///D:/Users/First%20Last/repo/src/Foo.kt");
        assert_eq!(uri_to_rel(root, &uri).as_deref(), Some("src/Foo.kt"));
    }

    // KLS may echo a lowercase drive letter while `root` carries an uppercase
    // one; the match must be case-insensitive on the drive or every location is
    // silently dropped.
    #[cfg(windows)]
    #[test]
    fn uri_to_rel_case_folds_drive_letter() {
        let root = Path::new(r"D:\repo");
        let uri = "file:///d:/repo/src/Foo.kt";
        assert_eq!(uri_to_rel(root, uri).as_deref(), Some("src/Foo.kt"));
    }

    #[test]
    fn percent_decode_is_utf8_correct() {
        assert_eq!(percent_decode("a%20b"), "a b");
        // `%C3%A9` is the two-byte UTF-8 encoding of `é`, not two chars.
        assert_eq!(percent_decode("caf%C3%A9"), "café");
        // A malformed escape is left literal rather than decoded to NUL.
        assert_eq!(percent_decode("100%zz"), "100%zz");
    }
}
