//! The typed event bus. Each event type declares its dispatch mode as part
//! of its contract (`design/kernel.md`):
//!
//! - [`Notify`] — fire-and-forget fan-out to every listener.
//! - [`Guard`] — a policy check where **denial is monotonic**: every listener
//!   is asked and any `Deny` wins; a later listener can never force-allow
//!   what one denied. Budget metering and boundary filtering are guards.
//! - [`Waterfall`] — listeners wrap each other and the built-in behavior
//!   middleware-style. A listener receives a [`Next`] value it must either
//!   consume (delegating inward) or discard by returning its own decision —
//!   "forgot to call next" cannot compile into "silently swallowed".
//!
//! Listener registration returns a [`Subscription`]; dropping it removes the
//! listener, which is what lets subscriptions be plugin effects.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Fire-and-forget events.
pub trait Notify: Send + Sync + 'static {}

/// Monotonic-deny policy checks.
pub trait Guard: Send + Sync + 'static {}

/// Middleware-style events with a decision type.
pub trait Waterfall: Send + Sync + 'static {
    type Decision;
}

/// A guard listener's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny(String),
}

impl Verdict {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

type NotifyFn<E> = Arc<dyn Fn(&E) + Send + Sync>;
type GuardFn<E> = Arc<dyn Fn(&E) -> Verdict + Send + Sync>;
type WaterfallFn<E> = Arc<dyn Fn(&E, Next<'_, E>) -> <E as Waterfall>::Decision + Send + Sync>;
/// The built-in behavior at the bottom of a waterfall.
type BaseFn<'a, E> = Box<dyn FnOnce(&E) -> <E as Waterfall>::Decision + 'a>;

/// The inward continuation a waterfall listener holds. Consuming it runs the
/// remaining listeners and finally the built-in behavior; not consuming it
/// means the listener's return value *is* the decision.
pub struct Next<'a, E: Waterfall> {
    rest: &'a [(u64, WaterfallFn<E>)],
    base: BaseFn<'a, E>,
}

impl<E: Waterfall> Next<'_, E> {
    pub fn invoke(self, event: &E) -> E::Decision {
        match self.rest.split_first() {
            Some(((_, head), rest)) => head(
                event,
                Next {
                    rest,
                    base: self.base,
                },
            ),
            None => (self.base)(event),
        }
    }
}

/// Removing handle for one listener; dropping it unsubscribes. Held inside a
/// fiber effect so unload unwinds the subscription like any other change.
pub struct Subscription {
    bus: Arc<EventBusInner>,
    event: TypeId,
    id: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.bus.remove(self.event, self.id);
    }
}

/// One event type's listeners: `(id, erased listener)`. The erasure is per
/// listener only — every event type's list has this same shape, so the map
/// holds the lists directly.
type Listeners = Vec<(u64, Box<dyn Any + Send + Sync>)>;

#[derive(Default)]
pub(crate) struct EventBusInner {
    listeners: Mutex<HashMap<TypeId, Listeners>>,
    next_id: AtomicU64,
}

impl EventBusInner {
    fn remove(&self, event: TypeId, id: u64) {
        let mut map = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(list) = map.get_mut(&event) {
            list.retain(|(i, _)| *i != id);
        }
    }
}

