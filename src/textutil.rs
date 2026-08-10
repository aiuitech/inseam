//! Small text helpers shared across the index pipeline and result rendering.

/// Truncate to at most `max` characters on a char boundary, appending an
/// ellipsis when anything was cut.
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// Collapse all whitespace runs (including newlines) into single spaces.
pub(crate) fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A single-line, length-bounded preview of fragment text for hints and views.
pub(crate) fn preview(s: &str, max: usize) -> String {
    truncate_chars(&collapse_ws(s), max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate_chars("héllo wörld", 30), "héllo wörld");
        let cut = truncate_chars("héllo wörld", 6);
        assert!(cut.chars().count() <= 6);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn preview_is_single_line() {
        assert_eq!(preview("a\n  b\t\tc\n", 20), "a b c");
    }
}
