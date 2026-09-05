//! SCIP binary-format ingestion for travsr Phase B language packages.
//!
//! Reads a `.scip` protobuf index produced by any scip-* tool (scip-go,
//! scip-python, scip-java, scip-clang, etc.) and returns Travsr `Node`s and
//! `Edge`s to the daemon via the plugin protocol.
//!
//! # Usage
//!
//! ```ignore
//! let resp = travsr_lang_scip_reader::ingest(&output_path, "", Language::Go)?;
//! ```
//!
//! # What gets extracted
//!
//! **Nodes**: one per definition occurrence (where `symbol_roles & 1 != 0`).
//! Kind is inferred from `SymbolInformation.kind` (the SCIP `SymbolKind` enum)
//! with a descriptor-suffix fallback for symbols that lack `SymbolInformation`.
//!
//! **Edges**: two sources:
//! 1. `SymbolInformation.relationships`: explicit symbol-to-symbol edges
//!    (`is_reference` → `ref/call`, `is_implementation` → `is-implementation`).
//! 2. Reference occurrences (`symbol_roles & 1 == 0`) resolved against the
//!    definition table, emitted as `ref/call` edges from a synthetic file node
//!    to the definition node. Capped at [`MAX_REF_EDGES_PER_DOC`] per document.

use anyhow::Context as _;
use protobuf::Message as _;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use travsr_core::{Edge, EdgeKind, Language, Node, NodeId, ScipRef, VName};
use travsr_plugin_sdk::InvokeResponse;

/// Maximum reference-occurrence edges emitted per document (per SCIP document,
/// not per invocation). Keeps memory footprint bounded for large repos.
const MAX_REF_EDGES_PER_DOC: usize = 5_000;

/// Ingest a SCIP binary-format file and return Travsr nodes and edges.
///
/// `corpus` should be the canonical corpus string (e.g. `github.com/org/repo`).
/// Pass `""` when corpus is not known at plugin invocation time.
pub fn ingest(
    scip_path: &Path,
    corpus: &str,
    language: Language,
) -> anyhow::Result<InvokeResponse> {
    let bytes = std::fs::read(scip_path)
        .with_context(|| format!("failed to read SCIP file: {}", scip_path.display()))?;
    if bytes.is_empty() {
        tracing::warn!("SCIP file is empty: {}", scip_path.display());
        return Ok(InvokeResponse::default());
    }
    let index = scip::types::Index::parse_from_bytes(&bytes)
        .with_context(|| format!("failed to parse SCIP protobuf from {}", scip_path.display()))?;
    ingest_index(&index, corpus, language)
}

