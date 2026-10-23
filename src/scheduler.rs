use crate::types::{Account, RoutingPolicy, Strategy, now_ms};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::sync::Notify;
use uuid::Uuid;

const MAX_BINDINGS: usize = 10_000;
const MAX_AFFINITY_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

#[derive(Clone, Default)]
pub struct Scheduler {
    inner: Arc<Mutex<State>>,
    changed: Arc<Notify>,
}

#[derive(Default)]
struct State {
    accounts: HashMap<String, Entry>,
    conversations: HashMap<(String, String), Binding>,
    responses: HashMap<String, Binding>,
    round_robin: usize,
}

struct Entry {
    account: Account,
    present: bool,
    model_states: HashMap<String, ModelState>,
}

#[derive(Default)]
struct ModelState {
    in_flight: u32,
    cooldown_until_ms: u64,
    last_ok_ms: Option<u64>,
    last_error: Option<String>,
    successes: u64,
    failures: u64,
    consecutive_failures: u32,
}

struct Binding {
    key_id: String,
    model: String,
    created_at_ms: u64,
}

#[derive(Clone, Default)]
pub struct Selection {
    pub model: String,
    pub conversation: Option<String>,
    pub previous_response_id: Option<String>,
    pub exclude: HashSet<String>,
    pub explicit_key: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectError {
    NoEligible,
    Busy,
    UnknownResponse,
    AffinityUnavailable,
}

impl fmt::Display for SelectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NoEligible => "没有符合模型和路由设置的账号",
            Self::Busy => "符合条件的账号正在使用或冷却中",
            Self::UnknownResponse => "previous_response_id 的账号绑定未知或已过期",
            Self::AffinityUnavailable => "previous_response_id 所绑定的账号当前不可用",
        })
    }
}

impl std::error::Error for SelectError {}

/// Keep this value alive until the entire response body has been consumed or dropped.
/// It intentionally cannot be cloned, because one lease owns one concurrency slot.
pub struct Lease {
    pub account: Account,
    pub model: String,
    scheduler: Scheduler,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(entry) = self
            .scheduler
            .lock()
            .accounts
            .get_mut(&self.account.id)
            .and_then(|entry| entry.model_states.get_mut(&self.model))
        {
            entry.in_flight = entry.in_flight.saturating_sub(1);
        }
        self.scheduler.signal();
    }
}

