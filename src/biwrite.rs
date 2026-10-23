//! Read-only adapter for BiWrite's persisted keep-alive candidates.
//!
//! `keptKeys` is a keep-alive/rejoin list, not a current health assertion.
//! `keyThrough` is historical and is persisted at most once an hour per key.
//! The caller must retain its previous snapshot when this module returns an error.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::types::{Account, ProviderSummary, SourceSnapshot, now_ms};

const MAX_KEYS: usize = 64;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const UPSTREAM: &str = "https://anyrouter.top/v1";

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    #[serde(default)]
    providers: Vec<Provider>,
    #[serde(default)]
    keys_in_file: bool,
    #[serde(default)]
    kept_keys: Option<HashMap<String, BTreeSet<String>>>,
    #[serde(default)]
    stopped_keys: HashMap<String, HashMap<String, serde_json::Value>>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Provider {
    id: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    wire_api: String,
    #[serde(default)]
    has_key: bool,
    #[serde(default)]
    key_count: usize,
    #[serde(default)]
    key_parts: usize,
    #[serde(default)]
    key_concurrency: Option<u32>,
    #[serde(default)]
    key_ignored: BTreeSet<String>,
    #[serde(default)]
    key_through: HashMap<String, u64>,
}

#[derive(Default, Deserialize)]
struct KeyFile {
    #[serde(default)]
    keys: HashMap<String, String>,
}

/// Default config location used by Tauri for `app.biwrite.desktop`.
/// This only constructs a path. It never creates directories or reads the keychain.
pub fn discover_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join("Library/Application Support/app.biwrite.desktop")
    }
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("app.biwrite.desktop")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
            })
            .join("app.biwrite.desktop")
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, label: &str) -> Result<T> {
    let metadata = fs::metadata(path).map_err(|_| {
        anyhow::anyhow!("无法读取 BiWrite {label}；请确认目录存在且当前用户有读取权限")
    })?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        bail!("BiWrite {label} 文件类型或大小异常，已保留此前的号池快照");
    }
    let bytes = fs::read(path)
        .map_err(|_| anyhow::anyhow!("读取 BiWrite {label} 失败，已保留此前的号池快照"))?;
    // Do not attach serde's error: invalid values may contain credentials.
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("BiWrite {label} 格式暂不可用，已保留此前的号池快照"))
}

fn allowed_upstream(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value.trim()) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some("anyrouter.top")
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && url.query().is_none()
        && url.fragment().is_none()
        && matches!(url.path(), "" | "/" | "/v1" | "/v1/")
}

fn fingerprint(key: &str) -> String {
    blake3::hash(key.trim().as_bytes()).to_hex()[..12].to_owned()
}

fn acceptable_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 1024 && key.bytes().all(|b| b.is_ascii_graphic())
}

/// Match BiWrite's vouched key count and sequential pool parts. Stale stored
/// accounts must not become eligible after the user shrinks or removes a pool.
fn provider_keys<'a>(provider: &Provider, store: &'a KeyFile) -> Vec<&'a str> {
    if !provider.has_key {
        return Vec::new();
    }
    let count = provider.key_count.clamp(1, MAX_KEYS);
    let Some(first) = store.keys.get(&provider.id).map(|s| s.trim()) else {
        return Vec::new();
    };
    if !acceptable_key(first) {
        return Vec::new();
    }
    let mut keys = vec![first];
    let parts = if count == 1 {
        0
    } else if provider.key_parts == 0 {
        MAX_KEYS
    } else {
        provider.key_parts.min(MAX_KEYS)
    };
    for index in 0..parts {
        if keys.len() >= count {
            break;
        }
        let account = if index == 0 {
            format!("{}#pool", provider.id)
        } else {
            format!("{}#pool{}", provider.id, index + 1)
        };
        let Some(part) = store.keys.get(&account) else {
            break;
        };
        for key in part.lines().map(str::trim) {
            if acceptable_key(key) && !keys.contains(&key) {
                keys.push(key);
            }
            if keys.len() >= count {
                break;
            }
        }
    }
    keys
}

