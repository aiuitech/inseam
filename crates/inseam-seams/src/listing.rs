//! A folder's content, as the index sees it (`design/indexing.md`,
//! folders). A folder source has no bytes of its own: the sweep composes
//! its text from what the level below has already landed — one line per
//! direct child naming it, its type, and its summary — and hands that text
//! to the transforms like any other source content. The summarizer reads
//! it as prose; the directory transform parses it back into one entry
//! fragment per child. Both sides live here so the two can never drift:
//! [`parse`] recovers exactly the entries [`compose`] wrote.
//!
//! The format is one header line followed by entry lines whose three
//! fields — name, type, summary — are separated by a tab (shown here as
//! `⇥`):
//!
//! ```text
//! Folder Notes: 1 folder, 2 files
//! photos ⇥ inode/directory ⇥ Photos from the 2024 kitchen renovation.
//! reno.md ⇥ text/markdown ⇥ Budget notes for the kitchen.
//! IMG_1.jpg ⇥ image/jpeg ⇥ (not indexed)
//! ```
//!
//! Folders come first, then files, each in locator order, so a language
//! model whose input is cut short still sees the aggregate lines.

use inseam_kernel::fragment::Mimetype;

use crate::text::collapse_ws;

/// What stands in for a child's summary when no run has built one yet: a
/// catalog-only child, or one past the deep budget.
pub const NOT_INDEXED: &str = "(not indexed)";

/// The line appended when a folder holds more children than a listing
/// carries. It has no tab, so [`parse`] skips it by shape.
pub const MORE_ENTRIES: &str = "(more entries not listed)";

/// Direct children a listing carries at most; the rest are summarized by
/// [`MORE_ENTRIES`]. Bounds the composed text and the entry fragments a
/// folder can plant.
pub const ENTRIES_MAX: u32 = 2_000;

/// One child of a folder as the listing names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingEntry {
    /// The child's last locator segment. Never contains a tab or a line
    /// break — such a name cannot be listed and [`compose`] skips it.
    pub name: String,
    pub mimetype: Mimetype,
    /// The child's summary, whitespace-collapsed; `None` when none landed.
    pub summary: Option<String>,
}

impl ListingEntry {
    pub fn is_folder(&self) -> bool {
        self.mimetype.is_directory()
    }

    /// Whether the name can be written on one tab-separated line.
    pub fn is_listable(&self) -> bool {
        !self.name.is_empty() && !self.name.contains(['\t', '\n', '\r'])
    }

    /// The text of the entry fragment that stands for this child in the
    /// folder's subtree: the name and type, deliberately not the summary —
    /// the summary is the child's own fragment, and repeating it under the
    /// folder would rank the folder beside the child on every query.
    pub fn fragment_text(&self) -> String {
        format!("{} ({})", self.name, self.mimetype.essence())
    }
}

/// The listing text for `folder_name` over `entries`, folders first. Entries
/// whose names cannot be listed are skipped; `truncated` appends
/// [`MORE_ENTRIES`]. Bounded by the caller's entry count.
pub fn compose(folder_name: &str, entries: &[ListingEntry], truncated: bool) -> String {
    let listable: Vec<&ListingEntry> = entries.iter().filter(|e| e.is_listable()).collect();
    let folders = listable.iter().filter(|e| e.is_folder()).count();
    let files = listable.len() - folders;
    let mut text = format!(
        "Folder {}: {} {}, {} {}\n",
        collapse_ws(folder_name),
        folders,
        plural(folders, "folder", "folders"),
        files,
        plural(files, "file", "files"),
    );
    let ordered = listable
        .iter()
        .filter(|e| e.is_folder())
        .chain(listable.iter().filter(|e| !e.is_folder()));
    for entry in ordered {
        let summary = entry
            .summary
            .as_deref()
            .map(collapse_ws)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| NOT_INDEXED.to_string());
        text.push_str(&entry.name);
        text.push('\t');
        text.push_str(entry.mimetype.essence());
        text.push('\t');
        text.push_str(&summary);
        text.push('\n');
    }
    if truncated {
        text.push_str(MORE_ENTRIES);
        text.push('\n');
    }
    assert!(
        parse(&text).len() == listable.len(),
        "a composed listing parses back whole"
    );
    text
}

/// The entries a listing carries, in listing order. The header and any line
/// without the three tab-separated fields are skipped, so text that is not
/// a listing parses to nothing rather than to garbage entries. Bounded by
/// the text's line count.
pub fn parse(text: &str) -> Vec<ListingEntry> {
    text.lines().skip(1).filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<ListingEntry> {
    let mut fields = line.splitn(3, '\t');
    let name = fields.next()?;
    let mimetype = Mimetype::parse(fields.next()?).ok()?;
    let summary = fields.next()?;
    let entry = ListingEntry {
        name: name.to_string(),
        mimetype,
        summary: if summary == NOT_INDEXED {
            None
        } else {
            Some(summary.to_string())
        },
    };
    if entry.is_listable() {
        Some(entry)
    } else {
        None
    }
}

fn plural(count: usize, one: &'static str, many: &'static str) -> &'static str {
    if count == 1 { one } else { many }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, mimetype: &str, summary: Option<&str>) -> ListingEntry {
        ListingEntry {
            name: name.into(),
            mimetype: Mimetype::parse(mimetype).expect("valid"),
            summary: summary.map(str::to_string),
        }
    }

    #[test]
    fn composes_folders_first_with_a_counting_header() {
        let text = compose(
            "Notes",
            &[
                entry("reno.md", "text/markdown", Some("Budget notes.")),
                entry("photos", "inode/directory", Some("Trip photos.")),
                entry("IMG_1.jpg", "image/jpeg", None),
            ],
            false,
        );
        assert_eq!(
            text,
            "Folder Notes: 1 folder, 2 files\n\
             photos\tinode/directory\tTrip photos.\n\
             reno.md\ttext/markdown\tBudget notes.\n\
             IMG_1.jpg\timage/jpeg\t(not indexed)\n"
        );
    }

    #[test]
    fn parse_recovers_what_compose_wrote() {
        let entries = vec![
            entry("a.md", "text/markdown", Some("multi\nline  summary")),
            entry("sub", "inode/directory", None),
        ];
        let parsed = parse(&compose("x", &entries, true));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "sub");
        assert_eq!(parsed[0].summary, None);
        assert_eq!(parsed[1].summary.as_deref(), Some("multi line summary"));
    }

    #[test]
    fn unlistable_names_are_skipped_and_plain_text_parses_to_nothing() {
        let text = compose("x", &[entry("bad\tname", "text/plain", None)], false);
        assert_eq!(text, "Folder x: 0 folders, 0 files\n");
        assert!(parse("# A document\n\nwith a\ttab\tinside\n").is_empty());
        assert!(parse("").is_empty());
    }

    #[test]
    fn the_truncation_marker_is_not_an_entry() {
        let text = compose("x", &[entry("a", "text/plain", None)], true);
        assert!(text.ends_with("(more entries not listed)\n"));
        assert_eq!(parse(&text).len(), 1);
    }

    #[test]
    fn fragment_text_names_the_child_and_its_type_only() {
        let e = entry("reno.md", "text/markdown", Some("Budget notes."));
        assert_eq!(e.fragment_text(), "reno.md (text/markdown)");
    }
}
