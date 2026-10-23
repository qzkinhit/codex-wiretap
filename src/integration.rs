//! Direct Codex integration plus explicit, reversible CC Switch manual-key monitoring.
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
use toml_edit::{DocumentMut, Item, Table, value};

const JOURNAL: &str = "integration-backup.json";
const MONITOR_JOURNAL: &str = "cc-monitor-backup.json";
const PROVIDER: &str = "pool_router";
const DIRECT_STRING_FIELDS: [&str; 4] =
    ["name", "base_url", "wire_api", "experimental_bearer_token"];
const DIRECT_BOOL_FIELDS: [&str; 2] = ["supports_websockets", "requires_openai_auth"];

#[derive(Clone, Serialize)]
pub struct ConnectionInfo {
    pub base_url: String,
    pub configured: bool,
    pub detail: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Backup {
    CcSwitch {
        database: PathBuf,
        provider_id: String,
        provider_name: String,
        original_row: Value,
        before_settings: String,
        after_settings: String,
        provider_key: String,
    },
    Codex {
        config_path: PathBuf,
        before_config: String,
        after_config: String,
        previous_provider: Option<String>,
    },
}

/// Only the fields owned by manual-key monitoring. Never serializes provider credentials.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorBackup {
    database: PathBuf,
    provider_id: String,
    provider_key: String,
    before_url: String,
    after_url: String,
    before_websockets: Option<bool>,
    after_websockets: Option<bool>,
}

struct CurrentProvider {
    id: String,
    name: String,
    settings: String,
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("无法确定用户目录")
}

fn validate_target(raw: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw).context("本地路由地址格式错误")?;
    ensure!(url.scheme() == "http", "接入地址必须使用本机 HTTP");
    ensure!(
        matches!(
            url.host_str(),
            Some("127.0.0.1") | Some("localhost") | Some("[::1]")
        ),
        "接入地址必须是回环地址"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "接入地址不能携带凭证、查询或片段"
    );
    ensure!(url.port().is_some(), "接入地址必须包含明确端口");
    ensure!(
        url.path().trim_end_matches('/') == "/v1",
        "接入地址路径必须为 /v1"
    );
    Ok(url)
}

fn normalized(raw: &str) -> String {
    raw.trim_end_matches('/').to_owned()
}

fn document(text: &str) -> Result<DocumentMut> {
    // Do not include TOML parse errors, which can quote a secret-bearing source line.
    text.parse()
        .map_err(|_| anyhow::anyhow!("Codex TOML 配置解析失败，未修改配置"))
}

fn active_endpoint(doc: &DocumentMut) -> Result<(String, String)> {
    let key = doc
        .get("model_provider")
        .and_then(Item::as_str)
        .context("Codex 没有配置活动供应商")?;
    let url = doc
        .get("model_providers")
        .and_then(|v| v.get(key))
        .and_then(|v| v.get("base_url"))
        .and_then(Item::as_str)
        .context("Codex 活动供应商没有 base_url")?;
    Ok((key.to_owned(), url.to_owned()))
}

