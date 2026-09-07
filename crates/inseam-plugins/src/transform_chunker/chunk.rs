//! Fallback chunker for structureless text: paragraph-boundary chunks merged
//! toward a target size. The design rejects fixed-size chunking as the
//! primary decomposition but keeps a dumb chunker as the fallback transform
//! for text without semantic structure (`design/indexing.md`).

use inseam_kernel::fragment::{Extent, Mimetype, NewFragment, RelationKind, Sprout};

/// Aim for chunks around this many characters; break at paragraph boundaries
/// once past it, and hard-break anywhere past twice it.
const TARGET_CHARS: usize = 1_600;

pub fn chunk(mimetype: &Mimetype, text: &str) -> Vec<Sprout> {
    chunk_with_target(mimetype, text, TARGET_CHARS)
}

pub(crate) fn chunk_with_target(mimetype: &Mimetype, text: &str, target: usize) -> Vec<Sprout> {
    let lines: Vec<&str> = text.lines().collect();
    let mut sprouts = Vec::new();
    let mut start: Option<usize> = None; // 0-based first line of current chunk
    let mut chars = 0usize;

    for (i, line) in lines.iter().enumerate() {
        let blank = line.trim().is_empty();
        if start.is_none() {
            if blank {
                continue;
            }
            start = Some(i);
        }
        chars += line.len() + 1;
        let over_target_at_boundary = blank && chars >= target;
        let hard_over = chars >= target * 2;
        if over_target_at_boundary || hard_over {
            push_chunk(&mut sprouts, mimetype, &lines, start.take(), i);
            chars = 0;
        }
    }
    push_chunk(
        &mut sprouts,
        mimetype,
        &lines,
        start,
        lines.len().saturating_sub(1),
    );
    sprouts
}

fn push_chunk(
    out: &mut Vec<Sprout>,
    mimetype: &Mimetype,
    lines: &[&str],
    start: Option<usize>,
    end: usize,
) {
    let Some(start) = start else { return };
    // Trim trailing blank lines out of the chunk.
    let mut end = end.min(lines.len().saturating_sub(1));
    while end > start && lines[end].trim().is_empty() {
        end -= 1;
    }
    let body = lines[start..=end].join("\n");
    if body.trim().is_empty() {
        return;
    }
    out.push(Sprout::leaf(
        NewFragment {
            mimetype: mimetype.clone(),
            text: Some(body),
            extent: Some(Extent::lines(start as u64 + 1, end as u64 + 1)),
            content_address: None,
        },
        RelationKind::contains(),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extents(sprouts: &[Sprout]) -> Vec<(u64, u64)> {
        sprouts
            .iter()
            .map(|s| match s.fragment.extent {
                Some(Extent::Lines { start, end }) => (start, end),
                other => panic!("chunk without line extent: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn small_text_is_one_chunk() {
        let out = chunk(&Mimetype::text_plain(), "one line\nand another\n");
        assert_eq!(out.len(), 1);
        assert_eq!(extents(&out), vec![(1, 2)]);
        assert_eq!(
            out[0].fragment.text.as_deref(),
            Some("one line\nand another")
        );
    }

    #[test]
    fn splits_at_paragraph_boundaries_past_target() {
        let para = "words ".repeat(30); // ~180 chars
        let text = format!("{para}\n\n{para}\n\n{para}\n");
        let out = chunk_with_target(&Mimetype::text_plain(), &text, 200);
        assert!(out.len() >= 2, "got {} chunks", out.len());
        // Chunks are ordered and non-overlapping.
        let ex = extents(&out);
        for pair in ex.windows(2) {
            assert!(pair[0].1 < pair[1].0);
        }
    }

    #[test]
    fn hard_breaks_giant_paragraphs() {
        let line = "x".repeat(80);
        let text = format!("{}\n", vec![line; 30].join("\n")); // one 2400-char paragraph
        let out = chunk_with_target(&Mimetype::text_plain(), &text, 500);
        assert!(out.len() >= 2);
    }

    #[test]
    fn keeps_parent_mimetype() {
        let m = Mimetype::parse("text/x-rust").expect("valid");
        let out = chunk(&m, "fn main() {}\n");
        assert_eq!(out[0].fragment.mimetype, m);
    }

    #[test]
    fn blank_only_text_yields_nothing() {
        assert!(chunk(&Mimetype::text_plain(), "\n\n  \n").is_empty());
    }
}
