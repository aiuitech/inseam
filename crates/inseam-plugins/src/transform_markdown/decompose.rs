//! The `text/markdown` structural transform: decompose by the document's own
//! semantic structure — heading sections nested as authored, each
//! `contains`-related to its parent, with links becoming `text/uri-list`
//! children of the section they appear in (`design/indexing.md`).

use pulldown_cmark::{Event, Options, Parser, Tag};

use inseam_kernel::fragment::{Extent, Mimetype, NewFragment, RelationKind, Sprout};

/// The markdown transform's own relation kind: a section `links-to` the URL
/// fragment found inside it — plugin vocabulary, not the kernel's.
pub fn links_to() -> RelationKind {
    RelationKind::new("links-to").expect("literal relation kind is valid")
}

// Headingless documents degrade to the chunker's paragraph chunking, so
// the two structural transforms agree on what a structureless text becomes.
use crate::transform_chunker::chunk;

/// The root essences this transform decomposes: markdown, and plain text,
/// whose `#` headings are the same outline and whose lack of them is the
/// same chunking.
pub fn claims_essence(essence: &str) -> bool {
    matches!(essence, "text/markdown" | "text/plain")
}

/// Decompose a document into its heading outline; sections keep the
/// parent's `mimetype`. Documents without headings fall back to paragraph
/// chunking. Links (http/https) become `uri-list` child sprouts of the
/// section containing them.
pub fn decompose(mimetype: &Mimetype, text: &str) -> Vec<Sprout> {
    let outline = scan(text);
    let lines = LineIndex::new(text);

    if outline.headings.is_empty() {
        let mut sprouts = chunk::chunk(mimetype, text);
        attach_links(&mut sprouts, &outline.links, &lines);
        return sprouts;
    }

    let mut sections = build_sections(&outline.headings, text.len());
    let first = outline.headings[0].start;
    if !text[..first].trim().is_empty() {
        sections.insert(
            0,
            Section {
                start: 0,
                own_end: first,
                end: first,
                children: Vec::new(),
            },
        );
    }
    let mut sprouts: Vec<Sprout> = sections
        .iter()
        .map(|s| s.to_sprout(mimetype, text, &lines))
        .collect();

    for (byte, url) in &outline.links {
        let line = lines.line_of(*byte);
        let link = Sprout::leaf(
            NewFragment {
                mimetype: Mimetype::uri_list(),
                text: Some(url.clone()),
                extent: Some(Extent::lines(line, line)),
                content_address: None,
            },
            links_to(),
        );
        match deepest_containing(&mut sprouts, &sections, *byte) {
            Some(sprout) => sprout.children.push(link),
            None => {
                if let Some(first) = sprouts.first_mut() {
                    first.children.push(link);
                }
            }
        }
    }
    sprouts
}

struct Outline {
    /// (level, start byte) per heading, in document order.
    headings: Vec<Heading>,
    /// (start byte, url) per http(s) link.
    links: Vec<(usize, String)>,
}

#[derive(Debug, Clone, Copy)]
struct Heading {
    level: usize,
    start: usize,
}

fn scan(text: &str) -> Outline {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    let mut headings = Vec::new();
    let mut links = Vec::new();
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => headings.push(Heading {
                level: level as usize,
                start: range.start,
            }),
            Event::Start(Tag::Link { dest_url, .. })
                if (dest_url.starts_with("http://") || dest_url.starts_with("https://")) =>
            {
                links.push((range.start, dest_url.into_string()));
            }
            _ => {}
        }
    }
    Outline { headings, links }
}

#[derive(Debug)]
struct Section {
    /// Byte where the section (its heading) starts.
    start: usize,
    /// Byte where the section's own text ends: the next heading of any level.
    own_end: usize,
    /// Byte where the whole section ends: the next heading at its level or
    /// shallower. The extent covers this full span, subsections included.
    end: usize,
    children: Vec<Section>,
}

fn build_sections(headings: &[Heading], end_limit: usize) -> Vec<Section> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < headings.len() {
        let h = headings[i];
        let mut j = i + 1;
        while j < headings.len() && headings[j].level > h.level {
            j += 1;
        }
        let end = headings.get(j).map_or(end_limit, |n| n.start);
        let own_end = headings.get(i + 1).map_or(end_limit, |n| n.start).min(end);
        out.push(Section {
            start: h.start,
            own_end,
            end,
            children: build_sections(&headings[i + 1..j], end),
        });
        i = j;
    }
    out
}

impl Section {
    fn to_sprout(&self, mimetype: &Mimetype, text: &str, lines: &LineIndex) -> Sprout {
        let body = text[self.start..self.own_end].trim_end();
        let start_line = lines.line_of(self.start);
        let end_line = lines.line_of(self.end.saturating_sub(1)).max(start_line);
        Sprout {
            fragment: NewFragment {
                mimetype: mimetype.clone(),
                text: Some(body.to_string()),
                extent: Some(Extent::lines(start_line, end_line)),
                content_address: None,
            },
            relation: RelationKind::contains(),
            children: self
                .children
                .iter()
                .map(|c| c.to_sprout(mimetype, text, lines))
                .collect(),
        }
    }
}

