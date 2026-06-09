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
//! 6. Map each reference location to its enclosing symbol → `ref/call` edge
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
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use travsr_core::{Edge, EdgeKind, Language, Node, NodeId, VName};
use travsr_plugin_sdk::{
    run_plugin, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
};

const TIMEOUT_SECS: u64 = 600;
const PROGRESS_WAIT_SECS: u64 = 30;
const MAX_REFS_PER_SYMBOL: usize = 500;

// ── Binary lookup ─────────────────────────────────────────────────────────────

static KLS_BIN: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

fn kls_binary() -> Option<PathBuf> {
    KLS_BIN
        .get_or_init(|| {
            // 1. ~/.travsr/bin/kotlin-language-server
            if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
                let p = home.join(".travsr/bin/kotlin-language-server");
                if p.exists() {
                    return Some(p);
                }
            }
            // 2. PATH — probe via --help (exits 1 but proves binary exists)
            let ok = Command::new("kotlin-language-server")
                .arg("--help")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok();
            if ok {
                Some(PathBuf::from("kotlin-language-server"))
            } else {
                None
            }
        })
        .clone()
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
    uri: String,
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

    fn node_id(&self, corpus: &str, rel_path: &str) -> NodeId {
        VName::new(corpus, "", rel_path, "kotlin", self.signature()).id()
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
    fn wait_for_progress_end(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut active: i32 = 0;
        let mut any_begin = false;

        loop {
            match self.recv_one(deadline)? {
                None => {
                    tracing::debug!("progress wait timed out (active={active}), proceeding");
                    return Ok(());
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
                            return Ok(());
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

fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn uri_to_rel(root: &Path, uri: &str) -> Option<String> {
    let path_str = uri.strip_prefix("file://")?;
    let decoded = percent_decode(path_str);
    let p = Path::new(&decoded);
    p.strip_prefix(root)
        .ok()
        .map(|rel| rel.to_string_lossy().into_owned())
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(decoded) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00"),
                16,
            ) {
                out.push(decoded as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
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
fn flatten_doc_syms(arr: &[Value], container: &str, uri: &str, out: &mut Vec<DocSym>) {
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
            uri: uri.to_string(),
        });

        let child_container = if container.is_empty() {
            name.clone()
        } else {
            format!("{}.{}", container, name)
        };

        if let Some(children) = sym["children"].as_array() {
            flatten_doc_syms(children, &child_container, uri, out);
        }
    }
}

// ── Enclosing symbol lookup ───────────────────────────────────────────────────

fn range_contains(r: &LspRange, line: u64, character: u64) -> bool {
    let after_start =
        line > r.start.line || (line == r.start.line && character >= r.start.character);
    let before_end = line < r.end.line || (line == r.end.line && character <= r.end.character);
    after_start && before_end
}

fn range_lines(r: &LspRange) -> u64 {
    r.end.line.saturating_sub(r.start.line)
}

/// Return the most specific (smallest-range) symbol that contains `(line, character)`.
fn find_enclosing<'a>(
    sym_map: &'a HashMap<String, Vec<DocSym>>,
    uri: &str,
    line: u64,
    character: u64,
) -> Option<&'a DocSym> {
    sym_map
        .get(uri)?
        .iter()
        .filter(|s| range_contains(&s.range, line, character))
        .min_by_key(|s| range_lines(&s.range))
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
    tracing::info!("waiting for KLS to index {} …", root.display());
    session.wait_for_progress_end(Duration::from_secs(PROGRESS_WAIT_SECS))?;
    tracing::info!("KLS ready");

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
            flatten_doc_syms(arr, "", &uri, &mut syms);
        }
        tracing::debug!("{}: {} symbols", rel_path, syms.len());
        sym_map.insert(uri.clone(), syms);
        rel_map.insert(uri, rel_path.clone());
    }

    // 6. Build nodes + ref/call edges
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();

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
                .with_line(sym.sel_range.start.line.saturating_add(1) as u32),
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
                let ref_char = loc["range"]["start"]["character"].as_u64().unwrap_or(0);

                let ref_rel = match uri_to_rel(root, ref_uri) {
                    Some(r) => r,
                    None => continue,
                };

                let caller_id: NodeId = find_enclosing(&sym_map, ref_uri, ref_line, ref_char)
                    .map(|enc| {
                        let enc_rel = rel_map
                            .get(&enc.uri)
                            .map(|s| s.as_str())
                            .unwrap_or(ref_rel.as_str());
                        enc.node_id(corpus, enc_rel)
                    })
                    .unwrap_or_else(|| {
                        VName::new(corpus, "", &ref_rel, "kotlin", format!("file:{}", ref_rel)).id()
                    });

                edges.push(Edge::new(caller_id, def_id, EdgeKind::RefCall));
            }
        }
    }

    tracing::info!("KLS phase B: {} nodes, {} edges", nodes.len(), edges.len());

    session.shutdown(deadline);
    Ok(InvokeResponse { nodes, edges })
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
