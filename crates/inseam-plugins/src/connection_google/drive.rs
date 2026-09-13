//! Google Drive as a host: every file the account can see is a source whose
//! locator is the Drive file id. Regular files are read as their own bytes;
//! Google-native documents (Docs, Sheets, Slides) have no byte form of
//! their own, so they are exported as text — and their envelope says so,
//! since what the index reads is what the connection serves. Folders,
//! forms, shortcuts, and the other native kinds with no text export are not
//! sources.

use std::sync::Arc;
use std::time::SystemTime;

use inseam_kernel::address::{Address, ContentLength, Envelope, HostId, Locator, Timestamp};
use inseam_kernel::fragment::Mimetype;
use inseam_seams::SeamError;
use inseam_seams::connection::{Connection, EnumeratedSource};
use inseam_seams::text::slice_lines;

use super::api::{GoogleApi, opt_str_field, str_field, timestamp_field};

/// Folders one scoped enumeration will descend into; a personal tree is
/// dozens, a shared drive hundreds, never unbounded.
pub const FOLDERS_MAX: u32 = 512;

/// Files per page — Drive's maximum.
const PAGE_SIZE: &str = "1000";

const FILE_FIELDS: &str = "nextPageToken,files(id,name,mimeType,size,createdTime,modifiedTime)";

const FOLDER_MIMETYPE: &str = "application/vnd.google-apps.folder";

/// The scope that means "every file", spelled either way.
fn is_everything(root: &str) -> bool {
    let root = root.trim();
    root.is_empty() || root == "all"
}

/// How a Google-native document is exported, when it can be: the export
/// mimetype is also what the envelope declares, because that is what
/// `read_bytes` returns.
pub fn native_export(mimetype: &str) -> Option<&'static str> {
    match mimetype {
        "application/vnd.google-apps.document" => Some("text/plain"),
        "application/vnd.google-apps.spreadsheet" => Some("text/csv"),
        "application/vnd.google-apps.presentation" => Some("text/plain"),
        _ => None,
    }
}

/// Whether a listed file is a source at all: regular files yes, exportable
/// native documents yes, every other native kind (folders, forms, sites,
/// shortcuts, maps) no.
fn is_source(mimetype: &str) -> bool {
    if !mimetype.starts_with("application/vnd.google-apps.") {
        return true;
    }
    native_export(mimetype).is_some()
}

pub struct DriveConnection {
    api: Arc<GoogleApi>,
    host: HostId,
    sources_max: usize,
}

impl DriveConnection {
    pub fn new(api: Arc<GoogleApi>, host: HostId, sources_max: usize) -> Self {
        Self {
            api,
            host,
            sources_max,
        }
    }

    fn address(&self, id: &str) -> Result<Address, SeamError> {
        let locator = Locator::new(id).map_err(|e| {
            SeamError::failed(format!("Drive file id `{id}` is not addressable: {e}"))
        })?;
        Ok(Address::new(self.host.clone(), locator))
    }

    /// The file id an address names, checked as one: Drive ids are URL-safe
    /// tokens, so anything else is refused before it reaches a query.
    fn file_id<'a>(&self, address: &'a Address) -> Result<&'a str, SeamError> {
        if address.host != self.host {
            return Err(SeamError::failed(format!(
                "address {address} names host `{}`, but this connection stewards `{}`",
                address.host, self.host
            )));
        }
        validate_id(address.locator.as_str())?;
        Ok(address.locator.as_str())
    }

    fn list_url(&self, q: &str) -> url::Url {
        self.api.api_url(
            &["drive", "v3", "files"],
            &[
                ("q", q),
                ("fields", FILE_FIELDS),
                ("pageSize", PAGE_SIZE),
                ("supportsAllDrives", "true"),
                ("includeItemsFromAllDrives", "true"),
            ],
        )
    }

    fn source_of(&self, file: &serde_json::Value, observed: Timestamp) -> Option<EnumeratedSource> {
        let id = opt_str_field(file, "id")?;
        let mimetype = str_field(file, "mimeType");
        if !is_source(mimetype) {
            return None;
        }
        let address = self.address(&id).ok()?;
        let size: u64 = str_field(file, "size").parse().unwrap_or(0);
        let content_type = Mimetype::parse(native_export(mimetype).unwrap_or(mimetype))
            .unwrap_or_else(|_| {
                Mimetype::parse("application/octet-stream").expect("literal mimetype is valid")
            });
        Some(EnumeratedSource {
            address,
            envelope: Envelope {
                source_type: "file".to_string(),
                content_type,
                // Native documents have no byte size until exported; the
                // sweep reads them by modified time.
                length: ContentLength::Bytes(size),
                created: timestamp_field(file, "createdTime"),
                modified: timestamp_field(file, "modifiedTime"),
                observed,
                properties: Vec::new(),
                facets: Vec::new(),
                hint: opt_str_field(file, "name"),
                content_digest: None,
            },
            raw_bytes: size,
        })
    }

    /// Every file under a folder and its subfolders, breadth first, bounded
    /// by folders visited and sources collected.
    async fn enumerate_folder(
        &self,
        folder: &str,
        observed: Timestamp,
    ) -> Result<Vec<EnumeratedSource>, SeamError> {
        validate_id(folder)?;
        let mut sources: Vec<EnumeratedSource> = Vec::new();
        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        queue.push_back(folder.to_string());
        let mut visited: u32 = 0;
        while let Some(folder) = queue.pop_front() {
            if visited >= FOLDERS_MAX {
                tracing::warn!(
                    folder,
                    "Drive scope has more than {FOLDERS_MAX} folders; the rest are not enumerated"
                );
                break;
            }
            visited += 1;
            let q = format!("'{folder}' in parents and trashed = false");
            let files = self
                .api
                .list_pages(self.list_url(&q), "files", self.sources_max)
                .await?;
            for file in &files {
                if str_field(file, "mimeType") == FOLDER_MIMETYPE {
                    if let Some(id) = opt_str_field(file, "id") {
                        queue.push_back(id);
                    }
                } else if sources.len() < self.sources_max {
                    sources.extend(self.source_of(file, observed));
                }
            }
            if sources.len() >= self.sources_max {
                break;
            }
        }
        Ok(sources)
    }
}

