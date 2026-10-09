//! Splits the body of a Ruby class too large to keep whole into runs of one
//! kind of declaration. A Rails model declares its associations, validations,
//! scopes and callbacks at class level, dozens to a model; windowed together
//! they embed as "this model in general" and match no particular question
//! about it, so each run becomes its own chunk named for what it declares.

/// The kind of class-level declaration a statement makes, or `None` for one
/// this module does not recognise (`include`, a custom macro, a method call).
fn family(statement: &str) -> Option<&'static str> {
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
        w if w.len() > 1
            && w.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && w.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') =>
        {
            "constants"
        }
        _ => return None,
    })
}

/// Cuts the 1-based inclusive line range `start..=end` of `lines` into runs of
/// statements of one family, each `(first, last, family)`. Statements start at
/// the body's indentation, read from the first statement at or after
/// `body_from`; deeper lines continue the statement above them. Comments go
/// with the statement after them, or with the last run when nothing follows. A statement of no known family extends the run before it.
pub(super) fn family_segments(
    lines: &[&str],
    start: usize,
    end: usize,
    body_from: usize,
) -> Vec<(usize, usize, Option<&'static str>)> {
    let indent = |l: &str| l.len() - l.trim_start().len();
    let is_code = |l: &str| {
        let t = l.trim_start();
        !t.is_empty() && !t.starts_with('#')
    };
    let Some(body_indent) = (body_from..=end)
        .map(|n| lines[n - 1])
        .find(|l| is_code(l))
        .map(indent)
    else {
        return vec![(start, end, None)];
    };

    let mut runs: Vec<(usize, usize, Option<&'static str>)> = Vec::new();
    let mut lead: Option<usize> = None;
    for n in start..=end {
        let line = lines[n - 1];
        if line.trim().is_empty() {
            continue;
        }
        if !is_code(line) {
            lead.get_or_insert(n);
            continue;
        }
        let text = line.trim_start();
        let opens = indent(line) == body_indent && !matches!(text, "end" | "}" | ")" | "]");
        let kind = family(text);
        let first = lead.take().unwrap_or(n);
        match runs.last_mut() {
            Some(run) if !opens || kind.is_none() || run.2 == kind => run.1 = n,
            _ => runs.push((first, n, kind)),
        }
    }
    match runs.last_mut() {
        Some(run) => run.1 = end,
        None => runs.push((start, end, None)),
    }
    runs
}