/// Ingest an already-decoded [`scip::types::Index`] (useful for unit tests).
pub fn ingest_index(
    index: &scip::types::Index,
    corpus: &str,
    language: Language,
) -> anyhow::Result<InvokeResponse> {
    let lang_str = language.as_str();
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();

    // #299 C1: C and C++ share one indexer (scip-clang) and one compile database,
    // so the `.h`-triggered C pass and the `.cpp`-triggered C++ pass both index the
    // whole project and emit the SAME symbols, under two different language tags,
    // producing duplicate nodes (`Animal#describe` as both `c` and `cpp`). Derive
    // each node's language from its file extension instead of the pass, so both
    // passes tag a given file identically and the duplicates collapse to one node
    // (identical VName → identical NodeId → deduped downstream).
    let c_family = matches!(language, Language::C | Language::Cpp);
    let doc_language = |path: &str| -> &'static str {
        if c_family {
            path.rsplit('.')
                .next()
                .and_then(Language::from_extension)
                .map(Language::as_str)
                .unwrap_or(lang_str)
        } else {
            lang_str
        }
    };

    // symbol_string → NodeId; populated in pass 1, consumed in passes 2 & 3.
    let mut def_ids: HashMap<String, NodeId> = HashMap::new();
    // Track which def_ids entries came from a header declaration so a later
    // source-file definition of the same symbol can take precedence (C/C++:
    // references must resolve to the definition, which carries the sites).
    let mut def_from_header: HashMap<String, bool> = HashMap::new();
    // #449: per-document definition spans (start_line, end_line, NodeId),
    // 1-indexed, used by pass 3b to attribute a cross-language reference to
    // its innermost enclosing definition.
    let mut doc_def_spans: HashMap<String, Vec<(u32, u32, NodeId)>> = HashMap::new();

    // ── Pass 1: definition nodes ─────────────────────────────────────────────
    for doc in &index.documents {
        let path = &doc.relative_path;

        // Build kind lookup from SymbolInformation declared in this document.
        let kind_map: HashMap<&str, String> = doc
            .symbols
            .iter()
            .map(|si| (si.symbol.as_str(), kind_from_scip_kind(si.kind)))
            .collect();

        for occ in &doc.occurrences {
            if occ.symbol.is_empty() {
                continue;
            }
            // SymbolRole::Definition == 1 in the SCIP proto.
            if (occ.symbol_roles & 1) == 0 {
                continue;
            }
            // G3: SCIP anonymous locals (`local N`) are intra-function noise:
            // on kubernetes they are 77% of all definitions. Drop at ingest.
            if occ.symbol.starts_with("local ") {
                continue;
            }
            // Parameter descriptors (`…#Greet().(name)`, terminal `(…)`) are the
            // same class of signature-local noise as `local N`: scip-dotnet emits
            // them as definitions (scip-go/scip-java do not), and without this
            // they surface as raw `scip:…(name)` graph nodes. Drop at ingest.
            if is_parameter_descriptor(&occ.symbol) {
                continue;
            }

            // G2: prefer `enclosing_range` (the full declaration body span,
            // e.g. a Go function's `{ … }` block) over `range` (just the name
            // token). Without the body span the enclosing-function lookup in
            // write_scip_attributed_batch can only match refs that sit on the
            // definition line itself.
            let enclosing_range = scip_enclosing_range(occ);
            let occ_range = scip_range(occ);
            let span: &[i32] = if enclosing_range.len() >= 3 {
                &enclosing_range
            } else {
                &occ_range
            };
            let line = span.first().copied().unwrap_or(0) as u32 + 1;
            // SCIP range encoding: 3-element = [start_line, start_col, end_col] (single line);
            // 4-element = [start_line, start_col, end_line, end_col] (multi-line).
            let end_line = if span.len() >= 4 {
                span[2] as u32 + 1
            } else {
                line // single-line declaration: end == start
            };
            let kind = kind_map
                .get(occ.symbol.as_str())
                .cloned()
                .unwrap_or_else(|| kind_from_symbol_string(&occ.symbol));

            let sig = format!("scip:{}:{}", path, occ.symbol);
            let node_lang = doc_language(path);
            let vname = VName::new(corpus, "", path, node_lang, &sig);
            let node_id = vname.id();
            // Prefer a source-file definition over a header declaration as the
            // canonical target for references (C/C++). First non-header wins;
            // otherwise first seen.
            let is_header = is_header_path(path);
            match def_from_header.get(&occ.symbol) {
                Some(false) => {} // already have a source def, keep it
                Some(true) if is_header => {}
                _ => {
                    def_ids.insert(occ.symbol.clone(), node_id);
                    def_from_header.insert(occ.symbol.clone(), is_header);
                }
            }
            doc_def_spans
                .entry(path.clone())
                .or_default()
                .push((line, end_line, node_id));
            nodes.push(
                Node::new(vname, &kind)
                    .with_line(line)
                    .with_end_line(end_line),
            );
        }
    }

    // ── Pass 2: structured relationship edges ────────────────────────────────
    // SymbolInformation.relationships gives explicit symbol → symbol edges that
    // SCIP tools emit when they have high confidence (e.g. go-to-definition).
    for doc in &index.documents {
        for sym_info in &doc.symbols {
            let Some(&src_id) = def_ids.get(sym_info.symbol.as_str()) else {
                continue;
            };
            for rel in &sym_info.relationships {
                if rel.symbol.is_empty() {
                    continue;
                }
                let Some(&dst_id) = def_ids.get(rel.symbol.as_str()) else {
                    continue;
                };
                let edge_kind = if rel.is_reference {
                    EdgeKind::RefCall
                } else if rel.is_implementation {
                    EdgeKind::IsImplementation
                } else {
                    continue;
                };
                edges.push(Edge::new(src_id, dst_id, edge_kind));
            }
        }
    }

    // ── Pass 3: reference-occurrence → ScipRef records (G2 attribution) ────────
    // Each occurrence carries a source line which `write_scip_attributed_batch`
    // uses to find the innermost enclosing function span and re-home the
    // ref/call edge from file → function (RFC-014 G2).
    let mut refs: Vec<ScipRef> = Vec::new();
    // #449 pass 3b: cross-language references. An ObjC → Swift bridged call
    // (`[Type registerEnvironments]`) references a symbol whose definition
    // lives in the *Swift* index, so it can never appear in this index's
    // def_ids. Instead of dropping it, hand it to the daemon as an
    // `UnresolvedCall` (src = innermost enclosing definition, callee_sig =
    // Phase A leading-keyword convention). `resolve_unresolved_calls` then
    // emits the RefCall edge + edge_sites row against the store. Gated to
    // ObjC: for go/java/c/c++ a ref without an in-index def is a dependency
    // or stdlib symbol, and converting those would flood the resolver.
    let cross_lang = matches!(language, Language::ObjectiveC);
    let mut unresolved: Vec<travsr_core::UnresolvedCall> = Vec::new();
    let project_root = index
        .metadata
        .as_ref()
        .map(|m| m.project_root.clone())
        .unwrap_or_default();
    for doc in &index.documents {
        let path = &doc.relative_path;
        // #813: the document's source, read once, so each occurrence's column can
        // be accepted only where every SCIP encoding agrees on what it means.
        // `None` (no text, unreadable file) simply leaves every column unset.
        let doc_src = document_text(doc, &project_root);
        let doc_lines: Vec<&str> = doc_src
            .as_deref()
            .map(|t| t.lines().collect())
            .unwrap_or_default();
        // #299 C1: scip-clang emits a reference occurrence at a symbol's *header
        // declaration* line (not a call site). Attributing those to a header
        // pollutes find_references with a spurious "use" on the declaration.
        // Header occurrences for C/C++ are declarations, not call sites, so skip.
        if c_family && is_header_path(path) {
            continue;
        }
        let mut count = 0usize;

        for occ in &doc.occurrences {
            if occ.symbol.is_empty() {
                continue;
            }
            if (occ.symbol_roles & 1) != 0 {
                continue; // skip definitions
            }
            // G3: references to anonymous locals are dropped with their defs.
            if occ.symbol.starts_with("local ") {
                continue;
            }
            // …and to parameters, dropped with their defs above.
            if is_parameter_descriptor(&occ.symbol) {
                continue;
            }
            if count >= MAX_REF_EDGES_PER_DOC {
                break;
            }
            // #299 F4: a range-less occurrence (malformed / partial SCIP)
            // carries no real position, so skip it rather than fabricating a
            // phantom `path:1` reference via `unwrap_or(1)`.
            let Some(start) = scip_range(occ).first().copied() else {
                continue;
            };
            let caller_line = start as u32 + 1;
            // #813: range element [1] is the occurrence's start character (see
            // `scip_range`). Accept it only inside the encoding-agnostic window.
            let caller_col = scip_range(occ)
                .get(1)
                .copied()
                .zip(doc_lines.get(start as usize))
                .and_then(|(c, line)| encoding_agnostic_col(line, c));
            if let Some(&callee_id) = def_ids.get(occ.symbol.as_str()) {
                refs.push(ScipRef {
                    caller_path: path.clone(),
                    caller_line,
                    callee_id,
                    // travsr-core added is_call to separate real calls from
                    // non-call refs (#650). SCIP occurrences carry no call/
                    // non-call signal we can key off here, so preserve prior
                    // behavior and match the wire default (default_true): every
                    // ingested occurrence is treated as a call. Refining this per
                    // symbol_role is a separate change.
                    is_call: true,
                    // #813: the occurrence's own column, kept only where the
                    // encoding cannot change its meaning (`encoding_agnostic_col`).
                    // `None` outside that window, so the daemon name-searches the
                    // line as it did before this field existed (no regression).
                    caller_col,
                });
                count += 1;
            } else if cross_lang {
                let Some(callee_sig) = leading_keyword_sig(&occ.symbol) else {
                    continue;
                };
                // Attribute to the innermost enclosing definition; a ref with
                // no enclosing def (top-level) is skipped rather than pinned
                // to the file.
                let Some(src) = innermost_enclosing_def(doc_def_spans.get(path), caller_line)
                else {
                    continue;
                };
                unresolved.push(travsr_core::UnresolvedCall {
                    src,
                    callee_sig,
                    alt_callee_sig: None,
                    hint_crate: None,
                    caller_line,
                    // SCIP cross-language refs carry no syntactic receiver type,
                    // so the daemon's #529 receiver-resolution path does not
                    // apply here. Neutral values keep the pre-#529 leaf-name
                    // resolution behavior for these edges.
                    is_method_call: false,
                    recv_type: None,
                    // #813: same encoding-agnostic column as the ScipRef case
                    // above; `None` when the unit would be ambiguous.
                    caller_col,
                });
                count += 1;
            }
        }
    }

    tracing::info!(
        nodes = nodes.len(),
        structural_edges = edges.len(),
        refs = refs.len(),
        unresolved_calls = unresolved.len(),
        "SCIP ingestion complete"
    );

    Ok(InvokeResponse {
        nodes,
        edges,
        refs,
        unresolved_calls: unresolved,
    })
}

