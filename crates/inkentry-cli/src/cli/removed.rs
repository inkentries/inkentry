// Matched against argv after clap has rejected it. Registering removed commands
// would list them in `--help`, and clap's did-you-mean would point `memory
// search` at the unrelated `memory archive`.

pub(crate) fn hint(argv: &[String]) -> Option<&'static str> {
    let path = subcommand_path(argv);
    match path.as_slice() {
        ["memory", "search", ..] => Some(MEMORY_SEARCH),
        ["graph", ..] => Some(GRAPH),
        ["search", ..]
            if argv
                .iter()
                .any(|a| a == "--mode" || a.starts_with("--mode=")) =>
        {
            Some(SEARCH_MODE)
        }
        _ => None,
    }
}

const MEMORY_SEARCH: &str = "\
error: `inkentry memory search` was removed; memory is searched by `inkentry search`.

  inkentry search \"<query>\" --only-memory    memory entries only
  inkentry search \"<query>\"                  code and memory, one ranked list

`--as-of`, `--expand-graph` and `--local-only` carry over unchanged. The rest of
the `inkentry memory` family is unaffected.";

const GRAPH: &str = "\
error: `inkentry graph` was removed; the code graph is reached through `search`.

  inkentry search \"<symbol>\" --graph              the symbol plus its 1-hop neighbours
  inkentry plumbing graph-edges --symbol <name>   exact edges, as JSONL

`inkentry memory graph` is a different command and still exists.";

const SEARCH_MODE: &str = "\
error: `--mode` was removed from `inkentry search`; ranking is always best-available.

  --mode text                   ->  --only-text
  --mode semantic|hybrid|auto   ->  no flag; that is the default
  --mode ast-grep               ->  no replacement; structural search was removed

Use `--only-code` / `--only-memory` to pick a corpus.";

const GLOBALS_TAKING_A_VALUE: &[&str] = &["--config", "-c", "--color"];

// The first two non-flag tokens are enough here; avoids reimplementing clap.
fn subcommand_path(argv: &[String]) -> Vec<&str> {
    let mut path = Vec::new();
    let mut it = argv.iter().skip(1);
    while let Some(a) = it.next() {
        if GLOBALS_TAKING_A_VALUE.contains(&a.as_str()) {
            it.next();
        } else if a.starts_with('-') {
            continue;
        } else {
            path.push(a.as_str());
            if path.len() == 2 {
                break;
            }
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("inkentry")
            .chain(args.iter().copied())
            .map(String::from)
            .collect()
    }

    #[test]
    fn memory_search_names_the_only_memory_replacement() {
        let h = hint(&argv(&["memory", "search", "why sqlite-vec"])).unwrap();
        assert!(h.contains("--only-memory"));
        assert!(!h.contains("archive"));
    }

    #[test]
    fn top_level_graph_names_both_replacements() {
        let h = hint(&argv(&["graph", "linearrag_search"])).unwrap();
        assert!(h.contains("search \"<symbol>\" --graph"));
        assert!(h.contains("plumbing graph-edges"));
    }

    #[test]
    fn memory_graph_is_not_a_removed_surface() {
        assert!(hint(&argv(&["memory", "graph", "12"])).is_none());
    }

    #[test]
    fn search_mode_maps_each_old_value() {
        for form in [
            vec!["search", "foo", "--mode", "text"],
            vec!["search", "foo", "--mode=ast-grep"],
        ] {
            let h = hint(&argv(&form)).unwrap();
            assert!(h.contains("--only-text"));
            assert!(h.contains("ast-grep"));
        }
    }

    #[test]
    fn mode_outside_search_is_not_hinted() {
        assert!(hint(&argv(&["status", "--mode", "text"])).is_none());
    }

    #[test]
    fn globals_before_the_subcommand_do_not_hide_it() {
        assert!(hint(&argv(&["--color", "never", "graph", "foo"])).is_some());
        assert!(hint(&argv(&["--config", "graph", "memory", "search", "q"])).is_some());
    }

    #[test]
    fn live_surfaces_get_no_hint() {
        assert!(hint(&argv(&["search", "foo", "--only-text"])).is_none());
        assert!(hint(&argv(&["memory", "list"])).is_none());
        assert!(hint(&argv(&[])).is_none());
    }
}
