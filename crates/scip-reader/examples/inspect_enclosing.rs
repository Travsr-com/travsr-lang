//! Dev tool: report how many SCIP definition occurrences carry enclosing_range.
//! Usage: cargo run -p travsr-lang-scip-reader --example inspect_enclosing -- <index.scip>

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: inspect_enclosing <index.scip>");
    let bytes = std::fs::read(&path)?;
    let index: scip::types::Index = protobuf::Message::parse_from_bytes(&bytes)?;

    let (mut defs, mut with_encl, mut locals, mut multi_line) = (0u64, 0u64, 0u64, 0u64);
    let mut sample = Vec::new();
    for doc in &index.documents {
        for occ in &doc.occurrences {
            if occ.symbol.is_empty() || (occ.symbol_roles & 1) == 0 {
                continue;
            }
            defs += 1;
            if occ.symbol.starts_with("local ") {
                locals += 1;
            }
            if occ.enclosing_range.len() >= 3 {
                with_encl += 1;
                let start = occ.enclosing_range[0];
                let end = if occ.enclosing_range.len() >= 4 {
                    occ.enclosing_range[2]
                } else {
                    start
                };
                if end > start {
                    multi_line += 1;
                    if sample.len() < 5 {
                        sample.push(format!(
                            "{}: {} range={:?} enclosing={:?}",
                            doc.relative_path, occ.symbol, occ.range, occ.enclosing_range
                        ));
                    }
                }
            }
        }
    }
    println!("definition occurrences: {defs}");
    println!("  with enclosing_range: {with_encl}");
    println!("  multi-line enclosing: {multi_line}");
    println!("  anonymous locals:     {locals}");
    for s in sample {
        println!("  sample: {s}");
    }
    Ok(())
}