/// The occurrence column, but only when it means the same thing under every
/// encoding SCIP permits.
///
/// `Document.position_encoding` may be UTF-8, UTF-16 or UTF-32, and in practice
/// it is often absent altogether: a real scip-ruby index emits no encoding field
/// at all, which decodes as `Unspecified`. So the wire does not reliably say
/// which unit `start_character` is counted in, and guessing wrong writes a wrong
/// editor position, which is a wrong edge (#813).
///
/// There is one window where no guess is required. While the line's prefix is
/// pure ASCII, one code unit is one byte under all four encodings, so the offset
/// is numerically identical whichever the indexer used. Measured over the 39,555
/// committed occurrences of the travsr repository, 99.985% fall inside it.
///
/// Outside that window (a non-ASCII character sits before the occurrence) the
/// unit is genuinely ambiguous, so this returns `None` and the daemon falls back
/// to its word-boundary name search, exactly as it did before the field existed.
/// Narrowing that remaining 0.015% needs per-emitter encoding evidence, not a
/// default.
pub fn encoding_agnostic_col(line: &str, character: i32) -> Option<u32> {
    let col = usize::try_from(character).ok()?;
    let bytes = line.as_bytes();
    if col > bytes.len() {
        return None;
    }
    bytes[..col].is_ascii().then_some(col as u32)
}

/// What a producer's reported column counts (#813).
///
/// Travsr's occurrence store keeps a 0-based UTF-8 byte column. A producer we
/// own declares its unit, so the consumer converts instead of guessing. A
/// producer we do not own (a third-party SCIP indexer) declares nothing
/// reliable, so [`ColUnit::Unknown`] falls back to the encoding-agnostic window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColUnit {
    /// Already a UTF-8 byte offset: used as-is once bounds-checked.
    Utf8,
    /// UTF-16 code units, converted against the line.
    Utf16,
    /// Not declared, or declared as something we do not know: keep the column
    /// only where every encoding agrees ([`encoding_agnostic_col`]).
    Unknown,
}

impl ColUnit {
    /// Parse a producer's `col_unit` declaration. Anything unrecognised, or
    /// absent, is [`ColUnit::Unknown`] and stays conservative.
    pub fn parse(declared: Option<&str>) -> Self {
        match declared {
            Some("utf8") => Self::Utf8,
            Some("utf16") => Self::Utf16,
            _ => Self::Unknown,
        }
    }
}

/// The 0-based UTF-8 byte column for `character` on `line`, given what the
/// producer says `character` counts.
///
/// Returns `None` when the position does not exist on this line, or when it
/// lands inside a character (a UTF-16 column pointing at the low half of a
/// surrogate pair), because half a character is not a position an editor can
/// resolve.
pub fn col_in_unit(line: &str, character: i32, unit: ColUnit) -> Option<u32> {
    let target = usize::try_from(character).ok()?;
    match unit {
        ColUnit::Unknown => encoding_agnostic_col(line, character),
        ColUnit::Utf8 => {
            (target <= line.len() && line.is_char_boundary(target)).then_some(target as u32)
        }
        ColUnit::Utf16 => {
            let mut units = 0usize;
            for (byte_idx, ch) in line.char_indices() {
                if units == target {
                    return Some(byte_idx as u32);
                }
                units += ch.len_utf16();
                if units > target {
                    return None; // inside a surrogate pair
                }
            }
            (units == target).then_some(line.len() as u32)
        }
    }
}

/// Line cache for readers whose occurrences arrive without source attached
/// (the LSP and SemanticDB backed languages), so a file is read at most once.
///
/// Exists so every language applies the *same* column-safety rule as the SCIP
/// path rather than each reimplementing it: a mis-slotted column is a wrong
/// editor position, and that is the one failure this rule exists to prevent.
#[derive(Default)]
pub struct SourceCache {
    files: std::collections::HashMap<std::path::PathBuf, Option<Vec<String>>>,
}

impl SourceCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// [`Self::col`] for a producer that declares its unit.
    pub fn col_in(&mut self, path: &Path, line: u32, character: i32, unit: ColUnit) -> Option<u32> {
        let entry = self.files.entry(path.to_path_buf()).or_insert_with(|| {
            std::fs::read_to_string(path)
                .ok()
                .map(|t| t.lines().map(|l| l.to_string()).collect())
        });
        let lines = entry.as_ref()?;
        let idx = usize::try_from(line.checked_sub(1)?).ok()?;
        col_in_unit(lines.get(idx)?, character, unit)
    }

    /// The byte column for `character` on 1-based `line` of `path`, or `None`
    /// when the file is unreadable, the line is missing, or the encoding could
    /// change what `character` means (see [`encoding_agnostic_col`]).
    pub fn col(&mut self, path: &Path, line: u32, character: i32) -> Option<u32> {
        let entry = self.files.entry(path.to_path_buf()).or_insert_with(|| {
            std::fs::read_to_string(path)
                .ok()
                .map(|t| t.lines().map(|l| l.to_string()).collect())
        });
        let lines = entry.as_ref()?;
        let idx = usize::try_from(line.checked_sub(1)?).ok()?;
        encoding_agnostic_col(lines.get(idx)?, character)
    }
}

/// Source lines for `doc`, or `None` when the text cannot be obtained.
///
/// Prefers the document's inlined `text`; the SCIP spec marks that optional and
/// says indexers are not expected to include it, in which case the client should
/// resolve `Index.metadata.project_root` against `Document.relative_path`. That
/// is why this needs no repo-root argument: the index carries its own root.
fn document_text(doc: &scip::types::Document, project_root: &str) -> Option<String> {
    if !doc.text.is_empty() {
        return Some(doc.text.clone());
    }
    let root = project_root.strip_prefix("file://").unwrap_or(project_root);
    if root.is_empty() {
        return None;
    }
    std::fs::read_to_string(Path::new(root).join(&doc.relative_path)).ok()
}

/// Occurrence `range` as the packed `[start_line, start_col, end_col]` (single
/// line) or `[start_line, start_col, end_line, end_col]` (multi-line) array the
/// rest of the reader expects, tolerant of both SCIP wire encodings.
///
/// scip-go/scip-python/scip-ruby/scip-clang emit `range` as the deprecated
/// packed `int32` array (proto field 1). scip-java 0.13.1 (and any producer on
/// a current `scip.proto`) instead sets the `typed_range` oneof: either
/// `single_line_range` (field 8, a `SingleLineRange { line, start_character,
/// end_character }`) or `multi_line_range` (field 9, a `MultiLineRange {
/// start_line, start_character, end_line, end_character }`). The `scip` 0.7
/// crate only knows fields 1..=7, so the typed arm lands in `unknown_fields`
/// and `occ.range` is empty; without recovery every Java reference occurrence
/// is dropped (issue #724).
///
/// Precedence: the spec says `typed_range` wins when both are set, but no known
/// producer sets both (scip-java sets only typed, the packed producers only
/// packed), so the packed field is taken first to borrow without allocating.
fn scip_range(occ: &scip::types::Occurrence) -> Cow<'_, [i32]> {
    if !occ.range.is_empty() {
        return Cow::Borrowed(&occ.range);
    }
    Cow::Owned(decode_typed_range(occ, 8, 9))
}

