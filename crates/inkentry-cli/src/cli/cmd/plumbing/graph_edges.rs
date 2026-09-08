use anyhow::{Result, bail};

use super::PlumbingGraphEdgesArgs;
use crate::storage::Database;

pub(super) fn graph_edges(args: PlumbingGraphEdgesArgs, db: &Database) -> Result<()> {
    if args.file.is_none() && args.symbol.is_none() {
        bail!("at least one of --file or --symbol is required");
    }

    let mut edges = vec![];

    if let Some(ref file) = args.file {
        // A path the index does not hold is an error, not an empty set: a
        // mistyped path must never read the same as a file with no edges.
        if db.file_id_for_path(file)?.is_none() {
            bail!(
                "'{file}' is not in the index; pass the project-relative path as stored (see `plumbing ls-files`)"
            );
        }
        edges.extend(db.edges_for_file(file)?);
    }
    if let Some(ref symbol) = args.symbol {
        let sym_edges = db.edges_for_symbol(symbol)?;
        for e in sym_edges {
            // Deduplicate when --file and --symbol both match the same edge.
            if !edges.iter().any(|x: &crate::storage::GraphEdge| {
                x.source_file == e.source_file
                    && x.target_name == e.target_name
                    && x.kind == e.kind
                    && x.line == e.line
            }) {
                edges.push(e);
            }
        }
    }

    if edges.is_empty() {
        std::process::exit(1);
    }

    for e in &edges {
        println!("{}", serde_json::to_string(e)?);
    }
    Ok(())
}
