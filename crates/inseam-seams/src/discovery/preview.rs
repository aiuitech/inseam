use std::collections::BTreeSet;

use inseam_kernel::address::ContentLength;
use serde_json::{Value, json};

use super::{ReadRequest, StructureRequest, encoded_chars, error_value};
use crate::operations::{ExpandResponse, QueryResult, ScanResponse};
use crate::text::truncate_chars;

const INDEX_TEXT_CHARS_MAX: usize = 65_536;
const EXCERPT_CHARS: usize = 320;
const QUERY_TERMS_MAX: usize = 24;

pub(super) fn candidate(row: &QueryResult, query: &str) -> Value {
    let summary = row.summary.as_deref().unwrap_or_default();
    let (lines, bytes) = match row.envelope.length {
        ContentLength::Lines(count) => (Some(count), None),
        ContentLength::Bytes(count) => (None, Some(count)),
    };
    let hints: Vec<_> = row
        .hints
        .iter()
        .take(3)
        .map(|hint| {
            json!({
                "text": truncate_chars(&hint.text, 280), "extent": hint.extent,
                "origin": "indexed_fragment", "mimetype": hint.mimetype,
            })
        })
        .collect();
    json!({"address": row.address, "title": row.envelope.title.as_deref().map(|s| truncate_chars(s, 200)),
        "source_type": truncate_chars(&row.envelope.source_type, 100),
        "content_type": truncate_chars(&row.envelope.content_type, 100),
        "lines_total": lines, "bytes_total": bytes,
        "indexed_summary_chars": summary.chars().count(),
        "size_note": "Indexed summary length is not necessarily full document length.",
        "version": row.envelope.content_digest, "modified": row.envelope.modified,
        "replicas":row.replicas.iter().take(4).collect::<Vec<_>>(),
        "replicas_omitted":row.replicas.len().saturating_sub(4),
        "summary": truncate_chars(summary, 240), "excerpts": excerpts(summary, query),
        "hints": hints, "score_for_this_query": row.score})
}

fn excerpts(text: &str, query: &str) -> Vec<Value> {
    let chars: Vec<_> = text.chars().take(INDEX_TEXT_CHARS_MAX).collect();
    let terms: BTreeSet<_> = crate::extract::strip_stopwords(query)
        .split_whitespace()
        .take(QUERY_TERMS_MAX)
        .map(str::to_lowercase)
        .collect();
    let mut windows: Vec<_> = (0..chars.len())
        .step_by(EXCERPT_CHARS / 2)
        .map(|start| {
            let end = (start + EXCERPT_CHARS).min(chars.len());
            let text: String = chars[start..end].iter().collect();
            let lower = text.to_lowercase();
            let matches = terms
                .iter()
                .filter(|term| lower.contains(term.as_str()))
                .count();
            (matches, start, text)
        })
        .collect();
    windows.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    let mut selected = Vec::new();
    let mut positions = Vec::new();
    for (_, start, text) in windows {
        if selected.len() == 2 {
            break;
        }
        if positions
            .iter()
            .any(|position: &usize| position.abs_diff(start) < EXCERPT_CHARS)
        {
            continue;
        }
        positions.push(start);
        selected.push(json!({"text": text, "origin": "indexed_summary",
            "offset_chars": start, "source_lines": null}));
    }
    selected
}

pub(super) fn scan(request: ReadRequest, response: ScanResponse, quota: usize) -> Value {
    let digest = blake3::hash(response.text.as_bytes()).to_hex().to_string();
    if let Some(expected) = &request.window_digest {
        if *expected != digest {
            return error_value("source window changed; restart the scan at offset_chars 0".into());
        }
    }
    let offset = usize::try_from(request.offset_chars).expect("u32 fits supported platforms");
    let available = response.text.chars().count();
    if offset > available {
        return error_value(
            "scan continuation exceeds current text; restart at offset_chars 0".into(),
        );
    }
    let chars: Vec<_> = response.text.chars().skip(offset).take(quota).collect();
    let mut value = json!({"operation": "scan", "address": response.address,
        "start": response.start, "end": response.end, "lines_total": response.lines_total,
        "offset_chars": offset, "delivered_chars": 0, "text": "",
        "window_digest":digest,
        "served_from_fragment": response.served_from_fragment, "next": null});
    let mut lower = 0;
    let mut upper = chars.len();
    // Sixteen bisections cover the 24,000-character response bound.
    for _ in 0..16 {
        if lower == upper {
            break;
        }
        let middle = lower + (upper - lower).div_ceil(2);
        scan_fill(
            &mut value, &chars, middle, offset, available, &request, &response,
        );
        if encoded_chars(&value) <= quota.saturating_sub(64) {
            lower = middle;
        } else {
            upper = middle - 1;
        }
    }
    scan_fill(
        &mut value, &chars, lower, offset, available, &request, &response,
    );
    if encoded_chars(&value) > quota.saturating_sub(64) {
        return error_value("scan metadata exceeds budget; request this scan alone".into());
    }
    value
}

fn scan_fill(
    value: &mut Value,
    chars: &[char],
    count: usize,
    offset: usize,
    available: usize,
    request: &ReadRequest,
    response: &ScanResponse,
) {
    value["text"] = json!(chars[..count].iter().collect::<String>());
    value["delivered_chars"] = json!(count);
    let end = response
        .lines_total
        .map_or(request.end, |total| total.min(request.end));
    value["next"] = if offset + count < available {
        json!({"source":request.source, "start":response.start,
            "end":response.end, "offset_chars":offset + count,
            "window_digest":value["window_digest"]})
    } else if response.end < end {
        json!({"source":request.source, "start":response.end + 1,
            "end":end, "offset_chars":0})
    } else {
        Value::Null
    };
}

pub(super) fn structure(
    request: StructureRequest,
    response: ExpandResponse,
    quota: usize,
) -> Value {
    let count = response.fragments.len() + response.neighbors.len() + response.relations.len();
    if count > 10_000 {
        return error_value(
            "expansion exceeds 10,000 entries; refine with a narrower search".into(),
        );
    }
    let offset = usize::try_from(request.offset).expect("u32 fits supported platforms");
    if offset > count {
        return error_value("expansion offset exceeds current structure".into());
    }
    let fragments = response
        .fragments
        .into_iter()
        .map(|fragment| ("fragment", json!(fragment)));
    let neighbors = response
        .neighbors
        .into_iter()
        .map(|fragment| ("neighbor", json!(fragment)));
    let relations = response
        .relations
        .into_iter()
        .map(|relation| ("relation", json!(relation)));
    let mut value = json!({"operation":"expand", "address":response.address,
        "items":[], "total":count, "next":null});
    for (index, (kind, mut item)) in fragments
        .chain(neighbors)
        .chain(relations)
        .enumerate()
        .skip(offset)
    {
        if let Some(text) = item["text"].as_str() {
            item["text"] = json!(truncate_chars(text, 500));
            item["text_is_preview"] = json!(true);
        }
        value["items"]
            .as_array_mut()
            .expect("created as array")
            .push(json!({"kind":kind,"value":item}));
        if encoded_chars(&value) > quota.saturating_sub(256) {
            value["items"]
                .as_array_mut()
                .expect("created as array")
                .pop();
            value["next"] = json!({"source":request.source,"offset":index});
            break;
        }
    }
    if encoded_chars(&value) > quota {
        return error_value("expansion metadata exceeds budget; expand this source alone".into());
    }
    value
}
