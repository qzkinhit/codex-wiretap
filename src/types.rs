use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Secret-bearing input. Deliberately does not implement Serialize or Debug.
#[derive(Clone)]
pub struct Account {
    pub id: String,
    pub secret: Arc<str>,
    pub upstream: String,
    pub label: String,
    pub models: Vec<String>,
    pub provider_ids: Vec<String>,
    pub source_max_concurrency: u32,
    pub model_concurrency: HashMap<String, u32>,
    pub last_source_success_ms: Option<u64>,
    pub model_source_success_ms: HashMap<String, u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderSummary {
    pub id: String,
    pub name: String,
    pub model: String,
    pub total_keys: usize,
    pub kept_keys: usize,
    pub imported_keys: usize,
}

#[derive(Clone)]
pub struct SourceSnapshot {
    pub accounts: Vec<Account>,
    pub providers: Vec<ProviderSummary>,
    pub warnings: Vec<String>,
    pub read_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    #[default]
    Sticky,
    Random,
    RoundRobin,
    Pinned,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountOverride {
    pub enabled: Option<bool>,
    pub max_concurrency: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingPolicy {
    pub strategy: Strategy,
    pub pinned_key: Option<String>,
    pub max_concurrency_per_key: u32,
    pub shared_account_concurrency: Option<u32>,
    pub account_overrides: HashMap<String, AccountOverride>,
    pub affinity_ttl_seconds: u64,
    pub cooldown_base_ms: u64,
    pub cooldown_max_ms: u64,
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        Self {
            strategy: Strategy::Sticky,
            pinned_key: None,
            max_concurrency_per_key: 1,
            shared_account_concurrency: None,
            account_overrides: HashMap::new(),
            affinity_ttl_seconds: 86_400,
            cooldown_base_ms: 2_000,
            cooldown_max_ms: 30_000,
        }
    }
}
