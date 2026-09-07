//! The conformance harness for the `connection` seam: the same four phases
//! as a transform's (`check.rs`), against a long-running component that is
//! configured, then asked to enumerate, read, and describe, reaching its
//! host only through canned network replies. The network is data here
//! exactly as the LLM is for a transform: a check names the replies its
//! host would give, a starved check names none and pins what the plugin
//! does offline.

use std::path::Path;
use std::sync::Arc;

use wasmtime::Engine;
use wasmtime::component::{Component, Linker};

use inseam_conformance::{
    CannedFetch, ConnectionCall, ConnectionCheck, ConnectionChecksFile, ConnectionOutcome,
    EmittedEnvelope, EmittedSource,
};
use inseam_kernel::address::Locator;
use inseam_seams::SeamError;
use inseam_seams::connection::HostKind;
use inseam_seams::fetch::{FetchRequest, FetchResponse, HostPattern};

use crate::check::{CheckReport, Outcome, Phase};
use crate::connection::{Instance, host_of};
use crate::connection_world::exports::inseam::plugin::connection::{
    Envelope as WitEnvelope, HostDescription as WitHost, Source as WitSource,
};
use crate::{
    ArtifactManifest, GrantedFetch, Grants, GuardedFetch, Invocation, WasmEntryConfig,
    build_linker, new_engine,
};

// ---------------------------------------------------------------------------
// The canned network
// ---------------------------------------------------------------------------

/// A reply the harness gives for one URL.
struct Reply {
    url: String,
    status: u16,
    body: Vec<u8>,
    content_type: Option<String>,
    authorized: bool,
}

/// The network as canned replies, under the manifest's allow list: a
/// request to a host the manifest does not name is refused exactly as the
/// bridge would refuse it, and a URL with no reply is the offline node.
pub(crate) struct CannedNetwork {
    replies: Vec<Reply>,
    allowed: Vec<HostPattern>,
}

impl CannedNetwork {
    /// No replies: every fetch fails, as on a node without a network.
    pub(crate) fn refusing(allowed: Vec<HostPattern>) -> Self {
        Self {
            replies: Vec::new(),
            allowed,
        }
    }

    /// One reply for every URL, with garbage in it: what a misbehaving host
    /// answers.
    pub(crate) fn garbage(allowed: Vec<HostPattern>) -> Self {
        Self {
            replies: vec![Reply {
                url: String::new(),
                status: 500,
                body: vec![0x00, 0xFF, 0x13, 0x37, b'{', b'"'],
                content_type: Some("application/octet-stream".into()),
                authorized: false,
            }],
            allowed,
        }
    }

    /// A check's replies, bodies resolved from fixtures beside the checks
    /// file.
    pub(crate) fn from_canned(
        canned: &[CannedFetch],
        checks_path: &Path,
        allowed: Vec<HostPattern>,
    ) -> Result<Self, String> {
        let mut replies = Vec::with_capacity(canned.len());
        for reply in canned {
            let body = match (&reply.body, reply.fixture_path(checks_path)?) {
                (Some(inline), _) => inline.as_bytes().to_vec(),
                (None, Some(path)) => {
                    std::fs::read(&path).map_err(|e| format!("fixture {}: {e}", path.display()))?
                }
                (None, None) => Vec::new(),
            };
            replies.push(Reply {
                url: reply.url.clone(),
                status: reply.status,
                body,
                content_type: reply.content_type.clone(),
                authorized: reply.authorized,
            });
        }
        Ok(Self { replies, allowed })
    }

    fn reply_for(&self, url: &str) -> Option<&Reply> {
        self.replies
            .iter()
            .find(|r| r.url == url || r.url.is_empty())
    }
}

