//! Read-only adapter for BiWrite's configured keys and keep-alive candidates.
//!
//! `keptKeys` is a keep-alive/rejoin list, not a current health assertion.
//! `keyThrough` is historical and is persisted at most once an hour per key.
//! The caller must retain its previous snapshot when this module returns an error.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::types::{Account, ProviderSummary, SourceSnapshot, now_ms};
use crate::upstream::normalize_base_url;

const MAX_KEYS: usize = 64;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

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
    #[serde(default)]
    key_names: HashMap<String, String>,
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
    let file = File::open(path).map_err(|_| {
        anyhow::anyhow!("无法读取 BiWrite {label}；请确认目录存在且当前用户有读取权限")
    })?;
    let metadata = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("无法检查 BiWrite {label} 文件，已保留此前的号池快照"))?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        bail!("BiWrite {label} 文件类型或大小异常，已保留此前的号池快照");
    }
    // Settings and key pools can be rewritten by BiWrite during a sync. Bound
    // the read on the opened file as well as checking its initial metadata.
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("读取 BiWrite {label} 失败，已保留此前的号池快照"))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("BiWrite {label} 文件过大，已保留此前的号池快照");
    }
    // Do not attach serde's error: invalid values may contain credentials.
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("BiWrite {label} 格式暂不可用，已保留此前的号池快照"))
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

fn contains_secret(value: &str, secrets: &KeyFile) -> bool {
    crate::probe::redact_sensitive(value, std::iter::empty()) != value
        || secrets.keys.values().any(|part| {
            part.lines()
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .any(|key| value.contains(key))
        })
}

fn safe_identifier(value: &str, secrets: &KeyFile) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:/".contains(&c))
        && !contains_secret(value, secrets)
}

fn safe_nickname(value: &str, secrets: &KeyFile) -> Option<String> {
    // Reject controls before trimming so a newline never becomes a valid label.
    // Bidi formatting and invisible separators also obscure displayed content.
    if value.chars().any(|c| {
        c.is_control()
            || matches!(c, '\u{00ad}' | '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
    }) {
        return None;
    }
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= 256
        && value.chars().count() <= 80
        && !contains_secret(value, secrets))
    .then(|| value.to_owned())
}

/// Read only settings to choose the one configured public Responses upstream.
/// Multiple distinct endpoints require explicit selection in the local panel.
pub fn discover_upstream(dir: &Path) -> Result<String> {
    let settings: Settings = read_json(&dir.join("settings.json"), "settings.json")?;
    let endpoints: BTreeSet<String> = settings
        .providers
        .iter()
        .filter(|p| p.has_key && p.kind == "openai_compatible" && p.wire_api == "responses")
        .filter_map(|p| normalize_base_url(&p.base_url).ok())
        .collect();
    match endpoints.len() {
        0 => bail!(
            "BiWrite 未配置带有密钥的公网 HTTPS Responses 上游；请先配置来源或在面板填写上游 Base URL"
        ),
        1 => Ok(endpoints.into_iter().next().expect("exactly one endpoint")),
        _ => bail!("BiWrite 配置了多个 Responses 上游；请在面板明确选择一个上游 Base URL"),
    }
}

/// Automatically select the sole configured endpoint, then import its pool.
pub fn load_snapshot(dir: &Path) -> Result<SourceSnapshot> {
    let upstream = discover_upstream(dir)?;
    load_snapshot_for(dir, &upstream)
}

/// Import only explicitly kept Responses candidates for the selected endpoint.
/// Secrets stay in memory and are never copied to the adapter's output files.
pub fn load_snapshot_for(dir: &Path, upstream: &str) -> Result<SourceSnapshot> {
    load_snapshot_with_scope(dir, upstream, false)
}