/// Drive ids are URL-safe tokens; anything else would need escaping inside
/// a query string and is refused instead.
fn validate_id(id: &str) -> Result<(), SeamError> {
    let valid = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(SeamError::Refused(format!(
            "`{id}` is not a Drive file or folder id"
        )))
    }
}

#[async_trait::async_trait]
impl Connection for DriveConnection {
    async fn enumerate(&self, root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
        let observed = Timestamp::from(SystemTime::now());
        if is_everything(root) {
            let files = self
                .api
                .list_pages(self.list_url("trashed = false"), "files", self.sources_max)
                .await?;
            return Ok(files
                .iter()
                .filter_map(|file| self.source_of(file, observed))
                .collect());
        }
        self.enumerate_folder(root.trim(), observed).await
    }

    fn locator_prefix(&self, root: &str) -> Option<String> {
        // File ids are flat: a folder scope has no locator prefix, so only
        // the whole-host scope reconciles deletions.
        is_everything(root).then(String::new)
    }

    async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
        let bytes = self.read_bytes(address).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn read_lines(
        &self,
        address: &Address,
        start: u64,
        end: u64,
    ) -> Result<String, SeamError> {
        let text = self.read_text(address).await?;
        slice_lines(&text, start, end)
    }

    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        let id = self.file_id(address)?;
        let metadata = self
            .api
            .get_json(self.api.api_url(
                &["drive", "v3", "files", id],
                &[("fields", "mimeType"), ("supportsAllDrives", "true")],
            ))
            .await?;
        let mimetype = str_field(&metadata, "mimeType");
        let url = match native_export(mimetype) {
            Some(export) => self.api.api_url(
                &["drive", "v3", "files", id, "export"],
                &[("mimeType", export)],
            ),
            None if mimetype.starts_with("application/vnd.google-apps.") => {
                return Err(SeamError::Refused(format!(
                    "{address} is a {mimetype}, which Drive cannot export as text"
                )));
            }
            None => self.api.api_url(
                &["drive", "v3", "files", id],
                &[("alt", "media"), ("supportsAllDrives", "true")],
            ),
        };
        self.api.get_bytes(url).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use axum::extract::{Path, Query};
    use axum::routing::get;

    use super::super::api::tests::fake_google;

