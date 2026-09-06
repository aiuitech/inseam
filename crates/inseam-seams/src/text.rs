//! The text conventions plugins share when they speak the seams: the
//! line arithmetic `scan` and `Extent::lines` agree on (1-based, inclusive),
//! the single-line previews hints and views are cut to, and which content
//! types the index reads as text at all. None of this is kernel business —
//! the kernel knows no file format — but every plugin that renders or slices
//! text must agree on it, so it lives beside the seam definitions rather than
//! being re-derived in each plugin.

use std::io::BufRead;

use inseam_kernel::fragment::Mimetype;

use crate::SeamError;

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

/// Check a scan's range before any content is read: 1-based, inclusive,
/// start no later than end. The same check guards every slice below, so a
/// connection that bypasses the operations layer answers the same error.
pub fn check_line_range(start: u64, end: u64) -> Result<(), SeamError> {
    if start == 0 || end < start {
        return Err(SeamError::ScanRange { start, end });
    }
    Ok(())
}

/// Slice a 1-based inclusive line range out of text, clamping the end to
/// the last line. A start beyond the text is [`SeamError::ScanBeyondEnd`],
/// naming the bound so a client can correct itself.
pub fn slice_lines(text: &str, start: u64, end: u64) -> Result<String, SeamError> {
    check_line_range(start, end)?;
    let lines_total = count_lines(text);
    if start > lines_total {
        return Err(SeamError::ScanBeyondEnd { start, lines_total });
    }
    let end = end.min(lines_total);
    assert!(end >= start);
    let out: Vec<&str> = text
        .lines()
        .skip(start as usize - 1)
        .take((end - start + 1) as usize)
        .collect();
    Ok(out.join("\n"))
}

/// [`slice_lines`] over a reader, stopping at line `end` instead of reading
/// the rest: how a host serves lines 5–9 of a 10 MB file without loading
/// it. Lines are decoded lossily one at a time and end exactly where
/// `str::lines` ends them — at `\n`, with a preceding `\r` dropped — so a
/// range read this way equals the same range sliced from the whole text.
pub fn slice_lines_from_reader<R: BufRead>(
    mut reader: R,
    start: u64,
    end: u64,
) -> Result<String, SeamError> {
    check_line_range(start, end)?;
    let mut out: Vec<String> = Vec::new();
    let mut raw: Vec<u8> = Vec::new();
    let mut lines_read: u64 = 0;
    // Bounded by `end`: the loop leaves as soon as line `end` is in hand,
    // or earlier at end of input.
    while lines_read < end {
        raw.clear();
        let bytes = reader
            .read_until(b'\n', &mut raw)
            .map_err(|e| SeamError::failed(format!("reading lines: {e}")))?;
        if bytes == 0 {
            break;
        }
        lines_read += 1;
        if lines_read >= start {
            out.push(line_without_terminator(&raw));
        }
    }
    assert!(lines_read <= end);
    if lines_read < start {
        return Err(SeamError::ScanBeyondEnd {
            start,
            lines_total: lines_read,
        });
    }
    Ok(out.join("\n"))
}

/// One raw line as `str::lines` would yield it: no trailing `\n`, and no
/// `\r` before it.
fn line_without_terminator(raw: &[u8]) -> String {
    let without_newline = raw.strip_suffix(b"\n").unwrap_or(raw);
    let without_return = without_newline
        .strip_suffix(b"\r")
        .unwrap_or(without_newline);
    String::from_utf8_lossy(without_return).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_lines_is_one_based_inclusive_and_clamps() {
        let text = "a\nb\nc\nd\n";
        assert_eq!(slice_lines(text, 2, 3).expect("slices"), "b\nc");
        assert_eq!(slice_lines(text, 3, 99).expect("clamps"), "c\nd");
    }

    #[test]
    fn slice_lines_names_a_start_beyond_the_end() {
        let error = slice_lines("a\nb\n", 9, 12).expect_err("beyond");
        assert!(matches!(
            error,
            SeamError::ScanBeyondEnd {
                start: 9,
                lines_total: 2
            }
        ));
    }

    #[test]
    fn slice_lines_rejects_ranges_that_are_not_one_based_inclusive() {
        assert!(matches!(
            slice_lines("a\nb\n", 0, 2),
            Err(SeamError::ScanRange { start: 0, end: 2 })
        ));
        assert!(matches!(
            slice_lines("a\nb\n", 2, 1),
            Err(SeamError::ScanRange { start: 2, end: 1 })
        ));
    }

    #[test]
    fn reader_slices_equal_text_slices_and_stop_at_the_end_line() {
        let cases: &[(&str, u64, u64)] = &[
            ("a\nb\nc\nd\n", 2, 3),
            ("a\r\nb\r\nc", 1, 2),
            ("a\nb\nc", 3, 3),
            ("a\n\nc\n", 1, 2),
            ("no newline at all", 1, 1),
            ("a\nb\n", 1, 99),
            ("h\u{e9}llo\nw\u{f6}rld\n", 2, 2),
        ];
        for (text, start, end) in cases {
            let from_text = slice_lines(text, *start, *end).expect("text slices");
            let mut reader = CountingReader::new(text.as_bytes());
            let from_reader =
                slice_lines_from_reader(&mut reader, *start, *end).expect("reader slices");
            assert_eq!(from_reader, from_text, "{text:?} {start}-{end}");
            let expected_reads = (*end).min(count_lines(text)) + 1;
            assert!(
                reader.reads <= expected_reads,
                "{text:?} {start}-{end}: {} reads for {expected_reads} lines",
                reader.reads
            );
        }
    }

    #[test]
    fn reader_slices_report_the_bound_when_start_is_beyond_the_end() {
        let error = slice_lines_from_reader("a\nb\n".as_bytes(), 5, 6).expect_err("beyond");
        assert!(matches!(
            error,
            SeamError::ScanBeyondEnd {
                start: 5,
                lines_total: 2
            }
        ));
        assert!(matches!(
            slice_lines_from_reader("a\n".as_bytes(), 0, 1),
            Err(SeamError::ScanRange { start: 0, end: 1 })
        ));
    }

    #[test]
    fn reader_slices_decode_invalid_utf8_lossily() {
        let bytes: &[u8] = b"ok\nbad \xff byte\nlast";
        let sliced = slice_lines_from_reader(bytes, 2, 3).expect("slices");
        assert_eq!(sliced, "bad \u{fffd} byte\nlast");
    }

    /// A reader that counts `read_until` calls, so a test can prove a
    /// range read stops at its end line.
    struct CountingReader<'a> {
        inner: &'a [u8],
        reads: u64,
    }

    impl<'a> CountingReader<'a> {
        fn new(inner: &'a [u8]) -> Self {
            Self { inner, reads: 0 }
        }
    }

    impl std::io::Read for CountingReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(buf)
        }
    }

    impl BufRead for CountingReader<'_> {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            self.reads += 1;
            self.inner.fill_buf()
        }

        fn consume(&mut self, amount: usize) {
            self.inner.consume(amount)
        }
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
