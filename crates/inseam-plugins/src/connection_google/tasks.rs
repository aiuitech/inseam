//! Google Tasks as a host: every task is a source whose locator is
//! `<task list id>/<task id>`. The connection serves a task as text — title,
//! status, dates, then the notes — so the envelope says `text/plain`. The
//! scope is a task-list id, or empty for every list the account has;
//! completed and hidden tasks are sources too, because what was done is as
//! findable as what is pending.

use std::sync::Arc;
use std::time::SystemTime;

use inseam_kernel::address::{Address, HostId, Locator, Timestamp};
use inseam_seams::connection::{Connection, EnumeratedSource};
use inseam_seams::text::slice_lines;
use inseam_seams::SeamError;

use super::api::{opt_str_field, rendered, split_locator, str_field, timestamp_field, GoogleApi};

/// Tasks per page — the Tasks API's maximum.
const PAGE_SIZE: &str = "100";

pub struct TasksConnection {
    api: Arc<GoogleApi>,
    host: HostId,
    sources_max: usize,
}

impl TasksConnection {
    pub fn new(api: Arc<GoogleApi>, host: HostId, sources_max: usize) -> Self {
        Self {
            api,
            host,
            sources_max,
        }
    }

    /// The task lists a scope names: the one given, or every list.
    async fn lists_for(&self, root: &str) -> Result<Vec<String>, SeamError> {
        let root = root.trim();
        if !root.is_empty() {
            return Ok(vec![root.to_string()]);
        }
        let listed = self
            .api
            .list_pages(
                self.api
                    .api_url(&["tasks", "v1", "users", "@me", "lists"], &[("maxResults", PAGE_SIZE)]),
                "items",
                self.sources_max,
            )
            .await?;
        Ok(listed.iter().filter_map(|l| opt_str_field(l, "id")).collect())
    }

    fn source_of(&self, list: &str, task: &serde_json::Value, observed: Timestamp) -> Option<EnumeratedSource> {
        let id = opt_str_field(task, "id")?;
        let locator = Locator::new(format!("{list}/{id}")).ok()?;
        let text = render_task(task);
        let r = rendered(
            "task",
            &text,
            None,
            timestamp_field(task, "updated"),
            observed,
            opt_str_field(task, "title"),
        );
        Some(EnumeratedSource {
            address: Address::new(self.host.clone(), locator),
            envelope: r.envelope,
            raw_bytes: r.raw_bytes,
        })
    }

    fn task_of<'a>(&self, address: &'a Address) -> Result<(&'a str, &'a str), SeamError> {
        if address.host != self.host {
            return Err(SeamError::failed(format!(
                "address {address} names host `{}`, but this connection stewards `{}`",
                address.host, self.host
            )));
        }
        split_locator(address.locator.as_str()).ok_or_else(|| {
            SeamError::Refused(format!(
                "`{}` is not a `<list>/<task>` locator",
                address.locator.as_str()
            ))
        })
    }
}

/// The task as the index reads it.
pub fn render_task(task: &serde_json::Value) -> String {
    let mut out = String::new();
    out.push_str("Task: ");
    out.push_str(str_field(task, "title"));
    out.push('\n');
    let status = match str_field(task, "status") {
        "completed" => "completed",
        _ => "pending",
    };
    out.push_str(&format!("Status: {status}\n"));
    for (label, key) in [("Due", "due"), ("Completed", "completed")] {
        if let Some(value) = opt_str_field(task, key) {
            out.push_str(&format!("{label}: {value}\n"));
        }
    }
    if let Some(notes) = opt_str_field(task, "notes") {
        out.push('\n');
        out.push_str(notes.trim_end());
        out.push('\n');
    }
    out
}