fn safe_identifier(value: &str, secrets: &KeyFile) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:/".contains(&c))
        && !secrets.keys.values().any(|part| {
            part.lines()
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .any(|key| value.contains(key))
        })
}

/// Import only explicitly kept Responses candidates for the supported origin.
/// Secrets stay in memory and are never copied to the adapter's output files.
pub fn load_snapshot(dir: &Path) -> Result<SourceSnapshot> {
    let settings: Settings = read_json(&dir.join("settings.json"), "settings.json")?;
    if !settings.keys_in_file {
        bail!(
            "BiWrite 当前使用系统钥匙串；此适配器只读取本地 keys.json。请在 BiWrite 中选择本地密钥文件模式后重试"
        );
    }
    let secrets: KeyFile = read_json(&dir.join("keys.json"), "keys.json")?;
    let mut accounts: BTreeMap<String, Account> = BTreeMap::new();
    let mut providers = Vec::new();
    let mut warnings = vec![
        "BiWrite 文件中的 keptKeys 表示保活候选，不能证明此刻可用；实际健康状态由代理请求更新"
            .into(),
        "并发限制仅覆盖本代理；BiWrite 自身的保活和其他客户端仍可能占用同一账号同一模型的额度"
            .into(),
    ];
    let empty_kept = HashMap::new();
    let kept = settings.kept_keys.as_ref().unwrap_or(&empty_kept);
    if settings.kept_keys.is_none() {
        warnings.push("BiWrite 尚未保存 keptKeys；不会将历史 keyThrough 自动当作可用候选".into());
    }
    let mut skipped_metadata = false;
    let mut skipped_endpoint = false;
    for provider in &settings.providers {
        if provider.kind != "openai_compatible" || provider.wire_api != "responses" {
            continue;
        }
        if !allowed_upstream(&provider.base_url) {
            skipped_endpoint = true;
            continue;
        }
        if !safe_identifier(&provider.id, &secrets) || !safe_identifier(&provider.model, &secrets) {
            skipped_metadata = true;
            continue;
        }
        let candidate_fingerprints = kept.get(&provider.id);
        let mut imported = 0;
        for key in provider_keys(provider, &secrets) {
            let print = fingerprint(key);
            if !candidate_fingerprints.is_some_and(|set| set.contains(&print))
                || provider.key_ignored.contains(&print)
                || settings
                    .stopped_keys
                    .get(&provider.id)
                    .is_some_and(|set| set.contains_key(&print))
            {
                continue;
            }
            // A full digest, rather than BiWrite's 48-bit display fingerprint,
            // is the account identity across providers and models.
            let id = blake3::hash(key.as_bytes()).to_hex().to_string();
            let limit = provider.key_concurrency.unwrap_or(1).clamp(1, 16);
            let last_ok = provider.key_through.get(&print).copied();
            let account = accounts.entry(id.clone()).or_insert_with(|| Account {
                label: format!("账号 · {}", &id[..8]),
                id,
                secret: Arc::from(key),
                upstream: UPSTREAM.into(),
                models: Vec::new(),
                provider_ids: Vec::new(),
                source_max_concurrency: limit,
                model_concurrency: HashMap::new(),
                last_source_success_ms: last_ok,
                model_source_success_ms: HashMap::new(),
            });
            // Limits and successful admission belong to key + model. The
            // legacy aggregate remains only as a fallback for older callers.
            account.source_max_concurrency = account.source_max_concurrency.min(limit);
            account.last_source_success_ms = account.last_source_success_ms.max(last_ok);
            account
                .model_concurrency
                .entry(provider.model.clone())
                .and_modify(|existing| *existing = (*existing).min(limit))
                .or_insert(limit);
            if let Some(last_ok) = last_ok {
                account
                    .model_source_success_ms
                    .entry(provider.model.clone())
                    .and_modify(|existing| *existing = (*existing).max(last_ok))
                    .or_insert(last_ok);
            }
            if !account.models.contains(&provider.model) {
                account.models.push(provider.model.clone());
            }
            if !account.provider_ids.contains(&provider.id) {
                account.provider_ids.push(provider.id.clone());
            }
            imported += 1;
        }
        providers.push(ProviderSummary {
            id: provider.id.clone(),
            // Never trust provider.name or keyNames for a public log label.
            name: format!("AnyRouter ({})", provider.model),
            model: provider.model.clone(),
            total_keys: provider
                .key_count
                .max(usize::from(provider.has_key))
                .min(MAX_KEYS),
            kept_keys: candidate_fingerprints.map_or(0, BTreeSet::len),
            imported_keys: imported,
        });
    }
    if skipped_endpoint {
        warnings.push("部分 Responses 来源不属于允许的 AnyRouter HTTPS 地址，已跳过".into());
    }
    if skipped_metadata {
        warnings.push("部分来源标识或模型名称格式异常，已跳过且未记录其原始内容".into());
    }
    for account in accounts.values_mut() {
        account.models.sort();
        account.provider_ids.sort();
    }
    Ok(SourceSnapshot {
        accounts: accounts.into_values().collect(),
        providers,
        warnings,
        read_at_ms: now_ms(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const A: &str = "sk-fixture-alpha-123456789";
    const B: &str = "sk-fixture-beta-987654321";
    const C: &str = "sk-fixture-gamma-246813579";

    fn provider(id: &str, model: &str, limit: u32) -> Value {
        json!({
            "id": id, "name": A, "kind": "openai_compatible",
            "baseUrl": UPSTREAM, "model": model, "wireApi": "responses",
            "hasKey": true, "keyCount": 1, "keyConcurrency": limit,
            "keyNames": {fingerprint(A): A}, "keyThrough": {fingerprint(A): 42}
        })
    }

    fn fixture(settings: Value, keys: Value) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("settings.json"),
            serde_json::to_vec(&settings).unwrap(),
        )
        .unwrap();
        fs::write(
            dir.path().join("keys.json"),
            serde_json::to_vec(&json!({"keys": keys})).unwrap(),
        )
        .unwrap();
        dir
    }

    #[test]
    fn merges_credentials_but_retains_independent_model_limits_and_success_times() {
        let mut sol = provider("p-s", "gpt-6.1-sol", 1);
        sol["keyThrough"] = json!({fingerprint(A): 84});
        let dir = fixture(
            json!({
                "keysInFile": true,
                "providers": [provider("p-a", "gpt-6-astra", 3), sol],
                "keptKeys": {"p-a": [fingerprint(A)], "p-s": [fingerprint(A)]}
            }),
            json!({"p-a": A, "p-s": A}),
        );
        let result = load_snapshot(dir.path()).unwrap();
        assert_eq!(result.accounts.len(), 1);
        let account = &result.accounts[0];
        assert_eq!(account.models, ["gpt-6-astra", "gpt-6.1-sol"]);
        assert_eq!(account.source_max_concurrency, 1);
        assert_eq!(account.model_concurrency["gpt-6-astra"], 3);
        assert_eq!(account.model_concurrency["gpt-6.1-sol"], 1);
        assert_eq!(account.model_source_success_ms["gpt-6-astra"], 42);
        assert_eq!(account.model_source_success_ms["gpt-6.1-sol"], 84);
        assert_eq!(account.id.len(), 64);
        assert_eq!(account.last_source_success_ms, Some(84));
        assert!(!account.label.contains(A));
        assert!(
            !serde_json::to_string(&result.providers)
                .unwrap()
                .contains(A)
        );
    }

    #[test]
    fn sol_admission_does_not_admit_same_key_to_astra() {
        let dir = fixture(
            json!({
                "keysInFile": true,
                "providers": [provider("p-a", "gpt-6-astra", 3), provider("p-s", "gpt-6.1-sol", 1)],
                "keptKeys": {"p-s": [fingerprint(A)]}
            }),
            json!({"p-a": A, "p-s": A}),
        );
        let result = load_snapshot(dir.path()).unwrap();
        assert_eq!(result.accounts.len(), 1);
        let account = &result.accounts[0];
        assert_eq!(account.models, ["gpt-6.1-sol"]);
        assert_eq!(account.provider_ids, ["p-s"]);
        assert!(!account.model_concurrency.contains_key("gpt-6-astra"));
        assert!(!account.model_source_success_ms.contains_key("gpt-6-astra"));
    }

    #[test]
    fn astra_stopped_or_ignored_does_not_remove_sol_for_same_key() {
        for ignored in [false, true] {
            let mut astra = provider("p-a", "gpt-6-astra", 3);
            if ignored {
                astra["keyIgnored"] = json!([fingerprint(A)]);
            }
            let dir = fixture(
                json!({
                    "keysInFile": true,
                    "providers": [astra, provider("p-s", "gpt-6.1-sol", 1)],
                    "keptKeys": {"p-a": [fingerprint(A)], "p-s": [fingerprint(A)]},
                    "stoppedKeys": if ignored { json!({}) } else { json!({"p-a": {fingerprint(A): {"reason": "stopped"}}}) }
                }),
                json!({"p-a": A, "p-s": A}),
            );
            let result = load_snapshot(dir.path()).unwrap();
            assert_eq!(result.accounts.len(), 1);
            assert_eq!(result.accounts[0].models, ["gpt-6.1-sol"]);
            assert_eq!(result.accounts[0].model_concurrency.len(), 1);
        }
    }

    #[test]
    fn duplicate_providers_for_one_key_model_use_minimum_limit_and_latest_success() {
        let mut second = provider("p-a2", "gpt-6-astra", 2);
        second["keyThrough"] = json!({fingerprint(A): 84});
        let dir = fixture(
            json!({
                "keysInFile": true,
                "providers": [provider("p-a", "gpt-6-astra", 3), second, provider("p-s", "gpt-6.1-sol", 1)],
                "keptKeys": {"p-a": [fingerprint(A)], "p-a2": [fingerprint(A)], "p-s": [fingerprint(A)]}
            }),
            json!({"p-a": A, "p-a2": A, "p-s": A}),
        );
        let result = load_snapshot(dir.path()).unwrap();
        let account = &result.accounts[0];
        assert_eq!(account.model_concurrency["gpt-6-astra"], 2);
        assert_eq!(account.model_concurrency["gpt-6.1-sol"], 1);
        assert_eq!(account.model_source_success_ms["gpt-6-astra"], 84);
        assert_eq!(account.model_source_success_ms["gpt-6.1-sol"], 42);
    }

    #[test]
    fn excludes_ignored_stopped_and_historical_only_keys() {
        let mut p = provider("p-a", "gpt-6-astra", 2);
        p["keyCount"] = json!(3);
        p["keyParts"] = json!(1);
        p["keyIgnored"] = json!([fingerprint(A)]);
        let dir = fixture(
            json!({
                "keysInFile": true, "providers": [p],
                "keptKeys": {"p-a": [fingerprint(A), fingerprint(B)]},
                "stoppedKeys": {"p-a": {fingerprint(B): {"reason": "stopped"}}}
            }),
            json!({"p-a": A, "p-a#pool": format!("{B}\n{C}")}),
        );
        let result = load_snapshot(dir.path()).unwrap();
        assert!(result.accounts.is_empty());
        assert_eq!(result.providers[0].kept_keys, 2);
    }

    #[test]
    fn follows_count_parts_and_missing_first_rules() {
        let mut p: Provider = serde_json::from_value(provider("p-a", "gpt-6-astra", 2)).unwrap();
        p.key_count = 2;
        let store = KeyFile {
            keys: HashMap::from([
                ("p-a".into(), A.into()),
                ("p-a#pool".into(), format!("{A}\n{B}\n{C}")),
                ("p-a#pool2".into(), C.into()),
            ]),
        };
        assert_eq!(provider_keys(&p, &store), [A, B]);
        p.key_count = 3;
        p.key_parts = 1;
        assert_eq!(provider_keys(&p, &store), [A, B, C]);
        p.has_key = false;
        assert!(provider_keys(&p, &store).is_empty());
        p.has_key = true;
        p.id = "missing".into();
        assert!(provider_keys(&p, &store).is_empty());
    }

    #[test]
    fn does_not_import_a_second_part_beyond_declared_boundary() {
        let mut p: Provider = serde_json::from_value(provider("p-a", "gpt-6-astra", 1)).unwrap();
        p.key_count = 3;
        p.key_parts = 1;
        let store = KeyFile {
            keys: HashMap::from([
                ("p-a".into(), A.into()),
                ("p-a#pool".into(), B.into()),
                ("p-a#pool2".into(), C.into()),
            ]),
        };
        assert_eq!(provider_keys(&p, &store), [A, B]);
    }

    #[test]
    fn restricts_upstream_origin_and_rejects_url_credentials() {
        for url in [
            "https://anyrouter.top",
            "https://anyrouter.top/v1",
            "https://anyrouter.top/v1/",
            "https://anyrouter.top:443/v1",
        ] {
            assert!(allowed_upstream(url));
        }
        for url in [
            "http://anyrouter.top/v1",
            "https://anyrouter.top.evil.test/v1",
            "https://x@anyrouter.top/v1",
            "https://anyrouter.top:444/v1",
            "https://anyrouter.top/v1?q=1",
            "https://anyrouter.top/v1#x",
            "https://anyrouter.top/other",
            "not-a-url",
        ] {
            assert!(!allowed_upstream(url));
        }
    }

    #[test]
    fn absent_kept_keys_does_not_promote_history_to_health() {
        let dir = fixture(
            json!({"keysInFile": true, "providers": [provider("p-a", "gpt-6-astra", 1)]}),
            json!({"p-a": A}),
        );
        let result = load_snapshot(dir.path()).unwrap();
        assert!(result.accounts.is_empty());
        assert!(result.warnings.iter().any(|w| w.contains("尚未保存")));
    }

    #[test]
    fn unsupported_keychain_mode_is_read_only_and_explained() {
        let dir = fixture(json!({"keysInFile": false}), json!({"p-a": A}));
        let before = fs::read(dir.path().join("keys.json")).unwrap();
        let error = load_snapshot(dir.path()).err().unwrap().to_string();
        assert!(error.contains("系统钥匙串"));
        assert!(!error.contains(A));
        assert_eq!(before, fs::read(dir.path().join("keys.json")).unwrap());
    }

    #[test]
    fn parsing_errors_never_echo_secret_values() {
        let dir = fixture(
            json!({"keysInFile": true, "providers": A}),
            json!({"p-a": A}),
        );
        let error = load_snapshot(dir.path()).err().unwrap().to_string();
        assert!(!error.contains(A));
        assert!(error.contains("格式暂不可用"));
    }

    #[test]
    fn fingerprint_matches_biwrite_blake3_truncation() {
        assert_eq!(
            fingerprint("  example  "),
            blake3::hash(b"example").to_hex()[..12]
        );
    }

    #[test]
    fn missing_keys_file_is_an_error_not_a_successful_empty_snapshot() {
        let dir = fixture(json!({"keysInFile": true}), json!({}));
        fs::remove_file(dir.path().join("keys.json")).unwrap();
        assert!(load_snapshot(dir.path()).is_err());
    }

    #[test]
    fn secret_in_identifier_is_rejected_without_echoing_it() {
        let dir = fixture(
            json!({"keysInFile": true, "providers": [provider("p-a", A, 1)], "keptKeys": {"p-a": [fingerprint(A)]}}),
            json!({"p-a": A}),
        );
        let result = load_snapshot(dir.path()).unwrap();
        assert!(result.accounts.is_empty());
        assert!(!result.warnings.join(" ").contains(A));
    }
}
