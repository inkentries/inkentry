// Edge extraction and in-file resolution, one file per concern.

mod bindings;
mod coverage;
mod receivers;
mod visibility;

use super::{Edge, EdgeExtractor, EdgeKind};

pub(super) fn calls(src: &str, path: &str, language: &str) -> Vec<Edge> {
    EdgeExtractor::extract(src, path, language)
        .expect("extract")
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .collect()
}

pub(super) fn call<'a>(edges: &'a [Edge], source: &str, target: &str) -> Option<&'a Edge> {
    edges
        .iter()
        .find(|e| e.source_name.as_deref() == Some(source) && e.target_name == target)
}

pub(super) fn edge_to<'a>(edges: &'a [Edge], target: &str, path: &str) -> &'a Edge {
    edges
        .iter()
        .find(|e| e.target_name == target)
        .unwrap_or_else(|| panic!("{path}: the call to {target} keeps its edge: {edges:?}"))
}
