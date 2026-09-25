use anyhow::Result;

use super::MemoryTagsArgs;
use crate::storage::MemoryStore;

// Sqlite-only: neither the git-notes carrier nor a remote backend exposes a vocabulary.
pub(super) async fn memory_tags(args: MemoryTagsArgs, mem_path: &std::path::Path) -> Result<()> {
    let store = MemoryStore::open(mem_path)?;
    let tags = store.tags_with_counts()?;

    match crate::utils::effective_format(&args.format) {
        "json" => {
            let obj: Vec<serde_json::Value> = tags
                .iter()
                .map(|(tag, count)| serde_json::json!({"tag": tag, "count": count}))
                .collect();
            println!("{}", serde_json::to_string_pretty(&obj)?);
        }
        _ => {
            if tags.is_empty() {
                println!("No tags recorded.");
                return Ok(());
            }
            for (tag, count) in &tags {
                println!("{count:>5}  {tag}");
            }
        }
    }
    Ok(())
}
