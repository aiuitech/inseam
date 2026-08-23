//! The LLM capability as the sweep grants it to transforms: one run meter
//! per transform entry, shared by every concurrent application of that
//! transform, and a narrowed handle that charges the meter exactly — the
//! reservation is a compare-and-swap, so `concurrency` applications racing
//! for the last call cannot overspend the budget between them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use inseam_kernel::substrate::{EventBus, Verdict};
use inseam_seams::llm::{ChatMessage, ChatRequest, Llm, LlmCall, LlmLane, VisionRequest};
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
/// the transform model per lane and its reasoning control, the run's lane
/// override, the guard bus, and the run's meters.
pub(super) struct Grantor {
    pub(super) llm: Option<Arc<dyn Llm>>,
    /// The interactive lane's model.
    pub(super) model: String,
    /// The batch lane's model — the endpoint's `transform_batch_model` fact
    /// — or `None` when the endpoint has no batch API, in which case the
    /// batch lane rides the interactive model.
    pub(super) batch_model: Option<String>,
    /// A request's lane for every transform this run, over each
    /// registration's own (`SweepRequest::llm_lane`).
    pub(super) lane_override: Option<LlmLane>,
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
            model: self.model_for(self.lane_of(registration)).to_string(),
        }))
    }

    /// The lane a registration's calls ride this run: the request's override
    /// when it names one, the registration's own lane otherwise.
    fn lane_of(&self, registration: &Registration) -> LlmLane {
        self.lane_override.unwrap_or(registration.llm_lane)
    }

    /// The model a lane's calls name. The batch lane falls back to the
    /// interactive model when the endpoint declared no batch model.
    fn model_for(&self, lane: LlmLane) -> &str {
        match lane {
            LlmLane::Interactive => &self.model,
            LlmLane::Batch => self.batch_model.as_deref().unwrap_or(&self.model),
        }
    }

    /// Whether any granted transform's calls will ride the batch lane this
    /// run — so the sweep can park enough planners for a batch job to fill.
    /// A registration with no budget never calls, so it does not count.
    pub(super) fn plans_on_batch_lane(&self, registrations: &[Arc<Registration>]) -> bool {
        let Some(batch_model) = self.batch_model.as_deref() else {
            return false;
        };
        if self.llm.is_none() {
            return false;
        }
        registrations
            .iter()
            .filter(|r| r.llm_call_budget > 0)
            .any(|r| self.model_for(self.lane_of(r)) == batch_model)
    }
}

/// The narrowed LLM capability granted to one transform for one run:
/// mechanical call counting against the per-run budget, plus the seam-level
/// [`LlmCall`] guard — a denial from any policy listener refuses the call.
struct MeteredLlm {
    llm: Arc<dyn Llm>,
    grantor: Arc<Grantor>,
    consumer: String,
    /// The lane's model, resolved at grant.
    model: String,
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
            self.model.clone(),
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
                model: &self.model,
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

    fn grantor(batch_model: Option<&str>, lane_override: Option<LlmLane>) -> Grantor {
        Grantor {
            llm: None,
            model: "google/gemini-2.5-flash-lite".to_string(),
            batch_model: batch_model.map(str::to_string),
            lane_override,
            reasoning_effort: None,
            bus: EventBus::new(),
            meters: RunMeters::for_registrations(&[]),
        }
    }

    #[test]
    fn the_batch_lane_names_the_batch_model_and_falls_back_without_one() {
        let with = grantor(Some("google/gemini-2.5-flash-lite:batch"), None);
        assert_eq!(with.model_for(LlmLane::Interactive), "google/gemini-2.5-flash-lite");
        assert_eq!(with.model_for(LlmLane::Batch), "google/gemini-2.5-flash-lite:batch");
        let without = grantor(None, None);
        assert_eq!(without.model_for(LlmLane::Batch), "google/gemini-2.5-flash-lite");
    }

    #[test]
    fn the_run_override_wins_over_the_registration_lane() {
        let registration = inseam_seams::transforms::Registration {
            entry_id: "summarizer".to_string(),
            name: "summarizer".to_string(),
            transform: Arc::new(NeverClaims),
            llm_call_budget: 1,
            llm_lane: LlmLane::Interactive,
            shape_fingerprint: "x".to_string(),
        };
        assert_eq!(grantor(None, None).lane_of(&registration), LlmLane::Interactive);
        assert_eq!(
            grantor(None, Some(LlmLane::Batch)).lane_of(&registration),
            LlmLane::Batch
        );
    }

    struct NeverClaims;

    #[async_trait::async_trait]
    impl inseam_seams::transforms::Transform for NeverClaims {
        fn kind(&self) -> inseam_seams::transforms::TransformKind {
            inseam_seams::transforms::TransformKind::Enrichment
        }
        fn claims(&self, _: &inseam_kernel::fragment::Mimetype, _: bool) -> bool {
            false
        }
        async fn apply(
            &self,
            _: inseam_seams::transforms::TransformCtx<'_>,
        ) -> inseam_seams::transforms::TransformOutput {
            inseam_seams::transforms::TransformOutput::default()
        }
    }

    #[test]
    fn zero_budget_never_grants() {
        let meter = RunMeter::new(0);
        assert!(!meter.has_budget());
        assert!(meter.charge().is_err());
        assert_eq!(meter.calls(), 0);
    }
}
