//! Per-day token accounting and cost estimation.
//!
//! The monitor keeps only the most recent requests, so daily totals are kept
//! here in a separate append-only ledger. Prices live in their own file rather
//! than in the main configuration, because the configuration rejects unknown
//! fields and an older build must stay able to read it.
//!
//! Cost is an estimate from a price table the user controls. The upstream is a
//! relay with its own multipliers, and hosted tool fees never appear in the
//! usage object, so the panel must present these numbers as estimates.

use crate::{config, monitor::RequestRecord};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::Path,
    sync::{Mutex, RwLock},
};

const PRICING_FILE: &str = "pricing.json";
const LEDGER_FILE: &str = "usage.json";
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Keep a little over a year so month-over-month comparisons stay possible.
const MAX_DAYS: usize = 400;
/// A day holds at most this many conversations before the rest are merged.
const MAX_CONVERSATIONS_PER_DAY: usize = 500;
const OTHER_BUCKET: &str = "其他";
const MAX_MODELS: usize = 64;
/// Inputs at or below this size use the short-context prices.
pub const LONG_CONTEXT_THRESHOLD: u64 = 272_000;

static MUTATIONS: Mutex<()> = Mutex::new(());

/// Prices in US dollars per million tokens, as published by the model vendor.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierPrices {
    pub input: f64,
    pub cached_input: f64,
    pub cache_write_30m: f64,
    pub cache_write_12h: f64,
    pub output: f64,
}

