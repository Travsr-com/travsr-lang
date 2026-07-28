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
//! **Nodes** — one per definition occurrence (where `symbol_roles & 1 != 0`).
//! Kind is inferred from `SymbolInformation.kind` (the SCIP `SymbolKind` enum)
//! with a descriptor-suffix fallback for symbols that lack `SymbolInformation`.
//!
//! **Edges** — two sources:
//! 1. `SymbolInformation.relationships` — explicit symbol-to-symbol edges
//!    (`is_reference` → `ref/call`, `is_implementation` → `is-implementation`).
//! 2. Reference occurrences (`symbol_roles & 1 == 0`) resolved against the
//!    definition table — emitted as `ref/call` edges from a synthetic file node
//!    to the definition node. Capped at [`MAX_REF_EDGES_PER_DOC`] per document.

use anyhow::Context as _;
use protobuf::Message as _;
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
    // whole project and emit the SAME symbols — under two different language tags,
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
            // G3: SCIP anonymous locals (`local N`) are intra-function noise —
            // on kubernetes they are 77% of all definitions. Drop at ingest.
            if occ.symbol.starts_with("local ") {
                continue;
            }

            // G2: prefer `enclosing_range` (the full declaration body span,
            // e.g. a Go function's `{ … }` block) over `range` (just the name
            // token). Without the body span the enclosing-function lookup in
            // write_scip_attributed_batch can only match refs that sit on the
            // definition line itself.
            let span: &[i32] = if occ.enclosing_range.len() >= 3 {
                &occ.enclosing_range
            } else {
                &occ.range
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
                Some(false) => {} // already have a source def — keep it
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
    for doc in &index.documents {
        let path = &doc.relative_path;
        // #299 C1: scip-clang emits a reference occurrence at a symbol's *header
        // declaration* line (not a call site). Attributing those to a header
        // pollutes find_references with a spurious "use" on the declaration.
        // Header occurrences for C/C++ are declarations, not call sites — skip.
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
            if count >= MAX_REF_EDGES_PER_DOC {
                break;
            }
            // #299 F4: a range-less occurrence (malformed / partial SCIP)
            // carries no real position — skip it rather than fabricating a
            // phantom `path:1` reference via `unwrap_or(1)`.
            let Some(start) = occ.range.first().copied() else {
                continue;
            };
            let caller_line = start as u32 + 1;
            if let Some(&callee_id) = def_ids.get(occ.symbol.as_str()) {
                refs.push(ScipRef {
                    caller_path: path.clone(),
                    caller_line,
                    callee_id,
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
                    hint_crate: None,
                    caller_line,
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
                                                 // callee_id must be the definition node — the daemon resolves src at write time.
        assert_eq!(resp.refs[0].callee_id, resp.nodes[0].id);
    }
}
