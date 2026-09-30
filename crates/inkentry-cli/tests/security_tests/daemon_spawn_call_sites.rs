// The interactive-stdin gate keeps every integration test from reaching these callers, so
// the last hop is pinned lexically: constructing a config at the call site instead of
// forwarding the loaded one would disconnect the personal config from every auto-started
// daemon while the crate stays green.

use std::path::{Path, PathBuf};

const CALL: &str = "ensure_server_running(";

const FORWARDED: [&str; 2] = ["&cfg", "cfg"];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("reading a CLI source directory") {
        let path = entry.expect("reading a directory entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn config_argument(after_call: &str) -> Option<String> {
    let inside = after_call.split_once(')')?.0;
    let cfg = inside.split(',').nth(1)?;
    Some(cfg.trim().to_string())
}

#[test]
fn every_auto_start_call_site_forwards_the_loaded_config() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);

    let mut call_sites = 0;
    let mut offenders = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("reading a CLI source file");
        for (start, _) in text.match_indices(CALL) {
            if text[..start].ends_with("fn ") {
                continue;
            }
            call_sites += 1;
            let after_call = &text[start + CALL.len()..];
            match config_argument(after_call) {
                Some(arg) if FORWARDED.contains(&arg.as_str()) => {}
                other => offenders.push(format!(
                    "{}:{}: config argument is {:?}",
                    file.display(),
                    text[..start].lines().count(),
                    other.unwrap_or_else(|| after_call.lines().next().unwrap_or("").to_string())
                )),
            }
        }
    }

    assert!(
        call_sites >= 2,
        "expected to find the auto-start call sites and found {call_sites}; if the call moved \
         or was renamed, this guard is scanning for nothing"
    );
    assert!(
        offenders.is_empty(),
        "an auto-start call site builds its own config instead of forwarding the loaded one, \
         so the personal config's llm_url would never reach the daemon it starts. Found: \
         {offenders:#?}"
    );
}
