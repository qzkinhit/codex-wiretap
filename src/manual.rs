//! User-maintained account source. Credentials are serialized only to the
//! private local store; public views and errors never contain them.

use crate::{
    config,
    types::{Account, ProviderSummary, SourceSnapshot, now_ms},
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::File,
    io::{ErrorKind, Read},
    path::Path,
    sync::{Arc, Mutex},
};

const FILE_NAME: &str = "manual-keys.json";
const UPSTREAM: &str = "https://anyrouter.top/v1";
const MAX_ACCOUNTS: usize = 256;
const MAX_MODELS: usize = 20;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
// Serialize read/modify/write operations in this service process. Readers see
// either complete version because config::private_write replaces atomically.
static MUTATIONS: Mutex<()> = Mutex::new(());

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    version: u32,
    accounts: Vec<StoredAccount>,
}

impl Default for Stored {
    fn default() -> Self {
        Self {
            version: 1,
            accounts: Vec::new(),
        }
    }
}

// Deliberately does not implement Debug. Serialize is only for the private file.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAccount {
    key: String,
    label: String,
    models: Vec<String>,
    max_concurrency: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddInput {
    key: String,
    models: Vec<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    max_concurrency: Option<u32>,
}

fn account_id(key: &str) -> String {
    blake3::hash(key.as_bytes()).to_hex().to_string()
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 1024 && key.bytes().all(|b| b.is_ascii_graphic())
}

fn valid_label(label: &str, key: &str) -> bool {
    !label.is_empty()
        && label.chars().count() <= 80
        && !label.chars().any(char::is_control)
        && !label.to_ascii_lowercase().contains("sk-")
        && !label.contains(key)
}

fn valid_models(models: &[String], key: &str) -> bool {
    !models.is_empty()
        && models.len() <= MAX_MODELS
        && models
            .iter()
            .all(|m| config::safe_label(m) && !m.contains(key))
}

fn validate_account(account: &StoredAccount) -> bool {
    valid_key(&account.key)
        && account.key.trim() == account.key
        && valid_label(&account.label, &account.key)
        && valid_models(&account.models, &account.key)
        && (1..=64).contains(&account.max_concurrency)
}

fn read_store(state_dir: &Path) -> Result<Stored> {
    let mut file = match File::open(state_dir.join(FILE_NAME)) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Stored::default()),
        Err(_) => bail!("无法读取手动账号文件，请检查目录权限"),
    };
    let metadata = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("无法检查手动账号文件"))?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        bail!("手动账号文件类型或大小异常，未修改原文件");
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("读取手动账号文件失败，未修改原文件"))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("手动账号文件过大，未修改原文件");
    }
    // serde can quote rejected values in its error. Never propagate that text.
    let mut stored: Stored = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("手动账号文件格式无效，未修改原文件"))?;
    let mut ids = BTreeSet::new();
    if stored.version != 1
        || stored.accounts.len() > MAX_ACCOUNTS
        || stored
            .accounts
            .iter()
            .any(|account| !validate_account(account) || !ids.insert(account_id(&account.key)))
    {
        bail!("手动账号文件包含不支持的版本或无效记录，未修改原文件");
    }
    for account in &mut stored.accounts {
        account.models.sort();
        account.models.dedup();
    }
    Ok(stored)
}

fn write_store(state_dir: &Path, stored: &Stored) -> Result<()> {
    let bytes =
        serde_json::to_vec_pretty(stored).map_err(|_| anyhow::anyhow!("无法生成手动账号文件"))?;
    config::private_write(&state_dir.join(FILE_NAME), &bytes)
        .map_err(|_| anyhow::anyhow!("无法保存手动账号文件，请检查目录权限与可用空间"))
}