fn current_provider(db: &Connection) -> Result<Option<CurrentProvider>> {
    let rows: i64 = db.query_row(
        "SELECT count(*) FROM providers WHERE app_type='codex' AND is_current=1",
        [],
        |r| r.get(0),
    )?;
    ensure!(rows <= 1, "CC Switch 当前供应商状态不唯一，未修改数据库");
    db.query_row(
        "SELECT id,name,settings_config FROM providers WHERE app_type='codex' AND is_current=1",
        [],
        |r| {
            Ok(CurrentProvider {
                id: r.get(0)?,
                name: r.get(1)?,
                settings: r.get(2)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn proxy_port(db: &Connection) -> Result<Option<u16>> {
    db.query_row("SELECT listen_port FROM proxy_config WHERE app_type='codex' AND (proxy_enabled=1 OR enabled=1)", [], |r| r.get(0)).optional().map_err(Into::into)
}

fn settings_document(settings: &str) -> Result<(Value, DocumentMut)> {
    let parsed: Value = serde_json::from_str(settings)
        .map_err(|_| anyhow::anyhow!("CC Switch 供应商 JSON 配置无效"))?;
    let text = parsed
        .get("config")
        .and_then(Value::as_str)
        .context("CC Switch 供应商缺少 Codex 配置")?;
    let doc = document(text)?;
    Ok((parsed, doc))
}

pub fn inspect_connection(base_url: &str) -> ConnectionInfo {
    match home_dir().and_then(|home| inspect_at(base_url, &home)) {
        Ok(info) => info,
        Err(_) => ConnectionInfo {
            base_url: base_url.to_owned(),
            configured: false,
            detail: "尚未确认 Codex 直连接入状态；请检查本地 Codex 配置。".into(),
        },
    }
}

fn inspect_at(base_url: &str, home: &Path) -> Result<ConnectionInfo> {
    validate_target(base_url)?;
    let config = fs::read_to_string(home.join(".codex/config.toml"))?;
    let (_, active) = active_endpoint(&document(&config)?)?;
    if normalized(&active) == normalized(base_url) {
        return Ok(ConnectionInfo { base_url:base_url.into(), configured:true, detail:"Codex 配置已直连 Pool Router，运行只需 BiWrite 与本软件。首次接入后，已打开会话是否采用新配置仍需请求验证，必要时重载一次。".into() });
    }
    Ok(ConnectionInfo {
        base_url: base_url.into(),
        configured: false,
        detail: "Codex 尚未直连 Pool Router；接入操作会备份配置并新增独立的本地供应商。".into(),
    })
}

/// Explicit mutation entry point, invoked only by the connect command/API.
pub fn connect(base_url: &str, local_token: &str, state_dir: &Path) -> Result<String> {
    connect_at(base_url, local_token, state_dir, &home_dir()?)
}

fn connect_at(base_url: &str, local_token: &str, state_dir: &Path, home: &Path) -> Result<String> {
    validate_target(base_url)?;
    ensure!(
        !local_token.is_empty() && !local_token.contains(['\n', '\r']),
        "本地访问令牌无效"
    );
    let path = home.join(".codex/config.toml");
    let original = fs::read_to_string(&path).context("无法读取 Codex 配置")?;
    let mut doc = document(&original)?;
    let prior_backup = if state_dir.join(JOURNAL).exists() {
        Some(read_backup(state_dir)?)
    } else {
        None
    };
    if let Some(Backup::Codex {
        config_path,
        after_config,
        previous_provider,
        ..
    }) = &prior_backup
    {
        ensure!(
            *config_path == path,
            "现有备份属于不同的 Codex 配置，未覆盖"
        );
        let installed = document(after_config)?;
        ensure_direct_requested(&installed, base_url, local_token)?;
        if known_direct_state(&doc, &installed, previous_provider.as_deref())? {
            return Ok("Codex 已直连同一 Pool Router，地址与本地令牌一致，无需重复修改。原始恢复备份及后续无关配置均已保留。".into());
        }
        if !has_direct_table(&doc) {
            if !doc.contains_key("model_providers") {
                doc["model_providers"] = Item::Table(Table::new());
            }
            let managed = installed
                .get("model_providers")
                .and_then(|v| v.get(PROVIDER))
                .cloned()
                .context("原始接入备份缺少本地供应商表")?;
            doc.get_mut("model_providers")
                .and_then(Item::as_table_like_mut)
                .context("model_providers 必须是 TOML 表")?
                .insert(PROVIDER, managed);
        }
        doc["model_provider"] = value(PROVIDER);
        ensure!(
            fs::read_to_string(&path)? == original,
            "Codex 配置在重新接入期间发生变化，未覆盖；原始备份保留"
        );
        private_write(&path, doc.to_string().as_bytes())?;
        return Ok("已从原供应商切回 Pool Router 直连。原始备份保留，当前模型、原供应商的新密钥和其他无关设置均未改动。已打开的 Codex 会话可能需要重新加载配置。".into());
    }
    if doc
        .get("model_providers")
        .is_some_and(|v| v.get(PROVIDER).is_some())
    {
        if prior_backup.is_some() {
            bail!("存在旧接入备份且 pool_router 供应商已出现，无法确认修改归属，未覆盖");
        }
        ensure_direct_requested(&doc, base_url, local_token)?;
        return Ok("Codex 已直连同一 Pool Router，地址与本地令牌一致，未重复修改。当前没有本程序的原始接入备份。".into());
    }
    let previous_provider = doc
        .get("model_provider")
        .and_then(Item::as_str)
        .map(str::to_owned);
    if !doc.contains_key("model_providers") {
        doc["model_providers"] = Item::Table(Table::new());
    }
    let providers = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
        .context("model_providers 必须是 TOML 表")?;
    let mut table = Table::new();
    table["name"] = value("Pool Router");
    table["base_url"] = value(normalized(base_url));
    table["wire_api"] = value("responses");
    table["supports_websockets"] = value(false);
    table["requires_openai_auth"] = value(false);
    table["experimental_bearer_token"] = value(local_token);
    providers.insert(PROVIDER, Item::Table(table));
    doc["model_provider"] = value(PROVIDER);
    let updated = doc.to_string();
    let migrated = matches!(prior_backup, Some(Backup::CcSwitch { .. }));
    if migrated {
        // The only default-connect path that touches CC Switch is recovery of our own journal.
        // Complete all direct-config validation first; a recovery conflict leaves Codex untouched.
        restore_primary(state_dir)?;
    }
    let backup = Backup::Codex {
        config_path: path.clone(),
        before_config: original.clone(),
        after_config: updated.clone(),
        previous_provider,
    };
    write_backup(state_dir, &backup)?;
    ensure!(
        fs::read_to_string(&path)? == original,
        "Codex 配置在接入期间发生变化，未覆盖；恢复备份已保留"
    );
    private_write(&path, updated.as_bytes())?;
    let prefix = if migrated {
        "已恢复并归档旧接入对 CC Switch 的修改。"
    } else {
        ""
    };
    Ok(format!(
        "{prefix}已将 Codex 直接接入 Pool Router，运行只需 BiWrite 与本软件，原供应商配置保留。首次接入可能需要重新打开会话或重载 Codex；后续号池切换无需重启。恢复备份已保存。"
    ))
}

fn ensure_direct_requested(doc: &DocumentMut, base_url: &str, token: &str) -> Result<()> {
    ensure!(
        doc.get("model_provider").and_then(Item::as_str) == Some(PROVIDER),
        "活动供应商已被后续修改，未覆盖"
    );
    let endpoint = normalized(base_url);
    for (field, expected) in [
        ("name", "Pool Router"),
        ("base_url", endpoint.as_str()),
        ("wire_api", "responses"),
        ("experimental_bearer_token", token),
    ] {
        ensure!(
            string_toml_field(doc, PROVIDER, field)?.as_deref() == Some(expected),
            "已有直连的受管字段与请求不一致，未覆盖；请检查或先恢复接入"
        );
    }
    for field in DIRECT_BOOL_FIELDS {
        ensure!(
            bool_toml_field(doc, PROVIDER, field)? == Some(false),
            "已有直连的受管协议或认证字段发生变化，未覆盖"
        );
    }
    Ok(())
}

fn ensure_direct_fields(current: &DocumentMut, installed: &DocumentMut) -> Result<()> {
    for field in DIRECT_STRING_FIELDS {
        ensure!(
            string_toml_field(current, PROVIDER, field)?
                == string_toml_field(installed, PROVIDER, field)?,
            "Pool Router 受管字段已被后续修改，未覆盖；接入备份保留"
        );
    }
    for field in DIRECT_BOOL_FIELDS {
        ensure!(
            bool_toml_field(current, PROVIDER, field)?
                == bool_toml_field(installed, PROVIDER, field)?,
            "Pool Router 受管协议或认证字段已被后续修改，未覆盖；接入备份保留"
        );
    }
    Ok(())
}

fn has_direct_table(doc: &DocumentMut) -> bool {
    doc.get("model_providers")
        .is_some_and(|v| v.get(PROVIDER).is_some())
}

/// Return true when direct is active, false when the known original provider is active.
/// A missing managed table is safe only in the known original state, as CC Switch may
/// rewrite that table away. Any surviving managed fields must still match our journal.
fn known_direct_state(
    current: &DocumentMut,
    installed: &DocumentMut,
    previous: Option<&str>,
) -> Result<bool> {
    let active = match current.get("model_provider") {
        None => None,
        Some(item) => Some(
            item.as_str()
                .context("活动供应商字段类型已改变，未覆盖；接入备份保留")?,
        ),
    };
    let direct = active == Some(PROVIDER);
    ensure!(
        direct || active == previous,
        "活动供应商已变为未知供应商，未覆盖；接入备份保留"
    );
    if direct || has_direct_table(current) {
        ensure_direct_fields(current, installed)?;
    }
    Ok(direct)
}

/// Compatibility-only API. The public `connect` command never selects this path.
pub fn connect_cc_legacy(base_url: &str, token: &str, state_dir: &Path) -> Result<String> {
    connect_cc(
        base_url,
        token,
        state_dir,
        &home_dir()?.join(".cc-switch/cc-switch.db"),
    )
}

/// Enable monitoring for the selected CC Switch Anyrouter provider without changing its key.
pub fn connect_legacy_monitor(base_url: &str, state_dir: &Path) -> Result<String> {
    connect_monitor_at(
        base_url,
        state_dir,
        &home_dir()?.join(".cc-switch/cc-switch.db"),
    )
}

fn read_cc_database(path: &Path) -> Result<Connection> {
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("无法读取 CC Switch 数据库")?;
    db.busy_timeout(Duration::from_secs(5))?;
    Ok(db)
}

fn anyrouter_source(raw: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(raw) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some("anyrouter.top")
        && url.port_or_known_default() == Some(443)
        && url.path().trim_end_matches('/') == "/v1"
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

fn read_monitor_backup(state_dir: &Path) -> Result<MonitorBackup> {
    serde_json::from_slice(
        &fs::read(state_dir.join(MONITOR_JOURNAL)).context("未找到 CC Switch 监控接入备份")?,
    )
    .map_err(|_| anyhow::anyhow!("CC Switch 监控接入备份格式无效"))
}

fn monitor_managed_fields_match(doc: &DocumentMut, backup: &MonitorBackup) -> Result<()> {
    ensure!(
        string_toml_field(doc, &backup.provider_key, "base_url")?.as_deref()
            == Some(&backup.after_url),
        "CC Switch 监控目标已被后续修改，未覆盖；监控备份保留"
    );
    ensure!(
        bool_toml_field(doc, &backup.provider_key, "supports_websockets")?
            == backup.after_websockets,
        "CC Switch 监控协议字段已被后续修改，未覆盖；监控备份保留"
    );
    Ok(())
}

fn connect_monitor_at(base_url: &str, state_dir: &Path, database: &Path) -> Result<String> {
    let target = validate_target(base_url)?;
    // Preflight uses a read-only connection; missing databases must never be created.
    let reader = read_cc_database(database)?;
    ensure!(
        proxy_port(&reader)? != target.port(),
        "监控目标端口属于 CC Switch 自身，会形成循环，未修改配置"
    );
    let row = current_provider(&reader)?.context("CC Switch 没有当前 Codex 供应商")?;
    let (mut settings, mut doc) = settings_document(&row.settings)?;
    let (provider_key, previous_url) = active_endpoint(&doc)?;
    if state_dir.join(MONITOR_JOURNAL).exists() {
        let backup = read_monitor_backup(state_dir)?;
        ensure!(
            backup.database == database
                && backup.provider_id == row.id
                && backup.provider_key == provider_key,
            "已有其他供应商的监控接入备份，请先恢复该监控接入；未修改当前供应商"
        );
        ensure!(
            backup.after_url == normalized(base_url),
            "已有监控接入目标不同，请先恢复；未覆盖原备份"
        );
        monitor_managed_fields_match(&doc, &backup)?;
        return Ok("CC Switch 当前供应商已经接入同一监控入口，真实密钥及其他设置保持原值，无需重复修改。运行时是否读取新目标仍需请求验证。".into());
    }
    ensure!(
        anyrouter_source(&previous_url),
        "手动密钥监控只接入 https://anyrouter.top/v1 供应商；未知本地代理可能形成循环，未修改配置"
    );
    ensure!(
        string_toml_field(&doc, &provider_key, "wire_api")?.as_deref() == Some("responses"),
        "手动密钥监控只支持 Responses 供应商，未修改协议或模型配置"
    );
    let before_websockets = bool_toml_field(&doc, &provider_key, "supports_websockets")?;
    let backup = MonitorBackup {
        database: database.to_owned(),
        provider_id: row.id.clone(),
        provider_key: provider_key.clone(),
        before_url: previous_url,
        after_url: normalized(base_url),
        before_websockets,
        after_websockets: Some(false),
    };
    doc["model_providers"][&provider_key]["base_url"] = value(&backup.after_url);
    doc["model_providers"][&provider_key]["supports_websockets"] = value(false);
    settings["config"] = Value::String(doc.to_string());
    let updated = serde_json::to_string(&settings)?;
    drop(reader);

    let mut db =
        Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(Duration::from_secs(5))?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure!(
        proxy_port(&tx)? != target.port(),
        "CC Switch 监听端口已改变，监控接入可能形成循环，未修改配置"
    );
    let fresh = current_provider(&tx)?.context("CC Switch 当前供应商发生变化")?;
    ensure!(
        fresh.id == row.id && fresh.settings == row.settings,
        "CC Switch 配置在接入期间发生变化，请重试；未覆盖任何字段"
    );
    fs::create_dir_all(state_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(state_dir, fs::Permissions::from_mode(0o700))?;
    }
    private_write_mode(
        &state_dir.join(MONITOR_JOURNAL),
        &serde_json::to_vec_pretty(&backup)?,
        true,
    )?;
    ensure!(tx.execute("UPDATE providers SET settings_config=?1 WHERE app_type='codex' AND id=?2 AND is_current=1 AND settings_config=?3", params![updated,row.id,row.settings])? == 1,
        "CC Switch 供应商配置发生变化，未覆盖；监控备份保留");
    tx.commit()?;
    Ok("已将 CC Switch 当前 Anyrouter 供应商接入本地监控，只修改转发地址并关闭 WebSocket。真实密钥、模型、推理强度、当前供应商选择及 Codex 配置均未改动。CC Switch 是否实时采用新目标仍需实际请求验证。".into())
}

fn monitor_settings(db: &Connection, backup: &MonitorBackup) -> Result<String> {
    db.query_row(
        "SELECT settings_config FROM providers WHERE app_type='codex' AND id=?",
        [&backup.provider_id],
        |r| r.get(0),
    )
    .context("监控接入的原供应商已不存在，备份保留")
}

fn restored_monitor_settings(current: &str, backup: &MonitorBackup) -> Result<String> {
    let (mut settings, mut doc) = settings_document(current)?;
    let current_url = string_toml_field(&doc, &backup.provider_key, "base_url")?;
    ensure!(
        current_url.as_deref() == Some(&backup.after_url)
            || current_url.as_deref() == Some(&backup.before_url),
        "CC Switch 监控目标已被后续修改，未覆盖；监控备份保留"
    );
    let changed_websockets = backup.before_websockets != backup.after_websockets;
    if changed_websockets {
        let current_ws = bool_toml_field(&doc, &backup.provider_key, "supports_websockets")?;
        ensure!(
            current_ws == backup.before_websockets || current_ws == backup.after_websockets,
            "CC Switch 监控协议字段已被后续修改，未覆盖；监控备份保留"
        );
    }
    doc["model_providers"][&backup.provider_key]["base_url"] = value(&backup.before_url);
    if changed_websockets {
        match backup.before_websockets {
            Some(original) => {
                doc["model_providers"][&backup.provider_key]["supports_websockets"] =
                    value(original)
            }
            None => {
                doc["model_providers"][&backup.provider_key]
                    .as_table_like_mut()
                    .context("原供应商表不存在")?
                    .remove("supports_websockets");
            }
        }
    }
    settings["config"] = Value::String(doc.to_string());
    Ok(serde_json::to_string(&settings)?)
}

fn preflight_monitor_restore(state_dir: &Path) -> Result<()> {
    let backup = read_monitor_backup(state_dir)?;
    let db = read_cc_database(&backup.database)?;
    restored_monitor_settings(&monitor_settings(&db, &backup)?, &backup)?;
    Ok(())
}

/// Restore only the URL/transport fields, retaining keys changed by the user after installation.
pub fn restore_legacy_monitor(state_dir: &Path) -> Result<String> {
    preflight_monitor_restore(state_dir)?;
    let backup = read_monitor_backup(state_dir)?;
    let mut db = Connection::open_with_flags(
        &backup.database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = monitor_settings(&tx, &backup)?;
    let updated = restored_monitor_settings(&current, &backup)?;
    if updated != current {
        ensure!(tx.execute("UPDATE providers SET settings_config=?1 WHERE app_type='codex' AND id=?2 AND settings_config=?3", params![updated,backup.provider_id,current])? == 1,
            "供应商在恢复期间发生变化，未覆盖；监控备份保留");
    }
    tx.commit()?;
    fs::rename(
        state_dir.join(MONITOR_JOURNAL),
        state_dir.join(format!("cc-monitor-restored-{}.json", uuid::Uuid::new_v4())),
    )?;
    Ok("已恢复 CC Switch 监控接入前的地址和协议字段；之后更换的密钥、模型、推理强度、当前供应商选择及其他设置均保留。监控备份已归档。".into())
}

fn connect_cc(base_url: &str, token: &str, state_dir: &Path, path: &Path) -> Result<String> {
    let target = validate_target(base_url)?;
    ensure!(
        !token.is_empty() && !token.contains(['\n', '\r']),
        "本地访问令牌无效"
    );
    ensure!(!state_dir.join(JOURNAL).exists(), "已有接入备份，未覆盖");
    let mut db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(Duration::from_secs(5))?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure!(
        proxy_port(&tx)? != target.port(),
        "目标端口属于 CC Switch 代理，接入会形成循环，未修改配置"
    );
    let row = current_provider(&tx)?.context("CC Switch 没有当前 Codex 供应商")?;
    let (mut settings, mut doc) = settings_document(&row.settings)?;
    let (provider_key, previous_url) = active_endpoint(&doc)?;
    let previous = reqwest::Url::parse(&previous_url).context("当前供应商目标地址无效")?;
    ensure!(
        row.id.starts_with("wiretap-")
            && matches!(
                previous.host_str(),
                Some("127.0.0.1") | Some("localhost") | Some("[::1]")
            ),
        "CC Switch 当前供应商不是已知本地抓包副本，未改写它；请使用独立 Codex 接入配置或先选择本地抓包供应商"
    );
    ensure!(
        normalized(base_url) != normalized(&previous_url),
        "当前供应商已经使用此目标，请先检查接入状态"
    );
    ensure!(
        settings.get("auth").is_some_and(Value::is_object),
        "CC Switch 供应商 auth 配置结构无效"
    );
    string_field(settings.get("auth").and_then(|v| v.get("OPENAI_API_KEY")))?;
    string_toml_field(&doc, &provider_key, "experimental_bearer_token")?;
    bool_toml_field(&doc, &provider_key, "supports_websockets")?;
    doc["model_providers"][&provider_key]["base_url"] = value(normalized(base_url));
    doc["model_providers"][&provider_key]["experimental_bearer_token"] = value(token);
    doc["model_providers"][&provider_key]["supports_websockets"] = value(false);
    settings["config"] = Value::String(doc.to_string());
    settings["auth"]["OPENAI_API_KEY"] = Value::String(token.into());
    let updated = serde_json::to_string(&settings)?;
    let original_row = snapshot_row(&tx, &row.id)?;
    let backup = Backup::CcSwitch {
        database: path.to_owned(),
        provider_id: row.id.clone(),
        provider_name: row.name,
        original_row,
        before_settings: row.settings.clone(),
        after_settings: updated.clone(),
        provider_key,
    };
    // Write-ahead journal while the SQLite write lock holds, before committing any mutation.
    write_backup(state_dir, &backup)?;
    let changed = tx.execute("UPDATE providers SET settings_config=?1 WHERE id=?2 AND app_type='codex' AND is_current=1 AND settings_config=?3", params![updated,row.id,row.settings])?;
    ensure!(
        changed == 1,
        "供应商状态发生变化，接入未完成；恢复日志已保留"
    );
    tx.commit()?;
    Ok("已将 CC Switch 当前本地抓包供应商的转发目标和凭证改为 Pool Router，Codex 原代理入口及供应商选择保持不变。数据库修改已提交；CC Switch 运行时缓存是否读取新目标须用实际请求验证，必要时在 CC Switch 重新选择该供应商。恢复备份已保存。".into())
}

fn snapshot_row(db: &Connection, id: &str) -> Result<Value> {
    let mut stmt = db.prepare("SELECT * FROM providers WHERE app_type='codex' AND id=?")?;
    let names: Vec<String> = stmt.column_names().iter().map(|v| v.to_string()).collect();
    let mut rows = stmt.query([id])?;
    let row = rows.next()?.context("当前供应商不存在")?;
    let mut out = serde_json::Map::new();
    for (i, name) in names.iter().enumerate() {
        use rusqlite::types::ValueRef;
        let value = match row.get_ref(i)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(v) => Value::from(v),
            ValueRef::Real(v) => Value::from(v),
            ValueRef::Text(v) => Value::String(std::str::from_utf8(v)?.into()),
            ValueRef::Blob(_) => bail!("供应商记录存在不支持的二进制字段，未修改数据库"),
        };
        out.insert(name.clone(), value);
    }
    Ok(Value::Object(out))
}

fn read_backup(state_dir: &Path) -> Result<Backup> {
    serde_json::from_slice(&fs::read(state_dir.join(JOURNAL)).context("未找到接入备份")?)
        .map_err(|_| anyhow::anyhow!("接入备份格式无效"))
}

fn preflight_primary_restore(state_dir: &Path) -> Result<()> {
    match read_backup(state_dir)? {
        Backup::CcSwitch {
            database,
            provider_id,
            before_settings,
            after_settings,
            provider_key,
            ..
        } => {
            let db = read_cc_database(&database)?;
            let current: String = db
                .query_row(
                    "SELECT settings_config FROM providers WHERE app_type='codex' AND id=?",
                    [provider_id],
                    |r| r.get(0),
                )
                .context("原供应商已不存在，未恢复")?;
            if current != before_settings {
                restore_cc_fields(&current, &before_settings, &after_settings, &provider_key)?;
            }
        }
        Backup::Codex {
            config_path,
            before_config,
            after_config,
            previous_provider,
        } => {
            let current = fs::read_to_string(config_path).context("无法读取现有 Codex 配置")?;
            if current != before_config {
                known_direct_state(
                    &document(&current)?,
                    &document(&after_config)?,
                    previous_provider.as_deref(),
                )?;
            }
        }
    }
    Ok(())
}

/// Restore every pending integration before uninstall. Validate all journals before any mutation.
pub fn restore(state_dir: &Path) -> Result<String> {
    let has_primary = state_dir.join(JOURNAL).exists();
    let has_monitor = state_dir.join(MONITOR_JOURNAL).exists();
    ensure!(has_primary || has_monitor, "未找到待恢复的接入备份");
    if has_primary {
        preflight_primary_restore(state_dir)?;
    }
    if has_monitor {
        preflight_monitor_restore(state_dir)?;
    }
    let mut messages = Vec::new();
    if has_primary {
        messages.push(restore_primary(state_dir)?);
    }
    // Each action rechecks under its own write lock. A later race returns an error with the
    // pending journal intact so the caller must keep the service installed and running.
    if has_monitor {
        messages.push(restore_legacy_monitor(state_dir)?);
    }
    Ok(messages.join("\n"))
}

fn restore_primary(state_dir: &Path) -> Result<String> {
    let path = state_dir.join(JOURNAL);
    let backup = read_backup(state_dir)?;
    let legacy = matches!(backup, Backup::CcSwitch { .. });
    match &backup {
        Backup::CcSwitch {
            database,
            provider_id,
            before_settings,
            after_settings,
            provider_key,
            ..
        } => {
            let mut db =
                Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
            db.busy_timeout(Duration::from_secs(5))?;
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current: String = tx
                .query_row(
                    "SELECT settings_config FROM providers WHERE app_type='codex' AND id=?",
                    [provider_id],
                    |r| r.get(0),
                )
                .context("原供应商已不存在，未恢复")?;
            if current != *before_settings {
                let restored =
                    restore_cc_fields(&current, before_settings, after_settings, provider_key)?;
                ensure!(tx.execute("UPDATE providers SET settings_config=? WHERE app_type='codex' AND id=? AND settings_config=?", params![restored,provider_id,current])? == 1, "供应商配置发生变化，未覆盖");
            }
            tx.commit()?;
        }
        Backup::Codex {
            config_path,
            before_config,
            after_config,
            previous_provider,
        } => {
            let current = fs::read_to_string(config_path).context("无法读取现有 Codex 配置")?;
            if current != *before_config {
                let mut doc = document(&current)?;
                let installed = document(after_config)?;
                let was_direct =
                    known_direct_state(&doc, &installed, previous_provider.as_deref())?;
                let before = document(before_config)?;
                if has_direct_table(&doc) {
                    let providers = doc
                        .get_mut("model_providers")
                        .and_then(Item::as_table_like_mut)
                        .context("供应商表不存在")?;
                    let table = providers
                        .get_mut(PROVIDER)
                        .and_then(Item::as_table_like_mut)
                        .context("Pool Router 供应商表不存在")?;
                    for field in DIRECT_STRING_FIELDS.into_iter().chain(DIRECT_BOOL_FIELDS) {
                        table.remove(field);
                    }
                    if table.is_empty() {
                        providers.remove(PROVIDER);
                    }
                    // Keep unrelated fields added to the inactive managed table.
                    if !before.contains_key("model_providers") && providers.is_empty() {
                        doc.remove("model_providers");
                    }
                }
                if was_direct {
                    match previous_provider {
                        Some(value_before) => {
                            doc["model_provider"] = before
                                .get("model_provider")
                                .cloned()
                                .unwrap_or_else(|| value(value_before))
                        }
                        None => {
                            doc.remove("model_provider");
                        }
                    }
                }
                let restored = if current == *after_config {
                    before_config.clone()
                } else {
                    doc.to_string()
                };
                if restored != current {
                    ensure!(
                        fs::read_to_string(config_path)? == current,
                        "Codex 配置在恢复期间发生变化，未覆盖；接入备份保留"
                    );
                    private_write(config_path, restored.as_bytes())?;
                }
            }
        }
    }
    // Keep the private original for audit/recovery, without reusing it for later connections.
    let archive = state_dir.join(format!(
        "integration-restored-{}.json",
        uuid::Uuid::new_v4()
    ));
    fs::rename(path, archive)?;
    if legacy {
        Ok("已恢复 Pool Router 旧接入对 CC Switch 的修改，保留后续无关修改及当前供应商选择。私有备份已归档。".into())
    } else {
        Ok("已恢复 Codex 接入前的活动供应商，并移除 Pool Router 自己写入的字段。后续无关修改已保留，私有备份已归档。首次恢复后可能需要重载 Codex。".into())
    }
}

fn string_field(value: Option<&Value>) -> Result<Option<String>> {
    match value {
        None => Ok(None),
        Some(Value::String(v)) => Ok(Some(v.clone())),
        _ => bail!("凭证字段类型不是字符串，未修改配置"),
    }
}

fn string_toml_field(doc: &DocumentMut, provider: &str, key: &str) -> Result<Option<String>> {
    match doc
        .get("model_providers")
        .and_then(|v| v.get(provider))
        .and_then(|v| v.get(key))
    {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(|s| Some(s.to_owned()))
            .context("供应商配置字段类型无效"),
    }
}

fn restore_cc_fields(current: &str, before: &str, after: &str, key: &str) -> Result<String> {
    if current == after {
        return Ok(before.into());
    }
    let (mut cur, mut cur_doc) = settings_document(current)?;
    let (old, old_doc) = settings_document(before)?;
    let (new, new_doc) = settings_document(after)?;
    let old_websockets = bool_toml_field(&old_doc, key, "supports_websockets")?;
    let new_websockets = bool_toml_field(&new_doc, key, "supports_websockets")?;
    let changed_websockets = old_websockets != new_websockets;
    // Check every owned field before writing any. Conflicts leave the journal and database intact.
    for field in ["base_url", "experimental_bearer_token"] {
        ensure!(
            string_toml_field(&cur_doc, key, field)? == string_toml_field(&new_doc, key, field)?,
            "Pool Router 修改的供应商字段已被后续改动，未覆盖；接入备份保留"
        );
    }
    if changed_websockets {
        ensure!(
            bool_toml_field(&cur_doc, key, "supports_websockets")? == new_websockets,
            "WebSocket 配置已被后续修改，未覆盖；接入备份保留"
        );
    }
    ensure!(
        string_field(cur.get("auth").and_then(|v| v.get("OPENAI_API_KEY")))?
            == string_field(new.get("auth").and_then(|v| v.get("OPENAI_API_KEY")))?,
        "供应商凭证已被后续修改，未覆盖；接入备份保留"
    );
    for field in ["base_url", "experimental_bearer_token"] {
        match string_toml_field(&old_doc, key, field)? {
            Some(old_value) => cur_doc["model_providers"][key][field] = value(old_value),
            None => {
                cur_doc["model_providers"][key]
                    .as_table_like_mut()
                    .context("供应商表不存在")?
                    .remove(field);
            }
        }
    }
    if changed_websockets {
        match old_websockets {
            Some(old_value) => {
                cur_doc["model_providers"][key]["supports_websockets"] = value(old_value)
            }
            None => {
                cur_doc["model_providers"][key]
                    .as_table_like_mut()
                    .context("供应商表不存在")?
                    .remove("supports_websockets");
            }
        }
    }
    match string_field(old.get("auth").and_then(|v| v.get("OPENAI_API_KEY")))? {
        Some(old_value) => cur["auth"]["OPENAI_API_KEY"] = Value::String(old_value),
        None => {
            cur.get_mut("auth")
                .and_then(Value::as_object_mut)
                .context("凭证表不存在")?
                .remove("OPENAI_API_KEY");
        }
    }
    cur["config"] = Value::String(cur_doc.to_string());
    Ok(serde_json::to_string(&cur)?)
}

fn bool_toml_field(doc: &DocumentMut, provider: &str, key: &str) -> Result<Option<bool>> {
    match doc
        .get("model_providers")
        .and_then(|v| v.get(provider))
        .and_then(|v| v.get(key))
    {
        None => Ok(None),
        Some(v) => v.as_bool().map(Some).context("供应商布尔配置字段类型无效"),
    }
}

fn write_backup(dir: &Path, backup: &Backup) -> Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    private_write_mode(
        &dir.join(JOURNAL),
        &serde_json::to_vec_pretty(backup)?,
        true,
    )
}

fn private_write(path: &Path, data: &[u8]) -> Result<()> {
    private_write_mode(path, data, false)
}

fn private_write_mode(path: &Path, data: &[u8], exclusive: bool) -> Result<()> {
    let parent = path.parent().context("文件没有父目录")?;
    ensure!(parent.is_dir(), "目标目录不存在");
    let tmp = parent.join(format!(".pool-router-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        if exclusive {
            // Publishing a completed journal must not replace a concurrent caller's backup.
            fs::hard_link(&tmp, path).context("接入备份已存在或无法独占创建，未覆盖原备份")?;
            fs::remove_file(&tmp)?;
        } else {
            fs::rename(&tmp, path)?;
        }
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    const TARGET: &str = "http://127.0.0.1:10813/v1";
    fn fixture(cc: bool) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        fs::create_dir(home.join(".codex")).unwrap();
        let config = format!(
            "# keep comment\nmodel = \"gpt-6-sol\"\nmodel_provider = \"custom\"\n[model_providers.custom]\nname = \"Original\"\nbase_url = \"{}\"\nwire_api = \"responses\"\n",
            if cc {
                "http://127.0.0.1:10811/v1"
            } else {
                "https://example.invalid/v1"
            }
        );
        fs::write(home.join(".codex/config.toml"), config).unwrap();
        if cc {
            fs::create_dir(home.join(".cc-switch")).unwrap();
            let db = Connection::open(home.join(".cc-switch/cc-switch.db")).unwrap();
            db.execute_batch("CREATE TABLE providers(id TEXT PRIMARY KEY,app_type TEXT,name TEXT,settings_config TEXT,is_current INTEGER,meta TEXT); CREATE TABLE proxy_config(app_type TEXT,listen_port INTEGER,proxy_enabled INTEGER,enabled INTEGER); INSERT INTO proxy_config VALUES('codex',10811,1,1);").unwrap();
            let provider = serde_json::json!({"auth":{"OPENAI_API_KEY":"original-secret"},"config":"model_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"http://127.0.0.1:10812/v1\"\nwire_api = \"responses\"\n"});
            db.execute("INSERT INTO providers VALUES('wiretap-original','codex','Original wiretap',?1,1,'{}')",[provider.to_string()]).unwrap();
        }
        let state = home.join("state");
        (dir, state)
    }

    #[test]
    fn cc_connect_restore_preserves_client_and_later_unrelated_edits() {
        let (dir, state) = fixture(true);
        let client_before = fs::read(dir.path().join(".codex/config.toml")).unwrap();
        connect_cc(
            TARGET,
            "local-token",
            &state,
            &dir.path().join(".cc-switch/cc-switch.db"),
        )
        .unwrap();
        assert_eq!(
            client_before,
            fs::read(dir.path().join(".codex/config.toml")).unwrap()
        );
        assert!(!inspect_at(TARGET, dir.path()).unwrap().configured);
        let db = Connection::open(dir.path().join(".cc-switch/cc-switch.db")).unwrap();
        let row = current_provider(&db).unwrap().unwrap();
        let mut settings: Value = serde_json::from_str(&row.settings).unwrap();
        settings["extra"] = Value::String("keep".into());
        db.execute(
            "UPDATE providers SET settings_config=?",
            [settings.to_string()],
        )
        .unwrap();
        restore(&state).unwrap();
        let restored: Value =
            serde_json::from_str(&current_provider(&db).unwrap().unwrap().settings).unwrap();
        assert_eq!(restored["extra"], "keep");
        assert_eq!(restored["auth"]["OPENAI_API_KEY"], "original-secret");
        assert!(!inspect_at(TARGET, dir.path()).unwrap().configured);
    }

    #[test]
    fn cc_restore_conflict_does_not_overwrite_user_secret() {
        let (dir, state) = fixture(true);
        connect_cc(
            TARGET,
            "local-token",
            &state,
            &dir.path().join(".cc-switch/cc-switch.db"),
        )
        .unwrap();
        let db = Connection::open(dir.path().join(".cc-switch/cc-switch.db")).unwrap();
        let mut settings: Value =
            serde_json::from_str(&current_provider(&db).unwrap().unwrap().settings).unwrap();
        settings["auth"]["OPENAI_API_KEY"] = Value::String("later-user-secret".into());
        db.execute(
            "UPDATE providers SET settings_config=?",
            [settings.to_string()],
        )
        .unwrap();
        assert!(restore(&state).is_err());
        assert!(state.join(JOURNAL).is_file());
        assert_eq!(
            current_provider(&db).unwrap().unwrap().settings,
            settings.to_string()
        );
    }

    #[test]
    fn direct_connect_restore_preserves_comments_and_new_user_config() {
        let (dir, state) = fixture(false);
        connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        let path = dir.path().join(".codex/config.toml");
        let mut doc = document(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            doc["model_providers"][PROVIDER]["supports_websockets"].as_bool(),
            Some(false)
        );
        doc["user_added"] = value(true);
        fs::write(&path, doc.to_string()).unwrap();
        restore(&state).unwrap();
        let restored = fs::read_to_string(path).unwrap();
        assert!(restored.contains("# keep comment"));
        let doc = document(&restored).unwrap();
        assert_eq!(doc["model_provider"].as_str(), Some("custom"));
        assert_eq!(doc["user_added"].as_bool(), Some(true));
        assert!(doc["model_providers"].get(PROVIDER).is_none());
    }

    #[test]
    fn refuses_loop_and_does_not_replace_first_backup() {
        let (dir, state) = fixture(true);
        assert!(
            connect_cc(
                "http://127.0.0.1:10811/v1",
                "local-token",
                &state,
                &dir.path().join(".cc-switch/cc-switch.db")
            )
            .is_err()
        );
        connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        let original = fs::read(state.join(JOURNAL)).unwrap();
        assert!(connect_at(TARGET, "next-token", &state, dir.path()).is_err());
        assert_eq!(original, fs::read(state.join(JOURNAL)).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(state.join(JOURNAL))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn cc_websocket_setting_is_disabled_and_original_value_restored() {
        for original in [None, Some(true), Some(false)] {
            let (dir, state) = fixture(true);
            let db = Connection::open(dir.path().join(".cc-switch/cc-switch.db")).unwrap();
            let (mut settings, mut doc) =
                settings_document(&current_provider(&db).unwrap().unwrap().settings).unwrap();
            if let Some(original) = original {
                doc["model_providers"]["custom"]["supports_websockets"] = value(original);
            }
            settings["config"] = Value::String(doc.to_string());
            db.execute(
                "UPDATE providers SET settings_config=?",
                [settings.to_string()],
            )
            .unwrap();
            connect_cc(
                TARGET,
                "local-token",
                &state,
                &dir.path().join(".cc-switch/cc-switch.db"),
            )
            .unwrap();
            let (mut installed, installed_doc) =
                settings_document(&current_provider(&db).unwrap().unwrap().settings).unwrap();
            assert_eq!(
                bool_toml_field(&installed_doc, "custom", "supports_websockets").unwrap(),
                Some(false)
            );
            installed["unrelated"] = Value::Bool(true);
            db.execute(
                "UPDATE providers SET settings_config=?",
                [installed.to_string()],
            )
            .unwrap();
            restore(&state).unwrap();
            let (restored, restored_doc) =
                settings_document(&current_provider(&db).unwrap().unwrap().settings).unwrap();
            assert_eq!(
                bool_toml_field(&restored_doc, "custom", "supports_websockets").unwrap(),
                original
            );
            assert_eq!(restored["unrelated"], true);
        }
    }

    #[test]
    fn cc_websocket_user_change_blocks_restore_without_overwrite() {
        let (dir, state) = fixture(true);
        connect_cc(
            TARGET,
            "local-token",
            &state,
            &dir.path().join(".cc-switch/cc-switch.db"),
        )
        .unwrap();
        let db = Connection::open(dir.path().join(".cc-switch/cc-switch.db")).unwrap();
        let (mut settings, mut doc) =
            settings_document(&current_provider(&db).unwrap().unwrap().settings).unwrap();
        doc["model_providers"]["custom"]["supports_websockets"] = value(true);
        settings["config"] = Value::String(doc.to_string());
        db.execute(
            "UPDATE providers SET settings_config=?",
            [settings.to_string()],
        )
        .unwrap();
        assert!(restore(&state).is_err());
        assert!(state.join(JOURNAL).exists());
        assert_eq!(
            current_provider(&db).unwrap().unwrap().settings,
            settings.to_string()
        );
    }

    #[test]
    fn default_direct_connect_ignores_existing_cc_and_keeps_auth_file() {
        let (dir, state) = fixture(true);
        let database = dir.path().join(".cc-switch/cc-switch.db");
        let before_db = fs::read(&database).unwrap();
        let auth = dir.path().join(".codex/auth.json");
        fs::write(&auth, b"{\"OPENAI_API_KEY\":\"unchanged-auth-fixture\"}").unwrap();
        let before_auth = fs::read(&auth).unwrap();
        connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        assert_eq!(before_db, fs::read(&database).unwrap());
        assert_eq!(before_auth, fs::read(&auth).unwrap());
        assert!(matches!(read_backup(&state).unwrap(), Backup::Codex { .. }));
        assert!(inspect_at(TARGET, dir.path()).unwrap().configured);
        let doc =
            document(&fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap()).unwrap();
        assert_eq!(
            active_endpoint(&doc).unwrap(),
            (PROVIDER.into(), TARGET.into())
        );
        assert_eq!(
            doc["model_providers"]["custom"]["base_url"].as_str(),
            Some("http://127.0.0.1:10811/v1")
        );
    }

    #[test]
    fn direct_connect_and_inspect_do_not_read_even_invalid_cc_database() {
        let (dir, state) = fixture(true);
        let database = dir.path().join(".cc-switch/cc-switch.db");
        fs::write(&database, b"deliberately not a SQLite database").unwrap();
        assert!(!inspect_at(TARGET, dir.path()).unwrap().configured);
        connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        assert!(inspect_at(TARGET, dir.path()).unwrap().configured);
        assert_eq!(
            fs::read(database).unwrap(),
            b"deliberately not a SQLite database"
        );
    }

    #[test]
    fn direct_idempotence_preserves_backup_and_unrelated_fields_then_restores() {
        let (dir, state) = fixture(false);
        connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        let backup = fs::read(state.join(JOURNAL)).unwrap();
        let path = dir.path().join(".codex/config.toml");
        let mut doc = document(&fs::read_to_string(&path).unwrap()).unwrap();
        doc["user_added"] = value(true);
        doc["model_providers"][PROVIDER]["stream_idle_timeout_ms"] = value(123_456);
        fs::write(&path, doc.to_string()).unwrap();
        let before = fs::read(&path).unwrap();
        connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        assert_eq!(backup, fs::read(state.join(JOURNAL)).unwrap());
        assert_eq!(before, fs::read(&path).unwrap());
        restore(&state).unwrap();
        let doc = document(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(doc["model_provider"].as_str(), Some("custom"));
        assert_eq!(doc["user_added"].as_bool(), Some(true));
        assert_eq!(
            doc["model_providers"][PROVIDER]["stream_idle_timeout_ms"].as_integer(),
            Some(123_456)
        );
        assert!(doc["model_providers"][PROVIDER].get("base_url").is_none());
    }

    #[test]
    fn direct_connect_rejects_managed_field_or_token_conflicts() {
        let (dir, state) = fixture(false);
        connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        assert!(connect_at(TARGET, "changed-token", &state, dir.path()).is_err());
        let path = dir.path().join(".codex/config.toml");
        let mut doc = document(&fs::read_to_string(&path).unwrap()).unwrap();
        doc["model_providers"][PROVIDER]["supports_websockets"] = value(true);
        fs::write(&path, doc.to_string()).unwrap();
        let changed = fs::read(&path).unwrap();
        assert!(connect_at(TARGET, "local-token", &state, dir.path()).is_err());
        assert!(restore(&state).is_err());
        assert_eq!(changed, fs::read(path).unwrap());
        assert!(state.join(JOURNAL).exists());
    }

    #[test]
    fn migrates_old_cc_backup_without_changing_current_cc_selection() {
        let (dir, state) = fixture(true);
        let database = dir.path().join(".cc-switch/cc-switch.db");
        let db = Connection::open(&database).unwrap();
        let original_settings = current_provider(&db).unwrap().unwrap().settings;
        connect_cc(TARGET, "local-token", &state, &database).unwrap();
        db.execute("UPDATE providers SET is_current=0", []).unwrap();
        db.execute(
            "INSERT INTO providers VALUES('new-current','codex','User choice',?1,1,'{}')",
            [&original_settings],
        )
        .unwrap();
        let message = connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        assert!(message.contains("已恢复并归档"));
        assert_eq!(current_provider(&db).unwrap().unwrap().id, "new-current");
        let restored: String = db
            .query_row(
                "SELECT settings_config FROM providers WHERE id='wiretap-original'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(restored, original_settings);
        assert!(matches!(read_backup(&state).unwrap(), Backup::Codex { .. }));
        assert_eq!(
            fs::read_dir(&state)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e
                    .file_name()
                    .to_string_lossy()
                    .starts_with("integration-restored-"))
                .count(),
            1
        );
        assert!(inspect_at(TARGET, dir.path()).unwrap().configured);
        restore(&state).unwrap();
        assert_eq!(current_provider(&db).unwrap().unwrap().id, "new-current");
        assert!(!inspect_at(TARGET, dir.path()).unwrap().configured);
    }

    #[test]
    fn legacy_migration_conflict_leaves_codex_and_backup_untouched() {
        let (dir, state) = fixture(true);
        let database = dir.path().join(".cc-switch/cc-switch.db");
        connect_cc(TARGET, "local-token", &state, &database).unwrap();
        let before = fs::read(dir.path().join(".codex/config.toml")).unwrap();
        let backup = fs::read(state.join(JOURNAL)).unwrap();
        let db = Connection::open(&database).unwrap();
        let mut settings: Value =
            serde_json::from_str(&current_provider(&db).unwrap().unwrap().settings).unwrap();
        settings["auth"]["OPENAI_API_KEY"] = Value::String("user-modified-fixture".into());
        db.execute(
            "UPDATE providers SET settings_config=?",
            [settings.to_string()],
        )
        .unwrap();
        assert!(connect_at(TARGET, "local-token", &state, dir.path()).is_err());
        assert_eq!(
            before,
            fs::read(dir.path().join(".codex/config.toml")).unwrap()
        );
        assert_eq!(backup, fs::read(state.join(JOURNAL)).unwrap());
    }

    fn monitor_fixture() -> (tempfile::TempDir, PathBuf) {
        let (dir, state) = fixture(true);
        let db = Connection::open(dir.path().join(".cc-switch/cc-switch.db")).unwrap();
        let (mut settings, mut doc) =
            settings_document(&current_provider(&db).unwrap().unwrap().settings).unwrap();
        doc["model"] = value("gpt-6-sol");
        doc["model_reasoning_effort"] = value("high");
        doc["model_providers"]["custom"]["base_url"] = value("https://anyrouter.top/v1");
        doc["model_providers"]["custom"]["supports_websockets"] = value(true);
        doc["model_providers"]["custom"]["experimental_bearer_token"] =
            value("original-provider-token-fixture");
        settings["config"] = Value::String(doc.to_string());
        db.execute(
            "UPDATE providers SET settings_config=?",
            [settings.to_string()],
        )
        .unwrap();
        (dir, state)
    }

    #[test]
    fn manual_key_monitor_changes_only_url_and_websockets_and_backup_has_no_keys() {
        let (dir, state) = monitor_fixture();
        let path = dir.path().join(".cc-switch/cc-switch.db");
        let db = Connection::open(&path).unwrap();
        let before = current_provider(&db).unwrap().unwrap();
        let (settings_before, doc_before) = settings_document(&before.settings).unwrap();
        let codex_before = fs::read(dir.path().join(".codex/config.toml")).unwrap();
        connect_monitor_at(TARGET, &state, &path).unwrap();
        let after = current_provider(&db).unwrap().unwrap();
        assert_eq!(after.id, before.id);
        let (settings_after, doc_after) = settings_document(&after.settings).unwrap();
        assert_eq!(settings_after["auth"], settings_before["auth"]);
        for field in ["model", "model_reasoning_effort", "model_provider"] {
            assert_eq!(doc_after[field].as_str(), doc_before[field].as_str());
        }
        assert_eq!(
            string_toml_field(&doc_after, "custom", "experimental_bearer_token").unwrap(),
            string_toml_field(&doc_before, "custom", "experimental_bearer_token").unwrap()
        );
        assert_eq!(active_endpoint(&doc_after).unwrap().1, TARGET);
        assert_eq!(
            bool_toml_field(&doc_after, "custom", "supports_websockets").unwrap(),
            Some(false)
        );
        assert_eq!(
            codex_before,
            fs::read(dir.path().join(".codex/config.toml")).unwrap()
        );
        let saved = fs::read_to_string(state.join(MONITOR_JOURNAL)).unwrap();
        assert!(!saved.contains("original-secret"));
        assert!(!saved.contains("original-provider-token-fixture"));
        assert!(!saved.contains("auth"));
        assert!(!saved.contains("settings_config"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(state.join(MONITOR_JOURNAL))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn manual_monitor_restores_after_user_changes_key_model_and_current_selection() {
        let (dir, state) = monitor_fixture();
        let path = dir.path().join(".cc-switch/cc-switch.db");
        connect_monitor_at(TARGET, &state, &path).unwrap();
        let db = Connection::open(&path).unwrap();
        let row = current_provider(&db).unwrap().unwrap();
        let (mut settings, mut doc) = settings_document(&row.settings).unwrap();
        settings["auth"]["OPENAI_API_KEY"] = Value::String("next-manual-key-fixture".into());
        doc["model"] = value("gpt-6-astra");
        doc["model_reasoning_effort"] = value("xhigh");
        doc["model_providers"]["custom"]["experimental_bearer_token"] =
            value("next-provider-token-fixture");
        doc["model_providers"]["custom"]["user_added"] = value(true);
        settings["config"] = Value::String(doc.to_string());
        db.execute(
            "UPDATE providers SET settings_config=?, is_current=0",
            [settings.to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO providers VALUES('other-current','codex','Other user choice',?1,1,'{}')",
            [settings.to_string()],
        )
        .unwrap();
        // The other provider is unrelated; the monitor restore only follows its journal's id.
        restore_legacy_monitor(&state).unwrap();
        assert_eq!(current_provider(&db).unwrap().unwrap().id, "other-current");
        let restored: String = db
            .query_row(
                "SELECT settings_config FROM providers WHERE id=?",
                [&row.id],
                |r| r.get(0),
            )
            .unwrap();
        let (settings, doc) = settings_document(&restored).unwrap();
        assert_eq!(
            settings["auth"]["OPENAI_API_KEY"],
            "next-manual-key-fixture"
        );
        assert_eq!(doc["model"].as_str(), Some("gpt-6-astra"));
        assert_eq!(doc["model_reasoning_effort"].as_str(), Some("xhigh"));
        assert_eq!(
            string_toml_field(&doc, "custom", "experimental_bearer_token")
                .unwrap()
                .as_deref(),
            Some("next-provider-token-fixture")
        );
        assert_eq!(
            doc["model_providers"]["custom"]["user_added"].as_bool(),
            Some(true)
        );
        assert_eq!(active_endpoint(&doc).unwrap().1, "https://anyrouter.top/v1");
        assert_eq!(
            bool_toml_field(&doc, "custom", "supports_websockets").unwrap(),
            Some(true)
        );
        assert!(!state.join(MONITOR_JOURNAL).exists());
    }

    #[test]
    fn manual_monitor_same_target_is_idempotent_after_key_change_and_rejects_conflict() {
        let (dir, state) = monitor_fixture();
        let path = dir.path().join(".cc-switch/cc-switch.db");
        connect_monitor_at(TARGET, &state, &path).unwrap();
        let backup = fs::read(state.join(MONITOR_JOURNAL)).unwrap();
        let db = Connection::open(&path).unwrap();
        let mut settings: Value =
            serde_json::from_str(&current_provider(&db).unwrap().unwrap().settings).unwrap();
        settings["auth"]["OPENAI_API_KEY"] = Value::String("next-key-fixture".into());
        db.execute(
            "UPDATE providers SET settings_config=?",
            [settings.to_string()],
        )
        .unwrap();
        connect_monitor_at(TARGET, &state, &path).unwrap();
        assert_eq!(
            settings.to_string(),
            current_provider(&db).unwrap().unwrap().settings
        );
        assert_eq!(backup, fs::read(state.join(MONITOR_JOURNAL)).unwrap());
        let mut doc = document(settings["config"].as_str().unwrap()).unwrap();
        doc["model_providers"]["custom"]["base_url"] = value("https://example.invalid/v1");
        settings["config"] = Value::String(doc.to_string());
        db.execute(
            "UPDATE providers SET settings_config=?",
            [settings.to_string()],
        )
        .unwrap();
        assert!(connect_monitor_at(TARGET, &state, &path).is_err());
        assert!(restore_legacy_monitor(&state).is_err());
        assert_eq!(
            settings.to_string(),
            current_provider(&db).unwrap().unwrap().settings
        );
        assert_eq!(backup, fs::read(state.join(MONITOR_JOURNAL)).unwrap());
    }

    #[test]
    fn manual_monitor_refuses_proxy_loops_unknown_local_sources_and_missing_databases() {
        let (dir, state) = monitor_fixture();
        let path = dir.path().join(".cc-switch/cc-switch.db");
        let before = fs::read(&path).unwrap();
        assert!(connect_monitor_at("http://127.0.0.1:10811/v1", &state, &path).is_err());
        assert_eq!(before, fs::read(&path).unwrap());
        let db = Connection::open(&path).unwrap();
        let (mut settings, mut doc) =
            settings_document(&current_provider(&db).unwrap().unwrap().settings).unwrap();
        doc["model_providers"]["custom"]["base_url"] = value("http://127.0.0.1:10812/v1");
        settings["config"] = Value::String(doc.to_string());
        db.execute(
            "UPDATE providers SET settings_config=?",
            [settings.to_string()],
        )
        .unwrap();
        assert!(connect_monitor_at(TARGET, &state, &path).is_err());
        assert!(!state.join(MONITOR_JOURNAL).exists());
        let missing = dir.path().join("missing.db");
        assert!(connect_monitor_at(TARGET, &state, &missing).is_err());
        assert!(!missing.exists());
    }

    #[test]
    fn restore_preflights_direct_and_manual_monitor_before_changing_either() {
        let (dir, state) = monitor_fixture();
        let database = dir.path().join(".cc-switch/cc-switch.db");
        connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        connect_monitor_at(TARGET, &state, &database).unwrap();
        let codex = dir.path().join(".codex/config.toml");
        let before_codex = fs::read(&codex).unwrap();
        let direct_backup = fs::read(state.join(JOURNAL)).unwrap();
        let monitor_backup = fs::read(state.join(MONITOR_JOURNAL)).unwrap();
        let db = Connection::open(&database).unwrap();
        let (mut settings, mut doc) =
            settings_document(&current_provider(&db).unwrap().unwrap().settings).unwrap();
        doc["model_providers"]["custom"]["base_url"] = value("https://example.invalid/v1");
        settings["config"] = Value::String(doc.to_string());
        db.execute(
            "UPDATE providers SET settings_config=?",
            [settings.to_string()],
        )
        .unwrap();
        assert!(restore(&state).is_err());
        assert_eq!(before_codex, fs::read(&codex).unwrap());
        assert_eq!(direct_backup, fs::read(state.join(JOURNAL)).unwrap());
        assert_eq!(
            monitor_backup,
            fs::read(state.join(MONITOR_JOURNAL)).unwrap()
        );

        doc["model_providers"]["custom"]["base_url"] = value(TARGET);
        settings["config"] = Value::String(doc.to_string());
        settings["auth"]["OPENAI_API_KEY"] = Value::String("post-install-key-fixture".into());
        db.execute(
            "UPDATE providers SET settings_config=?",
            [settings.to_string()],
        )
        .unwrap();
        let mut config = document(&String::from_utf8(before_codex.clone()).unwrap()).unwrap();
        config["model_providers"][PROVIDER]["experimental_bearer_token"] =
            value("user-token-fixture");
        fs::write(&codex, config.to_string()).unwrap();
        assert!(restore(&state).is_err());
        assert_eq!(
            settings.to_string(),
            current_provider(&db).unwrap().unwrap().settings
        );
        assert!(state.join(JOURNAL).exists() && state.join(MONITOR_JOURNAL).exists());

        fs::write(&codex, before_codex).unwrap();
        restore(&state).unwrap();
        let (restored, doc) =
            settings_document(&current_provider(&db).unwrap().unwrap().settings).unwrap();
        assert_eq!(
            restored["auth"]["OPENAI_API_KEY"],
            "post-install-key-fixture"
        );
        assert_eq!(active_endpoint(&doc).unwrap().1, "https://anyrouter.top/v1");
        assert!(!state.join(JOURNAL).exists() && !state.join(MONITOR_JOURNAL).exists());
        assert!(!inspect_at(TARGET, dir.path()).unwrap().configured);
    }

    #[test]
    fn direct_connect_keeps_independent_monitor_and_total_restore_handles_monitor_only() {
        let (dir, state) = monitor_fixture();
        let database = dir.path().join(".cc-switch/cc-switch.db");
        connect_monitor_at(TARGET, &state, &database).unwrap();
        let monitor_backup = fs::read(state.join(MONITOR_JOURNAL)).unwrap();
        let database_before = fs::read(&database).unwrap();
        connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        assert_eq!(database_before, fs::read(&database).unwrap());
        assert_eq!(
            monitor_backup,
            fs::read(state.join(MONITOR_JOURNAL)).unwrap()
        );
        restore_primary(&state).unwrap();
        assert!(!state.join(JOURNAL).exists());
        restore(&state).unwrap();
        assert!(!state.join(MONITOR_JOURNAL).exists());
    }

    #[test]
    fn reconnect_from_original_provider_keeps_current_model_new_keys_and_original_backup() {
        let (dir, state) = monitor_fixture();
        connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        let backup = fs::read(state.join(JOURNAL)).unwrap();
        let path = dir.path().join(".codex/config.toml");
        let mut doc = document(&fs::read_to_string(&path).unwrap()).unwrap();
        doc["model_provider"] = value("custom");
        doc["model"] = value("gpt-6-astra");
        doc["model_reasoning_effort"] = value("xhigh");
        doc["model_providers"]["custom"]["experimental_bearer_token"] = value("new-cc-key-fixture");
        doc["model_providers"][PROVIDER]["stream_idle_timeout_ms"] = value(234_567);
        fs::write(&path, doc.to_string()).unwrap();
        let auth = dir.path().join(".codex/auth.json");
        fs::write(&auth, b"{\"OPENAI_API_KEY\":\"new-cc-auth-fixture\"}").unwrap();
        let auth_before = fs::read(&auth).unwrap();
        let db_path = dir.path().join(".cc-switch/cc-switch.db");
        let db = Connection::open(&db_path).unwrap();
        let mut settings: Value =
            serde_json::from_str(&current_provider(&db).unwrap().unwrap().settings).unwrap();
        settings["auth"]["OPENAI_API_KEY"] = Value::String("new-cc-database-key-fixture".into());
        db.execute(
            "UPDATE providers SET settings_config=?",
            [settings.to_string()],
        )
        .unwrap();
        let db_before = fs::read(&db_path).unwrap();

        connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        let doc = document(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["model_provider"].as_str(), Some(PROVIDER));
        assert_eq!(doc["model"].as_str(), Some("gpt-6-astra"));
        assert_eq!(doc["model_reasoning_effort"].as_str(), Some("xhigh"));
        assert_eq!(
            string_toml_field(&doc, "custom", "experimental_bearer_token")
                .unwrap()
                .as_deref(),
            Some("new-cc-key-fixture")
        );
        assert_eq!(
            doc["model_providers"][PROVIDER]["stream_idle_timeout_ms"].as_integer(),
            Some(234_567)
        );
        assert_eq!(backup, fs::read(state.join(JOURNAL)).unwrap());
        assert_eq!(auth_before, fs::read(&auth).unwrap());
        assert_eq!(db_before, fs::read(&db_path).unwrap());
        restore(&state).unwrap();
        let restored = document(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(restored["model_provider"].as_str(), Some("custom"));
        assert_eq!(restored["model"].as_str(), Some("gpt-6-astra"));
        assert_eq!(
            string_toml_field(&restored, "custom", "experimental_bearer_token")
                .unwrap()
                .as_deref(),
            Some("new-cc-key-fixture")
        );
        assert_eq!(auth_before, fs::read(auth).unwrap());
        assert_eq!(db_before, fs::read(db_path).unwrap());
    }

    #[test]
    fn reconnect_recreates_removed_managed_table_without_overwriting_current_config() {
        let (dir, state) = fixture(false);
        connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        let backup = fs::read(state.join(JOURNAL)).unwrap();
        let path = dir.path().join(".codex/config.toml");
        let mut doc = document(&fs::read_to_string(&path).unwrap()).unwrap();
        doc["model_provider"] = value("custom");
        doc["model"] = value("gpt-6-astra");
        doc["model_providers"]
            .as_table_like_mut()
            .unwrap()
            .remove(PROVIDER);
        doc["model_providers"]["custom"]["experimental_bearer_token"] =
            value("rewritten-provider-key-fixture");
        doc["user_added"] = value(true);
        fs::write(&path, doc.to_string()).unwrap();
        connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        let doc = document(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["model_provider"].as_str(), Some(PROVIDER));
        assert_eq!(active_endpoint(&doc).unwrap().1, TARGET);
        assert_eq!(doc["model"].as_str(), Some("gpt-6-astra"));
        assert_eq!(doc["user_added"].as_bool(), Some(true));
        assert_eq!(
            string_toml_field(&doc, "custom", "experimental_bearer_token")
                .unwrap()
                .as_deref(),
            Some("rewritten-provider-key-fixture")
        );
        assert_eq!(backup, fs::read(state.join(JOURNAL)).unwrap());
    }

    #[test]
    fn original_default_without_provider_supports_roundtrip_after_table_removal() {
        let (dir, state) = fixture(false);
        let path = dir.path().join(".codex/config.toml");
        let mut original = document(&fs::read_to_string(&path).unwrap()).unwrap();
        original.remove("model_provider");
        original.remove("model_providers");
        fs::write(&path, original.to_string()).unwrap();
        connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        let backup = fs::read(state.join(JOURNAL)).unwrap();
        let mut doc = document(&fs::read_to_string(&path).unwrap()).unwrap();
        doc.remove("model_provider");
        doc.remove("model_providers");
        doc["model"] = value("gpt-6-astra");
        fs::write(&path, doc.to_string()).unwrap();
        connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
        assert_eq!(backup, fs::read(state.join(JOURNAL)).unwrap());
        assert!(inspect_at(TARGET, dir.path()).unwrap().configured);
        restore(&state).unwrap();
        let restored = document(&fs::read_to_string(path).unwrap()).unwrap();
        assert!(restored.get("model_provider").is_none());
        assert!(restored.get("model_providers").is_none());
        assert_eq!(restored["model"].as_str(), Some("gpt-6-astra"));
    }

    #[test]
    fn restore_accepts_already_original_top_with_or_without_managed_table() {
        for remove_table in [false, true] {
            let (dir, state) = fixture(false);
            connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
            let path = dir.path().join(".codex/config.toml");
            let mut doc = document(&fs::read_to_string(&path).unwrap()).unwrap();
            doc["model_provider"] = value("custom");
            doc["model"] = value("gpt-6-astra");
            doc["model_providers"]["custom"]["experimental_bearer_token"] =
                value("latest-key-fixture");
            if remove_table {
                doc["model_providers"]
                    .as_table_like_mut()
                    .unwrap()
                    .remove(PROVIDER);
            }
            fs::write(&path, doc.to_string()).unwrap();
            let current = fs::read(&path).unwrap();
            restore(&state).unwrap();
            let restored = document(&fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(restored["model_provider"].as_str(), Some("custom"));
            assert_eq!(restored["model"].as_str(), Some("gpt-6-astra"));
            assert_eq!(
                string_toml_field(&restored, "custom", "experimental_bearer_token")
                    .unwrap()
                    .as_deref(),
                Some("latest-key-fixture")
            );
            assert!(!has_direct_table(&restored));
            assert!(!state.join(JOURNAL).exists());
            if remove_table {
                assert_eq!(current, fs::read(path).unwrap());
            }
        }
    }

    #[test]
    fn reconnect_rejects_unknown_top_and_modified_managed_token_or_url() {
        for conflict in [
            "unknown_top",
            "unknown_top_without_table",
            "experimental_bearer_token",
            "base_url",
        ] {
            let (dir, state) = fixture(false);
            connect_at(TARGET, "local-token", &state, dir.path()).unwrap();
            let path = dir.path().join(".codex/config.toml");
            let mut doc = document(&fs::read_to_string(&path).unwrap()).unwrap();
            doc["model_provider"] = value("custom");
            match conflict {
                "unknown_top" => doc["model_provider"] = value("user_chosen_unknown_provider"),
                "unknown_top_without_table" => {
                    doc["model_provider"] = value("user_chosen_unknown_provider");
                    doc["model_providers"]
                        .as_table_like_mut()
                        .unwrap()
                        .remove(PROVIDER);
                }
                field => {
                    doc["model_providers"][PROVIDER][field] = value("user-modified-field-fixture")
                }
            }
            fs::write(&path, doc.to_string()).unwrap();
            let current = fs::read(&path).unwrap();
            let backup = fs::read(state.join(JOURNAL)).unwrap();
            assert!(
                connect_at(TARGET, "local-token", &state, dir.path()).is_err(),
                "{conflict}"
            );
            assert!(restore(&state).is_err(), "{conflict}");
            assert_eq!(current, fs::read(path).unwrap());
            assert_eq!(backup, fs::read(state.join(JOURNAL)).unwrap());
        }
    }
}
