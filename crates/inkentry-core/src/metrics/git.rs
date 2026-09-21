//! Plain commit-history facts for a metrics snapshot: HEAD's identity and the
//! commits reachable from it that fall inside a window. Deliberately
//! separate from `storage::git_notes`, which reads the memory carrier
//! (`refs/notes/inkentry`) rather than commit history.

use anyhow::Result;
use std::path::Path;
use tokio::process::Command;

/// A commit reachable from HEAD, with its committer time and the net lines
/// changed (added + removed) it introduced.
#[derive(Debug, Clone)]
pub struct CommitInWindow {
    pub sha: String,
    pub committer_time: i64,
    pub lines_changed: u64,
}

/// HEAD's commit sha and committer time (unix seconds), or `None` when `root`
/// is not inside a git repository, or is one with no commits yet. Both cases
/// leave nothing to anchor a window to, so the caller treats them alike
/// (ADR-098: "a project that is not a git repository gets no commit-based
/// metrics").
pub async fn head_commit(root: &Path) -> Option<(String, i64)> {
    let out = Command::new("git")
        .args(["log", "-1", "--format=%H%x1f%ct"])
        .current_dir(root)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_head_line(&String::from_utf8_lossy(&out.stdout))
}

fn parse_head_line(s: &str) -> Option<(String, i64)> {
    let (sha, ct) = s.trim().split_once('\u{1f}')?;
    let ct: i64 = ct.trim().parse().ok()?;
    Some((sha.to_string(), ct))
}

/// The full sha and committer time of every commit reachable from HEAD whose
/// committer time falls in `[window_start, window_end]` (both inclusive),
/// without per-commit diff stats.
///
/// Cheaper than [`commits_in_window`]: no `--numstat`, so git never computes
/// a diff for any commit. Used by `inkentry status`'s cheap `rec.commit_coverage`
/// check, which has no use for `cmp.lines_per_decision`'s line counts.
pub async fn commit_shas_in_window(
    root: &Path,
    window_start: i64,
    window_end: i64,
) -> Result<Vec<String>> {
    let out = Command::new("git")
        .args(["log", "--format=%H%x1f%ct"])
        .current_dir(root)
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git log failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(parse_sha_committer_lines(&text, window_start, window_end))
}

fn parse_sha_committer_lines(text: &str, window_start: i64, window_end: i64) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.split_once('\u{1f}'))
        .filter_map(|(sha, ct)| {
            let ct: i64 = ct.trim().parse().ok()?;
            (ct >= window_start && ct <= window_end).then(|| sha.to_string())
        })
        .collect()
}

/// Commits reachable from HEAD whose committer time falls in
/// `[window_start, window_end]` (both inclusive), each carrying its own lines
/// added + removed.
///
/// One `git log --numstat` call covers the whole history; the window filter
/// runs on the parsed committer timestamps here rather than via git's own
/// `--since`/`--until`, so the boundary matches exactly the committer time
/// the snapshot header reports HEAD under — git's date-string parsing is not
/// guaranteed to agree with a raw epoch comparison. Binary files (numstat's
/// `-\t-\tpath`) contribute 0; merge commits carry no numstat under plain
/// `git log` and so contribute 0 too, matching the tool's own default.
pub async fn commits_in_window(
    root: &Path,
    window_start: i64,
    window_end: i64,
) -> Result<Vec<CommitInWindow>> {
    let out = Command::new("git")
        .args(["log", "--format=@@inkentry@@%H%x1f%ct", "--numstat"])
        .current_dir(root)
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git log failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(parse_log_numstat(&text)
        .into_iter()
        .filter(|c| c.committer_time >= window_start && c.committer_time <= window_end)
        .collect())
}

/// Marker prefixing each commit's formatted header line, distinguishing it
/// from a numstat data line. Arbitrary but distinctive; a numstat path could
/// in principle collide with it, which would misattribute one file's line
/// count to the wrong commit — a cosmetic risk for a review-surface metric,
/// not a correctness-critical one.
const HEADER_MARKER: &str = "@@inkentry@@";

fn parse_log_numstat(text: &str) -> Vec<CommitInWindow> {
    let mut commits = Vec::new();
    let mut current: Option<CommitInWindow> = None;
    for line in text.lines() {
        if let Some(header) = line.strip_prefix(HEADER_MARKER) {
            if let Some(c) = current.take() {
                commits.push(c);
            }
            if let Some((sha, ct)) = header.split_once('\u{1f}')
                && let Ok(ct) = ct.trim().parse::<i64>()
            {
                current = Some(CommitInWindow {
                    sha: sha.to_string(),
                    committer_time: ct,
                    lines_changed: 0,
                });
            }
            continue;
        }
        let Some(c) = current.as_mut() else { continue };
        let mut cols = line.splitn(3, '\t');
        if let (Some(added), Some(removed), Some(_path)) = (cols.next(), cols.next(), cols.next()) {
            c.lines_changed += added.parse::<u64>().unwrap_or(0);
            c.lines_changed += removed.parse::<u64>().unwrap_or(0);
        }
    }
    if let Some(c) = current.take() {
        commits.push(c);
    }
    commits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_line_parses_sha_and_committer_time() {
        assert_eq!(
            parse_head_line("abc123\u{1f}1700000000\n"),
            Some(("abc123".to_string(), 1_700_000_000))
        );
    }

    #[test]
    fn a_non_git_directory_yields_no_header_line() {
        assert_eq!(parse_head_line(""), None);
        assert_eq!(parse_head_line("not-a-valid-line"), None);
    }

    #[test]
    fn numstat_lines_sum_added_and_removed_per_commit() {
        let text = format!(
            "{HEADER_MARKER}aaa\u{1f}100\n3\t1\tfoo.rs\n0\t2\tbar.rs\n\n\
             {HEADER_MARKER}bbb\u{1f}200\n5\t0\tbaz.rs\n"
        );
        let commits = parse_log_numstat(&text);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha, "aaa");
        assert_eq!(commits[0].committer_time, 100);
        assert_eq!(commits[0].lines_changed, 6);
        assert_eq!(commits[1].sha, "bbb");
        assert_eq!(commits[1].lines_changed, 5);
    }

    #[test]
    fn binary_file_markers_contribute_zero_lines() {
        let text = format!("{HEADER_MARKER}aaa\u{1f}100\n-\t-\timage.png\n2\t1\tfoo.rs\n");
        let commits = parse_log_numstat(&text);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].lines_changed, 3);
    }

    #[test]
    fn sha_committer_lines_are_filtered_to_the_window() {
        let text = "aaa\u{1f}100\nbbb\u{1f}200\nccc\u{1f}300\n";
        let shas = parse_sha_committer_lines(text, 150, 250);
        assert_eq!(shas, vec!["bbb".to_string()]);
    }

    #[test]
    fn a_merge_commit_with_no_numstat_lines_still_counts_as_a_commit_with_zero_lines() {
        let text =
            format!("{HEADER_MARKER}merge\u{1f}150\n\n{HEADER_MARKER}aaa\u{1f}100\n1\t1\tf.rs\n");
        let commits = parse_log_numstat(&text);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].lines_changed, 0);
        assert_eq!(commits[1].lines_changed, 2);
    }
}
