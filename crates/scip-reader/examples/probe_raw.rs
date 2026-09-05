use protobuf::Message;
fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&path)?;
    let index: scip::types::Index = Message::parse_from_bytes(&bytes)?;
    for doc in &index.documents {
        for occ in &doc.occurrences {
            if occ.symbol.contains("greet") || occ.symbol.contains("App#main") {
                let unknown: Vec<u32> = occ
                    .special_fields
                    .unknown_fields()
                    .iter()
                    .map(|(n, _)| n)
                    .collect();
                println!(
                    "sym={} roles={} range={:?} encl={:?} unknown_field_nums={:?}",
                    occ.symbol, occ.symbol_roles, occ.range, occ.enclosing_range, unknown
                );
            }
        }
    }
    Ok(())
}