/// Deliberately contains no secret-bearing Account or upstream request data.
#[derive(Clone, Debug, Serialize)]
pub struct AccountView {
    pub id: String,
    pub label: String,
    pub models: Vec<String>,
    pub enabled: bool,
    pub present: bool,
    pub in_flight: u32,
    pub max_concurrency: u32,
    pub cooldown_until_ms: Option<u64>,
    pub last_ok_ms: Option<u64>,
    pub last_error: Option<String>,
    pub successes: u64,
    pub failures: u64,
    pub source_last_ok_ms: Option<u64>,
    pub status: String,
    pub model_health: Vec<ModelHealthView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelHealthView {
    pub model: String,
    pub status: String,
    pub in_flight: u32,
    pub max_concurrency: u32,
    pub cooldown_until_ms: Option<u64>,
    pub last_ok_ms: Option<u64>,
    pub last_error: Option<String>,
    pub successes: u64,
    pub failures: u64,
    pub source_last_ok_ms: Option<u64>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn signal(&self) {
        self.changed.notify_waiters();
        // Retain one permit if a waiter has not yet subscribed.
        self.changed.notify_one();
    }

    pub fn notify(&self) -> Arc<Notify> {
        self.changed.clone()
    }

    /// IDs must be stable hashes of physical keys, as guaranteed by the source layer.
    /// Updating source metadata never resets runtime concurrency or health.
    pub fn sync_accounts(&self, accounts: Vec<Account>) {
        let mut state = self.lock();
        for entry in state.accounts.values_mut() {
            entry.present = false;
        }
        for account in accounts {
            let id = account.id.clone();
            if let Some(entry) = state.accounts.get_mut(&id) {
                if entry.present {
                    // Defensive de-duplication when several providers share one key.
                    for model in account.models {
                        if !entry.account.models.contains(&model) {
                            entry.account.models.push(model);
                        }
                    }
                    for provider in account.provider_ids {
                        if !entry.account.provider_ids.contains(&provider) {
                            entry.account.provider_ids.push(provider);
                        }
                    }
                    entry.account.source_max_concurrency = entry
                        .account
                        .source_max_concurrency
                        .min(account.source_max_concurrency);
                    entry.account.last_source_success_ms = entry
                        .account
                        .last_source_success_ms
                        .max(account.last_source_success_ms);
                    for (model, capacity) in account.model_concurrency {
                        entry
                            .account
                            .model_concurrency
                            .entry(model)
                            .and_modify(|existing| *existing = (*existing).min(capacity))
                            .or_insert(capacity);
                    }
                    for (model, last_ok) in account.model_source_success_ms {
                        entry
                            .account
                            .model_source_success_ms
                            .entry(model)
                            .and_modify(|existing| *existing = (*existing).max(last_ok))
                            .or_insert(last_ok);
                    }
                } else {
                    entry.account = account;
                    entry.present = true;
                }
            } else {
                state.accounts.insert(
                    id,
                    Entry {
                        account,
                        present: true,
                        model_states: HashMap::new(),
                    },
                );
            }
        }
        for entry in state.accounts.values_mut() {
            if entry.present {
                for model in &entry.account.models {
                    entry.model_states.entry(model.clone()).or_default();
                }
            }
            // A model can disappear independently of another model using the same key.
            entry.model_states.retain(|model, runtime| {
                runtime.in_flight > 0 || (entry.present && entry.account.models.contains(model))
            });
        }
        // Removed in-flight entries remain until a later sync so their leases release safely.
        // Response bindings are intentionally retained: a removed key must fail closed.
        state
            .accounts
            .retain(|_, entry| entry.present || total_in_flight(entry) > 0);
        drop(state);
        self.signal();
    }

    pub fn try_acquire(
        &self,
        selection: &Selection,
        policy: &RoutingPolicy,
    ) -> Result<Lease, SelectError> {
        let now = now_ms();
        let ttl = affinity_ttl_ms(policy);
        let mut state = self.lock();
        let requested_key = selection.explicit_key.as_ref().or_else(|| {
            (policy.strategy == Strategy::Pinned)
                .then_some(policy.pinned_key.as_ref())
                .flatten()
        });

        let hard_key = if let Some(response_id) = &selection.previous_response_id {
            let binding = state
                .responses
                .get(response_id)
                .ok_or(SelectError::UnknownResponse)?;
            if now.saturating_sub(binding.created_at_ms) >= ttl {
                state.responses.remove(response_id);
                return Err(SelectError::UnknownResponse);
            }
            if binding.model != selection.model {
                return Err(SelectError::AffinityUnavailable);
            }
            let key = binding.key_id.clone();
            if requested_key.is_some_and(|requested| requested != &key) {
                return Err(SelectError::AffinityUnavailable);
            }
            Some(key)
        } else {
            None
        };

        if policy.strategy == Strategy::Pinned && requested_key.is_none() {
            return Err(SelectError::NoEligible);
        }

        let chosen = if let Some(id) = &hard_key {
            let entry = state
                .accounts
                .get(id)
                .ok_or(SelectError::AffinityUnavailable)?;
            if !eligible(entry, selection, policy) {
                return Err(SelectError::AffinityUnavailable);
            }
            if !ready(entry, &selection.model, policy, now) {
                return Err(SelectError::Busy);
            }
            id.clone()
        } else {
            let eligible_ids: Vec<String> = state
                .accounts
                .iter()
                .filter(|(id, entry)| {
                    requested_key.is_none_or(|requested| requested == *id)
                        && eligible(entry, selection, policy)
                })
                .map(|(id, _)| id.clone())
                .collect();
            if eligible_ids.is_empty() {
                return Err(SelectError::NoEligible);
            }
            let mut available: Vec<String> = eligible_ids
                .into_iter()
                .filter(|id| ready(&state.accounts[id], &selection.model, policy, now))
                .collect();
            if available.is_empty() {
                return Err(SelectError::Busy);
            }
            available.sort();
            match policy.strategy {
                Strategy::Random => available[random_index(available.len())].clone(),
                Strategy::RoundRobin => {
                    let index = state.round_robin % available.len();
                    state.round_robin = state.round_robin.wrapping_add(1);
                    available[index].clone()
                }
                Strategy::Pinned => available[0].clone(),
                Strategy::Sticky => {
                    let sticky = selection
                        .conversation
                        .as_ref()
                        .and_then(|conversation| {
                            state
                                .conversations
                                .get(&(selection.model.clone(), conversation.clone()))
                        })
                        .filter(|binding| now.saturating_sub(binding.created_at_ms) < ttl)
                        .filter(|binding| available.contains(&binding.key_id))
                        .map(|binding| binding.key_id.clone());
                    if let Some(id) = sticky {
                        id
                    } else {
                        // Compare fractions without floating-point rounding.
                        let best = available
                            .iter()
                            .min_by(|left, right| {
                                let left_entry = &state.accounts[*left];
                                let right_entry = &state.accounts[*right];
                                (u64::from(model_in_flight(left_entry, &selection.model))
                                    * u64::from(max_concurrency(
                                        right_entry,
                                        &selection.model,
                                        policy,
                                    )))
                                .cmp(
                                    &(u64::from(model_in_flight(right_entry, &selection.model))
                                        * u64::from(max_concurrency(
                                            left_entry,
                                            &selection.model,
                                            policy,
                                        ))),
                                )
                            })
                            .expect("available is nonempty");
                        let best_entry = &state.accounts[best];
                        let ties: Vec<&String> = available
                            .iter()
                            .filter(|id| {
                                let entry = &state.accounts[*id];
                                u64::from(model_in_flight(entry, &selection.model))
                                    * u64::from(max_concurrency(
                                        best_entry,
                                        &selection.model,
                                        policy,
                                    ))
                                    == u64::from(model_in_flight(best_entry, &selection.model))
                                        * u64::from(max_concurrency(
                                            entry,
                                            &selection.model,
                                            policy,
                                        ))
                            })
                            .collect();
                        ties[random_index(ties.len())].clone()
                    }
                }
            }
        };

        let entry = state
            .accounts
            .get_mut(&chosen)
            .expect("selected account exists under lock");
        entry
            .model_states
            .entry(selection.model.clone())
            .or_default()
            .in_flight += 1;
        let account = entry.account.clone();
        if let Some(conversation) = &selection.conversation {
            let binding_key = (selection.model.clone(), conversation.clone());
            evict_oldest_if_full(&mut state.conversations, &binding_key);
            state.conversations.insert(
                binding_key,
                Binding {
                    key_id: chosen,
                    model: selection.model.clone(),
                    created_at_ms: now,
                },
            );
        }
        drop(state);
        Ok(Lease {
            account,
            model: selection.model.clone(),
            scheduler: self.clone(),
        })
    }

    pub fn mark_success(&self, id: &str, model: &str) {
        if let Some(entry) = self
            .lock()
            .accounts
            .get_mut(id)
            .and_then(|entry| entry.model_states.get_mut(model))
        {
            let now = now_ms();
            entry.successes = entry.successes.saturating_add(1);
            entry.last_ok_ms = Some(now);
            // A request admitted before another request failed can complete later.
            // Its success does not revoke that failure's remaining cooldown or Retry-After.
            if entry.cooldown_until_ms <= now {
                entry.consecutive_failures = 0;
                entry.last_error = None;
                entry.cooldown_until_ms = 0;
            }
        }
        self.signal();
    }

    pub fn mark_failure(
        &self,
        id: &str,
        model: &str,
        status: Option<u16>,
        retry_after_ms: Option<u64>,
        code: &str,
        policy: &RoutingPolicy,
    ) {
        if let Some(entry) = self
            .lock()
            .accounts
            .get_mut(id)
            .and_then(|entry| entry.model_states.get_mut(model))
        {
            entry.failures = entry.failures.saturating_add(1);
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
            entry.last_error = Some(safe_error_code(code));
            let base = policy.cooldown_base_ms.clamp(100, 30_000);
            let maximum = policy.cooldown_max_ms.clamp(base, 30_000);
            let exponential =
                base.saturating_mul(1u64 << entry.consecutive_failures.saturating_sub(1).min(20));
            let backoff = exponential.min(maximum);
            // 0..20% positive jitter. Normal transient failures remain <= 30 seconds.
            let jitter = random_index((backoff / 5 + 1) as usize) as u64;
            let transient = backoff.saturating_add(jitter).min(maximum);
            let delay = match status {
                Some(401 | 403) => exponential.clamp(60_000, 300_000),
                Some(429) => transient.max(retry_after_ms.unwrap_or(0).min(24 * 60 * 60 * 1_000)),
                None | Some(408 | 500..=599) => transient,
                _ => 0,
            };
            if delay > 0 {
                entry.cooldown_until_ms =
                    entry.cooldown_until_ms.max(now_ms().saturating_add(delay));
            }
        }
        self.signal();
    }

    /// A response ID can never be rebound to a different physical key or model.
    pub fn bind_response(&self, response_id: &str, key_id: &str, model: &str) {
        if response_id.is_empty() || response_id.len() > 1_024 {
            return;
        }
        let mut state = self.lock();
        if state.responses.contains_key(response_id) {
            return;
        }
        let response_id = response_id.to_owned();
        evict_oldest_if_full(&mut state.responses, &response_id);
        state.responses.insert(
            response_id,
            Binding {
                key_id: key_id.to_owned(),
                model: model.to_owned(),
                created_at_ms: now_ms(),
            },
        );
    }

    pub fn clear_cooldown(&self, id: &str, model: Option<&str>) -> bool {
        let found = if let Some(entry) = self.lock().accounts.get_mut(id) {
            if let Some(model) = model {
                if let Some(runtime) = entry.model_states.get_mut(model) {
                    runtime.cooldown_until_ms = 0;
                    runtime.consecutive_failures = 0;
                    true
                } else {
                    false
                }
            } else {
                for runtime in entry.model_states.values_mut() {
                    runtime.cooldown_until_ms = 0;
                    runtime.consecutive_failures = 0;
                }
                true
            }
        } else {
            false
        };
        if found {
            self.signal();
        }
        found
    }

    pub fn snapshot(&self, policy: &RoutingPolicy) -> Vec<AccountView> {
        let now = now_ms();
        let state = self.lock();
        let mut views: Vec<_> = state
            .accounts
            .values()
            .map(|entry| {
                let enabled = enabled(entry, policy);
                let mut model_health: Vec<_> = entry
                    .model_states
                    .iter()
                    .map(|(model, runtime)| {
                        let maximum = max_concurrency(entry, model, policy);
                        let cooldown =
                            (runtime.cooldown_until_ms > now).then_some(runtime.cooldown_until_ms);
                        let status = if !entry.present || !entry.account.models.contains(model) {
                            "removed"
                        } else if !enabled {
                            "disabled"
                        } else if cooldown.is_some() {
                            "cooldown"
                        } else if runtime.in_flight >= maximum
                            || shared_capacity_full(entry, policy)
                        {
                            "busy"
                        } else if runtime.successes > 0 {
                            "healthy"
                        } else {
                            "untested"
                        };
                        ModelHealthView {
                            model: model.clone(),
                            status: status.to_owned(),
                            in_flight: runtime.in_flight,
                            max_concurrency: maximum,
                            cooldown_until_ms: cooldown,
                            last_ok_ms: runtime.last_ok_ms,
                            last_error: runtime.last_error.clone(),
                            successes: runtime.successes,
                            failures: runtime.failures,
                            source_last_ok_ms: entry
                                .account
                                .model_source_success_ms
                                .get(model)
                                .copied(),
                        }
                    })
                    .collect();
                model_health.sort_by(|left, right| left.model.cmp(&right.model));
                let active: Vec<_> = model_health
                    .iter()
                    .filter(|view| view.status != "removed")
                    .collect();
                let sum_capacity = active
                    .iter()
                    .fold(0u32, |sum, view| sum.saturating_add(view.max_concurrency));
                let maximum = policy
                    .shared_account_concurrency
                    .map_or(sum_capacity, |limit| sum_capacity.min(limit.max(1)));
                let available = active
                    .iter()
                    .filter(|view| matches!(view.status.as_str(), "healthy" | "untested"))
                    .count();
                let status = if !entry.present {
                    "removed"
                } else if !enabled {
                    "disabled"
                } else if available > 0 && available < active.len() {
                    "partial"
                } else if available > 0 {
                    if active.iter().any(|view| view.status == "healthy") {
                        "healthy"
                    } else {
                        "untested"
                    }
                } else if !active.is_empty() && active.iter().all(|view| view.status == "cooldown")
                {
                    "cooldown"
                } else {
                    "busy"
                };
                AccountView {
                    id: entry.account.id.clone(),
                    label: entry.account.label.clone(),
                    models: entry.account.models.clone(),
                    enabled,
                    present: entry.present,
                    in_flight: total_in_flight(entry),
                    max_concurrency: maximum,
                    cooldown_until_ms: model_health
                        .iter()
                        .filter_map(|view| view.cooldown_until_ms)
                        .max(),
                    last_ok_ms: model_health.iter().filter_map(|view| view.last_ok_ms).max(),
                    last_error: model_health.iter().find_map(|view| view.last_error.clone()),
                    successes: model_health
                        .iter()
                        .fold(0u64, |sum, view| sum.saturating_add(view.successes)),
                    failures: model_health
                        .iter()
                        .fold(0u64, |sum, view| sum.saturating_add(view.failures)),
                    source_last_ok_ms: entry.account.last_source_success_ms,
                    status: status.to_owned(),
                    model_health,
                }
            })
            .collect();
        views.sort_by(|left, right| left.id.cmp(&right.id));
        views
    }
}

fn enabled(entry: &Entry, policy: &RoutingPolicy) -> bool {
    policy
        .account_overrides
        .get(&entry.account.id)
        .and_then(|item| item.enabled)
        .unwrap_or(true)
}

fn max_concurrency(entry: &Entry, model: &str, policy: &RoutingPolicy) -> u32 {
    policy
        .account_overrides
        .get(&entry.account.id)
        .and_then(|item| item.max_concurrency)
        .unwrap_or(policy.max_concurrency_per_key)
        .max(1)
        .min(
            entry
                .account
                .model_concurrency
                .get(model)
                .copied()
                .unwrap_or(entry.account.source_max_concurrency)
                .max(1),
        )
}

fn eligible(entry: &Entry, selection: &Selection, policy: &RoutingPolicy) -> bool {
    entry.present
        && enabled(entry, policy)
        && !selection.exclude.contains(&entry.account.id)
        && entry
            .account
            .models
            .iter()
            .any(|model| model == &selection.model)
}

fn ready(entry: &Entry, model: &str, policy: &RoutingPolicy, now: u64) -> bool {
    entry.model_states.get(model).is_some_and(|runtime| {
        runtime.cooldown_until_ms <= now
            && runtime.in_flight < max_concurrency(entry, model, policy)
            && !shared_capacity_full(entry, policy)
    })
}

fn model_in_flight(entry: &Entry, model: &str) -> u32 {
    entry
        .model_states
        .get(model)
        .map_or(0, |runtime| runtime.in_flight)
}

fn total_in_flight(entry: &Entry) -> u32 {
    entry
        .model_states
        .values()
        .fold(0u32, |sum, runtime| sum.saturating_add(runtime.in_flight))
}

fn shared_capacity_full(entry: &Entry, policy: &RoutingPolicy) -> bool {
    policy
        .shared_account_concurrency
        .is_some_and(|limit| total_in_flight(entry) >= limit.max(1))
}

fn affinity_ttl_ms(policy: &RoutingPolicy) -> u64 {
    policy
        .affinity_ttl_seconds
        .saturating_mul(1_000)
        .clamp(1_000, MAX_AFFINITY_MS)
}

fn random_index(count: usize) -> usize {
    debug_assert!(count > 0);
    (Uuid::new_v4().as_u128() % count as u128) as usize
}

fn evict_oldest_if_full<K: Eq + std::hash::Hash + Clone>(
    bindings: &mut HashMap<K, Binding>,
    incoming: &K,
) {
    if bindings.len() >= MAX_BINDINGS
        && !bindings.contains_key(incoming)
        && let Some(oldest) = bindings
            .iter()
            .min_by_key(|(_, binding)| binding.created_at_ms)
            .map(|(key, _)| key.clone())
    {
        bindings.remove(&oldest);
    }
}

fn safe_error_code(code: &str) -> String {
    if code.is_empty()
        || code.len() > 64
        || code.to_ascii_lowercase().contains("sk-")
        || !code
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-' | b'.'))
    {
        "upstream_error".to_owned()
    } else {
        code.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AccountOverride;
    use std::sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    fn account(id: &str, model: &str, capacity: u32) -> Account {
        Account {
            id: id.into(),
            secret: Arc::from(format!("sk-test-{id}")),
            upstream: "https://example.invalid/v1".into(),
            label: format!("账号 {id}"),
            models: vec![model.into()],
            provider_ids: vec![format!("provider-{model}")],
            source_max_concurrency: capacity,
            model_concurrency: HashMap::from([(model.into(), capacity)]),
            model_source_success_ms: HashMap::new(),
            last_source_success_ms: None,
        }
    }

    fn selection() -> Selection {
        Selection {
            model: "astra".into(),
            ..Selection::default()
        }
    }

    fn error(result: Result<Lease, SelectError>) -> SelectError {
        match result {
            Err(error) => error,
            Ok(_) => panic!("expected acquisition failure"),
        }
    }

    #[test]
    fn concurrency_is_atomic_and_bounded_by_source() {
        let scheduler = Scheduler::new();
        scheduler.sync_accounts(vec![account("a", "astra", 2)]);
        let mut policy = RoutingPolicy {
            max_concurrency_per_key: 50,
            ..RoutingPolicy::default()
        };
        policy.account_overrides.insert(
            "a".into(),
            AccountOverride {
                enabled: None,
                max_concurrency: Some(100),
            },
        );
        let barrier = Arc::new(Barrier::new(17));
        let release = Arc::new(Barrier::new(17));
        let successful = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let scheduler = scheduler.clone();
                let barrier = barrier.clone();
                let release = release.clone();
                let successful = successful.clone();
                let policy = policy.clone();
                scope.spawn(move || {
                    barrier.wait();
                    let lease = scheduler.try_acquire(&selection(), &policy).ok();
                    if lease.is_some() {
                        successful.fetch_add(1, Ordering::SeqCst);
                    }
                    release.wait();
                    drop(lease);
                });
            }
            barrier.wait();
            release.wait();
        });
        assert_eq!(successful.load(Ordering::SeqCst), 2);
        assert_eq!(scheduler.snapshot(&policy)[0].in_flight, 0);
        assert_eq!(scheduler.snapshot(&policy)[0].max_concurrency, 2);
    }

    #[test]
    fn lease_keeps_slot_until_response_drops_and_notifies() {
        use std::{
            future::Future,
            task::{Context, Waker},
        };

        let scheduler = Scheduler::new();
        let policy = RoutingPolicy::default();
        scheduler.sync_accounts(vec![account("a", "astra", 1)]);
        let lease = scheduler.try_acquire(&selection(), &policy).unwrap();
        scheduler.mark_success("a", "astra");
        assert_eq!(
            error(scheduler.try_acquire(&selection(), &policy)),
            SelectError::Busy
        );
        let notify = scheduler.notify();
        let mut context = Context::from_waker(Waker::noop());
        let mut previous_notification = Box::pin(notify.notified());
        assert!(previous_notification.as_mut().poll(&mut context).is_ready());
        let mut released_notification = Box::pin(notify.notified());
        assert!(
            released_notification
                .as_mut()
                .poll(&mut context)
                .is_pending()
        );
        drop(lease);
        assert!(released_notification.as_mut().poll(&mut context).is_ready());
        assert!(scheduler.try_acquire(&selection(), &policy).is_ok());
    }

    #[test]
    fn repeated_parallel_acquisition_never_exceeds_capacity() {
        let scheduler = Scheduler::new();
        scheduler.sync_accounts(vec![account("a", "astra", 3)]);
        let policy = RoutingPolicy {
            max_concurrency_per_key: 3,
            ..RoutingPolicy::default()
        };
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let successful = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(32));
        std::thread::scope(|scope| {
            for _ in 0..32 {
                let scheduler = scheduler.clone();
                let policy = policy.clone();
                let active = active.clone();
                let peak = peak.clone();
                let successful = successful.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..200 {
                        if let Ok(lease) = scheduler.try_acquire(&selection(), &policy) {
                            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(current, Ordering::SeqCst);
                            successful.fetch_add(1, Ordering::SeqCst);
                            assert!(current <= 3);
                            std::thread::yield_now();
                            active.fetch_sub(1, Ordering::SeqCst);
                            drop(lease);
                        } else {
                            std::thread::yield_now();
                        }
                    }
                });
            }
        });
        assert!(successful.load(Ordering::SeqCst) > 0);
        assert!(peak.load(Ordering::SeqCst) <= 3);
        assert_eq!(scheduler.snapshot(&policy)[0].in_flight, 0);
    }

    #[test]
    fn decreasing_capacity_does_not_reset_existing_leases() {
        let scheduler = Scheduler::new();
        let policy = RoutingPolicy {
            max_concurrency_per_key: 2,
            ..RoutingPolicy::default()
        };
        scheduler.sync_accounts(vec![account("a", "astra", 2)]);
        let first = scheduler.try_acquire(&selection(), &policy).unwrap();
        let second = scheduler.try_acquire(&selection(), &policy).unwrap();
        scheduler.sync_accounts(vec![account("a", "astra", 1)]);
        assert_eq!(scheduler.snapshot(&policy)[0].in_flight, 2);
        assert_eq!(
            error(scheduler.try_acquire(&selection(), &policy)),
            SelectError::Busy
        );
        drop(first);
        assert_eq!(
            error(scheduler.try_acquire(&selection(), &policy)),
            SelectError::Busy
        );
        drop(second);
        assert!(scheduler.try_acquire(&selection(), &policy).is_ok());
    }

    #[test]
    fn unknown_model_cannot_use_another_model_pool() {
        let scheduler = Scheduler::new();
        let policy = RoutingPolicy::default();
        scheduler.sync_accounts(vec![account("a", "sol", 1)]);
        assert_eq!(
            error(scheduler.try_acquire(&selection(), &policy)),
            SelectError::NoEligible
        );
    }

    #[test]
    fn sync_merges_duplicate_keys_and_preserves_in_flight() {
        let scheduler = Scheduler::new();
        let policy = RoutingPolicy::default();
        scheduler.sync_accounts(vec![account("a", "astra", 1), account("a", "sol", 5)]);
        let lease = scheduler.try_acquire(&selection(), &policy).unwrap();
        assert_eq!(scheduler.snapshot(&policy).len(), 1);
        assert_eq!(scheduler.snapshot(&policy)[0].models.len(), 2);
        scheduler.sync_accounts(vec![account("a", "astra", 1)]);
        assert_eq!(
            error(scheduler.try_acquire(&selection(), &policy)),
            SelectError::Busy
        );
        scheduler.sync_accounts(vec![]);
        assert_eq!(scheduler.snapshot(&policy)[0].status, "removed");
        assert_eq!(
            error(scheduler.try_acquire(&selection(), &policy)),
            SelectError::NoEligible
        );
        drop(lease);
        assert_eq!(scheduler.snapshot(&policy)[0].in_flight, 0);
        scheduler.sync_accounts(vec![]);
        assert!(scheduler.snapshot(&policy).is_empty());
    }

    #[test]
    fn previous_response_binding_is_hard_and_expires() {
        let scheduler = Scheduler::new();
        let policy = RoutingPolicy::default();
        scheduler.sync_accounts(vec![account("a", "astra", 1), account("b", "astra", 1)]);
        let mut request = selection();
        request.previous_response_id = Some("resp_1".into());
        assert_eq!(
            error(scheduler.try_acquire(&request, &policy)),
            SelectError::UnknownResponse
        );
        scheduler.bind_response("resp_1", "a", "astra");
        scheduler.bind_response("resp_1", "b", "astra");
        let lease = scheduler.try_acquire(&request, &policy).unwrap();
        assert_eq!(lease.account.id, "a");
        assert_eq!(
            error(scheduler.try_acquire(&request, &policy)),
            SelectError::Busy
        );
        drop(lease);
        scheduler.mark_failure("a", "astra", Some(503), None, "overloaded", &policy);
        assert_eq!(
            error(scheduler.try_acquire(&request, &policy)),
            SelectError::Busy
        );
        scheduler.clear_cooldown("a", None);
        request.explicit_key = Some("b".into());
        assert_eq!(
            error(scheduler.try_acquire(&request, &policy)),
            SelectError::AffinityUnavailable
        );
        request.explicit_key = None;
        scheduler.sync_accounts(vec![account("b", "astra", 1)]);
        assert_eq!(
            error(scheduler.try_acquire(&request, &policy)),
            SelectError::AffinityUnavailable
        );
        scheduler
            .lock()
            .responses
            .get_mut("resp_1")
            .unwrap()
            .created_at_ms = now_ms().saturating_sub(MAX_AFFINITY_MS + 1);
        assert_eq!(
            error(scheduler.try_acquire(&request, &policy)),
            SelectError::UnknownResponse
        );
    }

    #[test]
    fn sticky_falls_back_and_policies_change_without_restart() {
        let scheduler = Scheduler::new();
        let mut policy = RoutingPolicy::default();
        scheduler.sync_accounts(vec![account("a", "astra", 1), account("b", "astra", 1)]);
        let mut request = selection();
        request.conversation = Some("chat-1".into());
        request.explicit_key = Some("a".into());
        drop(scheduler.try_acquire(&request, &policy).unwrap());
        request.explicit_key = None;
        assert_eq!(
            scheduler.try_acquire(&request, &policy).unwrap().account.id,
            "a"
        );
        scheduler.mark_failure("a", "astra", Some(502), None, "upstream_error", &policy);
        assert_eq!(
            scheduler.try_acquire(&request, &policy).unwrap().account.id,
            "b"
        );
        scheduler.clear_cooldown("a", None);
        policy.strategy = Strategy::Pinned;
        policy.pinned_key = Some("a".into());
        assert_eq!(
            scheduler.try_acquire(&request, &policy).unwrap().account.id,
            "a"
        );
        policy.strategy = Strategy::RoundRobin;
        let first = scheduler
            .try_acquire(&request, &policy)
            .unwrap()
            .account
            .id
            .clone();
        let second = scheduler
            .try_acquire(&request, &policy)
            .unwrap()
            .account
            .id
            .clone();
        assert_ne!(first, second);
    }

    #[test]
    fn random_never_uses_excluded_disabled_or_incompatible_keys() {
        let scheduler = Scheduler::new();
        let mut policy = RoutingPolicy {
            strategy: Strategy::Random,
            ..RoutingPolicy::default()
        };
        scheduler.sync_accounts(vec![
            account("a", "astra", 1),
            account("b", "astra", 1),
            account("c", "sol", 1),
            account("d", "astra", 1),
        ]);
        policy.account_overrides.insert(
            "d".into(),
            AccountOverride {
                enabled: Some(false),
                max_concurrency: None,
            },
        );
        let mut request = selection();
        request.exclude.insert("b".into());
        for _ in 0..100 {
            assert_eq!(
                scheduler.try_acquire(&request, &policy).unwrap().account.id,
                "a"
            );
        }
    }

    #[test]
    fn cooldowns_recover_and_retry_after_is_respected() {
        let scheduler = Scheduler::new();
        let policy = RoutingPolicy::default();
        scheduler.sync_accounts(vec![account("a", "astra", 1)]);
        let before = now_ms();
        scheduler.mark_failure("a", "astra", Some(401), None, "invalid_key", &policy);
        assert!(scheduler.snapshot(&policy)[0].cooldown_until_ms.unwrap() >= before + 60_000);
        scheduler.clear_cooldown("a", None);
        scheduler.mark_failure(
            "a",
            "astra",
            Some(429),
            Some(120_000),
            "rate_limit",
            &policy,
        );
        assert!(scheduler.snapshot(&policy)[0].cooldown_until_ms.unwrap() >= before + 120_000);
        scheduler.clear_cooldown("a", None);
        for _ in 0..30 {
            scheduler.mark_failure("a", "astra", Some(503), None, "overloaded", &policy);
        }
        assert!(scheduler.snapshot(&policy)[0].cooldown_until_ms.unwrap() <= now_ms() + 30_000);
        scheduler
            .lock()
            .accounts
            .get_mut("a")
            .unwrap()
            .model_states
            .get_mut("astra")
            .unwrap()
            .cooldown_until_ms = now_ms().saturating_sub(1);
        assert!(scheduler.try_acquire(&selection(), &policy).is_ok());
        scheduler.mark_success("a", "astra");
        let view = &scheduler.snapshot(&policy)[0];
        assert_eq!(view.status, "healthy");
        assert!(view.last_error.is_none());
    }

    #[test]
    fn snapshot_has_no_secret_and_rejects_raw_error_text() {
        let scheduler = Scheduler::new();
        let policy = RoutingPolicy::default();
        scheduler.sync_accounts(vec![account("a", "astra", 1)]);
        scheduler.mark_failure("a", "astra", None, None, "sk-test-a", &policy);
        let json = serde_json::to_string(&scheduler.snapshot(&policy)).unwrap();
        assert!(!json.contains("sk-test-a"));
        assert!(!json.contains("secret"));
        assert!(json.contains("upstream_error"));
    }

    fn sol_selection() -> Selection {
        Selection {
            model: "sol".into(),
            ..Selection::default()
        }
    }

    #[test]
    fn astra_rate_limit_and_recovery_do_not_change_sol_health() {
        let scheduler = Scheduler::new();
        let policy = RoutingPolicy::default();
        scheduler.sync_accounts(vec![account("a", "astra", 1), account("a", "sol", 1)]);
        scheduler.mark_failure("a", "astra", Some(429), Some(60_000), "rate_limit", &policy);
        scheduler.mark_success("a", "sol");
        assert_eq!(
            error(scheduler.try_acquire(&selection(), &policy)),
            SelectError::Busy
        );
        assert!(scheduler.try_acquire(&sol_selection(), &policy).is_ok());
        let view = scheduler.snapshot(&policy).remove(0);
        assert_eq!(view.status, "partial");
        let astra = view
            .model_health
            .iter()
            .find(|view| view.model == "astra")
            .unwrap();
        let sol = view
            .model_health
            .iter()
            .find(|view| view.model == "sol")
            .unwrap();
        assert_eq!(astra.failures, 1);
        assert_eq!(astra.successes, 0);
        assert_eq!(sol.failures, 0);
        assert_eq!(sol.successes, 1);
        assert!(astra.cooldown_until_ms.is_some());
        assert!(sol.cooldown_until_ms.is_none());
        scheduler.mark_failure("a", "sol", Some(503), None, "overloaded", &policy);
        assert!(scheduler.clear_cooldown("a", Some("astra")));
        assert!(scheduler.try_acquire(&selection(), &policy).is_ok());
        assert_eq!(
            error(scheduler.try_acquire(&sol_selection(), &policy)),
            SelectError::Busy
        );
        assert!(!scheduler.clear_cooldown("a", Some("unknown_model")));
        assert!(scheduler.clear_cooldown("a", None));
        assert!(scheduler.try_acquire(&sol_selection(), &policy).is_ok());
    }

    #[test]
    fn concurrent_success_preserves_another_requests_retry_after() {
        let scheduler = Scheduler::new();
        let policy = RoutingPolicy {
            max_concurrency_per_key: 2,
            ..RoutingPolicy::default()
        };
        scheduler.sync_accounts(vec![account("a", "astra", 2), account("a", "sol", 1)]);
        let first = scheduler.try_acquire(&selection(), &policy).unwrap();
        let second = scheduler.try_acquire(&selection(), &policy).unwrap();
        scheduler.mark_failure(
            "a",
            "astra",
            Some(429),
            Some(120_000),
            "rate_limit",
            &policy,
        );
        drop(first);
        let cooldown = scheduler.snapshot(&policy)[0]
            .model_health
            .iter()
            .find(|view| view.model == "astra")
            .unwrap()
            .cooldown_until_ms;
        scheduler.mark_success("a", "astra");
        drop(second);
        scheduler.mark_success("a", "sol");
        assert!(scheduler.try_acquire(&sol_selection(), &policy).is_ok());
        assert_eq!(
            error(scheduler.try_acquire(&selection(), &policy)),
            SelectError::Busy
        );
        let view = scheduler.snapshot(&policy).remove(0);
        let astra = view
            .model_health
            .iter()
            .find(|view| view.model == "astra")
            .unwrap();
        assert_eq!(astra.cooldown_until_ms, cooldown);
        assert_eq!(astra.last_error.as_deref(), Some("rate_limit"));
        assert_eq!(astra.successes, 1);
        assert_eq!(astra.failures, 1);
        assert_eq!(astra.status, "cooldown");
        assert_eq!(astra.in_flight, 0);
        scheduler
            .lock()
            .accounts
            .get_mut("a")
            .unwrap()
            .model_states
            .get_mut("astra")
            .unwrap()
            .cooldown_until_ms = now_ms().saturating_sub(1);
        let recovered = scheduler.try_acquire(&selection(), &policy).unwrap();
        scheduler.mark_success("a", "astra");
        drop(recovered);
        let view = scheduler.snapshot(&policy).remove(0);
        let astra = view
            .model_health
            .iter()
            .find(|view| view.model == "astra")
            .unwrap();
        assert!(astra.cooldown_until_ms.is_none());
        assert!(astra.last_error.is_none());
        assert_eq!(astra.status, "healthy");
    }

    #[test]
    fn one_key_can_hold_one_request_per_model_simultaneously() {
        let scheduler = Scheduler::new();
        let policy = RoutingPolicy::default();
        scheduler.sync_accounts(vec![account("a", "astra", 1), account("a", "sol", 1)]);
        let astra = scheduler.try_acquire(&selection(), &policy).unwrap();
        let sol = scheduler.try_acquire(&sol_selection(), &policy).unwrap();
        assert_eq!(astra.account.id, sol.account.id);
        assert_eq!(astra.model, "astra");
        assert_eq!(sol.model, "sol");
        let view = scheduler.snapshot(&policy).remove(0);
        assert_eq!(view.in_flight, 2);
        assert_eq!(view.max_concurrency, 2);
        assert_eq!(
            error(scheduler.try_acquire(&selection(), &policy)),
            SelectError::Busy
        );
        assert_eq!(
            error(scheduler.try_acquire(&sol_selection(), &policy)),
            SelectError::Busy
        );
        drop(astra);
        assert!(scheduler.try_acquire(&selection(), &policy).is_ok());
        assert_eq!(
            error(scheduler.try_acquire(&sol_selection(), &policy)),
            SelectError::Busy
        );
        drop(sol);
        assert_eq!(scheduler.snapshot(&policy)[0].in_flight, 0);
    }

    #[test]
    fn shared_account_limit_is_optional_and_updates_live() {
        let scheduler = Scheduler::new();
        let mut policy = RoutingPolicy {
            shared_account_concurrency: Some(1),
            ..RoutingPolicy::default()
        };
        scheduler.sync_accounts(vec![account("a", "astra", 1), account("a", "sol", 1)]);
        let astra = scheduler.try_acquire(&selection(), &policy).unwrap();
        assert_eq!(
            error(scheduler.try_acquire(&sol_selection(), &policy)),
            SelectError::Busy
        );
        policy.shared_account_concurrency = None;
        let sol = scheduler.try_acquire(&sol_selection(), &policy).unwrap();
        assert_eq!(scheduler.snapshot(&policy)[0].in_flight, 2);
        policy.shared_account_concurrency = Some(1);
        assert_eq!(scheduler.snapshot(&policy)[0].max_concurrency, 1);
        drop(astra);
        assert_eq!(
            error(scheduler.try_acquire(&selection(), &policy)),
            SelectError::Busy
        );
        drop(sol);
        assert!(scheduler.try_acquire(&selection(), &policy).is_ok());
    }

    #[test]
    fn deleting_one_source_model_blocks_routing_but_preserves_its_lease() {
        let scheduler = Scheduler::new();
        let policy = RoutingPolicy::default();
        scheduler.sync_accounts(vec![account("a", "astra", 1), account("a", "sol", 1)]);
        let astra = scheduler.try_acquire(&selection(), &policy).unwrap();
        let sol = scheduler.try_acquire(&sol_selection(), &policy).unwrap();
        scheduler.bind_response("resp_astra", "a", "astra");
        scheduler.sync_accounts(vec![account("a", "sol", 1)]);
        assert_eq!(
            error(scheduler.try_acquire(&selection(), &policy)),
            SelectError::NoEligible
        );
        let mut continuation = selection();
        continuation.previous_response_id = Some("resp_astra".into());
        assert_eq!(
            error(scheduler.try_acquire(&continuation, &policy)),
            SelectError::AffinityUnavailable
        );
        let view = scheduler.snapshot(&policy).remove(0);
        assert_eq!(view.models, vec!["sol"]);
        assert_eq!(view.in_flight, 2);
        let astra_view = view
            .model_health
            .iter()
            .find(|view| view.model == "astra")
            .unwrap();
        assert_eq!(astra_view.status, "removed");
        assert_eq!(astra_view.in_flight, 1);
        drop(sol);
        assert!(scheduler.try_acquire(&sol_selection(), &policy).is_ok());
        drop(astra);
        assert_eq!(scheduler.snapshot(&policy)[0].in_flight, 0);
        scheduler.sync_accounts(vec![account("a", "sol", 1)]);
        assert_eq!(scheduler.snapshot(&policy)[0].model_health.len(), 1);
    }

    #[test]
    fn source_limits_and_success_timestamps_are_per_model() {
        let scheduler = Scheduler::new();
        let policy = RoutingPolicy {
            max_concurrency_per_key: 10,
            ..RoutingPolicy::default()
        };
        let mut astra_account = account("a", "astra", 1);
        let mut sol_account = account("a", "sol", 3);
        astra_account
            .model_source_success_ms
            .insert("astra".into(), 1_000);
        sol_account
            .model_source_success_ms
            .insert("sol".into(), 2_000);
        scheduler.sync_accounts(vec![astra_account, sol_account]);
        let astra = scheduler.try_acquire(&selection(), &policy).unwrap();
        let sol: Vec<_> = (0..3)
            .map(|_| scheduler.try_acquire(&sol_selection(), &policy).unwrap())
            .collect();
        assert_eq!(
            error(scheduler.try_acquire(&selection(), &policy)),
            SelectError::Busy
        );
        assert_eq!(
            error(scheduler.try_acquire(&sol_selection(), &policy)),
            SelectError::Busy
        );
        let view = scheduler.snapshot(&policy).remove(0);
        assert_eq!(view.in_flight, 4);
        assert_eq!(view.max_concurrency, 4);
        let astra_view = view
            .model_health
            .iter()
            .find(|view| view.model == "astra")
            .unwrap();
        let sol_view = view
            .model_health
            .iter()
            .find(|view| view.model == "sol")
            .unwrap();
        assert_eq!(astra_view.source_last_ok_ms, Some(1_000));
        assert_eq!(sol_view.source_last_ok_ms, Some(2_000));
        drop(astra);
        drop(sol);
        assert_eq!(scheduler.snapshot(&policy)[0].in_flight, 0);
    }

    #[test]
    fn response_binding_rejects_model_changes_and_model_collision() {
        let scheduler = Scheduler::new();
        let policy = RoutingPolicy::default();
        scheduler.sync_accounts(vec![account("a", "astra", 1), account("a", "sol", 1)]);
        scheduler.bind_response("resp_1", "a", "astra");
        scheduler.bind_response("resp_1", "a", "sol");
        let mut sol = sol_selection();
        sol.previous_response_id = Some("resp_1".into());
        assert_eq!(
            error(scheduler.try_acquire(&sol, &policy)),
            SelectError::AffinityUnavailable
        );
        let mut astra = selection();
        astra.previous_response_id = Some("resp_1".into());
        assert!(scheduler.try_acquire(&astra, &policy).is_ok());
    }
}
