//! The substrate's contract, end to end (`design/kernel.md`): reactive
//! activation with no boot order, loud missing dependencies, effect-unwind
//! teardown, provider hot-swap restarting consumers, per-fiber failure
//! containment, and confluence — the quiescent state after any history of
//! edits equals a fresh boot of the final composition.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use inseam_kernel::substrate::{
    ApplyCx, Composition, Facts, FiberState, Inject, Kernel, Manifest, Plugin, PluginError,
    PluginFactory, ServiceKey, SubstrateError,
};

// A tiny test seam: a greeter service and plugins around it.

trait Greeter: Send + Sync {
    fn greet(&self) -> String;
}

const GREETER: ServiceKey<dyn Greeter> = ServiceKey::new("greeter");

struct FixedGreeter(String);

impl Greeter for FixedGreeter {
    fn greet(&self) -> String {
        self.0.clone()
    }
}

/// Journal shared by all test plugins so tests can assert order and effects.
type Journal = Arc<Mutex<Vec<String>>>;

struct ProviderPlugin {
    greeting: String,
    journal: Journal,
}

#[async_trait::async_trait]
impl Plugin for ProviderPlugin {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "greeter-provider",
            inject: &[],
            provides: &["greeter"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        cx.provide(
            &GREETER,
            Arc::new(FixedGreeter(self.greeting.clone())) as Arc<dyn Greeter>,
            Facts::new().with("tone", "friendly"),
        )?;
        let journal = Arc::clone(&self.journal);
        let id = cx.entry_id().to_string();
        journal.lock().unwrap().push(format!("provider {id} up"));
        cx.effect("journal provider down", move || {
            journal.lock().unwrap().push(format!("provider {id} down"));
        });
        Ok(())
    }
}

struct ConsumerPlugin {
    journal: Journal,
}

#[async_trait::async_trait]
impl Plugin for ConsumerPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("greeter")];
        Manifest {
            name: "greeter-consumer",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let greeter = cx.get(&GREETER)?;
        let journal = Arc::clone(&self.journal);
        journal
            .lock()
            .unwrap()
            .push(format!("consumer heard: {}", greeter.greet()));
        // Hold the withdrawn-service handle through teardown, proving a
        // consumer can use the capability it is losing to clean up.
        cx.effect("journal consumer down", move || {
            journal
                .lock()
                .unwrap()
                .push(format!("consumer down, still heard: {}", greeter.greet()));
        });
        Ok(())
    }
}

struct FailingPlugin;

#[async_trait::async_trait]
impl Plugin for FailingPlugin {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "failer",
            inject: &[],
            provides: &[],
        }
    }

    async fn apply(&self, _cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        Err(PluginError::new("deliberate failure"))
    }
}

/// A plugin that reaches for a service its manifest never declared.
struct SneakyPlugin;

#[async_trait::async_trait]
impl Plugin for SneakyPlugin {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "sneaky",
            inject: &[], // greeter is deliberately not declared
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let _ = cx.get(&GREETER)?; // must refuse
        Ok(())
    }
}

struct TestFactory {
    name: &'static str,
    journal: Journal,
    applies: Arc<AtomicUsize>,
}

impl PluginFactory for TestFactory {
    fn name(&self) -> &str {
        self.name
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        self.applies.fetch_add(0, Ordering::Relaxed);
        Ok(match self.name {
            "greeter-provider" => Box::new(ProviderPlugin {
                greeting: config
                    .get("greeting")
                    .and_then(|v| v.as_str())
                    .unwrap_or("hello")
                    .to_string(),
                journal: Arc::clone(&self.journal),
            }),
            "greeter-consumer" => Box::new(ConsumerPlugin {
                journal: Arc::clone(&self.journal),
            }),
            "failer" => Box::new(FailingPlugin),
            "sneaky" => Box::new(SneakyPlugin),
            other => return Err(PluginError::new(format!("unknown test plugin {other}"))),
        })
    }
}

