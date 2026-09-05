//! Dev tool: run the real ingest() on a .scip file and report node/ref counts.
//! Usage: cargo run -p travsr-lang-scip-reader --example probe_refs -- <index.scip>
use travsr_core::Language;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: probe_refs <index.scip>");
    let resp =
        travsr_lang_scip_reader::ingest(std::path::Path::new(&path), "local/java", Language::Java)?;
    println!(
        "nodes={} edges={} refs={} unresolved={}",
        resp.nodes.len(),
        resp.edges.len(),
        resp.refs.len(),
        resp.unresolved_calls.len()
    );
    for n in &resp.nodes {
        println!(
            "  NODE {} kind={} line={:?}",
            n.vname.signature, n.kind, n.line
        );
    }
    for r in &resp.refs {
        println!(
            "  REF {}:{} -> callee_id={:?}",
            r.caller_path, r.caller_line, r.callee_id
        );
    }
    Ok(())
}
