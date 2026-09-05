//! OCR enrichment plugin: transcribes visible text from images — image
//! sources at the root, and images a document links to (fragments carrying
//! a content reference, whose bytes the host hands over the same way) —
//! via one metered vision call, emitting a single `text/plain;via=ocr`
//! fragment that `transcribes` the image. Degrades to empty output whenever
//! a capability is withheld or the image has no text.

wit_bindgen::generate!({
    path: "../../crates/inseam-wasm-host/wit",
    world: "transform-plugin",
});

use exports::inseam::plugin::transform::{ClaimSpec, Envelope, Fragment, Guest, Output};
use inseam::plugin::host;

/// Upper bound on the emitted transcript, in characters.
const MAX_TRANSCRIPT_CHARS: usize = 20_000;

const PROMPT: &str = "Transcribe ALL visible text in this image verbatim. \
Return only the transcribed text, with no preamble and no commentary. \
If the image contains no text, return an empty string.";

struct Plugin;

impl Guest for Plugin {
    fn claims() -> ClaimSpec {
        ClaimSpec {
            mimetypes: vec![
                "image/png".into(),
                "image/jpeg".into(),
                "image/webp".into(),
            ],
            roots_only: false,
        }
    }

    fn apply(
        _env: Envelope,
        mimetype: String,
        _is_root: bool,
        _text: Option<String>,
    ) -> Result<Output, String> {
        let empty = Output { fragments: vec![] };

        // Degrade, never error: a withheld capability or spent budget means
        // this source simply goes unenriched.
        let Ok(bytes) = host::source_bytes() else {
            return Ok(empty);
        };
        let Ok(text) = host::llm_describe_image(PROMPT, &mimetype, &bytes) else {
            return Ok(empty);
        };

        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(empty);
        }

        let mut transcript = trimmed.to_owned();
        if let Some((cut, _)) = transcript.char_indices().nth(MAX_TRANSCRIPT_CHARS) {
            transcript.truncate(cut);
        }

        host::log(&format!(
            "ocr: transcribed {} chars",
            transcript.chars().count()
        ));

        Ok(Output {
            fragments: vec![Fragment {
                parent: None,
                mimetype: "text/plain;via=ocr".into(),
                relation: "transcribes".into(),
                text: Some(transcript),
            }],
        })
    }
}

export!(Plugin);