fn factories(journal: &Journal) -> Vec<Arc<dyn PluginFactory>> {
    ["greeter-provider", "greeter-consumer", "failer", "sneaky"]
        .into_iter()
        .map(|name| {
            Arc::new(TestFactory {
                name,
                journal: Arc::clone(journal),
                applies: Arc::new(AtomicUsize::new(0)),
            }) as Arc<dyn PluginFactory>
        })
        .collect()
}

async fn kernel(journal: &Journal, dir: &std::path::Path) -> Kernel {
    Kernel::boot(dir, factories(journal), Vec::new())
        .await
        .expect("boots")
}

fn composition(toml_text: &str) -> Composition {
    Composition::parse(toml_text, "test").expect("test composition parses")
}

#[tokio::test]
async fn activation_is_reactive_not_ordered() {
    let journal: Journal = Default::default();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut kernel = kernel(&journal, dir.path()).await;
    // The consumer is listed BEFORE its provider; file order carries no
    // semantics.
    kernel
        .reconcile(&composition(
            r#"
            [[entry]]
            id = "c"
            plugin = "greeter-consumer"

            [[entry]]
            id = "p"
            plugin = "greeter-provider"
            [entry.config]
            greeting = "hi"
            "#,
        ))
        .await
        .expect("settles");
    let log = journal.lock().unwrap().clone();
    assert!(log.contains(&"consumer heard: hi".to_string()), "{log:?}");
    assert!(kernel
        .fibers()
        .iter()
        .all(|f| f.state == FiberState::Active));
}

#[tokio::test]
async fn missing_required_dependencies_are_loud_and_name_the_keys() {
    let journal: Journal = Default::default();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut kernel = kernel(&journal, dir.path()).await;
    let err = kernel
        .reconcile(&composition(
            "[[entry]]\nid = \"c\"\nplugin = \"greeter-consumer\"",
        ))
        .await
        .expect_err("cannot settle");
    let message = err.to_string();
    assert!(message.contains("c"), "names the entry: {message}");
    assert!(message.contains("greeter"), "names the key: {message}");
}

#[tokio::test]
async fn unload_unwinds_effects_in_reverse_with_consumers_first() {
    let journal: Journal = Default::default();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut kernel = kernel(&journal, dir.path()).await;
    let full = composition(
        r#"
        [[entry]]
        id = "p"
        plugin = "greeter-provider"

        [[entry]]
        id = "c"
        plugin = "greeter-consumer"
        "#,
    );
    kernel.reconcile(&full).await.expect("settles");
    journal.lock().unwrap().clear();

    // Remove the provider: the consumer must tear down FIRST, and its
    // teardown can still call the withdrawn service through its held Arc.
    kernel
        .reconcile(&composition(
            "[[entry]]\nid = \"c\"\nplugin = \"greeter-consumer\"\ndisabled = true",
        ))
        .await
        .expect("settles to empty");
    let log = journal.lock().unwrap().clone();
    assert_eq!(
        log,
        vec![
            "consumer down, still heard: hello".to_string(),
            "provider p down".to_string(),
        ],
        "consumers before providers, effects reversed"
    );
}

#[tokio::test]
async fn provider_config_change_restarts_consumers_against_the_new_provider() {
    let journal: Journal = Default::default();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut kernel = kernel(&journal, dir.path()).await;
    kernel
        .reconcile(&composition(
            r#"
            [[entry]]
            id = "p"
            plugin = "greeter-provider"
            [entry.config]
            greeting = "old"

            [[entry]]
            id = "c"
            plugin = "greeter-consumer"
            "#,
        ))
        .await
        .expect("settles");
    journal.lock().unwrap().clear();

    kernel
        .reconcile(&composition(
            r#"
            [[entry]]
            id = "p"
            plugin = "greeter-provider"
            [entry.config]
            greeting = "new"

            [[entry]]
            id = "c"
            plugin = "greeter-consumer"
            "#,
        ))
        .await
        .expect("settles");
    let log = journal.lock().unwrap().clone();
    assert!(
        log.contains(&"consumer heard: new".to_string()),
        "consumer restarted against the new provider: {log:?}"
    );
    assert!(
        log.iter().any(|l| l == "consumer down, still heard: old"),
        "old consumer tore down while the old provider was readable: {log:?}"
    );
}

