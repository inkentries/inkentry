//! Splits the body of a Ruby class too large to keep whole into runs of one
//! kind of declaration. A Rails model declares its associations, validations,
//! scopes and callbacks at class level, dozens to a model; windowed together
//! they embed as "this model in general" and match no particular question
//! about it, so each run becomes its own chunk named for what it declares.

/// A run of statements: first and last line (1-based, inclusive) and the kind
/// of declaration it makes, `None` for statements of no known kind.
pub(super) type Run = (usize, usize, Option<&'static str>);

/// The kind of class-level declaration a statement makes, or `None` for one
/// this module does not recognise (`include`, a custom macro, a method call).
fn kind(statement: &str) -> Option<&'static str> {
    let word: String = statement
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    Some(match word.as_str() {
        "belongs_to" | "has_many" | "has_one" | "has_and_belongs_to_many" => "associations",
        "validates" | "validate" | "validates_with" => "validations",
        w if w.starts_with("validates_") => "validations",
        "scope" | "default_scope" => "scopes",
        w if w.starts_with("before_") || w.starts_with("after_") || w.starts_with("around_") => {
            "callbacks"
        }
        "enum" | "monetize" | "attribute" | "store_accessor" | "encrypts" | "serialize"
        | "normalizes" | "alias_attribute" => "attributes",
        "delegate" => "delegations",
        w if is_constant_name(w) && is_assignment(&statement[w.len()..]) => "constants",
        _ => return None,
    })
}

fn is_constant_name(word: &str) -> bool {
    word.len() > 1
        && word.starts_with(|c: char| c.is_ascii_uppercase())
        && word
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

// `= value`, but not `==`, `=~` or `=>`: a heredoc's closing `MSG` and
// `STATUSES.each` start with an upper-case word too.
fn is_assignment(rest: &str) -> bool {
    let rest = rest.trim_start();
    rest.starts_with('=')
        && !rest.starts_with("==")
        && !rest.starts_with("=~")
        && !rest.starts_with("=>")
}

/// Cuts the 1-based inclusive line range `start..=end` of `lines` into runs of
/// statements of one kind. Statements start at the body's indentation, read
/// from the first statement after `decl_line` (or from `start` when the gap
/// does not hold the declaration); deeper lines continue the statement above
/// them. Comments go with the statement after them, or with the last run when
/// nothing follows. A statement of no known kind extends the run before it,
/// and a run of no known kind with fewer than `min_word_chars` letters and
/// digits joins a neighbour rather than standing alone.
pub(super) fn runs(
    lines: &[&str],
    start: usize,
    end: usize,
    decl_line: Option<usize>,
    min_word_chars: usize,
) -> Vec<Run> {
    let indent = |l: &str| l.len() - l.trim_start().len();
    let is_code = |l: &str| {
        let t = l.trim_start();
        !t.is_empty() && !t.starts_with('#')
    };
    let body_from = decl_line.map_or(start, |d| d + 1);
    let Some(body_indent) = (body_from..=end)
        .map(|n| lines[n - 1])
        .find(|l| is_code(l) && !l.starts_with("=begin"))
        .map(indent)
    else {
        return vec![(start, end, None)];
    };

    let mut runs: Vec<Run> = Vec::new();
    let mut lead: Option<usize> = None;
    let mut in_block_comment = false;
    for n in start..=end {
        let line = lines[n - 1];
        if line.trim().is_empty() {
            continue;
        }
        if in_block_comment || line.starts_with("=begin") || !is_code(line) {
            if line.starts_with("=begin") {
                in_block_comment = true;
            } else if line.starts_with("=end") {
                in_block_comment = false;
            }
            lead.get_or_insert(n);
            continue;
        }
        let text = line.trim_start();
        let opens = indent(line) == body_indent && !matches!(text, "end" | "}" | ")" | "]");
        let this = kind(text);
        let first = lead.take().unwrap_or(n);
        match runs.last_mut() {
            Some(run) if !opens || this.is_none() || run.2 == this => run.1 = n,
            _ => runs.push((first, n, this)),
        }
    }
    match runs.last_mut() {
        Some(run) => run.1 = end,
        None => runs.push((start, end, None)),
    }
    absorb_short_runs(runs, lines, decl_line, min_word_chars)
}

// Only the first run can be of no known kind: a later statement of no known
// kind extends the run before it. When that first run is too short to stand
// alone (`private`, `extend Foo`) and does not hold the class's declaration,
// it joins the run after it, so no line is lost.
fn absorb_short_runs(
    mut runs: Vec<Run>,
    lines: &[&str],
    decl_line: Option<usize>,
    min_word_chars: usize,
) -> Vec<Run> {
    if let [(first, last, None), next, ..] = runs.as_mut_slice() {
        let holds_decl = decl_line.is_some_and(|d| (*first..=*last).contains(&d));
        let word_chars = lines[*first - 1..*last]
            .iter()
            .flat_map(|l| l.chars())
            .filter(|c| c.is_alphanumeric())
            .count();
        if !holds_decl && word_chars < min_word_chars {
            next.0 = *first;
            runs.remove(0);
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::runs;

    #[test]
    fn a_comment_ending_the_gap_stays_in_the_last_run() {
        let lines = [
            "class Invoice < ApplicationRecord",
            "  has_many :fees",
            "  # a note about the associations above",
        ];
        assert_eq!(
            runs(&lines, 1, 3, Some(1), 16),
            vec![(1, 1, None), (2, 3, Some("associations"))]
        );
    }

    #[test]
    fn a_short_leading_run_joins_the_run_after_it() {
        let lines = ["  private", "  has_many :fees"];
        assert_eq!(
            runs(&lines, 1, 2, None, 16),
            vec![(1, 2, Some("associations"))]
        );
    }

    #[test]
    fn a_short_leading_run_holding_the_declaration_stands_alone() {
        let lines = ["class Tax", "  belongs_to :invoice"];
        assert_eq!(
            runs(&lines, 1, 2, Some(1), 16),
            vec![(1, 1, None), (2, 2, Some("associations"))]
        );
    }
}