/// Add a key, or merge the submitted models into the same physical key.
/// A supplied label replaces the old label; omission preserves an existing one.
/// The submitted concurrency (default one) applies to all of that key's models.
pub fn add(state_dir: &Path, input: Value) -> Result<String> {
    let input: AddInput = serde_json::from_value(input)
        .map_err(|_| anyhow::anyhow!("手动账号参数格式错误，请检查密钥、模型和并发字段"))?;
    let key = input.key.trim().to_owned();
    if !valid_key(&key) {
        bail!("密钥必须为不含空白的可打印 ASCII 字符，长度为 1 至 1024 字节");
    }
    if !valid_models(&input.models, &key) {
        bail!("请填写 1 至 20 个有效模型名称，模型字段不能包含密钥");
    }
    let label = input.label.map(|label| label.trim().to_owned());
    if label
        .as_ref()
        .is_some_and(|label| !valid_label(label, &key))
    {
        bail!("昵称须为 1 至 80 个字符，不能含控制字符、密钥或 sk- 字样");
    }
    let max_concurrency = input.max_concurrency.unwrap_or(1);
    if !(1..=64).contains(&max_concurrency) {
        bail!("单模型并发上限须为 1 至 64");
    }
    let id = account_id(&key);
    let _lock = MUTATIONS.lock().unwrap_or_else(|error| error.into_inner());
    let mut stored = read_store(state_dir)?;
    if let Some(existing) = stored
        .accounts
        .iter_mut()
        .find(|account| account.key == key)
    {
        let merged: BTreeSet<_> = existing
            .models
            .iter()
            .cloned()
            .chain(input.models)
            .collect();
        if merged.len() > MAX_MODELS {
            bail!("合并后单个账号的模型数超过 20，未修改原账号");
        }
        existing.models = merged.into_iter().collect();
        if let Some(label) = label {
            existing.label = label;
        }
        existing.max_concurrency = max_concurrency;
    } else {
        if stored.accounts.len() >= MAX_ACCOUNTS {
            bail!("手动账号已达到 256 个，添加前请先删除不再使用的账号");
        }
        let models: BTreeSet<_> = input.models.into_iter().collect();
        stored.accounts.push(StoredAccount {
            key,
            label: label.unwrap_or_else(|| "手动账号".into()),
            models: models.into_iter().collect(),
            max_concurrency,
        });
    }
    write_store(state_dir, &stored)?;
    Ok(id)
}

/// Delete by public hash ID. Missing files and unknown IDs do not create files.
pub fn remove(state_dir: &Path, id: &str) -> Result<bool> {
    if !config::valid_key_id(id) {
        bail!("账号标识格式无效");
    }
    let id = id.to_ascii_lowercase();
    let _lock = MUTATIONS.lock().unwrap_or_else(|error| error.into_inner());
    let mut stored = read_store(state_dir)?;
    let before = stored.accounts.len();
    stored
        .accounts
        .retain(|account| account_id(&account.key) != id);
    if stored.accounts.len() == before {
        return Ok(false);
    }
    write_store(state_dir, &stored)?;
    Ok(true)
}

/// Public management view. Construct explicit allowlisted fields rather than
/// serializing StoredAccount, because that type contains the credential.
pub fn public_accounts(state_dir: &Path) -> Result<Value> {
    let stored = read_store(state_dir)?;
    Ok(
        json!({"accounts": stored.accounts.iter().map(|account| json!({
        "id": account_id(&account.key), "label": account.label,
        "models": account.models, "max_concurrency": account.max_concurrency
    })).collect::<Vec<_>>()}),
    )
}