#[tokio::test]
async fn failure_lands_the_fiber_alone() {
    let journal: Journal = Default::default();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut kernel = kernel(&journal, dir.path()).await;
    kernel
        .reconcile(&composition(
            r#"
            [[entry]]
            id = "bad"
            plugin = "failer"

            [[entry]]
            id = "p"
            plugin = "greeter-provider"

            [[entry]]
            id = "c"
            plugin = "greeter-consumer"
            "#,
        ))
        .await
        .expect("the rest settles");
    let fibers = kernel.fibers();
    let state_of = |id: &str| {
        fibers
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.state.clone())
            .expect("fiber present")
    };
    assert!(matches!(state_of("bad"), FiberState::Failed(_)));
    assert_eq!(state_of("p"), FiberState::Active);
    assert_eq!(state_of("c"), FiberState::Active);
}

#[tokio::test]
async fn undeclared_access_is_refused() {
    let journal: Journal = Default::default();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut kernel = kernel(&journal, dir.path()).await;
    kernel
        .reconcile(&composition(
            r#"
            [[entry]]
            id = "p"
            plugin = "greeter-provider"

            [[entry]]
            id = "s"
            plugin = "sneaky"
            "#,
        ))
        .await
        .expect("settles; the sneak just fails");
    let fibers = kernel.fibers();
    let sneaky = fibers.iter().find(|f| f.id == "s").expect("present");
    let FiberState::Failed(reason) = &sneaky.state else {
        panic!("undeclared access must fail the fiber, got {:?}", sneaky.state);
    };
    assert!(reason.contains("without declaring"), "{reason}");
}

#[tokio::test]
async fn confluence_dynamic_history_leaves_no_trace() {
    let dir_a = tempfile::tempdir().expect("tempdir");
    let dir_b = tempfile::tempdir().expect("tempdir");
    let final_composition = composition(
        r#"
        [[entry]]
        id = "p"
        plugin = "greeter-provider"
        [entry.config]
        greeting = "final"

        [[entry]]
        id = "c"
        plugin = "greeter-consumer"
        "#,
    );

    // Kernel A: a messy history — extra entries mounted and removed, config
    // churn, a failure — ending at the final composition.
    let journal_a: Journal = Default::default();
    let mut kernel_a = kernel(&journal_a, dir_a.path()).await;
    kernel_a
        .reconcile(&composition(
            r#"
            [[entry]]
            id = "p"
            plugin = "greeter-provider"
            [entry.config]
            greeting = "draft"

            [[entry]]
            id = "doomed"
            plugin = "failer"
            "#,
        ))
        .await
        .expect("settles");
    kernel_a
        .reconcile(&composition(
            r#"
            [[entry]]
            id = "c"
            plugin = "greeter-consumer"

            [[entry]]
            id = "p"
            plugin = "greeter-provider"
            [entry.config]
            greeting = "second draft"
            "#,
        ))
        .await
        .expect("settles");
    kernel_a.reconcile(&final_composition).await.expect("settles");

    // Kernel B: a fresh boot of the final composition.
    let journal_b: Journal = Default::default();
    let mut kernel_b = kernel(&journal_b, dir_b.path()).await;
    kernel_b.reconcile(&final_composition).await.expect("settles");

    // Quiescent states match: same fibers, same states, same providers,
    // same live effects.
    let view = |k: &Kernel| {
        let mut fibers: Vec<(String, String, FiberState, Vec<String>)> = k
            .fibers()
            .into_iter()
            .map(|f| (f.id, f.plugin, f.state, f.effects))
            .collect();
        fibers.sort_by(|a, b| a.0.cmp(&b.0));
        (fibers, k.providers())
    };
    assert_eq!(view(&kernel_a), view(&kernel_b));
    assert!(
        journal_a
            .lock()
            .unwrap()
            .contains(&"consumer heard: final".to_string()),
        "the surviving consumer runs against the final config"
    );
}
