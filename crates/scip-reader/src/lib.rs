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
use travsr_core::{Edge, EdgeKind, Language, Node, NodeId, VName};
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

    // symbol_string → NodeId; populated in pass 1, consumed in passes 2 & 3.
    let mut def_ids: HashMap<String, NodeId> = HashMap::new();

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

            let line = occ.range.first().copied().unwrap_or(0) as u32 + 1;
            let kind = kind_map
                .get(occ.symbol.as_str())
                .cloned()
                .unwrap_or_else(|| kind_from_symbol_string(&occ.symbol));

            let sig = format!("scip:{}:{}", path, occ.symbol);
            let vname = VName::new(corpus, "", path, lang_str, &sig);
            let node_id = vname.id();
            def_ids.insert(occ.symbol.clone(), node_id);
            nodes.push(Node::new(vname, &kind).with_line(line));
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

    // ── Pass 3: reference-occurrence edges ───────────────────────────────────
    // For each reference occurrence resolved to a known definition, emit a
    // ref/call edge from a synthetic "file" node (representing the referencing
    // source file) to the definition node. Capped per document.
    for doc in &index.documents {
        let path = &doc.relative_path;
        let file_id = VName::new(corpus, "", path, lang_str, format!("scip:file:{}", path)).id();
        let mut count = 0usize;

        for occ in &doc.occurrences {
            if occ.symbol.is_empty() {
                continue;
            }
            if (occ.symbol_roles & 1) != 0 {
                continue; // skip definitions
            }
            if count >= MAX_REF_EDGES_PER_DOC {
                break;
            }
            if let Some(&dst_id) = def_ids.get(occ.symbol.as_str()) {
                edges.push(Edge::new(file_id, dst_id, EdgeKind::RefCall));
                count += 1;
            }
        }
    }

    tracing::info!(
        nodes = nodes.len(),
        edges = edges.len(),
        "SCIP ingestion complete"
    );

    Ok(InvokeResponse { nodes, edges })
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
        assert_eq!(resp.edges.len(), 1, "one ref/call edge from doc_b to def");
    }
}
