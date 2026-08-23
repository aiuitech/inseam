//! The LLM capability as the sweep grants it to transforms: one run meter
//! per transform entry, shared by every concurrent application of that
//! transform, and a narrowed handle that charges the meter exactly — the
//! reservation is a compare-and-swap, so `concurrency` applications racing
//! for the last call cannot overspend the budget between them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use inseam_kernel::substrate::{EventBus, Verdict};
use inseam_seams::llm::{ChatMessage, ChatRequest, Llm, LlmCall, VisionRequest};
use inseam_seams::transforms::{GrantedLlm, Registration};
use inseam_seams::SeamError;

/// Per-transform, per-run LLM metering shared with the granted handles.
pub(super) struct RunMeter {
    calls: AtomicUsize,
    budget: usize,
}

impl RunMeter {
    fn new(budget: usize) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            budget,
        }
    }

    /// Calls charged so far this run.
    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    /// Whether a further call could still be charged.
    fn has_budget(&self) -> bool {
        self.calls() < self.budget
    }

    /// Reserve one call, atomically against the budget.
    fn charge(&self) -> Result<(), ()> {
        self.calls
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |calls| {
                if calls < self.budget {
                    Some(calls + 1)
                } else {
                    None
                }
            })
            .map(|_| ())
            .map_err(|_| ())
    }
}

/// Every registration's meter for one run, keyed by entry id. Built once
/// per sweep, before any planning starts, so concurrent planners share the
/// same counters.
pub(super) struct RunMeters {
    meters: HashMap<String, RunMeter>,
}

impl RunMeters {
    pub(super) fn for_registrations(registrations: &[Arc<Registration>]) -> Self {
        let meters = registrations
            .iter()
            .map(|r| (r.entry_id.clone(), RunMeter::new(r.llm_call_budget)))
            .collect();
        Self { meters }
    }

    /// Calls charged per entry this run, entries with none omitted.
    pub(super) fn calls_by_entry(&self) -> impl Iterator<Item = (&str, usize)> {
        self.meters
            .iter()
            .map(|(entry, meter)| (entry.as_str(), meter.calls()))
            .filter(|(_, calls)| *calls > 0)
    }
}

/// What a planner needs to grant the LLM: the node's provider (when mounted),
/// the transform model and its reasoning control, the guard bus, and the
/// run's meters.
pub(super) struct Grantor {
    pub(super) llm: Option<Arc<dyn Llm>>,
    pub(super) model: String,
    /// Sent with every transform-grade call when set (`design/indexing.md`:
    /// transforms want the cheapest direct answer, never a thinking trace).
    pub(super) reasoning_effort: Option<String>,
    pub(super) bus: EventBus,
    pub(super) meters: RunMeters,
}

impl Grantor {
    /// Capability mediation: the LLM handle is granted only while the
    /// transform's per-run budget lasts; withheld, the transform falls back
    /// or emits nothing. Every call also passes the seam-level `LlmCall`
    /// guard. The grant is advisory — the handle's charge is what is exact.
    pub(super) fn grant(self: &Arc<Self>, registration: &Registration) -> Option<Arc<dyn GrantedLlm>> {
        let llm = self.llm.as_ref()?;
        let meter = self.meters.meters.get(&registration.entry_id)?;
        if !meter.has_budget() {
            return None;
        }
        Some(Arc::new(MeteredLlm {
            llm: Arc::clone(llm),
            grantor: Arc::clone(self),
            consumer: registration.entry_id.clone(),
        }))
    }
}

/// The narrowed LLM capability granted to one transform for one run:
/// mechanical call counting against the per-run budget, plus the seam-level
/// [`LlmCall`] guard — a denial from any policy listener refuses the call.
struct MeteredLlm {
    llm: Arc<dyn Llm>,
    grantor: Arc<Grantor>,
    consumer: String,
}

impl MeteredLlm {
    fn charge(&self) -> Result<(), SeamError> {
        let meter = self
            .grantor
            .meters
            .meters
            .get(&self.consumer)
            .expect("a granted handle's entry has a meter");
        if meter.charge().is_err() {
            return Err(SeamError::Refused(format!(
                "llm budget for `{}` is spent this run",
                self.consumer
            )));
        }
        if let Verdict::Deny(reason) = self.grantor.bus.check(&LlmCall {
            consumer: self.consumer.clone(),
        }) {
            return Err(SeamError::Refused(reason));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl GrantedLlm for MeteredLlm {
    async fn complete(&self, system: &str, user: &str) -> Result<String, SeamError> {
        self.charge()?;
        let request = ChatRequest::new(
            self.grantor.model.clone(),
            vec![ChatMessage::system(system), ChatMessage::user(user)],
        )
        .with_reasoning_effort(self.grantor.reasoning_effort.as_deref());
        let reply = self.llm.chat(&request).await?;
        Ok(reply.content.unwrap_or_default())
    }

    async fn describe_image(
        &self,
        prompt: &str,
        mimetype: &str,
        image: &[u8],
    ) -> Result<String, SeamError> {
        self.charge()?;
        self.llm
            .describe_image(&VisionRequest {
                model: &self.grantor.model,
                prompt,
                mimetype,
                image,
                reasoning_effort: self.grantor.reasoning_effort.as_deref(),
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_charges_exactly_to_budget() {
        let meter = RunMeter::new(3);
        assert!(meter.has_budget());
        assert!(meter.charge().is_ok());
        assert!(meter.charge().is_ok());
        assert!(meter.charge().is_ok());
        assert!(!meter.has_budget());
        assert!(meter.charge().is_err());
        assert_eq!(meter.calls(), 3);
    }

    #[test]
    fn zero_budget_never_grants() {
        let meter = RunMeter::new(0);
        assert!(!meter.has_budget());
        assert!(meter.charge().is_err());
        assert_eq!(meter.calls(), 0);
    }
}
