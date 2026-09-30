// Callers chunk input lists at this size to stay under SQLite's bound-parameter
// cap (999 on older builds). Halve it for a statement that binds the same slice
// twice.
pub(crate) const SQLITE_MAX_BIND: usize = 30_000;

// Anonymous `?` rather than `?N` lets a query bind the same slice twice.
// `n == 0` yields `NULL`, so `IN (...)` stays valid SQL and matches no rows.
pub(crate) fn placeholders(n: usize) -> String {
    if n == 0 {
        return "NULL".to_string();
    }
    let mut s = "?,".repeat(n);
    s.pop();
    s
}

#[cfg(test)]
mod tests {
    use super::placeholders;

    #[test]
    fn zero_matches_nothing() {
        assert_eq!(placeholders(0), "NULL");
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