/// Occurrence `enclosing_range` with the same dual-encoding tolerance as
/// [`scip_range`]: the deprecated packed `int32` array (field 7), or the
/// `typed_enclosing_range` oneof: `single_line_enclosing_range` (field 10) or
/// `multi_line_enclosing_range` (field 11).
fn scip_enclosing_range(occ: &scip::types::Occurrence) -> Cow<'_, [i32]> {
    if !occ.enclosing_range.is_empty() {
        return Cow::Borrowed(&occ.enclosing_range);
    }
    Cow::Owned(decode_typed_range(occ, 10, 11))
}

/// Decode a `typed_range` / `typed_enclosing_range` oneof arm from the
/// occurrence's unknown fields into the packed `int32` array. `single` is the
/// field number of the `SingleLineRange` arm (8 for range, 10 for enclosing),
/// `multi` the `MultiLineRange` arm (9 / 11). Returns empty when neither arm is
/// present (a genuinely range-less occurrence, handled by the caller's guard).
fn decode_typed_range(occ: &scip::types::Occurrence, single: u32, multi: u32) -> Vec<i32> {
    for (num, value) in occ.special_fields.unknown_fields() {
        // SingleLineRange packs to 3 elements, MultiLineRange to 4. Key the
        // arity off which field number carried the message, not off how many
        // fields the wire happened to contain (see zero-elision note below).
        let arity = if num == single {
            3
        } else if num == multi {
            4
        } else {
            continue;
        };
        if let protobuf::UnknownValueRef::LengthDelimited(bytes) = value {
            return parse_range_submessage(bytes, arity);
        }
    }
    Vec::new()
}

/// Parse a `SingleLineRange` (arity 3) or `MultiLineRange` (arity 4) sub-message
/// into the packed array. Sub-message field `i` (1-based) is packed element
/// `i-1`, all varints.
///
/// proto3 does not serialize zero-valued scalar fields, so a missing field
/// number means that element is 0. Fill the fixed-arity slots (defaulting to 0)
/// rather than truncating at the first gap. Otherwise an occurrence on line 0
/// (the first line of a file) or starting at column 0 (a top-level declaration)
/// would decode short or empty and its reference would be dropped (#724).
fn parse_range_submessage(bytes: &[u8], arity: usize) -> Vec<i32> {
    let mut out = vec![0i32; arity];
    let mut i = 0usize;
    while i < bytes.len() {
        let Some((tag, next)) = read_varint(bytes, i) else {
            break;
        };
        i = next;
        let field = (tag >> 3) as usize;
        let wire = tag & 0x7;
        if wire != 0 {
            break; // range elements are all varints; anything else is malformed
        }
        let Some((val, next)) = read_varint(bytes, i) else {
            break;
        };
        i = next;
        if (1..=arity).contains(&field) {
            out[field - 1] = val as i32;
        }
    }
    out
}

/// Minimal protobuf varint reader: returns the decoded value and the index just
/// past it, or `None` if the buffer ends mid-varint.
fn read_varint(bytes: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let mut shift = 0u32;
    let mut result = 0u64;
    loop {
        let byte = *bytes.get(i)?;
        i += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((result, i));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

/// #449: Phase A leading-keyword signature for a SCIP *method* symbol whose
/// definition is not in this index (cross-language target).
///
/// `objc . corp 0.0.0 ClassC#registerEnvironments().` → `fn:registerEnvironments`
/// `objc . corp 0.0.0 Foo#setWidth:height:().`        → `fn:setWidth`
///
/// Returns `None` for non-method descriptors (classes, properties, modules),
/// since only calls are worth an `UnresolvedCall`.
fn leading_keyword_sig(symbol: &str) -> Option<String> {
    let leaf = symbol.split_whitespace().last()?;
    let body = leaf.strip_suffix("().")?;
    let name = body.rsplit_once('#').map(|(_, n)| n).unwrap_or(body);
    let leading = name.split(':').next().unwrap_or(name);
    let leading = leading.trim_matches('`');
    if leading.is_empty() {
        return None;
    }
    Some(format!("fn:{leading}"))
}

/// #449: innermost (smallest-span) definition whose `[start, end]` line range
/// contains `line`. Spans are 1-indexed inclusive.
fn innermost_enclosing_def(spans: Option<&Vec<(u32, u32, NodeId)>>, line: u32) -> Option<NodeId> {
    spans?
        .iter()
        .filter(|(start, end, _)| *start <= line && line <= *end)
        .min_by_key(|(start, end, _)| end - start)
        .map(|&(_, _, id)| id)
}

/// Map a SCIP `SymbolInformation.kind` field to a Travsr kind string.
///
/// Uses enum variant matching so the mapping stays correct regardless of the
/// integer values assigned by the protobuf code generator.
fn kind_from_scip_kind(
    kind: protobuf::EnumOrUnknown<scip::types::symbol_information::Kind>,
) -> String {
    use scip::types::symbol_information::Kind;
    match kind.enum_value_or_default() {
        Kind::Class
        | Kind::Interface
        | Kind::Enum
        | Kind::Struct
        | Kind::Trait
        | Kind::SingletonClass
        | Kind::TypeClass
        | Kind::Mixin
        | Kind::Extension => "class",

        Kind::Function
        | Kind::Method
        | Kind::StaticMethod
        | Kind::AbstractMethod
        | Kind::PureVirtualMethod
        | Kind::TraitMethod
        | Kind::MethodSpecification
        | Kind::MethodAlias
        | Kind::Accessor
        | Kind::Getter
        | Kind::Setter
        | Kind::Subscript
        | Kind::Operator
        | Kind::SingletonMethod
        | Kind::TypeClassMethod => "function",

        Kind::Constructor => "constructor",

        Kind::Field
        | Kind::StaticField
        | Kind::StaticDataMember
        | Kind::StaticProperty
        | Kind::Attribute => "field",

        Kind::Variable
        | Kind::StaticVariable
        | Kind::Constant
        | Kind::EnumMember
        | Kind::Parameter => "variable",

        Kind::Module | Kind::Namespace | Kind::Package | Kind::PackageObject | Kind::File => {
            "module"
        }

        Kind::Type
        | Kind::TypeAlias
        | Kind::Union
        | Kind::TypeParameter
        | Kind::AssociatedType
        | Kind::TypeFamily => "type",

        Kind::Macro => "macro",

        // UnspecifiedKind and any future variants fall through to the default.
        _ => "definition",
    }
    .into()
}

/// Whether a repo-relative path is a C/C++ header (declaration site). Used to
/// prefer a source-file definition over a header declaration when resolving the
/// canonical node for a symbol's references (#299 C1).
fn is_header_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".h") || p.ends_with(".hpp") || p.ends_with(".hh") || p.ends_with(".hxx")
}