#[async_trait::async_trait]
impl GrantedFetch for CannedNetwork {
    async fn fetch(
        &self,
        request: FetchRequest,
        authorize: bool,
    ) -> Result<FetchResponse, SeamError> {
        let host = request.url.host_str().unwrap_or_default();
        if !self.allowed.iter().any(|p| p.matches(host)) {
            return Err(SeamError::Refused(format!(
                "host `{host}` is not in the manifest's hosts"
            )));
        }
        let Some(reply) = self.reply_for(request.url.as_str()) else {
            return Err(SeamError::Unavailable(format!(
                "conformance check: no canned reply for {}",
                request.url
            )));
        };
        if reply.authorized && !authorize {
            return Ok(FetchResponse {
                status: 401,
                headers: Vec::new(),
                body: Vec::new(),
            });
        }
        Ok(FetchResponse {
            status: reply.status,
            headers: reply
                .content_type
                .iter()
                .map(|ct| ("content-type".to_string(), ct.clone()))
                .collect(),
            body: reply.body.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

/// The phases after `manifest parses`, for a `seam = "connection"` artifact.
pub(crate) async fn check(
    mut report: CheckReport,
    artifact: &Path,
    manifest: ArtifactManifest,
) -> CheckReport {
    let fuel = WasmEntryConfig::default().fuel;

    // ---- static ----------------------------------------------------------
    let Some(kind) = static_phase(&mut report, &manifest) else {
        return report;
    };
    let allowed = manifest.capabilities.host_patterns().unwrap_or_default();

    // ---- mount ------------------------------------------------------------
    let checks_path = artifact.with_extension("checks.toml");
    let checks = std::fs::read_to_string(&checks_path)
        .ok()
        .and_then(|raw| ConnectionChecksFile::parse(&raw).ok());
    let config_toml = checks
        .as_ref()
        .and_then(|c| toml::to_string(&c.config).ok())
        .unwrap_or_default();
    let engine = new_engine();
    let Some((component, linker)) = mount_phase(&mut report, &engine, artifact) else {
        return report;
    };
    let starter = Starter {
        engine: &engine,
        component: &component,
        linker: &linker,
        name: &manifest.name,
        fuel,
        config_toml: &config_toml,
        allowed: &allowed,
    };
    let Some(described) = describe_phase(&mut report, &starter, &kind).await else {
        return report;
    };
    report.effective_claims = vec![format!(
        "{}:{}",
        described.kind,
        described.principal.trim().to_lowercase()
    )];

    // ---- contract ----------------------------------------------------------
    contract_phase(&mut report, &starter).await;

    // ---- golden -------------------------------------------------------------
    let Ok(raw_checks) = std::fs::read_to_string(&checks_path) else {
        report.push(
            Phase::Golden,
            "checks file",
            Outcome::Fail(format!(
                "no {} — golden checks are mandatory; write them first (docs/plugins/validation.md)",
                checks_path.display()
            )),
        );
        return report;
    };
    let checks = match ConnectionChecksFile::parse(&raw_checks) {
        Ok(c) => c,
        Err(e) => {
            report.push(
                Phase::Golden,
                "checks file",
                Outcome::Fail(format!("{}: {e}", checks_path.display())),
            );
            return report;
        }
    };
    match checks.required_coverage() {
        Ok(()) => report.push(Phase::Golden, "mandatory coverage", Outcome::Pass),
        Err(unmet) => {
            for reason in unmet {
                report.push(Phase::Golden, "mandatory coverage", Outcome::Fail(reason));
            }
        }
    }
    for check in &checks.check {
        let outcome = run_golden(&starter, &checks_path, check).await;
        report.push(Phase::Golden, check.name.clone(), outcome);
    }
    report
}

/// Static coherence of a connection manifest; returns the host kind when
/// the phases after it make sense.
fn static_phase(report: &mut CheckReport, manifest: &ArtifactManifest) -> Option<String> {
    let kind = match manifest.host_kind.as_deref().map(HostKind::new) {
        Some(Ok(kind)) => {
            report.push(Phase::Static, "host kind is declared", Outcome::Pass);
            kind
        }
        Some(Err(e)) => {
            report.push(
                Phase::Static,
                "host kind is declared",
                Outcome::Fail(e.to_string()),
            );
            return None;
        }
        None => {
            report.push(
                Phase::Static,
                "host kind is declared",
                Outcome::Fail("a connection manifest must name `host_kind`".into()),
            );
            return None;
        }
    };
    for item in crate::check::capability_items(manifest) {
        report.items.push(item);
    }
    if !manifest.claims.is_empty() {
        report.push(
            Phase::Static,
            "claims are inert",
            Outcome::Warn("a connection manifest's `claims` are ignored".into()),
        );
    }
    if manifest.capabilities.hosts.is_empty() {
        report.push(
            Phase::Static,
            "hosts are named",
            Outcome::Warn("the manifest names no hosts; every fetch will refuse".into()),
        );
    } else {
        report.push(Phase::Static, "hosts are named", Outcome::Pass);
    }
    Some(kind.as_str().to_string())
}

fn mount_phase(
    report: &mut CheckReport,
    engine: &Engine,
    artifact: &Path,
) -> Option<(Component, Linker<Invocation>)> {
    let component = match std::fs::read(artifact)
        .map_err(|e| format!("cannot read artifact: {e}"))
        .and_then(|bytes| {
            Component::new(engine, &bytes).map_err(|e| format!("not a valid component: {e}"))
        }) {
        Ok(c) => {
            report.push(Phase::Mount, "component compiles", Outcome::Pass);
            c
        }
        Err(e) => {
            report.push(Phase::Mount, "component compiles", Outcome::Fail(e));
            return None;
        }
    };
    match build_linker(engine) {
        Ok(linker) => Some((component, linker)),
        Err(e) => {
            report.push(Phase::Mount, "bridge linker", Outcome::Fail(e.to_string()));
            None
        }
    }
}

/// Everything needed to start one instance with one network.
struct Starter<'a> {
    engine: &'a Engine,
    component: &'a Component,
    linker: &'a Linker<Invocation>,
    name: &'a str,
    fuel: u64,
    config_toml: &'a str,
    allowed: &'a [HostPattern],
}

impl Starter<'_> {
    async fn start(
        &self,
        network: Arc<dyn GrantedFetch>,
        config_toml: &str,
    ) -> Result<Instance, String> {
        Instance::start(
            self.engine,
            self.component,
            self.linker,
            self.name,
            Grants {
                llm: None,
                bytes: None,
                fetch: Some(network),
                fetch_calls_max: WasmEntryConfig::default().fetch_calls_max,
            },
            self.fuel,
            config_toml,
        )
        .await
    }

    fn refusing(&self) -> Arc<dyn GrantedFetch> {
        Arc::new(CannedNetwork::refusing(self.allowed.to_vec()))
    }
}

/// Configure with the checks file's config, then ask the component who it
/// is: the kind must be the manifest's, the answer must be deterministic.
async fn describe_phase(
    report: &mut CheckReport,
    starter: &Starter<'_>,
    kind: &str,
) -> Option<WitHost> {
    let mut instance = match starter.start(starter.refusing(), starter.config_toml).await {
        Ok(i) => {
            report.push(Phase::Mount, "instantiates and configures", Outcome::Pass);
            i
        }
        Err(e) => {
            report.push(
                Phase::Mount,
                "instantiates and configures",
                Outcome::Fail(e),
            );
            return None;
        }
    };
    let described = match instance.describe_host().await {
        Ok(Ok(host)) => host,
        Ok(Err(e)) => {
            report.push(Phase::Mount, "describes its host", Outcome::Fail(e));
            return None;
        }
        Err(trap) => {
            report.push(
                Phase::Mount,
                "describes its host",
                Outcome::Fail(format!("trapped: {trap}")),
            );
            return None;
        }
    };
    match host_of(starter.name, kind, &described) {
        Ok(_) => report.push(Phase::Mount, "describes its host", Outcome::Pass),
        Err(e) => {
            report.push(Phase::Mount, "describes its host", Outcome::Fail(e));
            return None;
        }
    }
    let again = instance.describe_host().await;
    let same = matches!(&again, Ok(Ok(h)) if h.kind == described.kind && h.principal == described.principal);
    report.push(
        Phase::Mount,
        "host description is deterministic",
        if same {
            Outcome::Pass
        } else {
            Outcome::Fail("two describe-host calls returned different answers".into())
        },
    );
    match instance.capabilities().await {
        Ok(caps) => report.push(
            Phase::Mount,
            "exports capabilities",
            if caps.enumerates || caps.writable {
                Outcome::Pass
            } else {
                Outcome::Warn(
                    "neither enumerates nor writable: the host can only be fetched from".into(),
                )
            },
        ),
        Err(trap) => report.push(
            Phase::Mount,
            "exports capabilities",
            Outcome::Fail(format!("trapped: {trap}")),
        ),
    }
    Some(described)
}

/// The hostile battery: offline, a lying host, garbage locators, a
/// garbage config. Degrade (an error is fine), never trap.
async fn contract_phase(report: &mut CheckReport, starter: &Starter<'_>) {
    let offline = match starter.start(starter.refusing(), starter.config_toml).await {
        Ok(i) => Some(i),
        Err(e) => {
            report.push(Phase::Contract, "starts offline", Outcome::Fail(e));
            None
        }
    };
    if let Some(mut instance) = offline {
        report.push(
            Phase::Contract,
            "enumerates offline",
            trap_item(instance.enumerate("").await.map(|_| ())),
        );
        report.push(
            Phase::Contract,
            "reads a garbage locator offline",
            trap_item(
                instance
                    .read_bytes("\u{0}\u{FFFD} ../garbage")
                    .await
                    .map(|_| ()),
            ),
        );
        report.push(
            Phase::Contract,
            "describes a garbage locator offline",
            trap_item(
                instance
                    .describe("\u{0}\u{FFFD} ../garbage")
                    .await
                    .map(|_| ()),
            ),
        );
    }
    let lying: Arc<dyn GrantedFetch> = Arc::new(CannedNetwork::garbage(starter.allowed.to_vec()));
    match starter.start(lying, starter.config_toml).await {
        Ok(mut instance) => {
            report.push(
                Phase::Contract,
                "survives a host answering garbage",
                trap_item(instance.enumerate("").await.map(|_| ())),
            );
        }
        Err(e) => report.push(
            Phase::Contract,
            "survives a host answering garbage",
            Outcome::Fail(e),
        ),
    }
    let garbage_config = "not = = toml\n\u{0}";
    match starter.start(starter.refusing(), garbage_config).await {
        Ok(_) => report.push(
            Phase::Contract,
            "refuses a garbage config",
            Outcome::Warn("configure accepted unparseable TOML; prefer refusing it by name".into()),
        ),
        Err(e) if e.contains("trapped") => {
            report.push(
                Phase::Contract,
                "refuses a garbage config",
                Outcome::Fail(e),
            );
        }
        Err(_) => report.push(Phase::Contract, "refuses a garbage config", Outcome::Pass),
    }
}

fn trap_item(result: Result<(), wasmtime::Error>) -> Outcome {
    match result {
        Ok(()) => Outcome::Pass,
        Err(trap) => Outcome::Fail(format!("trapped: {trap}")),
    }
}

async fn run_golden(starter: &Starter<'_>, checks_path: &Path, check: &ConnectionCheck) -> Outcome {
    let network: Arc<dyn GrantedFetch> =
        match CannedNetwork::from_canned(&check.fetch, checks_path, starter.allowed.to_vec()) {
            Ok(n) => Arc::new(n),
            Err(reason) => return Outcome::Fail(reason),
        };
    let locator = match check.locator() {
        Ok(l) => l.to_string(),
        Err(reason) => return Outcome::Fail(reason),
    };
    let mut instance = match starter.start(network, starter.config_toml).await {
        Ok(i) => i,
        Err(e) => return Outcome::Fail(e),
    };
    let outcome = match check.call {
        ConnectionCall::Enumerate => match instance.enumerate(&check.root).await {
            Ok(Ok(sources)) => match hygiene(&sources) {
                Ok(()) => ConnectionOutcome::Sources(emitted_sources(&sources)),
                Err(reason) => return Outcome::Fail(reason),
            },
            Ok(Err(e)) => ConnectionOutcome::Error(e),
            Err(trap) => return Outcome::Fail(format!("trapped: {trap}")),
        },
        ConnectionCall::Read => match instance.read_bytes(&locator).await {
            Ok(Ok(bytes)) => ConnectionOutcome::Bytes(bytes),
            Ok(Err(e)) => ConnectionOutcome::Error(e),
            Err(trap) => return Outcome::Fail(format!("trapped: {trap}")),
        },
        ConnectionCall::Describe => match instance.describe(&locator).await {
            Ok(Ok(envelope)) => ConnectionOutcome::Envelope(emitted_envelope(&envelope)),
            Ok(Err(e)) => ConnectionOutcome::Error(e),
            Err(trap) => return Outcome::Fail(format!("trapped: {trap}")),
        },
    };
    match check.verdict(&outcome) {
        Ok(()) => Outcome::Pass,
        Err(reason) => Outcome::Fail(reason),
    }
}

/// Every enumerated locator must be one the bridge would keep.
fn hygiene(sources: &[WitSource]) -> Result<(), String> {
    if sources.len() > inseam_conformance::golden::SOURCES_PER_CHECK_MAX {
        return Err(format!(
            "enumeration returned {} sources; a check is an example, not a corpus",
            sources.len()
        ));
    }
    for (i, source) in sources.iter().enumerate() {
        if let Err(e) = Locator::new(source.locator.as_str()) {
            return Err(format!(
                "source {i} locator {:?}: {e}; the bridge would drop it",
                source.locator
            ));
        }
    }
    Ok(())
}

fn emitted_sources(sources: &[WitSource]) -> Vec<EmittedSource> {
    sources
        .iter()
        .map(|s| EmittedSource {
            locator: s.locator.clone(),
            content_type: s.envelope.content_type.clone(),
            hint: s.envelope.hint.clone(),
        })
        .collect()
}

fn emitted_envelope(envelope: &WitEnvelope) -> EmittedEnvelope {
    EmittedEnvelope {
        content_type: envelope.content_type.clone(),
        hint: envelope.hint.clone(),
    }
}

// ---------------------------------------------------------------------------
// One-off call: the authoring loop's "what does it answer for THIS host"
// ---------------------------------------------------------------------------

/// Which call `try_connection` makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TryCall {
    Enumerate { root: String },
    Read { locator: String },
    Describe { locator: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriedSource {
    pub locator: String,
    pub content_type: String,
    pub hint: Option<String>,
    pub raw_bytes: u64,
}

/// What one live call answered, plus the host the component described.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TryConnectionOutcome {
    pub host_kind: String,
    pub host_principal: String,
    pub host_display_name: String,
    pub sources: Vec<TriedSource>,
    pub bytes: Option<Vec<u8>>,
    pub envelope: Option<(String, Option<String>)>,
    pub notes: Vec<String>,
    /// The plugin returned `Err` for the call.
    pub plugin_error: Option<String>,
}

/// Configure a connection artifact with `config_toml` and make one call
/// through the **live** guarded network under the manifest's allow list —
/// no grant, so `authorize` refuses. Errors are the cases where nothing
/// could run at all; a plugin that ran and returned `Err` is an outcome.
pub async fn try_connection(
    artifact: &Path,
    config_toml: &str,
    call: TryCall,
) -> Result<TryConnectionOutcome, String> {
    let manifest_path = artifact.with_extension("manifest.toml");
    let manifest: ArtifactManifest = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))
        .and_then(|raw| {
            toml::from_str(&raw).map_err(|e| format!("{}: {e}", manifest_path.display()))
        })?;
    if manifest.seam != "connection" {
        return Err(format!(
            "{} implements the `{}` seam, not `connection`",
            manifest.name, manifest.seam
        ));
    }
    let engine = new_engine();
    let component = Component::from_file(&engine, artifact)
        .map_err(|e| format!("component does not compile: {e}"))?;
    let linker = build_linker(&engine).map_err(|e| format!("bridge linker: {e}"))?;
    let config = WasmEntryConfig::default();
    let mut notes = Vec::new();
    let network: Arc<dyn GrantedFetch> = match GuardedFetch::for_manifest(&manifest, &config, None)?
    {
        Some(live) => {
            notes.push(format!(
                "fetch is live under the manifest's hosts {:?}; authorize refuses (no grant)",
                manifest.capabilities.hosts
            ));
            live
        }
        None => {
            notes.push("the manifest names no hosts; every fetch refuses".into());
            Arc::new(CannedNetwork::refusing(Vec::new()))
        }
    };
    let starter = Starter {
        engine: &engine,
        component: &component,
        linker: &linker,
        name: &manifest.name,
        fuel: config.fuel,
        config_toml,
        allowed: &[],
    };
    let mut instance = starter.start(network, config_toml).await?;
    let described = match instance.describe_host().await {
        Ok(Ok(host)) => host,
        Ok(Err(e)) => return Err(format!("describe-host: {e}")),
        Err(trap) => return Err(format!("describe-host trapped: {trap}")),
    };
    let mut outcome = TryConnectionOutcome {
        host_kind: described.kind,
        host_principal: described.principal,
        host_display_name: described.display_name,
        notes,
        ..TryConnectionOutcome::default()
    };
    match call {
        TryCall::Enumerate { root } => match instance.enumerate(&root).await {
            Ok(Ok(sources)) => {
                outcome.sources = sources
                    .iter()
                    .map(|s| TriedSource {
                        locator: s.locator.clone(),
                        content_type: s.envelope.content_type.clone(),
                        hint: s.envelope.hint.clone(),
                        raw_bytes: s.raw_bytes,
                    })
                    .collect();
            }
            Ok(Err(e)) => outcome.plugin_error = Some(e),
            Err(trap) => return Err(format!("trapped: {trap}")),
        },
        TryCall::Read { locator } => match instance.read_bytes(&locator).await {
            Ok(Ok(bytes)) => outcome.bytes = Some(bytes),
            Ok(Err(e)) => outcome.plugin_error = Some(e),
            Err(trap) => return Err(format!("trapped: {trap}")),
        },
        TryCall::Describe { locator } => match instance.describe(&locator).await {
            Ok(Ok(envelope)) => outcome.envelope = Some((envelope.content_type, envelope.hint)),
            Ok(Err(e)) => outcome.plugin_error = Some(e),
            Err(trap) => return Err(format!("trapped: {trap}")),
        },
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_seams::fetch::FetchMethod;

    fn request(url: &str) -> FetchRequest {
        FetchRequest {
            method: FetchMethod::Get,
            url: url::Url::parse(url).expect("url"),
            headers: Vec::new(),
            body: None,
        }
    }

    #[tokio::test]
    async fn the_canned_network_honors_the_allow_list_and_authorization() {
        let allowed = vec![HostPattern::parse("api.example").expect("pattern")];
        let network = CannedNetwork::from_canned(
            &[CannedFetch {
                url: "https://api.example/private".into(),
                status: 200,
                body: Some("secret".into()),
                body_file: None,
                content_type: Some("text/plain".into()),
                authorized: true,
            }],
            Path::new("x/x.checks.toml"),
            allowed,
        )
        .expect("builds");
        let refused = network
            .fetch(request("https://other.example/x"), false)
            .await;
        assert!(matches!(refused, Err(SeamError::Refused(_))));
        let missing = network
            .fetch(request("https://api.example/missing"), false)
            .await;
        assert!(matches!(missing, Err(SeamError::Unavailable(_))));
        let unauthorized = network
            .fetch(request("https://api.example/private"), false)
            .await
            .expect("answers");
        assert_eq!(unauthorized.status, 401);
        let authorized = network
            .fetch(request("https://api.example/private"), true)
            .await
            .expect("answers");
        assert_eq!(authorized.status, 200);
        assert_eq!(authorized.body, b"secret");
        assert_eq!(authorized.headers[0].0, "content-type");
    }

    #[test]
    fn hygiene_refuses_malformed_locators() {
        let source = |locator: &str| {
            WitSource {
            locator: locator.into(),
            envelope: WitEnvelope {
                source_type: "file".into(),
                content_type: "text/plain".into(),
                length: crate::connection_world::exports::inseam::plugin::connection::ContentLength::Bytes(1),
                created: None,
                modified: None,
                hint: None,
            },
            raw_bytes: 1,
        }
        };
        assert!(hygiene(&[source("a/b")]).is_ok());
        assert!(hygiene(&[source("")]).is_err());
    }
}