/// The bus. One per kernel; handed to plugins through their apply context.
#[derive(Clone, Default)]
pub struct EventBus {
    inner: Arc<EventBusInner>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    fn push<L: Send + Sync + 'static>(&self, event: TypeId, listener: L) -> Subscription {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let mut map = self
            .inner
            .listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.entry(event)
            .or_default()
            .push((id, Box::new(listener)));
        Subscription {
            bus: Arc::clone(&self.inner),
            event,
            id,
        }
    }

    fn snapshot<L: Clone + 'static>(&self, event: TypeId) -> Vec<(u64, L)> {
        let map = self
            .inner
            .listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.get(&event)
            .map(|list| {
                list.iter()
                    .filter_map(|(id, l)| l.downcast_ref::<L>().map(|l| (*id, l.clone())))
                    .collect()
            })
            .unwrap_or_default()
    }

    // -- notify ----------------------------------------------------------

    pub fn on<E: Notify>(&self, listener: impl Fn(&E) + Send + Sync + 'static) -> Subscription {
        self.push(TypeId::of::<E>(), Arc::new(listener) as NotifyFn<E>)
    }

    pub fn emit<E: Notify>(&self, event: &E) {
        for (_, listener) in self.snapshot::<NotifyFn<E>>(TypeId::of::<E>()) {
            listener(event);
        }
    }

    // -- guard -----------------------------------------------------------

    pub fn on_guard<E: Guard>(
        &self,
        listener: impl Fn(&E) -> Verdict + Send + Sync + 'static,
    ) -> Subscription {
        self.push(TypeId::of::<E>(), Arc::new(listener) as GuardFn<E>)
    }

    /// Ask every guard listener; the first `Deny` wins and no later listener
    /// can overturn it. No listeners means `Allow`.
    pub fn check<E: Guard>(&self, event: &E) -> Verdict {
        for (_, listener) in self.snapshot::<GuardFn<E>>(TypeId::of::<E>()) {
            if let Verdict::Deny(reason) = listener(event) {
                return Verdict::Deny(reason);
            }
        }
        Verdict::Allow
    }

    // -- waterfall ---------------------------------------------------------

    pub fn wrap<E: Waterfall>(
        &self,
        listener: impl Fn(&E, Next<'_, E>) -> E::Decision + Send + Sync + 'static,
    ) -> Subscription {
        self.push(TypeId::of::<E>(), Arc::new(listener) as WaterfallFn<E>)
    }

    /// Dispatch through the wrap chain (most recently registered outermost)
    /// down to `base`, the built-in behavior.
    pub fn dispatch<E: Waterfall>(
        &self,
        event: &E,
        base: impl FnOnce(&E) -> E::Decision,
    ) -> E::Decision {
        let mut chain = self.snapshot::<WaterfallFn<E>>(TypeId::of::<E>());
        chain.reverse(); // newest listener wraps outermost
        Next {
            rest: &chain,
            base: Box::new(base),
        }
        .invoke(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Ping(u32);
    impl Notify for Ping {}

    struct SpendRequest {
        amount: u32,
    }
    impl Guard for SpendRequest {}

    struct Fetch {
        url: &'static str,
    }
    impl Waterfall for Fetch {
        type Decision = String;
    }

    #[test]
    fn notify_reaches_every_listener_until_unsubscribed() {
        let bus = EventBus::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&count);
        let c2 = Arc::clone(&count);
        let s1 = bus.on::<Ping>(move |_| {
            c1.fetch_add(1, Ordering::SeqCst);
        });
        let _s2 = bus.on::<Ping>(move |p| {
            c2.fetch_add(p.0 as usize, Ordering::SeqCst);
        });
        bus.emit(&Ping(10));
        assert_eq!(count.load(Ordering::SeqCst), 11);
        drop(s1);
        bus.emit(&Ping(10));
        assert_eq!(count.load(Ordering::SeqCst), 21);
    }

    #[test]
    fn guard_denial_is_monotonic() {
        let bus = EventBus::new();
        assert!(bus.check(&SpendRequest { amount: 5 }).is_allowed());
        let _deny = bus.on_guard::<SpendRequest>(|e| {
            if e.amount > 3 {
                Verdict::Deny("over budget".into())
            } else {
                Verdict::Allow
            }
        });
        // A later always-allow listener cannot overturn the denial.
        let _allow = bus.on_guard::<SpendRequest>(|_| Verdict::Allow);
        assert_eq!(
            bus.check(&SpendRequest { amount: 5 }),
            Verdict::Deny("over budget".into())
        );
        assert!(bus.check(&SpendRequest { amount: 2 }).is_allowed());
    }

    #[test]
    fn waterfall_wraps_base_and_can_short_circuit() {
        let bus = EventBus::new();
        let base = |f: &Fetch| format!("fetched {}", f.url);
        assert_eq!(bus.dispatch(&Fetch { url: "a" }, base), "fetched a");

        let _audit = bus.wrap::<Fetch>(|e, next| format!("[audit] {}", next.invoke(e)));
        let _block = bus.wrap::<Fetch>(|e, next| {
            if e.url == "evil" {
                "blocked".to_string() // decision without consuming next
            } else {
                next.invoke(e)
            }
        });
        assert_eq!(bus.dispatch(&Fetch { url: "a" }, base), "[audit] fetched a");
        assert_eq!(bus.dispatch(&Fetch { url: "evil" }, base), "blocked");
    }
}