/// Import the selected endpoint's declared key pools with an explicit scope.
/// All-key scope includes ignored, stopped and non-kept keys when requested,
/// but never treats admission or historical timestamps as current health.
pub fn load_snapshot_with_scope(
    dir: &Path,
    upstream: &str,
    import_all: bool,
) -> Result<SourceSnapshot> {
    let upstream = normalize_base_url(upstream)?;
    let settings: Settings = read_json(&dir.join("settings.json"), "settings.json")?;
    if !settings.keys_in_file {
        bail!(
            "BiWrite 当前使用系统钥匙串；此适配器只读取本地 keys.json。请在 BiWrite 中选择本地密钥文件模式后重试"
        );
    }
    let secrets: KeyFile = read_json(&dir.join("keys.json"), "keys.json")?;
    let mut accounts: BTreeMap<String, Account> = BTreeMap::new();
    let mut nicknames: BTreeMap<String, BTreeSet<(String, String, String)>> = BTreeMap::new();
    let mut providers = Vec::new();
    let mut warnings = vec![
        if import_all {
            "当前同步所选上游在 BiWrite 中配置的全部合法 Key，包含未保活、已忽略和已停用的 Key；导入不代表当前可用，keyThrough 仅为历史记录，实际健康状态由代理请求更新"
        } else {
            "当前仅同步 BiWrite 的 keptKeys 保活候选，并排除已忽略和已停用的 Key；保活候选不能证明此刻可用，实际健康状态由代理请求更新"
        }.into(),
        "并发限制仅覆盖本代理；BiWrite 自身的保活和其他客户端仍可能占用同一账号同一模型的额度"
            .into(),
    ];
    let empty_kept = HashMap::new();
    let kept = settings.kept_keys.as_ref().unwrap_or(&empty_kept);
    if settings.kept_keys.is_none() {
        warnings.push(
            if import_all {
                "BiWrite 尚未保存 keptKeys；当前按全部 Key 模式同步已配置密钥，保活候选数量仍记为 0"
            } else {
                "BiWrite 尚未保存 keptKeys；不会将历史 keyThrough 自动当作可用候选"
            }
            .into(),
        );
    }
    let mut skipped_metadata = false;
    let mut skipped_endpoint = false;
    let mut skipped_nickname = false;
    for provider in &settings.providers {
        if provider.kind != "openai_compatible" || provider.wire_api != "responses" {
            continue;
        }
        if normalize_base_url(&provider.base_url).ok().as_deref() != Some(upstream.as_str()) {
            skipped_endpoint = true;
            continue;
        }
        if !safe_identifier(&provider.id, &secrets) || !safe_identifier(&provider.model, &secrets) {
            skipped_metadata = true;
            continue;
        }
        let candidate_fingerprints = kept.get(&provider.id);
        let total_keys = provider
            .key_count
            .max(usize::from(provider.has_key))
            .min(MAX_KEYS);
        let mut imported = 0;
        for key in provider_keys(provider, &secrets) {
            let print = fingerprint(key);
            // A full digest, rather than BiWrite's 48-bit display fingerprint,
            // is the account identity across providers and models.
            let id = blake3::hash(key.as_bytes()).to_hex().to_string();
            if let Some(raw_name) = provider.key_names.get(&print) {
                if let Some(name) = safe_nickname(raw_name, &secrets) {
                    // Metadata can supply a label without admitting that model.
                    // Provider id, model and name order make conflicts stable
                    // across settings reordering and HashMap iteration.
                    nicknames.entry(id.clone()).or_default().insert((
                        provider.id.clone(),
                        provider.model.clone(),
                        name,
                    ));
                } else if !raw_name.is_empty() {
                    skipped_nickname = true;
                }
            }
            if !import_all
                && (!candidate_fingerprints.is_some_and(|set| set.contains(&print))
                    || provider.key_ignored.contains(&print)
                    || settings
                        .stopped_keys
                        .get(&provider.id)
                        .is_some_and(|set| set.contains_key(&print)))
            {
                continue;
            }
            let limit = provider.key_concurrency.unwrap_or(1).clamp(1, 16);
            let last_ok = provider.key_through.get(&print).copied();
            let account = accounts.entry(id.clone()).or_insert_with(|| Account {
                label: format!("账号 · {}", &id[..8]),
                id,
                secret: Arc::from(key),
                upstream: upstream.clone(),
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
        if import_all {
            if imported == 0 {
                warnings.push(format!(
                    "来源 {}（{}）已选择全部 Key 同步，但未读到当前声明范围内的有效密钥；请检查密钥配置、数量及分片是否完整",
                    provider.id, provider.model,
                ));
            }
        } else if candidate_fingerprints.is_none_or(BTreeSet::is_empty) {
            warnings.push(format!(
                "来源 {}（{}）没有已保存的 keptKeys 保活候选，配置密钥数为 {}；不会将 keyThrough 历史或已存密钥自动视为可用候选",
                provider.id, provider.model, total_keys,
            ));
        } else if imported == 0 {
            warnings.push(format!(
                "来源 {}（{}）的 {} 个保活候选均未导入；候选可能已停用、被忽略，或不在当前声明且格式有效的密钥池中",
                provider.id, provider.model, candidate_fingerprints.map_or(0, BTreeSet::len),
            ));
        }
        providers.push(ProviderSummary {
            id: provider.id.clone(),
            name: format!("Responses ({})", provider.model),
            model: provider.model.clone(),
            total_keys,
            kept_keys: candidate_fingerprints.map_or(0, BTreeSet::len),
            imported_keys: imported,
        });
    }
    if skipped_endpoint {
        warnings.push("部分 Responses 来源与所选上游地址不一致或地址不受支持，已跳过".into());
    }
    if skipped_metadata {
        warnings.push("部分来源标识或模型名称格式异常，已跳过且未记录其原始内容".into());
    }
    if skipped_nickname {
        warnings.push(
            "部分账号昵称为空白、过长或含有密钥及不安全字符，已忽略并使用其他有效昵称或账号摘要"
                .into(),
        );
    }
    for account in accounts.values_mut() {
        if let Some((_, _, name)) = nicknames.get(&account.id).and_then(|names| names.first()) {
            account.label.clone_from(name);
        }
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
    use std::fs;

    const UPSTREAM: &str = "https://api.example.com/v1";
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
        let dir = fixture(
            json!({"keysInFile": false, "providers": [provider("p-a", "gpt-6-astra", 1)]}),
            json!({"p-a": A}),
        );
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
        let dir = fixture(
            json!({"keysInFile": true, "providers": [provider("p-a", "gpt-6-astra", 1)]}),
            json!({}),
        );
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

    #[test]
    fn discovery_deduplicates_normalized_sources_and_reads_settings_only() {
        let mut root = provider("p-root", "gpt-6-astra", 1);
        root["baseUrl"] = json!("https://API.EXAMPLE.COM:443/");
        let mut slash = provider("p-slash", "gpt-6.1-sol", 1);
        slash["baseUrl"] = json!(format!("{UPSTREAM}/"));
        let mut no_key = provider("no-key", "gpt-6.1-sol", 1);
        no_key["hasKey"] = json!(false);
        no_key["baseUrl"] = json!("https://other.example.com/v1");
        let mut other_api = provider("other-api", "gpt-6.1-sol", 1);
        other_api["wireApi"] = json!("chat_completions");
        other_api["baseUrl"] = json!("https://other.example.com/v1");
        let dir = fixture(
            json!({"providers": [root, slash, no_key, other_api]}),
            json!({}),
        );
        fs::remove_file(dir.path().join("keys.json")).unwrap();
        assert_eq!(discover_upstream(dir.path()).unwrap(), UPSTREAM);
    }

    #[test]
    fn discovery_rejects_ambiguity_and_missing_usable_sources_without_echoing_urls() {
        let mut second = provider("p-second", "gpt-6.1-sol", 1);
        second["baseUrl"] = json!("https://other.example.com/v1");
        let dir = fixture(
            json!({"providers": [provider("p-first", "gpt-6-astra", 1), second]}),
            json!({}),
        );
        let error = discover_upstream(dir.path()).unwrap_err().to_string();
        assert!(error.contains("多个"));
        assert!(error.contains("面板"));
        assert!(!error.contains("example.com"));

        let mut invalid = provider("p-invalid", "gpt-6.1-sol", 1);
        invalid["baseUrl"] = json!(format!("https://{A}@api.example.com/v1"));
        let dir = fixture(json!({"providers": [invalid]}), json!({}));
        let error = discover_upstream(dir.path()).unwrap_err().to_string();
        assert!(error.contains("未配置"));
        assert!(!error.contains(A));
    }

    #[test]
    fn selected_endpoint_filters_models_and_never_reassigns_other_sources_keys() {
        let other_url = "https://other.example.com/v1";
        let mut second = provider("p-second", "gpt-6.1-sol", 1);
        second["baseUrl"] = json!(other_url);
        let settings = json!({
            "keysInFile": true,
            "providers": [provider("p-first", "gpt-6-astra", 1), second],
            "keptKeys": {"p-first": [fingerprint(A)], "p-second": [fingerprint(A)]}
        });
        let dir = fixture(settings.clone(), json!({"p-first": A, "p-second": A}));
        let first = load_snapshot_for(dir.path(), "https://API.EXAMPLE.COM/").unwrap();
        assert_eq!(first.accounts.len(), 1);
        assert_eq!(first.accounts[0].upstream, UPSTREAM);
        assert_eq!(first.accounts[0].models, ["gpt-6-astra"]);
        assert_eq!(first.accounts[0].provider_ids, ["p-first"]);
        assert_eq!(first.providers.len(), 1);
        let second = load_snapshot_for(dir.path(), other_url).unwrap();
        assert_eq!(second.accounts[0].upstream, other_url);
        assert_eq!(second.accounts[0].models, ["gpt-6.1-sol"]);

        let dir = fixture(settings, json!({"p-first": A}));
        let missing = load_snapshot_for(dir.path(), other_url).unwrap();
        assert!(missing.accounts.is_empty());
        assert!(missing.warnings.iter().any(|w| w.contains("均未导入")));
        let unrelated = load_snapshot_for(dir.path(), "https://unrelated.example.com").unwrap();
        assert!(unrelated.accounts.is_empty());
        assert!(unrelated.providers.is_empty());
    }

    #[test]
    fn nickname_matches_fingerprint_and_updates_on_next_snapshot() {
        let mut p = provider("p-a", "gpt-6-astra", 1);
        p["keyNames"] = json!({fingerprint(A): " qzkinhit ", fingerprint(B): "wrong-account"});
        let mut settings = json!({
            "keysInFile": true, "providers": [p], "keptKeys": {"p-a": [fingerprint(A)]}
        });
        let dir = fixture(settings.clone(), json!({"p-a": A}));
        let before = load_snapshot(dir.path()).unwrap();
        assert_eq!(before.accounts[0].label, "qzkinhit");
        settings["providers"][0]["keyNames"][fingerprint(A)] = json!("qzkinlss");
        fs::write(
            dir.path().join("settings.json"),
            serde_json::to_vec(&settings).unwrap(),
        )
        .unwrap();
        let after = load_snapshot(dir.path()).unwrap();
        assert_eq!(after.accounts[0].label, "qzkinlss");
        assert_eq!(before.accounts[0].id, after.accounts[0].id);
        assert_eq!(after.accounts[0].models, ["gpt-6-astra"]);
    }

    #[test]
    fn nickname_selection_is_stable_across_models_and_can_fall_back_to_safe_metadata() {
        let mut astra = provider("a-provider", "gpt-6-astra", 1);
        astra["keyNames"] = json!({fingerprint(A): "qzkinhit"});
        let mut sol = provider("s-provider", "gpt-6.1-sol", 1);
        sol["keyNames"] = json!({fingerprint(A): "qzkinlss"});
        for providers in [json!([astra, sol]), json!([sol, astra])] {
            let dir = fixture(
                json!({"keysInFile": true, "providers": providers, "keptKeys": {"s-provider": [fingerprint(A)]}}),
                json!({"a-provider": A, "s-provider": A}),
            );
            let result = load_snapshot(dir.path()).unwrap();
            assert_eq!(result.accounts[0].label, "qzkinhit");
            assert_eq!(result.accounts[0].models, ["gpt-6.1-sol"]);
        }
        astra["keyNames"] = json!({fingerprint(A): A});
        let dir = fixture(
            json!({"keysInFile": true, "providers": [astra, sol], "keptKeys": {"a-provider": [fingerprint(A)]}}),
            json!({"a-provider": A, "s-provider": A}),
        );
        let result = load_snapshot(dir.path()).unwrap();
        assert_eq!(result.accounts[0].label, "qzkinlss");
        assert_eq!(result.accounts[0].models, ["gpt-6-astra"]);
    }

    #[test]
    fn unsafe_nicknames_never_leak_any_stored_or_unrecognized_credentials() {
        let unrelated = "unprefixed-private-credential-12345";
        for name in [
            A.to_owned(),
            format!("copy {B}"),
            format!("copy {unrelated}"),
            "unknown sk-not-in-store".into(),
            "unknown SK-UPPERCASE".into(),
            "Bearer opaque-nickname-fixture".into(),
            "Authorization: Basic bmlja25hbWUtZml4dHVyZQ==".into(),
            r#"{"api_key":"opaque-nickname-fixture"}"#.into(),
            "https://user:password@example.com/private".into(),
            "line\nbreak".into(),
            "\ttrimmed-control".into(),
            "qzkin\u{202e}hit".into(),
            " ".into(),
            "a".repeat(81),
            "中".repeat(81),
        ] {
            let mut p = provider("p-a", "gpt-6-astra", 1);
            p["keyNames"] = json!({fingerprint(A): name});
            let dir = fixture(
                json!({"keysInFile": true, "providers": [p], "keptKeys": {"p-a": [fingerprint(A)]}}),
                json!({"p-a": A, "unrelated-stale#pool": format!("{B}\n{unrelated}")}),
            );
            let result = load_snapshot(dir.path()).unwrap();
            assert_eq!(
                result.accounts[0].label,
                format!("账号 · {}", &result.accounts[0].id[..8])
            );
            let public = format!(
                "{} {} {}",
                result.accounts[0].label,
                result.warnings.join(" "),
                serde_json::to_string(&result.providers).unwrap()
            );
            for secret in [A, B, unrelated, "sk-not-in-store", "SK-UPPERCASE"] {
                assert!(!public.contains(secret));
            }
        }
    }

    #[test]
    fn missing_sol_candidates_explains_configured_keys_without_admitting_history() {
        let mut astra = provider("p-astra", "gpt-6-astra", 1);
        astra["keyCount"] = json!(2);
        astra["keyParts"] = json!(1);
        let mut sol = provider("p-sol", "gpt-6.1-sol", 1);
        sol["keyCount"] = json!(11);
        sol["keyParts"] = json!(1);
        let pool = (0..10)
            .map(|n| format!("sk-fixture-sol-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let dir = fixture(
            json!({"keysInFile": true, "providers": [astra, sol], "keptKeys": {"p-astra": [fingerprint(A), fingerprint(B)]}}),
            json!({"p-astra": A, "p-astra#pool": B, "p-sol": A, "p-sol#pool": pool}),
        );
        let result = load_snapshot(dir.path()).unwrap();
        assert_eq!(result.accounts.len(), 2);
        assert!(result.accounts.iter().all(|a| a.models == ["gpt-6-astra"]));
        let sol = result.providers.iter().find(|p| p.id == "p-sol").unwrap();
        assert_eq!(
            (sol.total_keys, sol.kept_keys, sol.imported_keys),
            (11, 0, 0)
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("gpt-6.1-sol") && w.contains("11") && w.contains("没有已保存"))
        );
    }

    #[test]
    fn all_scope_imports_both_models_without_promoting_sol_history_to_health() {
        for historical_sol in [None, Some(84)] {
            let mut sol = provider("p-sol", "gpt-6.1-sol", 1);
            sol["keyThrough"] = historical_sol
                .map(|timestamp| json!({fingerprint(A): timestamp}))
                .unwrap_or_else(|| json!({}));
            let dir = fixture(
                json!({
                    "keysInFile": true,
                    "providers": [provider("p-astra", "gpt-6-astra", 3), sol],
                    "keptKeys": {"p-astra": [fingerprint(A)]}
                }),
                json!({"p-astra": A, "p-sol": A}),
            );
            let result = load_snapshot_with_scope(dir.path(), UPSTREAM, true).unwrap();
            assert_eq!(result.accounts.len(), 1);
            let account = &result.accounts[0];
            assert_eq!(account.models, ["gpt-6-astra", "gpt-6.1-sol"]);
            assert_eq!(account.model_concurrency["gpt-6-astra"], 3);
            assert_eq!(account.model_concurrency["gpt-6.1-sol"], 1);
            assert_eq!(account.model_source_success_ms["gpt-6-astra"], 42);
            assert_eq!(
                account.model_source_success_ms.get("gpt-6.1-sol").copied(),
                historical_sol
            );
            let sol_summary = result.providers.iter().find(|p| p.id == "p-sol").unwrap();
            assert_eq!(
                (
                    sol_summary.total_keys,
                    sol_summary.kept_keys,
                    sol_summary.imported_keys
                ),
                (1, 0, 1)
            );
            assert!(
                !result
                    .warnings
                    .iter()
                    .any(|w| w.contains("没有已保存") || w.contains("均未导入"))
            );
            assert!(result.warnings.iter().any(|w| w.contains("全部合法 Key")));

            let scheduler = crate::scheduler::Scheduler::new();
            scheduler.sync_accounts(result.accounts);
            let snapshot = scheduler.snapshot(&crate::types::RoutingPolicy::default());
            assert_eq!(snapshot[0].successes, 0);
            for model in &snapshot[0].model_health {
                assert_eq!(model.status, "untested");
                assert_eq!(model.successes, 0);
                assert_eq!(model.last_ok_ms, None);
            }
            let sol_health = snapshot[0]
                .model_health
                .iter()
                .find(|m| m.model == "gpt-6.1-sol")
                .unwrap();
            assert_eq!(sol_health.source_last_ok_ms, historical_sol);

            let kept_only = load_snapshot_with_scope(dir.path(), UPSTREAM, false).unwrap();
            assert_eq!(kept_only.accounts[0].models, ["gpt-6-astra"]);
            assert_eq!(
                kept_only
                    .providers
                    .iter()
                    .find(|p| p.id == "p-sol")
                    .unwrap()
                    .imported_keys,
                0
            );
        }
    }

    #[test]
    fn all_scope_includes_ignored_stopped_and_non_kept_keys_but_kept_scope_still_filters() {
        let mut p = provider("p-all", "gpt-6-astra", 2);
        p["keyCount"] = json!(3);
        p["keyParts"] = json!(1);
        p["keyIgnored"] = json!([fingerprint(A)]);
        p["keyNames"] =
            json!({fingerprint(A): "qzkinhit", fingerprint(B): B, fingerprint(C): "qzkinlss"});
        let dir = fixture(
            json!({
                "keysInFile": true, "providers": [p],
                "keptKeys": {"p-all": [fingerprint(A), fingerprint(B)]},
                "stoppedKeys": {"p-all": {fingerprint(B): {"reason":"stopped"}}}
            }),
            json!({"p-all": A, "p-all#pool": format!("{B}\n{C}")}),
        );
        let result = load_snapshot_with_scope(dir.path(), UPSTREAM, true).unwrap();
        assert_eq!(result.accounts.len(), 3);
        assert_eq!(result.providers[0].kept_keys, 2);
        assert_eq!(result.providers[0].imported_keys, 3);
        let labels: BTreeMap<_, _> = result
            .accounts
            .iter()
            .map(|a| (a.secret.as_ref(), a.label.as_str()))
            .collect();
        assert_eq!(labels[A], "qzkinhit");
        assert_eq!(labels[C], "qzkinlss");
        assert!(labels[B].starts_with("账号 · "));
        let filtered = load_snapshot_with_scope(dir.path(), UPSTREAM, false).unwrap();
        assert!(filtered.accounts.is_empty());
        assert_eq!(filtered.providers[0].kept_keys, 2);
        assert_eq!(filtered.providers[0].imported_keys, 0);
        assert!(filtered.warnings.iter().any(|w| w.contains("均未导入")));
        assert!(
            load_snapshot_for(dir.path(), UPSTREAM)
                .unwrap()
                .accounts
                .is_empty()
        );
    }

    #[test]
    fn all_scope_continues_without_kept_metadata_but_never_crosses_the_selected_endpoint() {
        let mut other = provider("p-other", "gpt-6.1-sol", 1);
        other["baseUrl"] = json!("https://other.example.com/v1");
        let dir = fixture(
            json!({"keysInFile": true, "providers": [provider("p-local", "gpt-6-astra", 1), other]}),
            json!({"p-local": A, "p-other": A}),
        );
        let result = load_snapshot_with_scope(dir.path(), UPSTREAM, true).unwrap();
        assert_eq!(result.accounts.len(), 1);
        assert_eq!(result.accounts[0].models, ["gpt-6-astra"]);
        assert_eq!(result.accounts[0].upstream, UPSTREAM);
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.providers[0].kept_keys, 0);
        assert_eq!(result.providers[0].imported_keys, 1);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("尚未保存 keptKeys") && w.contains("全部 Key 模式"))
        );
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.contains("没有已保存") || w.contains("均未导入"))
        );
    }

    #[test]
    fn all_scope_respects_shrinking_count_parts_and_removed_provider_keys() {
        let mut p = provider("p-current", "gpt-6-astra", 1);
        p["keyCount"] = json!(3);
        p["keyParts"] = json!(2);
        let mut settings = json!({"keysInFile": true, "providers": [p]});
        let dir = fixture(
            settings.clone(),
            json!({"p-current": A, "p-current#pool": B, "p-current#pool2": C, "p-removed": C}),
        );
        let original_keys = fs::read(dir.path().join("keys.json")).unwrap();
        let first = load_snapshot_with_scope(dir.path(), UPSTREAM, true).unwrap();
        assert_eq!(first.accounts.len(), 3);
        let scheduler = crate::scheduler::Scheduler::new();
        scheduler.sync_accounts(first.accounts);

        for (count, parts, has_key, expected) in [
            (2, 2, true, 2),
            (3, 1, true, 2),
            (1, 2, true, 1),
            (3, 2, false, 0),
        ] {
            settings["providers"][0]["keyCount"] = json!(count);
            settings["providers"][0]["keyParts"] = json!(parts);
            settings["providers"][0]["hasKey"] = json!(has_key);
            fs::write(
                dir.path().join("settings.json"),
                serde_json::to_vec(&settings).unwrap(),
            )
            .unwrap();
            let result = load_snapshot_with_scope(dir.path(), UPSTREAM, true).unwrap();
            assert_eq!(result.accounts.len(), expected);
            assert_eq!(result.providers[0].imported_keys, expected);
            assert!(result.accounts.iter().all(|a| a.secret.as_ref() != C));
            scheduler.sync_accounts(result.accounts);
            assert_eq!(
                scheduler
                    .snapshot(&crate::types::RoutingPolicy::default())
                    .len(),
                expected
            );
        }
        assert_eq!(
            fs::read(dir.path().join("keys.json")).unwrap(),
            original_keys
        );
    }

    #[test]
    fn oversized_files_are_rejected_without_copying_raw_content_to_errors() {
        for label in ["settings.json", "keys.json"] {
            let dir = fixture(json!({"keysInFile": true}), json!({}));
            let file = File::options()
                .write(true)
                .open(dir.path().join(label))
                .unwrap();
            file.set_len(MAX_FILE_BYTES + 1).unwrap();
            let error = load_snapshot_with_scope(dir.path(), UPSTREAM, true)
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("文件类型或大小异常"));
            assert!(error.contains("保留此前"));
            assert!(!error.contains(A));
        }
    }
}
