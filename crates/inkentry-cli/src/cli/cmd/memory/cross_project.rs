use std::path::Path;

use crate::registry::{Project, Registry};
use crate::storage::memory::Note;
use crate::storage::{LocalMemoryBackend, MemoryBackend, MemoryStore, NoteId};

const CROSS_CUTTING_KINDS: &[&str] = &["decision", "requirement"];

const CROSS_PROJECT_TAGS: &[&str] = &["locked", "cross-project"];

// Empty rather than an error when the registry, the project, or its deps are
// missing.
fn resolve_dep_projects(index_db_path: &Path) -> Vec<Project> {
    let Ok(reg) = Registry::open() else {
        return vec![];
    };
    // index_db_path = <root>/.inkentry/index.db
    let project_root = index_db_path
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(index_db_path);
    let Ok(Some(project)) = reg.find_project_for_path(project_root) else {
        return vec![];
    };
    reg.get_deps(project.id).unwrap_or_default()
}

// A missing `memory.db` is normal (linked for code search only) and skipped
// silently; open/query errors warn and skip.
async fn query_dep_cross_cutting(dep: &Project) -> Vec<Note> {
    let mem_db_path = dep.db_path.with_file_name("memory.db");
    if !mem_db_path.exists() {
        return vec![];
    }

    let store = match MemoryStore::open(&mem_db_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "cross-project memory: could not open dep DB {}: {e}",
                mem_db_path.display()
            );
            return vec![];
        }
    };
    let backend = LocalMemoryBackend::new(store);

    let source_project = crate::cli::cmd::helpers::project_display_name(&dep.root_path);
    let source_project_path = dep.root_path.to_string_lossy().into_owned();

    let mut cross_cutting = Vec::new();

    for kind in CROSS_CUTTING_KINDS {
        let notes = match backend.list(Some(kind), 500, false, None).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    "cross-project memory: list failed for dep {} kind={kind}: {e}",
                    mem_db_path.display()
                );
                continue;
            }
        };

        for mut note in notes {
            if is_cross_cutting(&note.tags) {
                note.source_project = Some(source_project.clone());
                note.source_project_path = Some(source_project_path.clone());
                cross_cutting.push(note);
            }
        }
    }

    cross_cutting
}

fn is_cross_cutting(tags: &[String]) -> bool {
    tags.iter()
        .any(|t| CROSS_PROJECT_TAGS.iter().any(|&ct| t == ct))
}

// `seen` holds `(root_path, id)` pairs already emitted, so two deps sharing a
// grandparent project do not surface its notes twice.
pub(crate) async fn collect_dep_cross_cutting(
    index_db_path: &Path,
    seen: &mut std::collections::HashSet<(String, NoteId)>,
) -> Vec<Note> {
    let deps = resolve_dep_projects(index_db_path);
    let mut result = Vec::new();
    for dep in &deps {
        let root_key = dep.root_path.to_string_lossy().into_owned();
        for note in query_dep_cross_cutting(dep).await {
            if seen.insert((root_key.clone(), note.id.clone())) {
                result.push(note);
            }
        }
    }
    result
}
