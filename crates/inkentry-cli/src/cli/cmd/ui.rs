use super::color::cprintln;
use indicatif::{ProgressBar, ProgressStyle};
use std::io::IsTerminal as _;

pub(crate) fn is_tty() -> bool {
    std::io::stderr().is_terminal()
}

pub(crate) fn spinner(message: impl Into<std::borrow::Cow<'static, str>>) -> ProgressBar {
    if is_tty() && !crate::utils::is_agent_mode() {
        let sp = ProgressBar::new_spinner();
        sp.set_message(message);
        sp.enable_steady_tick(std::time::Duration::from_millis(80));
        sp
    } else {
        ProgressBar::hidden()
    }
}

pub(crate) fn progress_style(prefix: &str) -> ProgressStyle {
    ProgressStyle::with_template(&format!(
        "{{spinner:.cyan}} {prefix} [{{bar:38.cyan/blue}}] {{pos}}/{{len}} {{wide_msg}}"
    ))
    .unwrap()
    .progress_chars("=>-")
}

pub(crate) fn short_path(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

pub(crate) fn print_chunks_text(chunks: &[crate::search::SearchResult]) {
    for (i, c) in chunks.iter().enumerate() {
        let name = c.name.as_deref().unwrap_or("<anonymous>");
        cprintln!(
            "{:2}. \x1b[2m{}:{}-{}\x1b[0m  \x1b[33m[{}: {}]\x1b[0m",
            i + 1,
            c.language,
            c.start_line,
            c.end_line,
            c.node_type,
            name,
        );
        let lines: Vec<&str> = c.content.lines().collect();
        let preview = lines.len().min(6);
        for line in &lines[..preview] {
            println!("    {line}");
        }
        if lines.len() > preview {
            cprintln!("    \x1b[2m… ({} more lines)\x1b[0m", lines.len() - preview);
        }
        println!();
    }
}
