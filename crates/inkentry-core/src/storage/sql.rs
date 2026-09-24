// Callers chunk input lists at this size to stay under SQLite's bound-parameter
// cap (999 on older builds). Halve it for a statement that binds the same slice
// twice.
pub(crate) const SQLITE_MAX_BIND: usize = 30_000;

// Empty for `n == 0`, so callers must return early rather than emit `IN ()`.
// Anonymous `?` rather than `?N` lets a query bind the same slice twice.
pub(crate) fn placeholders(n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let mut s = "?,".repeat(n);
    s.pop(); // drop trailing comma
    s
}

#[cfg(test)]
mod tests {
    use super::placeholders;

    #[test]
    fn zero_is_empty() {
        assert_eq!(placeholders(0), "");
    }

    #[test]
    fn one_placeholder() {
        assert_eq!(placeholders(1), "?");
    }

    #[test]
    fn three_placeholders() {
        assert_eq!(placeholders(3), "?,?,?");
    }
}
