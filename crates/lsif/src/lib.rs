//! Minimal LSIF JSON-Lines ingestion.
//!
//! Parses LSIF output from tools like rust-analyzer, travsr-lsif-ts,
//! scip (via conversion), etc. and returns Travsr Node + Edge records.
//!
//! This is a "definitions-first" pass: we extract definition ranges
//! as nodes. Full call graph (reference/call edges) is a future enhancement
//! tracked in travsr#254.

use anyhow::Context as _;
use serde::Deserialize;
use std::collections::HashMap;
use travsr_core::{Language, Node, VName};
use travsr_plugin_sdk::InvokeResponse;

// ── LSIF record types ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct LsifRecord {
    id: serde_json::Value,
    #[serde(rename = "type")]
    record_type: String,
    label: Option<String>,
    // Vertex fields
    uri: Option<String>,
    // Edge fields
    #[serde(rename = "outV")]
    out_v: Option<serde_json::Value>,
    #[serde(rename = "inV")]
    in_v: Option<serde_json::Value>,
    #[serde(rename = "inVs")]
    in_vs: Option<Vec<serde_json::Value>>,
    // Range fields
    start: Option<Position>,
    #[allow(dead_code)] // kept for spec completeness; not yet used in ingestion
    end: Option<Position>,
    // Result item
    document: Option<serde_json::Value>,
    property: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct Position {
    line: u32,
    character: u32,
}

/// Ingest LSIF JSON-Lines output and return Travsr nodes and edges.
pub fn ingest(lsif: &str, corpus: &str, language: Language) -> anyhow::Result<InvokeResponse> {
    let mut documents: HashMap<String, String> = HashMap::new(); // id → uri
    let mut ranges: HashMap<String, (String, Position)> = HashMap::new(); // range_id → (doc_id, start)
    let mut def_results: HashMap<String, Vec<String>> = HashMap::new(); // defResult_id → [range_ids]
    let mut result_set_to_def: HashMap<String, String> = HashMap::new(); // resultSet_id → defResult_id
    let mut range_to_result_set: HashMap<String, String> = HashMap::new(); // range_id → resultSet_id

    // Parse all records
    for (lineno, line) in lsif.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let record: LsifRecord = serde_json::from_str(line)
            .with_context(|| format!("LSIF parse error at line {lineno}: {line}"))?;

        let id = id_to_string(&record.id);

        match record.record_type.as_str() {
            "vertex" => match record.label.as_deref() {
                Some("document") => {
                    if let Some(uri) = record.uri {
                        documents.insert(id, uri);
                    }
                }
                Some("range") => {
                    if let Some(start) = record.start {
                        let doc_ref = record
                            .document
                            .as_ref()
                            .map(id_to_string)
                            .unwrap_or_default();
                        // doc_ref may be empty; "contains" edges will fill it in later
                        ranges.insert(id, (doc_ref, start));
                    }
                }
                _ => {}
            },
            "edge" => match record.label.as_deref() {
                Some("contains") => {
                    // document → [ranges]
                    let doc_id = record.out_v.map(|v| id_to_string(&v)).unwrap_or_default();
                    let range_ids: Vec<String> = record
                        .in_vs
                        .unwrap_or_default()
                        .iter()
                        .map(id_to_string)
                        .collect();
                    for rid in range_ids {
                        if let Some(r) = ranges.get_mut(&rid) {
                            if r.0.is_empty() {
                                r.0 = doc_id.clone();
                            }
                        }
                    }
                }
                Some("next") => {
                    // range → resultSet
                    if let (Some(out), Some(inv)) = (record.out_v, record.in_v) {
                        range_to_result_set.insert(id_to_string(&out), id_to_string(&inv));
                    }
                }
                Some("textDocument/definition") => {
                    // resultSet → definitionResult
                    if let (Some(out), Some(inv)) = (record.out_v, record.in_v) {
                        result_set_to_def.insert(id_to_string(&out), id_to_string(&inv));
                    }
                }
                Some("item") => {
                    if record.property.as_deref() == Some("definitions") {
                        let def_id = record.out_v.map(|v| id_to_string(&v)).unwrap_or_default();
                        let range_ids: Vec<String> = record
                            .in_vs
                            .unwrap_or_default()
                            .iter()
                            .map(id_to_string)
                            .collect();
                        def_results.entry(def_id).or_default().extend(range_ids);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    // Build definition ranges: for each range that is a definition site, emit a Node
    let mut nodes = Vec::new();
    let lang_str = language.as_str();

    for (range_id, (doc_id, start)) in &ranges {
        let result_set_id = match range_to_result_set.get(range_id) {
            Some(rs) => rs,
            None => continue,
        };
        let def_result_id = match result_set_to_def.get(result_set_id) {
            Some(dr) => dr,
            None => continue,
        };
        // This range is a definition site if it appears in a definitionResult
        let is_def = def_results.values().any(|rs| rs.contains(range_id))
            || def_results.contains_key(def_result_id);
        if !is_def {
            continue;
        }

        let uri = documents.get(doc_id).cloned().unwrap_or_default();
        let vname_path = uri_to_vname_path(&uri);
        if vname_path.is_empty() {
            continue;
        }

        let sig = format!("lsif:def:{}:{}:{}", vname_path, start.line, start.character);
        let vname = VName::new(corpus, "", &vname_path, lang_str, &sig);
        nodes.push(Node::new(vname, "definition").with_line(start.line + 1));
    }

    tracing::info!(
        nodes = nodes.len(),
        "LSIF ingestion complete ({} definition nodes)",
        nodes.len()
    );

    Ok(InvokeResponse {
        nodes,
        edges: vec![],
    })
}

fn id_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        _ => v.to_string(),
    }
}

fn uri_to_vname_path(uri: &str) -> String {
    // Strip "file://" prefix and make path repo-relative if possible
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    // Normalize to forward slashes
    path.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_lsif_returns_empty_response() {
        let resp = ingest("", "test", Language::Rust).unwrap();
        assert!(resp.nodes.is_empty());
        assert!(resp.edges.is_empty());
    }

    #[test]
    fn invalid_json_returns_err() {
        let result = ingest("not json\n", "test", Language::Rust);
        assert!(result.is_err());
    }
}
