//! The text conventions plugins share when they speak the seams: the
//! line arithmetic `scan` and `Extent::lines` agree on (1-based, inclusive),
//! the single-line previews hints and views are cut to, and which content
//! types the index reads as text at all. None of this is kernel business —
//! the kernel knows no file format — but every plugin that renders or slices
//! text must agree on it, so it lives beside the seam definitions rather than
//! being re-derived in each plugin.

use inseam_kernel::fragment::Mimetype;

/// Whether content of this type is worth reading and indexing as text:
/// all of `text/*` plus the structured-text application types. This is
/// indexing policy shared by the connection (lines vs. bytes in the
/// envelope), the chunker (what it claims), and the operations (`scan` and
/// `fetch` return text only for these) — one list, so they never disagree.
pub fn is_indexable_text(mimetype: &Mimetype) -> bool {
    if mimetype.is_text() {
        return true;
    }
    matches!(
        mimetype.essence(),
        "application/json"
            | "application/x-yaml"
            | "application/yaml"
            | "application/toml"
            | "application/xml"
            | "application/javascript"
            | "application/x-sh"
            | "application/sql"
            | "image/svg+xml"
    )
}

/// Truncate to at most `max` characters on a char boundary, appending an
/// ellipsis when anything was cut.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// Collapse all whitespace runs (including newlines) into single spaces.
pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A single-line, length-bounded preview of fragment text for hints and views.
pub fn preview(s: &str, max: usize) -> String {
    truncate_chars(&collapse_ws(s), max)
}

/// Line count as `scan` and extents see it.
pub fn count_lines(text: &str) -> u64 {
    text.lines().count() as u64
}

/// Slice a 1-based inclusive line range out of text, clamping the end. A
/// start beyond the text is an error message naming the bounds.
pub fn slice_lines(text: &str, start: u64, end: u64) -> Result<String, String> {
    let len = count_lines(text);
    if start == 0 || start > len {
        return Err(format!("scan start line {start} is beyond the {len}-line source"));
    }
    let end = end.min(len).max(start);
    let out: Vec<&str> = text
        .lines()
        .skip(start as usize - 1)
        .take((end - start + 1) as usize)
        .collect();
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_lines_is_one_based_inclusive_and_clamps() {
        let text = "a\nb\nc\nd\n";
        assert_eq!(slice_lines(text, 2, 3).expect("slices"), "b\nc");
        assert_eq!(slice_lines(text, 3, 99).expect("clamps"), "c\nd");
        assert!(slice_lines(text, 9, 12).is_err());
        assert!(slice_lines(text, 0, 2).is_err());
    }

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

    #[test]
    fn indexable_text_covers_text_and_structured_application_types() {
        let m = |s: &str| Mimetype::parse(s).expect("valid mimetype");
        assert!(is_indexable_text(&m("text/markdown")));
        assert!(is_indexable_text(&m("application/json")));
        assert!(is_indexable_text(&m("image/svg+xml")));
        assert!(!is_indexable_text(&m("image/jpeg")));
        assert!(!is_indexable_text(&m("application/pdf")));
    }
}