/// Walk the sprout tree and the parallel section tree to find the deepest
/// sprout whose section span contains `byte`.
fn deepest_containing<'a>(
    sprouts: &'a mut [Sprout],
    sections: &[Section],
    byte: usize,
) -> Option<&'a mut Sprout> {
    let idx = sections
        .iter()
        .position(|s| s.start <= byte && byte < s.end)?;
    let section = &sections[idx];
    let sprout = &mut sprouts[idx];
    // Section children lead the sprout's child list (links append after), so
    // indices line up between the two trees.
    if section
        .children
        .iter()
        .any(|c| c.start <= byte && byte < c.end)
    {
        let n = section.children.len();
        deepest_containing(&mut sprout.children[..n], &section.children, byte)
    } else {
        Some(sprout)
    }
}

/// Attach links to the chunk sprout whose line extent contains them
/// (the no-headings fallback path).
fn attach_links(sprouts: &mut [Sprout], links: &[(usize, String)], lines: &LineIndex) {
    for (byte, url) in links {
        let line = lines.line_of(*byte);
        let target = sprouts.iter_mut().find(|s| {
            matches!(
                s.fragment.extent,
                Some(Extent::Lines { start, end }) if start <= line && line <= end
            )
        });
        if let Some(target) = target {
            target.children.push(Sprout::leaf(
                NewFragment {
                    mimetype: Mimetype::uri_list(),
                    text: Some(url.clone()),
                    extent: Some(Extent::lines(line, line)),
                    content_address: None,
                },
                links_to(),
            ));
        }
    }
}

/// Byte offset -> 1-based line number.
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { starts }
    }

    fn line_of(&self, byte: usize) -> u64 {
        self.starts.partition_point(|&s| s <= byte) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "\
intro paragraph before any heading

# Kitchen

Planning the [moodboard](https://example.com/mood) here.

## Budget

Cabinet quotes and totals.

## Timeline

Demo in June.

# Garden

Fence and beds.
";

    #[test]
    fn builds_the_heading_outline() {
        let sprouts = decompose(&Mimetype::markdown(), DOC);
        // preamble + two H1s
        assert_eq!(sprouts.len(), 3);
        let kitchen = &sprouts[1];
        let text = kitchen.fragment.text.as_deref().expect("has text");
        assert!(text.starts_with("# Kitchen"));
        assert!(!text.contains("Cabinet"), "own text excludes subsections");
        // Budget + Timeline subsections, plus the link that sits in the
        // section's own text.
        let subsections: Vec<_> = kitchen
            .children
            .iter()
            .filter(|c| c.fragment.mimetype.essence() == "text/markdown")
            .collect();
        assert_eq!(subsections.len(), 2);
        assert!(
            subsections[0]
                .fragment
                .text
                .as_deref()
                .expect("text")
                .starts_with("## Budget")
        );
    }

    #[test]
    fn extents_cover_full_sections_in_lines() {
        let sprouts = decompose(&Mimetype::markdown(), DOC);
        let kitchen = &sprouts[1];
        let Some(Extent::Lines { start, end }) = kitchen.fragment.extent else {
            panic!("kitchen section has a line extent");
        };
        // "# Kitchen" is line 3; the section runs until "# Garden" (line 15).
        assert_eq!(start, 3);
        assert!(end >= 13, "extent spans subsections, got end {end}");
        let garden = &sprouts[2];
        let Some(Extent::Lines { start, .. }) = garden.fragment.extent else {
            panic!("garden section has a line extent");
        };
        assert_eq!(start, 15);
    }

    #[test]
    fn links_become_uri_list_children_of_their_section() {
        let sprouts = decompose(&Mimetype::markdown(), DOC);
        let kitchen = &sprouts[1];
        let links: Vec<_> = kitchen
            .children
            .iter()
            .filter(|c| c.fragment.mimetype.essence() == "text/uri-list")
            .collect();
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].fragment.text.as_deref(),
            Some("https://example.com/mood")
        );
        assert_eq!(links[0].relation, links_to());
        assert_eq!(links[0].fragment.extent, Some(Extent::lines(5, 5)));
    }

    #[test]
    fn headingless_documents_fall_back_to_chunks() {
        let doc = "just a paragraph\n\nand [a link](https://example.com/x) in another\n";
        let sprouts = decompose(&Mimetype::markdown(), doc);
        assert!(!sprouts.is_empty());
        assert!(
            sprouts
                .iter()
                .all(|s| s.fragment.mimetype.essence() == "text/markdown")
        );
        let links: usize = sprouts.iter().map(|s| s.children.len()).sum();
        assert_eq!(links, 1);
    }

    #[test]
    fn ignores_anchor_and_relative_links() {
        let doc = "# A\n\nsee [there](#below) and [file](./local.md)\n";
        let sprouts = decompose(&Mimetype::markdown(), doc);
        assert!(sprouts[0].children.is_empty());
    }

    #[test]
    fn empty_document_yields_nothing() {
        assert!(decompose(&Mimetype::markdown(), "").is_empty());
        assert!(decompose(&Mimetype::markdown(), "   \n\n  ").is_empty());
    }
}