/// A SCIP symbol whose terminal descriptor is a Parameter (`…().(name)`, ending
/// in `)`). These are signature-local, like `local N`, and are dropped from the
/// graph so they don't surface as raw `scip:` nodes. Method descriptors
/// terminate with `.` (`Greet().`), types with `#`, modules with `/`, so only a
/// Parameter descriptor ends with `)`.
fn is_parameter_descriptor(symbol: &str) -> bool {
    symbol.ends_with(')')
}

/// Fallback kind inference from the SCIP descriptor suffix when
/// `SymbolInformation.kind == 0` (UnspecifiedKind).
fn kind_from_symbol_string(symbol: &str) -> String {
    if symbol.ends_with('#') {
        "class"
    } else if symbol.ends_with('/') {
        "module"
    } else if symbol.ends_with('.') {
        "function"
    } else {
        "definition"
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::Language;

    #[test]
    fn parameter_descriptor_detection() {
        // scip-dotnet parameter symbol (terminal `(name)`) → dropped.
        assert!(is_parameter_descriptor(
            "scip-dotnet nuget . . Demo/Greeter#Greet().(name)"
        ));
        // real navigable symbols are kept: method (`.`), type (`#`), module (`/`).
        assert!(!is_parameter_descriptor(
            "scip-dotnet nuget . . Demo/Greeter#Greet()."
        ));
        assert!(!is_parameter_descriptor(
            "scip-dotnet nuget . . Demo/Greeter#"
        ));
        assert!(!is_parameter_descriptor("scip-dotnet nuget . . Demo/"));
    }

    /// #813: a producer we own declares what its column counts, so the consumer
    /// converts instead of guessing. Measured against the real emitters: the
    /// Swift one reports UTF-8 bytes, the Dart one UTF-16 code units, and on a
    /// line with a non-ASCII character those are different numbers.
    #[test]
    fn a_declared_unit_is_converted_rather_than_guessed() {
        // `    var s = "Caf\u{e9}"; return g.hello();` : `hello` starts at byte 30,
        // which the Dart emitter reports as UTF-16 column 29.
        let mixed = "    var s = \"Caf\u{e9}\"; return g.hello();";
        assert_eq!(col_in_unit(mixed, 29, ColUnit::Utf16), Some(30));
        assert_eq!(&mixed.as_bytes()[30..35], b"hello");
        // The same number read as bytes is the wrong position, which is why an
        // undeclared producer abstains here instead of guessing.
        assert_eq!(col_in_unit(mixed, 29, ColUnit::Unknown), None);
        // A byte-native producer (SwiftSyntax) is used as-is, even here.
        assert_eq!(col_in_unit(mixed, 30, ColUnit::Utf8), Some(30));

        // On an ASCII line every unit agrees, so all three answer the same.
        let ascii = "    return g.hello();";
        for unit in [ColUnit::Utf8, ColUnit::Utf16, ColUnit::Unknown] {
            assert_eq!(col_in_unit(ascii, 13, unit), Some(13), "{unit:?}");
        }

        // A position inside a character is not a position an editor can resolve.
        let emoji = "let x = \"\u{1F600}\"; y();";
        assert_eq!(col_in_unit(emoji, 10, ColUnit::Utf16), None); // low surrogate
        assert_eq!(col_in_unit(emoji, 10, ColUnit::Utf8), None); // mid character

        // Out of range abstains rather than panicking.
        assert_eq!(col_in_unit(ascii, 999, ColUnit::Utf8), None);
        assert_eq!(col_in_unit(ascii, -1, ColUnit::Utf16), None);
    }

    /// An emitter too old to declare a unit, or one declaring something we do
    /// not know, stays on the conservative path rather than being assumed.
    #[test]
    fn an_undeclared_unit_stays_conservative() {
        assert_eq!(ColUnit::parse(Some("utf8")), ColUnit::Utf8);
        assert_eq!(ColUnit::parse(Some("utf16")), ColUnit::Utf16);
        assert_eq!(ColUnit::parse(None), ColUnit::Unknown);
        assert_eq!(ColUnit::parse(Some("utf32")), ColUnit::Unknown);
        assert_eq!(ColUnit::parse(Some("")), ColUnit::Unknown);
    }

    /// #813: a column is kept only where every SCIP encoding agrees on what it
    /// counts, because the wire often does not say (a real scip-ruby index emits
    /// no `position_encoding` at all). Inside the ASCII prefix all four encodings
    /// give the same number; outside it the unit is a guess, and a guessed column
    /// is a wrong editor position.
    #[test]
    fn a_column_is_kept_only_where_the_encoding_cannot_change_it() {
        // Pure ASCII prefix: byte == UTF-8 == UTF-16 == UTF-32, so it is safe.
        assert_eq!(encoding_agnostic_col("    @p.hello", 4), Some(4));
        assert_eq!(encoding_agnostic_col("let x = foo();", 8), Some(8));
        // A non-ASCII character before the column makes the unit matter, and the
        // index does not say which one it used: abstain.
        assert_eq!(
            encoding_agnostic_col("    x = \"Caf\u{e9}\"; @p.hello", 20),
            None
        );
        // Non-ASCII AFTER the column is irrelevant, the prefix still decides.
        assert_eq!(encoding_agnostic_col("    @c.gr\u{e9}et", 4), Some(4));
        // Degenerate inputs abstain rather than producing an out-of-range column.
        assert_eq!(encoding_agnostic_col("abc", -1), None);
        assert_eq!(encoding_agnostic_col("abc", 9), None);
        // End-of-line is in range (a zero-width position at the end).
        assert_eq!(encoding_agnostic_col("abc", 3), Some(3));
    }

    /// The document's own `text` is preferred, and an absent one falls back to
    /// the index's `project_root`, which is why populating the column needs no
    /// repo-root argument threaded through the reader.
    #[test]
    fn document_text_prefers_inlined_text_then_the_project_root() {
        let mut doc = scip::types::Document::new();
        doc.relative_path = "a.rb".to_string();
        doc.text = "inlined".to_string();
        assert_eq!(
            document_text(&doc, "file:///nowhere").as_deref(),
            Some("inlined")
        );

        let tmp = std::env::temp_dir().join("travsr_scip_reader_doctext");
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(tmp.join("a.rb"), "from disk").unwrap();
        doc.text = String::new();
        let root = format!("file://{}", tmp.display());
        assert_eq!(document_text(&doc, &root).as_deref(), Some("from disk"));
        // No root and no inlined text: nothing to read, so no column is derived.
        assert_eq!(document_text(&doc, ""), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn empty_index_returns_empty_response() {
        let index = scip::types::Index::default();
        let resp = ingest_index(&index, "test", Language::Go).unwrap();
        assert!(resp.nodes.is_empty());
        assert!(resp.edges.is_empty());
    }

    fn k(kind: scip::types::symbol_information::Kind) -> String {
        kind_from_scip_kind(kind.into())
    }

    #[test]
    fn kind_maps_class_variants() {
        use scip::types::symbol_information::Kind;
        assert_eq!(k(Kind::Class), "class");
        assert_eq!(k(Kind::Interface), "class");
        assert_eq!(k(Kind::Trait), "class");
        assert_eq!(k(Kind::Enum), "class");
        assert_eq!(k(Kind::Struct), "class");
    }

    #[test]
    fn kind_maps_function_variants() {
        use scip::types::symbol_information::Kind;
        assert_eq!(k(Kind::Function), "function");
        assert_eq!(k(Kind::Method), "function");
        assert_eq!(k(Kind::StaticMethod), "function");
    }

    #[test]
    fn kind_maps_variable_and_field() {
        use scip::types::symbol_information::Kind;
        assert_eq!(k(Kind::Variable), "variable");
        assert_eq!(k(Kind::Constant), "variable");
        assert_eq!(k(Kind::Field), "field");
    }

    #[test]
    fn kind_from_symbol_string_infers_suffix() {
        assert_eq!(
            kind_from_symbol_string("npm . foo 1.0.0 SomeClass#"),
            "class"
        );
        assert_eq!(kind_from_symbol_string("go mod SomePkg/"), "module");
        assert_eq!(
            kind_from_symbol_string("go mod SomePkg/method."),
            "function"
        );
        assert_eq!(
            kind_from_symbol_string("go mod SomePkg/unknown"),
            "definition"
        );
    }

    #[test]
    fn definition_node_extracted_from_index() {
        let sym: String = "go mod example.com 1.0.0 SomeFunc.".into();
        let occ = scip::types::Occurrence {
            symbol: sym.clone(),
            symbol_roles: 1, // Definition
            range: vec![5, 0, 5, 10],
            ..Default::default()
        };
        let sym_info = scip::types::SymbolInformation {
            symbol: sym,
            kind: scip::types::symbol_information::Kind::Function.into(),
            ..Default::default()
        };
        let doc = scip::types::Document {
            relative_path: "main.go".into(),
            occurrences: vec![occ],
            symbols: vec![sym_info],
            ..Default::default()
        };
        let index = scip::types::Index {
            documents: vec![doc],
            ..Default::default()
        };

        let resp = ingest_index(&index, "github.com/example/repo", Language::Go).unwrap();
        assert_eq!(resp.nodes.len(), 1);
        assert_eq!(resp.nodes[0].kind, "function");
        assert_eq!(resp.nodes[0].line, Some(6)); // 0-indexed line 5 → 1-indexed line 6
    }

    #[test]
    fn reference_occurrence_produces_edge() {
        // Definition in doc A, reference in doc B
        let def_sym = "go mod example.com 1.0.0 MyFunc.".to_string();

        let def_occ = scip::types::Occurrence {
            symbol: def_sym.clone(),
            symbol_roles: 1,
            range: vec![0, 0],
            ..Default::default()
        };
        let doc_a = scip::types::Document {
            relative_path: "pkg/foo.go".into(),
            occurrences: vec![def_occ],
            ..Default::default()
        };
        let ref_occ = scip::types::Occurrence {
            symbol: def_sym.clone(),
            symbol_roles: 0, // Reference
            range: vec![3, 4],
            ..Default::default()
        };
        let doc_b = scip::types::Document {
            relative_path: "cmd/main.go".into(),
            occurrences: vec![ref_occ],
            ..Default::default()
        };
        let index = scip::types::Index {
            documents: vec![doc_a, doc_b],
            ..Default::default()
        };

        let resp = ingest_index(&index, "github.com/example/repo", Language::Go).unwrap();
        assert_eq!(resp.nodes.len(), 1, "one definition node");
        // G2: reference occurrences become ScipRef records (not raw edges) so the
        // daemon can attribute them to the enclosing function at write time.
        assert_eq!(resp.refs.len(), 1, "one ScipRef from doc_b to def");
        assert_eq!(resp.refs[0].caller_path, "cmd/main.go");
        assert_eq!(resp.refs[0].caller_line, 4); // 0-indexed line 3 → 1-indexed 4
        assert_eq!(resp.refs[0].callee_id, resp.nodes[0].id);
    }

    /// Encode a `SingleLineRange` (3 elems) or `MultiLineRange` (4 elems) typed
    /// oneof sub-message the way real protobuf does: field `i` (1-based) carries
    /// packed element `i-1` as a varint, and proto3 OMITS any field whose value
    /// is 0 (implicit-presence scalars are not serialized when zero). Emitting
    /// explicit zeros here would hide the zero-elision decode path (#724).
    fn typed_range_submessage(elems: &[i32]) -> Vec<u8> {
        let mut out = Vec::new();
        for (idx, &v) in elems.iter().enumerate() {
            if v == 0 {
                continue; // proto3 does not serialize zero-valued scalars
            }
            out.push((((idx + 1) as u32) << 3) as u8); // tag: field number, wire type 0 (varint)
            let mut val = v as u64;
            loop {
                let mut byte = (val & 0x7f) as u8;
                val >>= 7;
                if val != 0 {
                    byte |= 0x80;
                }
                out.push(byte);
                if val == 0 {
                    break;
                }
            }
        }
        out
    }

    /// Build a scip-java occurrence carrying its position in a typed-range oneof
    /// arm (`field`), as scip-java 0.13.1 emits it, instead of the packed array.
    fn java_occ(sym: &str, roles: i32, field: u32, elems: &[i32]) -> scip::types::Occurrence {
        let mut occ = scip::types::Occurrence {
            symbol: sym.to_string(),
            symbol_roles: roles,
            ..Default::default()
        };
        occ.special_fields
            .mut_unknown_fields()
            .add_length_delimited(field, typed_range_submessage(elems));
        occ
    }

    /// Serialize the index to protobuf bytes and parse it back. `add_length_
    /// delimited` installs the typed range directly into `unknown_fields`, so
    /// only a real serialize + parse proves the encoding survives the wire, and
    /// it guards against a future `scip` crate bump that promotes fields 8..=11
    /// to known fields, at which point the bytes leave `unknown_fields` and this
    /// test fails loudly instead of silently passing while prod breaks.
    fn roundtrip(index: scip::types::Index) -> scip::types::Index {
        let bytes = index.write_to_bytes().expect("serialize scip index");
        scip::types::Index::parse_from_bytes(&bytes).expect("parse scip index")
    }

    #[test]
    fn scip_java_typed_range_ref_produces_edge() {
        // Regression for #724: scip-java 0.13.1 emits occurrence positions in
        // the `typed_range` oneof (single_line_range = field 8), not the packed
        // `range` array, so `occ.range` is empty. The reader must recover the
        // position from the retained unknown field, otherwise every Java
        // reference occurrence is dropped and no call edges form.
        let sym = "scip-java maven m 1.0.0 com/example/Greeter#greet().";
        // def at line 4 (0-indexed 3): SingleLineRange { line=3, start=4, end=9 }
        let def = java_occ(sym, 1, 8, &[3, 4, 9]);
        // ref at line 6 (0-indexed 5): SingleLineRange { line=5, start=8, end=13 }
        let r = java_occ(sym, 0, 8, &[5, 8, 13]);
        let index = roundtrip(scip::types::Index {
            documents: vec![
                scip::types::Document {
                    relative_path: "Greeter.java".into(),
                    occurrences: vec![def],
                    ..Default::default()
                },
                scip::types::Document {
                    relative_path: "App.java".into(),
                    occurrences: vec![r],
                    ..Default::default()
                },
            ],
            ..Default::default()
        });

        let resp = ingest_index(&index, "m", Language::Java).unwrap();
        assert_eq!(resp.nodes.len(), 1, "one definition node from typed range");
        assert_eq!(
            resp.nodes[0].line,
            Some(4),
            "def line recovered from field 8"
        );
        assert_eq!(
            resp.refs.len(),
            1,
            "typed-range ref must not be dropped (#724)"
        );
        assert_eq!(resp.refs[0].caller_path, "App.java");
        assert_eq!(resp.refs[0].caller_line, 6); // 0-indexed 5 → 1-indexed 6
        assert_eq!(resp.refs[0].callee_id, resp.nodes[0].id);
    }

    #[test]
    fn scip_java_ref_on_line_zero_survives_zero_elision() {
        // #724 bug (a): proto3 does not serialize a zero-valued `line`, so a ref
        // on the first line of a file (line 0) has field 1 absent on the wire.
        // The decoder must read the fixed 3-element arity and default the
        // missing element to 0, not truncate. Otherwise the ref decodes to an
        // empty range and is silently dropped, reproducing the #724 symptom.
        let sym = "scip-java maven m 1.0.0 com/example/Greeter#greet().";
        let def = java_occ(sym, 1, 8, &[3, 4, 9]);
        // ref on line 0: SingleLineRange { line=0(elided), start=8, end=13 }
        let r = java_occ(sym, 0, 8, &[0, 8, 13]);
        let index = roundtrip(scip::types::Index {
            documents: vec![
                scip::types::Document {
                    relative_path: "Greeter.java".into(),
                    occurrences: vec![def],
                    ..Default::default()
                },
                scip::types::Document {
                    relative_path: "App.java".into(),
                    occurrences: vec![r],
                    ..Default::default()
                },
            ],
            ..Default::default()
        });

        let resp = ingest_index(&index, "m", Language::Java).unwrap();
        assert_eq!(
            resp.refs.len(),
            1,
            "ref on line 0 must not be dropped (#724a)"
        );
        assert_eq!(resp.refs[0].caller_line, 1); // 0-indexed 0 → 1-indexed 1
    }

    #[test]
    fn scip_java_multi_line_range_ref_produces_edge() {
        // #724 bug (b): a reference carried in the multi_line_range arm (field 9,
        // a 4-element MultiLineRange) must decode too, not just single_line_range
        // (field 8). Before the fix, field 9 was never read and the ref dropped.
        let sym = "scip-java maven m 1.0.0 com/example/Greeter#greet().";
        let def = java_occ(sym, 1, 8, &[3, 4, 9]);
        // ref: MultiLineRange { start_line=5, start=8, end_line=6, end=2 }
        let r = java_occ(sym, 0, 9, &[5, 8, 6, 2]);
        let index = roundtrip(scip::types::Index {
            documents: vec![
                scip::types::Document {
                    relative_path: "Greeter.java".into(),
                    occurrences: vec![def],
                    ..Default::default()
                },
                scip::types::Document {
                    relative_path: "App.java".into(),
                    occurrences: vec![r],
                    ..Default::default()
                },
            ],
            ..Default::default()
        });

        let resp = ingest_index(&index, "m", Language::Java).unwrap();
        assert_eq!(
            resp.refs.len(),
            1,
            "multi_line_range ref must not be dropped (#724b)"
        );
        assert_eq!(resp.refs[0].caller_line, 6); // start_line 5 → 1-indexed 6
    }

    #[test]
    fn typed_range_helpers_decode_all_four_field_numbers() {
        // All four typed-range field numbers must decode, with zero-elided
        // elements defaulting to 0 (not truncating): single_line_range (8) and
        // single_line_enclosing_range (10) are 3-element; multi_line_range (9)
        // and multi_line_enclosing_range (11) are 4-element.
        let mk = |field: u32, elems: &[i32]| java_occ("s", 0, field, elems);
        assert_eq!(scip_range(&mk(8, &[5, 8, 13])).into_owned(), vec![5, 8, 13]);
        assert_eq!(
            scip_range(&mk(9, &[5, 8, 6, 2])).into_owned(),
            vec![5, 8, 6, 2]
        );
        assert_eq!(
            scip_enclosing_range(&mk(10, &[5, 8, 20])).into_owned(),
            vec![5, 8, 20]
        );
        assert_eq!(
            scip_enclosing_range(&mk(11, &[5, 8, 9, 2])).into_owned(),
            vec![5, 8, 9, 2]
        );
        // zero elision: line 0 and column 0 must round-trip to 0, not truncate.
        assert_eq!(scip_range(&mk(8, &[0, 0, 9])).into_owned(), vec![0, 0, 9]);
        assert_eq!(
            scip_range(&mk(9, &[0, 0, 0, 4])).into_owned(),
            vec![0, 0, 0, 4]
        );
        // classic packed producers borrow their native field without allocating.
        let packed = scip::types::Occurrence {
            range: vec![1, 2, 3],
            ..Default::default()
        };
        assert!(matches!(scip_range(&packed), Cow::Borrowed(_)));
    }

    #[test]
    fn objc_ref_without_def_becomes_unresolved_call() {
        // #449: a bridged ObjC → Swift call references a symbol with no def in
        // the ObjC index. It must surface as an UnresolvedCall attributed to
        // the innermost enclosing definition, with a leading-keyword sig.
        let caller_sym = "objc . corp 0.0.0 Bridge#run().".to_string();
        let bridged_sym = "objc . corp 0.0.0 ClassC#registerEnvironments().".to_string();

        let def_occ = scip::types::Occurrence {
            symbol: caller_sym.clone(),
            symbol_roles: 1,
            // name token range; enclosing_range covers the method body
            range: vec![2, 0, 5],
            enclosing_range: vec![2, 0, 8, 1],
            ..Default::default()
        };
        let ref_occ = scip::types::Occurrence {
            symbol: bridged_sym,
            symbol_roles: 0,
            range: vec![4, 4],
            ..Default::default()
        };
        let doc = scip::types::Document {
            relative_path: "Bridge.m".into(),
            occurrences: vec![def_occ, ref_occ],
            ..Default::default()
        };
        let index = scip::types::Index {
            documents: vec![doc],
            ..Default::default()
        };

        let resp = ingest_index(&index, "corp", Language::ObjectiveC).unwrap();
        assert!(resp.refs.is_empty(), "no in-index target, no ScipRef");
        assert_eq!(resp.unresolved_calls.len(), 1);
        let uc = &resp.unresolved_calls[0];
        assert_eq!(uc.callee_sig, "fn:registerEnvironments");
        assert_eq!(uc.caller_line, 5); // 0-indexed 4 → 1-indexed 5
        assert_eq!(uc.src, resp.nodes[0].id, "attributed to enclosing method");
        assert!(uc.hint_crate.is_none());
    }

    #[test]
    fn objc_ref_without_enclosing_def_is_skipped() {
        let ref_occ = scip::types::Occurrence {
            symbol: "objc . corp 0.0.0 ClassC#registerEnvironments().".to_string(),
            symbol_roles: 0,
            range: vec![0, 0],
            ..Default::default()
        };
        let doc = scip::types::Document {
            relative_path: "Bridge.m".into(),
            occurrences: vec![ref_occ],
            ..Default::default()
        };
        let index = scip::types::Index {
            documents: vec![doc],
            ..Default::default()
        };
        let resp = ingest_index(&index, "corp", Language::ObjectiveC).unwrap();
        assert!(resp.unresolved_calls.is_empty());
    }

    #[test]
    fn non_method_ref_without_def_is_not_unresolved() {
        // Class refs (`ClassC#`) without defs are not calls, skip.
        let def_occ = scip::types::Occurrence {
            symbol: "objc . corp 0.0.0 Bridge#run().".to_string(),
            symbol_roles: 1,
            range: vec![0, 0, 3],
            enclosing_range: vec![0, 0, 5, 1],
            ..Default::default()
        };
        let ref_occ = scip::types::Occurrence {
            symbol: "objc . corp 0.0.0 ClassC#".to_string(),
            symbol_roles: 0,
            range: vec![2, 4],
            ..Default::default()
        };
        let doc = scip::types::Document {
            relative_path: "Bridge.m".into(),
            occurrences: vec![def_occ, ref_occ],
            ..Default::default()
        };
        let index = scip::types::Index {
            documents: vec![doc],
            ..Default::default()
        };
        let resp = ingest_index(&index, "corp", Language::ObjectiveC).unwrap();
        assert!(resp.unresolved_calls.is_empty());
    }

    #[test]
    fn go_ref_without_def_stays_dropped() {
        // The cross-language pass is gated to ObjC: dependency/stdlib refs in
        // other SCIP languages must not become UnresolvedCalls.
        let def_occ = scip::types::Occurrence {
            symbol: "go mod example.com 1.0.0 Caller().".to_string(),
            symbol_roles: 1,
            range: vec![0, 0, 6],
            enclosing_range: vec![0, 0, 9, 1],
            ..Default::default()
        };
        let ref_occ = scip::types::Occurrence {
            symbol: "go mod fmt 1.0.0 Println().".to_string(),
            symbol_roles: 0,
            range: vec![3, 1],
            ..Default::default()
        };
        let doc = scip::types::Document {
            relative_path: "main.go".into(),
            occurrences: vec![def_occ, ref_occ],
            ..Default::default()
        };
        let index = scip::types::Index {
            documents: vec![doc],
            ..Default::default()
        };
        let resp = ingest_index(&index, "example.com", Language::Go).unwrap();
        assert!(resp.unresolved_calls.is_empty());
    }

    #[test]
    fn leading_keyword_sig_forms() {
        assert_eq!(
            leading_keyword_sig("objc . c 0.0.0 ClassC#registerEnvironments()."),
            Some("fn:registerEnvironments".into())
        );
        assert_eq!(
            leading_keyword_sig("objc . c 0.0.0 Foo#setWidth:height:()."),
            Some("fn:setWidth".into())
        );
        // Non-method descriptors: class, property, protocol.
        assert_eq!(leading_keyword_sig("objc . c 0.0.0 ClassC#"), None);
        assert_eq!(leading_keyword_sig("objc . c 0.0.0 ClassC#prop."), None);
        assert_eq!(leading_keyword_sig("objc . c 0.0.0 Proto/"), None);
    }

    #[test]
    fn innermost_enclosing_def_picks_smaller_nested_span() {
        // A class span (1..=20) and a method span nested inside it (5..=10):
        // a line inside the method must resolve to the method, not the class,
        // even though both spans contain it.
        let class_id = NodeId(1);
        let method_id = NodeId(2);
        let spans = vec![(1, 20, class_id), (5, 10, method_id)];
        assert_eq!(innermost_enclosing_def(Some(&spans), 7), Some(method_id));
        // Outside the method but still inside the class: falls back to the class.
        assert_eq!(innermost_enclosing_def(Some(&spans), 15), Some(class_id));
        // Outside both: no enclosing definition.
        assert_eq!(innermost_enclosing_def(Some(&spans), 25), None);
        // No spans at all for the document.
        assert_eq!(innermost_enclosing_def(None, 7), None);
    }

    #[test]
    fn ref_scip_ref_callee_matches_def_node() {
        // G2: ScipRef.callee_id must equal the definition node's NodeId so that
        // write_scip_attributed_batch can resolve the target without extra lookups.
        let corpus = "local/java";
        let path = "src/main/java/com/example/Foo.java";
        let def_sym = "com.example Foo#doWork().".to_string();

        let def_occ = scip::types::Occurrence {
            symbol: def_sym.clone(),
            symbol_roles: 1,
            range: vec![0, 0],
            ..Default::default()
        };
        let doc_def = scip::types::Document {
            relative_path: "src/main/java/com/example/Bar.java".into(),
            occurrences: vec![def_occ],
            ..Default::default()
        };
        let ref_occ = scip::types::Occurrence {
            symbol: def_sym.clone(),
            symbol_roles: 0,
            range: vec![2, 4],
            ..Default::default()
        };
        let doc_ref = scip::types::Document {
            relative_path: path.into(),
            occurrences: vec![ref_occ],
            ..Default::default()
        };
        let index = scip::types::Index {
            documents: vec![doc_def, doc_ref],
            ..Default::default()
        };

        let resp = ingest_index(&index, corpus, Language::Java).unwrap();
        assert_eq!(resp.refs.len(), 1, "one ScipRef");
        assert_eq!(resp.refs[0].caller_path, path);
        assert_eq!(resp.refs[0].caller_line, 3); // 0-indexed line 2 → 1-indexed 3
                                                 // callee_id must be the definition node, the daemon resolves src at write time.
        assert_eq!(resp.refs[0].callee_id, resp.nodes[0].id);
    }
}