    fn files_router() -> axum::Router {
        axum::Router::new()
            .route(
                "/drive/v3/files",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    let query = q.get("q").cloned().unwrap_or_default();
                    let files = if query == "trashed = false" {
                        serde_json::json!([
                            {"id": "doc1", "name": "Plan.gdoc", "mimeType": "application/vnd.google-apps.document", "modifiedTime": "2025-01-02T03:04:05Z"},
                            {"id": "bin1", "name": "notes.md", "mimeType": "text/markdown", "size": "12", "createdTime": "2025-01-01T00:00:00Z"},
                            {"id": "fold", "name": "Folder", "mimeType": "application/vnd.google-apps.folder"},
                            {"id": "form", "name": "Survey", "mimeType": "application/vnd.google-apps.form"}
                        ])
                    } else if query == "'fold' in parents and trashed = false" {
                        serde_json::json!([
                            {"id": "sub", "name": "Sub", "mimeType": "application/vnd.google-apps.folder"},
                            {"id": "inner", "name": "inner.txt", "mimeType": "text/plain", "size": "3"}
                        ])
                    } else if query == "'sub' in parents and trashed = false" {
                        serde_json::json!([
                            {"id": "deep", "name": "deep.txt", "mimeType": "text/plain", "size": "4"}
                        ])
                    } else {
                        serde_json::json!([])
                    };
                    axum::Json(serde_json::json!({"files": files}))
                }),
            )
            .route(
                "/drive/v3/files/{id}",
                get(|Path(id): Path<String>, Query(q): Query<HashMap<String, String>>| async move {
                    if q.get("alt").map(String::as_str) == Some("media") {
                        return format!("bytes of {id}").into_response();
                    }
                    let mimetype = if id == "doc1" { "application/vnd.google-apps.document" } else { "text/plain" };
                    axum::Json(serde_json::json!({"mimeType": mimetype})).into_response()
                }),
            )
            .route(
                "/drive/v3/files/{id}/export",
                get(|Path(id): Path<String>, Query(q): Query<HashMap<String, String>>| async move {
                    format!("export of {id} as {}", q.get("mimeType").cloned().unwrap_or_default())
                }),
            )
    }

    use axum::response::IntoResponse;

    fn host() -> HostId {
        HostId::new("google-drive-test").expect("valid")
    }

    #[tokio::test]
    async fn enumerates_every_file_with_native_documents_as_their_export_type() {
        let (api, _server) = fake_google(files_router()).await;
        let drive = DriveConnection::new(Arc::new(api), host(), 100);
        let sources = drive.enumerate("").await.expect("enumerates");
        let ids: Vec<&str> = sources.iter().map(|s| s.address.locator.as_str()).collect();
        assert_eq!(
            ids,
            vec!["doc1", "bin1"],
            "folders and forms are not sources"
        );
        assert_eq!(sources[0].envelope.content_type.essence(), "text/plain");
        assert_eq!(sources[0].envelope.modified, Some(Timestamp(1_735_787_045)));
        assert_eq!(sources[0].envelope.hint.as_deref(), Some("Plan.gdoc"));
        assert_eq!(sources[1].envelope.content_type.essence(), "text/markdown");
        assert_eq!(sources[1].raw_bytes, 12);
        assert_eq!(drive.locator_prefix(""), Some(String::new()));
        assert_eq!(drive.locator_prefix("fold"), None);
    }

    #[tokio::test]
    async fn a_folder_scope_descends_into_subfolders() {
        let (api, _server) = fake_google(files_router()).await;
        let drive = DriveConnection::new(Arc::new(api), host(), 100);
        let sources = drive.enumerate("fold").await.expect("enumerates");
        let ids: Vec<&str> = sources.iter().map(|s| s.address.locator.as_str()).collect();
        assert_eq!(ids, vec!["inner", "deep"]);
        let capped = DriveConnection::new(Arc::new(fake_google(files_router()).await.0), host(), 1);
        assert_eq!(capped.enumerate("fold").await.expect("enumerates").len(), 1);
        assert!(matches!(
            drive.enumerate("bad id").await,
            Err(SeamError::Refused(_))
        ));
    }

    #[tokio::test]
    async fn reads_regular_files_as_media_and_native_documents_as_exports() {
        let (api, _server) = fake_google(files_router()).await;
        let drive = DriveConnection::new(Arc::new(api), host(), 100);
        let regular: Address = "inseam://google-drive-test/bin1".parse().expect("address");
        assert_eq!(
            drive.read_text(&regular).await.expect("reads"),
            "bytes of bin1"
        );
        let native: Address = "inseam://google-drive-test/doc1".parse().expect("address");
        assert_eq!(
            drive.read_text(&native).await.expect("reads"),
            "export of doc1 as text/plain"
        );
        assert_eq!(
            drive.read_lines(&native, 1, 1).await.expect("reads"),
            "export of doc1 as text/plain"
        );
        let foreign: Address = "inseam://elsewhere/bin1".parse().expect("address");
        assert!(drive.read_text(&foreign).await.is_err());
    }

    #[test]
    fn native_kinds_are_classified() {
        assert_eq!(
            native_export("application/vnd.google-apps.spreadsheet"),
            Some("text/csv")
        );
        assert!(is_source("image/png"));
        assert!(!is_source(FOLDER_MIMETYPE));
        assert!(!is_source("application/vnd.google-apps.shortcut"));
    }
}