#[async_trait::async_trait]
impl Connection for TasksConnection {
    async fn enumerate(&self, root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
        let observed = Timestamp::from(SystemTime::now());
        let mut sources: Vec<EnumeratedSource> = Vec::new();
        for list in self.lists_for(root).await? {
            let remaining = self.sources_max.saturating_sub(sources.len());
            if remaining == 0 {
                break;
            }
            let tasks = self
                .api
                .list_pages(
                    self.api.api_url(
                        &["tasks", "v1", "lists", &list, "tasks"],
                        &[
                            ("showCompleted", "true"),
                            ("showHidden", "true"),
                            ("maxResults", PAGE_SIZE),
                        ],
                    ),
                    "items",
                    remaining,
                )
                .await?;
            sources.extend(tasks.iter().filter_map(|t| self.source_of(&list, t, observed)));
        }
        Ok(sources)
    }

    fn locator_prefix(&self, root: &str) -> Option<String> {
        Some(root.trim().to_string())
    }

    async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
        let (list, task) = self.task_of(address)?;
        let task = self
            .api
            .get_json(self.api.api_url(&["tasks", "v1", "lists", list, "tasks", task], &[]))
            .await?;
        Ok(render_task(&task))
    }

    async fn read_lines(&self, address: &Address, start: u64, end: u64) -> Result<String, SeamError> {
        let text = self.read_text(address).await?;
        slice_lines(&text, start, end).map_err(SeamError::failed)
    }

    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        Ok(self.read_text(address).await?.into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::extract::Path;
    use axum::routing::get;

    use super::super::api::tests::fake_google;

    fn task(id: &str, title: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "title": title, "status": "needsAction", "due": "2025-02-01T00:00:00.000Z",
            "updated": "2025-01-02T03:04:05.000Z", "notes": "call the bank"
        })
    }

    fn router() -> axum::Router {
        axum::Router::new()
            .route(
                "/tasks/v1/users/@me/lists",
                get(|| async { axum::Json(serde_json::json!({"items": [{"id": "l1"}, {"id": "l2"}]})) }),
            )
            .route(
                "/tasks/v1/lists/{list}/tasks",
                get(|Path(list): Path<String>| async move {
                    let items = if list == "l1" {
                        serde_json::json!([task("t1", "Renew passport"), task("t2", "Taxes")])
                    } else {
                        serde_json::json!([task("t3", "Groceries")])
                    };
                    axum::Json(serde_json::json!({"items": items}))
                }),
            )
            .route(
                "/tasks/v1/lists/{list}/tasks/{task}",
                get(|Path((_, id)): Path<(String, String)>| async move { axum::Json(task(&id, "Renew passport")) }),
            )
    }

    fn host() -> HostId {
        HostId::new("google-tasks-test").expect("valid")
    }

    #[tokio::test]
    async fn enumerates_every_list_or_one() {
        let (api, _server) = fake_google(router()).await;
        let tasks = TasksConnection::new(Arc::new(api), host(), 100);
        let all = tasks.enumerate("").await.expect("enumerates");
        let locators: Vec<&str> = all.iter().map(|s| s.address.locator.as_str()).collect();
        assert_eq!(locators, vec!["l1/t1", "l1/t2", "l2/t3"]);
        assert_eq!(all[0].envelope.hint.as_deref(), Some("Renew passport"));
        assert_eq!(all[0].envelope.modified, Some(Timestamp(1_735_787_045)));
        assert_eq!(tasks.enumerate("l2").await.expect("enumerates").len(), 1);
        let capped = TasksConnection::new(Arc::new(fake_google(router()).await.0), host(), 2);
        assert_eq!(capped.enumerate("").await.expect("enumerates").len(), 2);
    }

    #[tokio::test]
    async fn reads_a_task_as_text() {
        let (api, _server) = fake_google(router()).await;
        let tasks = TasksConnection::new(Arc::new(api), host(), 100);
        let address: Address = "inseam://google-tasks-test/l1/t1".parse().expect("address");
        assert_eq!(
            tasks.read_text(&address).await.expect("reads"),
            "Task: Renew passport\nStatus: pending\nDue: 2025-02-01T00:00:00.000Z\n\ncall the bank\n"
        );
        assert_eq!(tasks.read_lines(&address, 2, 2).await.expect("reads"), "Status: pending");
    }
}