/// A manual account is only a user-supplied candidate, never a live health claim.
pub fn load_snapshot(state_dir: &Path) -> Result<SourceSnapshot> {
    let stored = read_store(state_dir)?;
    let mut model_counts = BTreeMap::<String, usize>::new();
    let accounts = stored
        .accounts
        .into_iter()
        .map(|account| {
            for model in &account.models {
                *model_counts.entry(model.clone()).or_default() += 1;
            }
            Account {
                id: account_id(&account.key),
                secret: Arc::from(account.key),
                upstream: UPSTREAM.into(),
                label: account.label,
                provider_ids: account
                    .models
                    .iter()
                    .map(|model| format!("manual:{model}"))
                    .collect(),
                model_concurrency: account
                    .models
                    .iter()
                    .map(|model| (model.clone(), account.max_concurrency))
                    .collect(),
                models: account.models,
                source_max_concurrency: account.max_concurrency,
                last_source_success_ms: None,
                model_source_success_ms: HashMap::new(),
            }
        })
        .collect();
    let providers = model_counts
        .into_iter()
        .map(|(model, count)| ProviderSummary {
            id: format!("manual:{model}"),
            name: format!("手动账号 ({model})"),
            model,
            total_keys: count,
            kept_keys: 0,
            imported_keys: count,
        })
        .collect();
    Ok(SourceSnapshot {
        accounts,
        providers,
        warnings: vec![
            "当前使用手动维护的账号列表，未同步 BiWrite；账号是否可用由代理请求结果确认".into(),
        ],
        read_at_ms: now_ms(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const KEY: &str = "sk-manual-test-fixture-123456789";
    const ASTRA: &str = "gpt-6-astra";
    const SOL: &str = "gpt-6.1-sol";

    fn input(key: &str) -> Value {
        json!({"key": key, "models": [ASTRA]})
    }

    #[test]
    fn missing_file_is_empty_and_reading_or_removing_does_not_create_it() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_snapshot(dir.path()).unwrap().accounts.is_empty());
        assert_eq!(
            public_accounts(dir.path()).unwrap(),
            json!({"accounts": []})
        );
        assert!(!remove(dir.path(), &account_id(KEY)).unwrap());
        assert!(!dir.path().join(FILE_NAME).exists());
    }

    #[test]
    fn add_trims_key_and_builds_model_specific_candidates_without_health_claims() {
        let dir = tempfile::tempdir().unwrap();
        let id = add(
            dir.path(),
            json!({"key":format!("  {KEY}\n"),"models":[ASTRA,SOL],"label":"研究账号"}),
        )
        .unwrap();
        assert_eq!(id, account_id(KEY));
        let snapshot = load_snapshot(dir.path()).unwrap();
        assert_eq!(snapshot.accounts.len(), 1);
        let account = &snapshot.accounts[0];
        assert_eq!(account.secret.as_ref(), KEY);
        assert_eq!(account.upstream, UPSTREAM);
        assert_eq!(account.label, "研究账号");
        assert_eq!(account.model_concurrency[ASTRA], 1);
        assert_eq!(account.model_concurrency[SOL], 1);
        assert!(account.last_source_success_ms.is_none());
        assert!(account.model_source_success_ms.is_empty());
        assert_eq!(snapshot.providers.len(), 2);
        assert!(
            snapshot
                .providers
                .iter()
                .all(|p| p.kept_keys == 0 && p.imported_keys == 1)
        );
        assert!(snapshot.warnings[0].contains("手动维护"));
    }

    #[test]
    fn duplicate_add_merges_models_updates_fields_and_preserves_identity() {
        let dir = tempfile::tempdir().unwrap();
        let id = add(dir.path(), input(KEY)).unwrap();
        let same = add(
            dir.path(),
            json!({"key":KEY,"models":[SOL,SOL],"label":"新昵称","max_concurrency":3}),
        )
        .unwrap();
        assert_eq!(id, same);
        let public = public_accounts(dir.path()).unwrap();
        assert_eq!(public["accounts"].as_array().unwrap().len(), 1);
        assert_eq!(public["accounts"][0]["models"], json!([ASTRA, SOL]));
        assert_eq!(public["accounts"][0]["label"], "新昵称");
        let snapshot = load_snapshot(dir.path()).unwrap();
        assert_eq!(snapshot.accounts[0].model_concurrency[ASTRA], 3);
        assert_eq!(snapshot.accounts[0].model_concurrency[SOL], 3);
        add(dir.path(), input(KEY)).unwrap();
        let public = public_accounts(dir.path()).unwrap();
        assert_eq!(public["accounts"][0]["label"], "新昵称");
        assert_eq!(public["accounts"][0]["max_concurrency"], 1);
    }

    #[test]
    fn public_view_contains_only_allowlisted_fields_and_no_credential() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), input(KEY)).unwrap();
        let public = public_accounts(dir.path()).unwrap();
        assert!(!public.to_string().contains(KEY));
        let account = public["accounts"][0].as_object().unwrap();
        let keys: BTreeSet<_> = account.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            BTreeSet::from(["id", "label", "max_concurrency", "models"])
        );
    }

    #[test]
    fn invalid_inputs_never_echo_secrets_or_create_files() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            json!({"key":KEY,"models":KEY}),
            json!({"key":KEY,"models":[]}),
            json!({"key":KEY,"models":["bad model"]}),
            json!({"key":KEY,"models":[ASTRA],"label":KEY}),
            json!({"key":KEY,"models":[ASTRA],"label":"sk-不能作为昵称"}),
            json!({"key":KEY,"models":[ASTRA],"label":"名".repeat(81)}),
            json!({"key":KEY,"models":[ASTRA],"max_concurrency":0}),
            json!({"key":KEY,"models":[ASTRA],"max_concurrency":65}),
            json!({"key":KEY,"models":[ASTRA],"extra":KEY}),
            input("has inner space"),
            input("非ASCII密钥"),
            input(""),
            input(&"a".repeat(1025)),
        ] {
            let error = add(dir.path(), bad).err().unwrap().to_string();
            assert!(!error.contains(KEY));
        }
        assert!(!dir.path().join(FILE_NAME).exists());
    }

    #[test]
    fn models_limit_is_enforced_on_input_and_after_merge() {
        let dir = tempfile::tempdir().unwrap();
        let models: Vec<_> = (0..20).map(|i| format!("fixture-model-{i}")).collect();
        add(dir.path(), json!({"key":KEY,"models":models})).unwrap();
        let before = fs::read(dir.path().join(FILE_NAME)).unwrap();
        assert!(add(dir.path(), input(KEY)).is_err());
        assert_eq!(before, fs::read(dir.path().join(FILE_NAME)).unwrap());
        assert!(
            add(
                dir.path(),
                json!({"key":"fixture-other-key-123","models":vec![ASTRA;21]})
            )
            .is_err()
        );
    }

    #[test]
    fn account_limit_is_enforced_but_existing_account_can_update_at_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let stored = Stored {
            version: 1,
            accounts: (0..MAX_ACCOUNTS)
                .map(|i| StoredAccount {
                    key: format!("sk-capacity-fixture-{i:04}"),
                    label: "容量测试".into(),
                    models: vec![ASTRA.into()],
                    max_concurrency: 1,
                })
                .collect(),
        };
        write_store(dir.path(), &stored).unwrap();
        assert!(add(dir.path(), input(KEY)).is_err());
        add(
            dir.path(),
            json!({"key":"sk-capacity-fixture-0000","models":[SOL]}),
        )
        .unwrap();
        assert_eq!(
            load_snapshot(dir.path()).unwrap().accounts.len(),
            MAX_ACCOUNTS
        );
    }

    #[test]
    fn removal_targets_one_hash_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let a = add(dir.path(), input(KEY)).unwrap();
        let b = add(dir.path(), input("sk-other-manual-fixture-123456789")).unwrap();
        assert!(remove(dir.path(), &a).unwrap());
        assert!(!remove(dir.path(), &a).unwrap());
        let snapshot = load_snapshot(dir.path()).unwrap();
        assert_eq!(snapshot.accounts.len(), 1);
        assert_eq!(snapshot.accounts[0].id, b);
        assert!(remove(dir.path(), "invalid-id").is_err());
    }

    #[test]
    fn corrupted_store_is_not_overwritten_and_errors_are_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = format!("{{\"version\":1,\"accounts\":\"{KEY}\"}}").into_bytes();
        fs::write(dir.path().join(FILE_NAME), &bytes).unwrap();
        for error in [
            add(dir.path(), input(KEY)).err().unwrap(),
            remove(dir.path(), &account_id(KEY)).err().unwrap(),
            public_accounts(dir.path()).err().unwrap(),
            load_snapshot(dir.path()).err().unwrap(),
        ] {
            assert!(!error.to_string().contains(KEY));
        }
        assert_eq!(fs::read(dir.path().join(FILE_NAME)).unwrap(), bytes);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_store_is_private_and_does_not_leave_temporary_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("new-private-state");
        add(&state_dir, input(KEY)).unwrap();
        assert_eq!(
            fs::metadata(&state_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(state_dir.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        add(&state_dir, json!({"key":KEY,"models":[SOL]})).unwrap();
        assert_eq!(fs::read_dir(&state_dir).unwrap().count(), 1);
        assert_eq!(
            fs::metadata(state_dir.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn concurrent_adds_do_not_lose_accounts() {
        let dir = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for i in 0..8 {
                let path = dir.path();
                scope.spawn(move || {
                    add(path, input(&format!("sk-parallel-manual-fixture-{i}"))).unwrap()
                });
            }
        });
        assert_eq!(load_snapshot(dir.path()).unwrap().accounts.len(), 8);
    }
}