impl TierPrices {
    fn valid(&self) -> bool {
        [
            self.input,
            self.cached_input,
            self.cache_write_30m,
            self.cache_write_12h,
            self.output,
        ]
        .iter()
        .all(|price| price.is_finite() && *price >= 0.0 && *price <= 10_000.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPrices {
    pub short: TierPrices,
    pub long: TierPrices,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    version: u32,
    /// Multiplies every estimate. A relay that bills at its own rate is
    /// expressed here instead of by rewriting every price.
    pub rate_multiplier: f64,
    pub display_currency: String,
    /// Display currency per US dollar.
    pub usd_to_display: f64,
    /// Minutes added to UTC when deciding which day a request belongs to.
    pub day_offset_minutes: i64,
    /// Service tiers bill at a multiple of the standard price.
    pub tier_multipliers: BTreeMap<String, f64>,
    pub models: BTreeMap<String, ModelPrices>,
    /// Where the defaults came from, shown next to the estimates.
    pub source_note: String,
}

impl Default for Pricing {
    fn default() -> Self {
        let model = |short: [f64; 5], long: [f64; 5]| ModelPrices {
            short: TierPrices {
                input: short[0],
                cached_input: short[1],
                cache_write_30m: short[2],
                cache_write_12h: short[3],
                output: short[4],
            },
            long: TierPrices {
                input: long[0],
                cached_input: long[1],
                cache_write_30m: long[2],
                cache_write_12h: long[3],
                output: long[4],
            },
        };
        Self {
            version: 1,
            rate_multiplier: 1.0,
            display_currency: "CNY".into(),
            usd_to_display: 7.2,
            day_offset_minutes: 480,
            tier_multipliers: [
                ("standard", 1.0),
                ("default", 1.0),
                ("auto", 1.0),
                ("flex", 0.5),
                ("batch", 0.5),
                ("priority", 2.0),
                ("fast", 2.0),
                ("ultrafast", 6.0),
                ("scale", 1.0),
            ]
            .into_iter()
            .map(|(name, factor)| (name.to_owned(), factor))
            .collect(),
            models: [
                (
                    "gpt-6-astra",
                    model(
                        [10.0, 1.0, 12.5, 25.0, 50.0],
                        [20.0, 2.0, 25.0, 50.0, 75.0],
                    ),
                ),
                (
                    "gpt-6.1-sol",
                    model([2.0, 0.1, 2.5, 5.0, 10.0], [4.0, 0.2, 5.0, 10.0, 15.0]),
                ),
            ]
            .into_iter()
            .map(|(name, prices)| (name.to_owned(), prices))
            .collect(),
            source_note: "默认值取自 2026-10-25 的公开标准档价格，单位为美元每百万 token。中转服务按自己的倍率结算，请按实际结算价调整。".into(),
        }
    }
}

impl Pricing {
    fn validate(&self) -> bool {
        self.version == 1
            && self.rate_multiplier.is_finite()
            && (0.0..=1000.0).contains(&self.rate_multiplier)
            && self.usd_to_display.is_finite()
            && (0.0..=10_000.0).contains(&self.usd_to_display)
            && (-24 * 60..=24 * 60).contains(&self.day_offset_minutes)
            && config::safe_label(&self.display_currency)
            && self.display_currency.len() <= 8
            && self.source_note.chars().count() <= 400
            && self.models.len() <= MAX_MODELS
            && self.tier_multipliers.len() <= MAX_MODELS
            && self.tier_multipliers.iter().all(|(tier, factor)| {
                config::safe_label(tier) && factor.is_finite() && (0.0..=100.0).contains(factor)
            })
            && self.models.iter().all(|(name, prices)| {
                config::safe_label(name) && prices.short.valid() && prices.long.valid()
            })
    }

    /// Prices for one request, or None when the model has no configured price.
    pub fn prices_for(&self, model: Option<&str>, input_tokens: u64) -> Option<TierPrices> {
        let entry = self.models.get(model?)?;
        Some(if input_tokens > LONG_CONTEXT_THRESHOLD {
            entry.long
        } else {
            entry.short
        })
    }

    pub fn tier_multiplier(&self, tier: Option<&str>) -> f64 {
        tier.map(str::to_ascii_lowercase)
            .and_then(|tier| self.tier_multipliers.get(&tier).copied())
            .unwrap_or(1.0)
    }
}

/// The three disjoint parts of the input plus the output, in tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tokens {
    pub input: u64,
    /// Input that was neither read from nor written to the cache.
    pub ordinary: u64,
    pub cached: u64,
    pub cache_write_30m: u64,
    pub cache_write_12h: u64,
    pub output: u64,
    /// Already inside `output`. Reported separately for display only.
    pub reasoning: u64,
}

impl Tokens {
    fn add(&mut self, other: &Tokens) {
        self.input += other.input;
        self.ordinary += other.ordinary;
        self.cached += other.cached;
        self.cache_write_30m += other.cache_write_30m;
        self.cache_write_12h += other.cache_write_12h;
        self.output += other.output;
        self.reasoning += other.reasoning;
    }
}

/// Split one record's input into its billable parts.
///
/// The upstream reports cached reads and cache writes as disjoint subsets of
/// `input_tokens`. A missing lifetime split is charged at the cheaper 30 minute
/// rate, which keeps the estimate a lower bound instead of inventing a number.
pub fn tokens_of(record: &RequestRecord) -> Option<Tokens> {
    let input = record.input_tokens?;
    let output = record.output_tokens.unwrap_or(0);
    let cached = record.cached_tokens.unwrap_or(0).min(input);
    let written = record.cache_write_tokens.unwrap_or(0).min(input - cached);
    let split_30m = record.cache_write_30m_tokens;
    let split_12h = record.cache_write_12h_tokens;
    let (cache_write_30m, cache_write_12h) = match (split_30m, split_12h) {
        (Some(short), Some(long)) if short + long <= written => (short, long),
        (Some(short), None) if short <= written => (short, written - short),
        (None, Some(long)) if long <= written => (written - long, long),
        _ => (written, 0),
    };
    Some(Tokens {
        input,
        ordinary: input
            .saturating_sub(cached)
            .saturating_sub(cache_write_30m)
            .saturating_sub(cache_write_12h),
        cached,
        cache_write_30m,
        cache_write_12h,
        output,
        reasoning: record.reasoning_tokens.unwrap_or(0).min(output),
    })
}

/// Cost in US dollars. Reasoning tokens are already inside `output` and must
/// not be charged again.
pub fn cost_usd(tokens: &Tokens, prices: &TierPrices, tier_multiplier: f64, rate: f64) -> f64 {
    let per_million = tokens.ordinary as f64 * prices.input
        + tokens.cached as f64 * prices.cached_input
        + tokens.cache_write_30m as f64 * prices.cache_write_30m
        + tokens.cache_write_12h as f64 * prices.cache_write_12h
        + tokens.output as f64 * prices.output;
    per_million / 1_000_000.0 * tier_multiplier * rate
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bucket {
    pub requests: u64,
    pub tokens: Tokens,
    pub cost_usd: f64,
    /// Requests whose model had no configured price.
    pub unpriced_requests: u64,
}

impl Bucket {
    fn add(&mut self, tokens: &Tokens, cost: Option<f64>) {
        self.requests += 1;
        self.tokens.add(tokens);
        match cost {
            Some(cost) => self.cost_usd += cost,
            None => self.unpriced_requests += 1,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Day {
    pub total: Bucket,
    /// Requests that finished without any usage report, so they are not counted.
    pub uncounted_requests: u64,
    pub by_model: BTreeMap<String, Bucket>,
    pub by_key: BTreeMap<String, Bucket>,
    pub by_effort: BTreeMap<String, Bucket>,
    pub by_conversation: BTreeMap<String, Bucket>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    version: u32,
    days: BTreeMap<String, Day>,
}

impl Default for Stored {
    fn default() -> Self {
        Self {
            version: 1,
            days: BTreeMap::new(),
        }
    }
}

/// Days are keyed by the local calendar date so "today" matches the user's day.
pub fn local_date(started_at_ms: u64, offset_minutes: i64) -> String {
    let seconds = (started_at_ms / 1000) as i64 + offset_minutes * 60;
    let days = seconds.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's days-from-civil inverse, valid across the Gregorian range.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (year + i64::from(month <= 2), month, day)
}

pub struct Ledger {
    state: RwLock<Stored>,
    pricing: RwLock<Pricing>,
    dirty: RwLock<bool>,
}

impl Ledger {
    /// Reads both files. A damaged ledger starts empty rather than failing the
    /// service, because accounting must never block routing.
    pub fn load(state_dir: &Path) -> Self {
        let pricing = read_pricing(state_dir).unwrap_or_default();
        let state = read_ledger(state_dir).unwrap_or_default();
        Self {
            state: RwLock::new(state),
            pricing: RwLock::new(pricing),
            dirty: RwLock::new(false),
        }
    }

    pub fn pricing(&self) -> Pricing {
        self.pricing
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_pricing(&self, state_dir: &Path, pricing: Pricing) -> Result<()> {
        if !pricing.validate() {
            bail!("价目表包含无效数值，未修改原文件");
        }
        let _lock = MUTATIONS.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = serde_json::to_vec_pretty(&pricing)
            .map_err(|_| anyhow::anyhow!("无法生成价目表文件"))?;
        config::private_write(&state_dir.join(PRICING_FILE), &bytes)
            .map_err(|_| anyhow::anyhow!("无法保存价目表，请检查目录权限与可用空间"))?;
        *self.pricing.write().unwrap_or_else(|e| e.into_inner()) = pricing;
        Ok(())
    }

    /// Adds one finished request. Records without a usage report are counted
    /// separately so the panel can show how much it could not account for.
    pub fn record(&self, record: &RequestRecord) {
        let pricing = self.pricing();
        let date = local_date(record.started_at_ms, pricing.day_offset_minutes);
        let model = record
            .response_model
            .as_deref()
            .or(record.effective_model.as_deref())
            .or(record.requested_model.as_deref());
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        let day = state.days.entry(date).or_default();
        let Some(tokens) = tokens_of(record) else {
            day.uncounted_requests += 1;
            drop(state);
            *self.dirty.write().unwrap_or_else(|e| e.into_inner()) = true;
            return;
        };
        let cost = pricing.prices_for(model, tokens.input).map(|prices| {
            cost_usd(
                &tokens,
                &prices,
                pricing.tier_multiplier(record.response_service_tier.as_deref()),
                pricing.rate_multiplier,
            )
        });
        day.total.add(&tokens, cost);
        let bucket = |map: &mut BTreeMap<String, Bucket>, key: Option<&str>, cap: usize| {
            let key = key.unwrap_or("未知").to_owned();
            let key = if map.len() >= cap && !map.contains_key(&key) {
                OTHER_BUCKET.to_owned()
            } else {
                key
            };
            map.entry(key).or_default().add(&tokens, cost);
        };
        bucket(&mut day.by_model, model, MAX_MODELS);
        bucket(&mut day.by_key, record.key_id.as_deref(), MAX_MODELS);
        bucket(
            &mut day.by_effort,
            record
                .response_effort
                .as_deref()
                .or(record.effective_effort.as_deref()),
            MAX_MODELS,
        );
        bucket(
            &mut day.by_conversation,
            record.conversation_id.as_deref(),
            MAX_CONVERSATIONS_PER_DAY,
        );
        while state.days.len() > MAX_DAYS {
            let Some(oldest) = state.days.keys().next().cloned() else {
                break;
            };
            state.days.remove(&oldest);
        }
        drop(state);
        *self.dirty.write().unwrap_or_else(|e| e.into_inner()) = true;
    }

    /// Writes the ledger when it changed. Called on a timer, because writing on
    /// every request would rewrite the whole file each time.
    pub fn flush(&self, state_dir: &Path) -> Result<()> {
        {
            let mut dirty = self.dirty.write().unwrap_or_else(|e| e.into_inner());
            if !*dirty {
                return Ok(());
            }
            *dirty = false;
        }
        let _lock = MUTATIONS.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = self.state.read().unwrap_or_else(|e| e.into_inner()).clone();
        let bytes =
            serde_json::to_vec(&snapshot).map_err(|_| anyhow::anyhow!("无法生成用量账本"))?;
        config::private_write(&state_dir.join(LEDGER_FILE), &bytes)
            .map_err(|_| anyhow::anyhow!("无法保存用量账本，请检查目录权限与可用空间"))
    }

    /// The most recent `days` calendar days, oldest first, plus today's totals.
    pub fn snapshot(&self, days: usize, today: &str) -> Value {
        let pricing = self.pricing();
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        let rows: Vec<(&String, &Day)> = state.days.iter().collect();
        let window: Vec<&(&String, &Day)> = rows.iter().rev().take(days.max(1)).rev().collect();
        let day_value = |date: &str, day: &Day| {
            json!({
                "date": date,
                "requests": day.total.requests,
                "uncounted_requests": day.uncounted_requests,
                "tokens": day.total.tokens,
                "cost_usd": day.total.cost_usd,
                "unpriced_requests": day.total.unpriced_requests,
            })
        };
        let group = |map: &BTreeMap<String, Bucket>| {
            let mut rows: Vec<(&String, &Bucket)> = map.iter().collect();
            rows.sort_by(|left, right| {
                right
                    .1
                    .cost_usd
                    .total_cmp(&left.1.cost_usd)
                    .then_with(|| right.1.tokens.input.cmp(&left.1.tokens.input))
            });
            rows.into_iter()
                .map(|(name, bucket)| {
                    json!({
                        "name": name,
                        "requests": bucket.requests,
                        "tokens": bucket.tokens,
                        "cost_usd": bucket.cost_usd,
                        "unpriced_requests": bucket.unpriced_requests,
                    })
                })
                .collect::<Vec<Value>>()
        };
        let empty = Day::default();
        let current = state.days.get(today).unwrap_or(&empty);
        let mut range = Bucket::default();
        let mut uncounted = 0;
        for (_, day) in window.iter() {
            range.requests += day.total.requests;
            range.tokens.add(&day.total.tokens);
            range.cost_usd += day.total.cost_usd;
            range.unpriced_requests += day.total.unpriced_requests;
            uncounted += day.uncounted_requests;
        }
        json!({
            "today": day_value(today, current),
            "range": {
                "days": window.len(),
                "requests": range.requests,
                "uncounted_requests": uncounted,
                "tokens": range.tokens,
                "cost_usd": range.cost_usd,
                "unpriced_requests": range.unpriced_requests,
            },
            "daily": window.iter().map(|(date, day)| day_value(date, day)).collect::<Vec<Value>>(),
            "by_model": group(&current.by_model),
            "by_key": group(&current.by_key),
            "by_effort": group(&current.by_effort),
            "by_conversation": group(&current.by_conversation),
            "pricing": pricing,
            "long_context_threshold": LONG_CONTEXT_THRESHOLD,
        })
    }
}

fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut file = File::open(path)?;
    let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if size > MAX_FILE_BYTES {
        bail!("文件过大");
    }
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_pricing(state_dir: &Path) -> Result<Pricing> {
    let bytes = read_bytes(&state_dir.join(PRICING_FILE))?;
    let pricing: Pricing =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("价目表格式无效"))?;
    if !pricing.validate() {
        bail!("价目表包含无效数值");
    }
    Ok(pricing)
}

fn read_ledger(state_dir: &Path) -> Result<Stored> {
    let bytes = read_bytes(&state_dir.join(LEDGER_FILE))?;
    let mut stored: Stored =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("用量账本格式无效"))?;
    if stored.version != 1 {
        bail!("用量账本版本不受支持");
    }
    while stored.days.len() > MAX_DAYS {
        let Some(oldest) = stored.days.keys().next().cloned() else {
            break;
        };
        stored.days.remove(&oldest);
    }
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(input: u64, cached: u64, written: u64, output: u64) -> RequestRecord {
        RequestRecord {
            started_at_ms: 1_792_900_000_000,
            response_model: Some("gpt-6.1-sol".into()),
            input_tokens: Some(input),
            output_tokens: Some(output),
            cached_tokens: Some(cached),
            cache_write_tokens: Some(written),
            ..RequestRecord::default()
        }
    }

    #[test]
    fn input_splits_into_three_disjoint_parts() {
        let mut row = record(12_000, 4_000, 7_000, 1_035);
        row.cache_write_30m_tokens = Some(3_000);
        row.cache_write_12h_tokens = Some(4_000);
        let tokens = tokens_of(&row).expect("usage present");
        assert_eq!(tokens.ordinary, 1_000);
        assert_eq!(tokens.cached, 4_000);
        assert_eq!(tokens.cache_write_30m, 3_000);
        assert_eq!(tokens.cache_write_12h, 4_000);
        assert_eq!(
            tokens.ordinary + tokens.cached + tokens.cache_write_30m + tokens.cache_write_12h,
            tokens.input
        );
    }

    #[test]
    fn a_missing_lifetime_split_is_charged_at_the_cheaper_rate() {
        let tokens = tokens_of(&record(100, 20, 50, 5)).expect("usage present");
        assert_eq!(tokens.cache_write_30m, 50);
        assert_eq!(tokens.cache_write_12h, 0);
        assert_eq!(tokens.ordinary, 30);
    }

    #[test]
    fn inconsistent_upstream_counts_never_produce_a_negative_part() {
        // Cached and written both claim the whole input.
        let tokens = tokens_of(&record(100, 100, 100, 0)).expect("usage present");
        assert_eq!(tokens.cached, 100);
        assert_eq!(tokens.cache_write_30m, 0);
        assert_eq!(tokens.ordinary, 0);
        assert_eq!(tokens.input, 100);
        let mut row = record(100, 10, 10, 10);
        row.cache_write_30m_tokens = Some(900);
        row.cache_write_12h_tokens = Some(900);
        let tokens = tokens_of(&row).expect("usage present");
        assert_eq!(tokens.cache_write_30m + tokens.cache_write_12h, 10);
    }

    #[test]
    fn a_request_without_usage_is_not_counted() {
        let mut row = record(1, 0, 0, 1);
        row.input_tokens = None;
        assert!(tokens_of(&row).is_none());
    }

    #[test]
    fn cost_follows_the_published_worked_example() {
        // (1000 + 4000*0.1 + 3000*1.25 + 4000*2.5) / 1e6 at an input price of 1.
        let tokens = Tokens {
            input: 12_000,
            ordinary: 1_000,
            cached: 4_000,
            cache_write_30m: 3_000,
            cache_write_12h: 4_000,
            output: 0,
            reasoning: 0,
        };
        let prices = TierPrices {
            input: 1.0,
            cached_input: 0.1,
            cache_write_30m: 1.25,
            cache_write_12h: 2.5,
            output: 0.0,
        };
        let cost = cost_usd(&tokens, &prices, 1.0, 1.0);
        assert!((cost - 0.015_15).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn reasoning_tokens_do_not_add_to_the_cost() {
        let prices = Pricing::default().models["gpt-6.1-sol"].short;
        let plain = Tokens {
            input: 0,
            ordinary: 0,
            cached: 0,
            cache_write_30m: 0,
            cache_write_12h: 0,
            output: 1_000,
            reasoning: 0,
        };
        let thinking = Tokens {
            reasoning: 900,
            ..plain
        };
        assert_eq!(
            cost_usd(&plain, &prices, 1.0, 1.0),
            cost_usd(&thinking, &prices, 1.0, 1.0)
        );
    }

    #[test]
    fn long_inputs_and_service_tiers_change_the_price() {
        let pricing = Pricing::default();
        let short = pricing
            .prices_for(Some("gpt-6-astra"), LONG_CONTEXT_THRESHOLD)
            .expect("priced");
        let long = pricing
            .prices_for(Some("gpt-6-astra"), LONG_CONTEXT_THRESHOLD + 1)
            .expect("priced");
        assert_eq!(short.input, 10.0);
        assert_eq!(long.input, 20.0);
        assert!(pricing.prices_for(Some("unknown-model"), 1).is_none());
        assert!(pricing.prices_for(None, 1).is_none());
        assert_eq!(pricing.tier_multiplier(Some("Fast")), 2.0);
        assert_eq!(pricing.tier_multiplier(Some("default")), 1.0);
        assert_eq!(pricing.tier_multiplier(None), 1.0);
        assert_eq!(pricing.tier_multiplier(Some("unlisted")), 1.0);
    }

    #[test]
    fn days_follow_the_configured_offset() {
        // 2026-10-25T16:40:00Z is already the 26th in UTC+8.
        let ms = 1_792_946_400_000;
        assert_eq!(local_date(ms, 0), "2026-10-25");
        assert_eq!(local_date(ms, 480), "2026-10-26");
        assert_eq!(local_date(ms, -480), "2026-10-25");
        assert_eq!(local_date(0, 0), "1970-01-01");
        // A leap day must not shift.
        assert_eq!(local_date(1_709_164_800_000, 0), "2024-02-29");
    }

    #[test]
    fn the_ledger_accumulates_by_day_and_by_group() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ledger = Ledger::load(dir.path());
        let mut first = record(1_000, 0, 0, 100);
        first.key_id = Some("key-a".into());
        first.conversation_id = Some("conv-1".into());
        first.response_effort = Some("high".into());
        ledger.record(&first);
        let mut second = record(2_000, 500, 0, 200);
        second.key_id = Some("key-b".into());
        second.conversation_id = Some("conv-1".into());
        second.response_effort = Some("xhigh".into());
        ledger.record(&second);
        let mut missing = record(1, 0, 0, 1);
        missing.input_tokens = None;
        ledger.record(&missing);

        let today = local_date(first.started_at_ms, 480);
        let value = ledger.snapshot(30, &today);
        assert_eq!(value["today"]["requests"], json!(2));
        assert_eq!(value["today"]["uncounted_requests"], json!(1));
        assert_eq!(value["today"]["tokens"]["input"], json!(3_000));
        assert_eq!(value["today"]["tokens"]["output"], json!(300));
        assert_eq!(value["today"]["tokens"]["cached"], json!(500));
        assert_eq!(value["by_key"].as_array().expect("rows").len(), 2);
        // One conversation carried both requests.
        assert_eq!(value["by_conversation"].as_array().expect("rows").len(), 1);
        assert_eq!(value["by_conversation"][0]["requests"], json!(2));
        assert_eq!(value["by_effort"].as_array().expect("rows").len(), 2);
        assert!(value["today"]["cost_usd"].as_f64().expect("cost") > 0.0);
    }

    #[test]
    fn an_unpriced_model_is_counted_without_a_cost() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ledger = Ledger::load(dir.path());
        let mut row = record(1_000, 0, 0, 10);
        row.response_model = Some("some-new-model".into());
        ledger.record(&row);
        let today = local_date(row.started_at_ms, 480);
        let value = ledger.snapshot(30, &today);
        assert_eq!(value["today"]["requests"], json!(1));
        assert_eq!(value["today"]["unpriced_requests"], json!(1));
        assert_eq!(value["today"]["cost_usd"], json!(0.0));
        assert_eq!(value["today"]["tokens"]["input"], json!(1_000));
    }

    #[test]
    fn the_ledger_survives_a_restart_and_rejects_a_damaged_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ledger = Ledger::load(dir.path());
        let row = record(1_000, 0, 0, 100);
        ledger.record(&row);
        ledger.flush(dir.path()).expect("flush");
        let today = local_date(row.started_at_ms, 480);

        let reopened = Ledger::load(dir.path());
        assert_eq!(reopened.snapshot(30, &today)["today"]["requests"], json!(1));

        std::fs::write(dir.path().join(LEDGER_FILE), b"{ not json").expect("write");
        let broken = Ledger::load(dir.path());
        assert_eq!(broken.snapshot(30, &today)["today"]["requests"], json!(0));
    }

    #[test]
    fn pricing_round_trips_and_invalid_values_are_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ledger = Ledger::load(dir.path());
        let pricing = Pricing {
            rate_multiplier: 1.4,
            usd_to_display: 7.1,
            ..Pricing::default()
        };
        ledger.set_pricing(dir.path(), pricing).expect("accepted");
        assert_eq!(Ledger::load(dir.path()).pricing().rate_multiplier, 1.4);

        let bad = Pricing {
            rate_multiplier: f64::NAN,
            ..Pricing::default()
        };
        assert!(ledger.set_pricing(dir.path(), bad).is_err());
        let mut negative = Pricing::default();
        negative
            .models
            .get_mut("gpt-6.1-sol")
            .expect("model")
            .short
            .input = -1.0;
        assert!(ledger.set_pricing(dir.path(), negative).is_err());
        // The rejected writes left the accepted value in place.
        assert_eq!(Ledger::load(dir.path()).pricing().rate_multiplier, 1.4);
    }

    #[test]
    fn only_the_most_recent_days_are_kept() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ledger = Ledger::load(dir.path());
        for day in 0..(MAX_DAYS + 10) {
            let mut row = record(10, 0, 0, 1);
            row.started_at_ms = 1_600_000_000_000 + (day as u64) * 86_400_000;
            ledger.record(&row);
        }
        let state = ledger.state.read().expect("state");
        assert_eq!(state.days.len(), MAX_DAYS);
    }
}
