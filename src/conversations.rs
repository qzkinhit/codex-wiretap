//! 按请求携带的明确 ID 读取本地对话标题。标题只用于监控展示，不读取消息正文。
use rusqlite::{Connection, OpenFlags, params_from_iter};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

const CACHE_TTL: Duration = Duration::from_secs(5);
const MAX_CACHE_ENTRIES: usize = 1024;
const MAX_LOOKUP_IDS: usize = 1024;
const QUERY_BATCH_SIZE: usize = 200;
const MAX_DATABASES: usize = 8;
const MAX_TITLE_CHARS: usize = 160;
const BUSY_TIMEOUT: Duration = Duration::from_millis(25);

struct CacheEntry {
    checked_at: Instant,
    title: Option<String>,
}

pub struct ConversationIndex {
    codex_home: PathBuf,
    cache: Mutex<HashMap<String, CacheEntry>>,
}

impl ConversationIndex {
    pub fn new(codex_home: PathBuf) -> Self {
        Self {
            codex_home,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn from_environment() -> Self {
        Self::new(crate::config::codex_home_dir().unwrap_or_default())
    }

    /// 应从阻塞线程调用。数据库不可用或索引正忙时返回已有标题或空映射。
    pub fn lookup(&self, ids: &[String]) -> HashMap<String, String> {
        let mut seen = HashSet::new();
        let requested: Vec<String> = ids
            .iter()
            .filter_map(|value| normalize_id(value))
            .filter(|id| seen.insert(id.clone()))
            .take(MAX_LOOKUP_IDS)
            .collect();
        if requested.is_empty() {
            return HashMap::new();
        }

        // 索引是展示辅助信息；并发查询或锁中毒不应阻塞请求代理。
        let Ok(mut cache) = self.cache.try_lock() else {
            return HashMap::new();
        };
        let now = Instant::now();
        cache.retain(|_, entry| now.duration_since(entry.checked_at) < CACHE_TTL);
        let missing: Vec<String> = requested
            .iter()
            .filter(|id| !cache.contains_key(*id))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let mut found = HashMap::new();
            for path in database_paths(&self.codex_home) {
                let pending: Vec<&String> = missing
                    .iter()
                    .filter(|id| !found.contains_key(*id))
                    .collect();
                if pending.is_empty() {
                    break;
                }
                // 缺列、锁库、损坏和升级中的数据库均允许回退。
                if let Ok(titles) = read_titles(&path, &pending) {
                    found.extend(titles);
                }
            }
            // 给正、负结果使用同样的 TTL，避免每次刷新重复扫描缺失 ID。
            let checked_at = Instant::now();
            for id in missing {
                cache.insert(
                    id.clone(),
                    CacheEntry {
                        checked_at,
                        title: found.remove(&id).flatten(),
                    },
                );
            }
            if cache.len() > MAX_CACHE_ENTRIES {
                cache.retain(|id, _| seen.contains(id));
            }
        }
        requested
            .into_iter()
            .filter_map(|id| {
                let title = cache.get(&id)?.title.clone()?;
                Some((id, title))
            })
            .collect()
    }
}

fn normalize_id(value: &str) -> Option<String> {
    if value.len() != 36 {
        return None;
    }
    let id = uuid::Uuid::parse_str(value).ok()?;
    (!id.is_nil()).then(|| id.hyphenated().to_string())
}

fn clean_title(value: &str) -> Option<String> {
    let normalized: String = value
        .chars()
        .map(|c| {
            if c.is_control()
                || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                ' '
            } else {
                c
            }
        })
        .collect();
    let title: String = normalized
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect();
    let title = title.trim_end();
    (!title.is_empty()).then(|| title.to_owned())
}

fn database_paths(home: &Path) -> Vec<PathBuf> {
    // 用户目录未设置时不把当前工作目录当成 Codex 数据目录。
    if home.as_os_str().is_empty() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(home) else {
        return Vec::new();
    };
    let mut databases = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(version) = name
            .to_str()
            .and_then(|name| name.strip_prefix("state_"))
            .and_then(|name| name.strip_suffix(".sqlite"))
            .filter(|version| !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|version| version.parse::<u64>().ok())
        else {
            continue;
        };
        databases.push((version, entry.path()));
        databases.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
        databases.truncate(MAX_DATABASES);
    }
    databases.into_iter().map(|(_, path)| path).collect()
}

fn open_readonly(path: &Path) -> rusqlite::Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(BUSY_TIMEOUT)?;
    db.pragma_update(None, "query_only", true)?;
    Ok(db)
}

fn read_titles(path: &Path, ids: &[&String]) -> rusqlite::Result<HashMap<String, Option<String>>> {
    let db = open_readonly(path)?;
    let mut schema = db.prepare("PRAGMA table_info(threads)")?;
    let columns: HashSet<String> = schema
        .query_map([], |row| row.get(1))?
        .collect::<rusqlite::Result<_>>()?;
    let has_title = columns.contains("title");
    let has_name = columns.contains("name");
    if !columns.contains("id") || (!has_title && !has_name) {
        return Ok(HashMap::new());
    }
    // 标识符完全固定，正文、preview、cwd 均不进入 SELECT。
    let title_field = if has_title {
        "CASE WHEN typeof(\"title\") = 'text' THEN substr(\"title\", 1, 500) ELSE NULL END"
    } else {
        "NULL"
    };
    let name_field = if has_name {
        "CASE WHEN typeof(\"name\") = 'text' THEN substr(\"name\", 1, 500) ELSE NULL END"
    } else {
        "NULL"
    };
    let mut found = HashMap::new();
    for batch in ids.chunks(QUERY_BATCH_SIZE) {
        let placeholders = vec!["?"; batch.len()].join(",");
        let sql = format!(
            "SELECT \"id\", {title_field}, {name_field} FROM threads WHERE \"id\" IN ({placeholders})"
        );
        let mut statement = db.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(batch.iter().copied()))?;
        while let Some(row) = rows.next()? {
            let Ok(id) = row.get::<_, String>(0) else {
                continue;
            };
            let Some(id) = normalize_id(&id) else {
                continue;
            };
            // name 保存界面名称，title 兼容旧 schema；非文本内容不转换成标题。
            let title = row
                .get::<_, String>(2)
                .ok()
                .and_then(|value| clean_title(&value))
                .or_else(|| {
                    row.get::<_, String>(1)
                        .ok()
                        .and_then(|value| clean_title(&value))
                });
            found.insert(id, title);
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::tempdir;

    const FIRST: &str = "11111111-1111-4111-8111-111111111111";
    const SECOND: &str = "22222222-2222-4222-8222-222222222222";
    const THIRD: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

    fn make_db(home: &Path, version: u64) -> Connection {
        let db = Connection::open(home.join(format!("state_{version}.sqlite"))).unwrap();
        db.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT, name TEXT, body TEXT, preview TEXT, cwd TEXT)",
        )
        .unwrap();
        db
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn expire(index: &ConversationIndex) {
        for entry in index.cache.lock().unwrap().values_mut() {
            entry.checked_at = Instant::now() - CACHE_TTL;
        }
    }

    #[test]
    fn exact_ids_and_same_titles_remain_separate() {
        let home = tempdir().unwrap();
        let db = make_db(home.path(), 5);
        for id in [FIRST, SECOND, THIRD] {
            db.execute(
                "INSERT INTO threads (id, title) VALUES (?1, ?2)",
                params![id, "中文对话"],
            )
            .unwrap();
        }
        let index = ConversationIndex::new(home.path().to_path_buf());
        let result = index.lookup(&ids(&[
            FIRST,
            SECOND,
            "11111111-1111-4111-8111",
            "00000000-0000-0000-0000-000000000000",
            "anything' OR 1=1",
        ]));
        assert_eq!(result.len(), 2);
        assert_eq!(result.get(FIRST).map(String::as_str), Some("中文对话"));
        assert_eq!(result.get(SECOND), result.get(FIRST));
        assert!(!result.contains_key(THIRD));
        assert_eq!(index.cache.lock().unwrap().len(), 2);
        let uppercase = index.lookup(&ids(&[&THIRD.to_uppercase()]));
        assert_eq!(uppercase.get(THIRD).map(String::as_str), Some("中文对话"));
    }

    #[test]
    fn rename_and_missing_title_refresh_after_cache_expiry() {
        let home = tempdir().unwrap();
        let db = make_db(home.path(), 5);
        db.execute(
            "INSERT INTO threads (id, title, name) VALUES (?1, '原始长标题', '旧名称')",
            [FIRST],
        )
        .unwrap();
        let index = ConversationIndex::new(home.path().to_path_buf());
        let requested = ids(&[FIRST, SECOND]);
        assert_eq!(index.lookup(&requested).get(FIRST).unwrap(), "旧名称");
        assert!(
            index
                .cache
                .lock()
                .unwrap()
                .get(SECOND)
                .unwrap()
                .title
                .is_none()
        );
        db.execute("UPDATE threads SET name = '新名称' WHERE id = ?1", [FIRST])
            .unwrap();
        db.execute(
            "INSERT INTO threads (id, title) VALUES (?1, '后来创建')",
            [SECOND],
        )
        .unwrap();
        let cached = index.lookup(&requested);
        assert_eq!(cached.get(FIRST).unwrap(), "旧名称");
        assert!(!cached.contains_key(SECOND));
        expire(&index);
        let refreshed = index.lookup(&requested);
        assert_eq!(refreshed.get(FIRST).unwrap(), "新名称");
        assert_eq!(refreshed.get(SECOND).unwrap(), "后来创建");
    }

    #[test]
    fn empty_titles_never_fall_back_to_body_preview_or_cwd() {
        let home = tempdir().unwrap();
        let db = make_db(home.path(), 5);
        db.execute(
            "INSERT INTO threads (id, title, name, body, preview, cwd) VALUES (?1, '  ', NULL, '秘密正文', '秘密摘要', '/秘密目录')",
            [FIRST],
        )
        .unwrap();
        db.execute(
            "INSERT INTO threads (id, title, name) VALUES (?1, NULL, '兼容名称')",
            [SECOND],
        )
        .unwrap();
        let old = make_db(home.path(), 4);
        old.execute(
            "INSERT INTO threads (id, title) VALUES (?1, '已清除的旧标题')",
            [FIRST],
        )
        .unwrap();
        let result =
            ConversationIndex::new(home.path().to_path_buf()).lookup(&ids(&[FIRST, SECOND]));
        assert!(!result.contains_key(FIRST));
        assert_eq!(result.get(SECOND).unwrap(), "兼容名称");
    }

    #[test]
    fn schema_version_order_and_broken_newer_databases_fall_back() {
        let home = tempdir().unwrap();
        let old = make_db(home.path(), 2);
        old.execute(
            "INSERT INTO threads (id, title) VALUES (?1, '旧版')",
            [FIRST],
        )
        .unwrap();
        let recent = make_db(home.path(), 10);
        recent
            .execute(
                "INSERT INTO threads (id, title) VALUES (?1, '新版')",
                [FIRST],
            )
            .unwrap();
        let missing = Connection::open(home.path().join("state_11.sqlite")).unwrap();
        missing
            .execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, body TEXT)")
            .unwrap();
        fs::write(home.path().join("state_12.sqlite"), "not a database").unwrap();
        let no_id = Connection::open(home.path().join("state_13.sqlite")).unwrap();
        no_id
            .execute_batch("CREATE TABLE threads (title TEXT)")
            .unwrap();
        let names = Connection::open(home.path().join("state_9.sqlite")).unwrap();
        names
            .execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, name TEXT)")
            .unwrap();
        names
            .execute("INSERT INTO threads VALUES (?1, '历史名称列')", [SECOND])
            .unwrap();
        fs::write(home.path().join("state_future.sqlite"), "ignored").unwrap();
        let result =
            ConversationIndex::new(home.path().to_path_buf()).lookup(&ids(&[FIRST, SECOND]));
        assert_eq!(result.get(FIRST).unwrap(), "新版");
        assert_eq!(result.get(SECOND).unwrap(), "历史名称列");
    }

    #[test]
    fn locked_and_absent_databases_are_tolerated() {
        let home = tempdir().unwrap();
        let index = ConversationIndex::new(home.path().to_path_buf());
        assert!(index.lookup(&ids(&[FIRST])).is_empty());
        let db = make_db(home.path(), 5);
        db.execute_batch("BEGIN EXCLUSIVE").unwrap();
        expire(&index);
        assert!(index.lookup(&ids(&[FIRST])).is_empty());
        db.execute_batch("ROLLBACK").unwrap();
        let absent = home.path().join("does-not-exist");
        assert!(
            ConversationIndex::new(absent.clone())
                .lookup(&ids(&[FIRST]))
                .is_empty()
        );
        assert!(!absent.exists());
    }

    #[test]
    fn reader_is_readonly_and_does_not_evaluate_body_columns() {
        let home = tempdir().unwrap();
        let path = home.path().join("state_5.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE source (id TEXT PRIMARY KEY, title TEXT);
             CREATE VIEW threads AS SELECT id, title, abs(-9223372036854775808) AS body,
               abs(-9223372036854775808) AS preview, abs(-9223372036854775808) AS cwd FROM source;",
        )
        .unwrap();
        db.execute("INSERT INTO source VALUES (?1, '只读标题')", [FIRST])
            .unwrap();
        assert!(
            db.query_row::<i64, _, _>("SELECT body FROM threads", [], |row| row.get(0))
                .is_err()
        );
        let before = fs::read(&path).unwrap();
        let read = open_readonly(&path).unwrap();
        assert_eq!(
            read.pragma_query_value::<i32, _>(None, "query_only", |row| row.get(0))
                .unwrap(),
            1
        );
        assert!(
            read.execute("UPDATE source SET title = '不允许'", [])
                .is_err()
        );
        // 即使关闭 query_only，连接本身仍以只读模式打开。
        read.pragma_update(None, "query_only", false).unwrap();
        assert!(
            read.execute("UPDATE source SET title = '不允许'", [])
                .is_err()
        );
        let result = ConversationIndex::new(home.path().to_path_buf()).lookup(&ids(&[FIRST]));
        assert_eq!(result.get(FIRST).unwrap(), "只读标题");
        assert_eq!(fs::read(&path).unwrap(), before);
        let nonexistent = home.path().join("state_999.sqlite");
        assert!(open_readonly(&nonexistent).is_err());
        assert!(!nonexistent.exists());
    }

    #[test]
    fn cleans_controls_bidi_whitespace_and_unicode_length() {
        assert_eq!(
            clean_title("  论文\t标题\n\u{202e} 隐藏\u{2066}\u{061c}\u{200f}文字  ").as_deref(),
            Some("论文 标题 隐藏 文字")
        );
        assert!(clean_title("\n\t\u{202e} ").is_none());
        assert_eq!(clean_title(&"中".repeat(180)).unwrap().chars().count(), 160);
    }

    #[test]
    fn lookup_and_cache_are_bounded_with_negative_entries() {
        let home = tempdir().unwrap();
        let index = ConversationIndex::new(home.path().to_path_buf());
        let first: Vec<String> = (1..=MAX_LOOKUP_IDS + 100)
            .map(|value| uuid::Uuid::from_u128(value as u128).to_string())
            .collect();
        assert!(index.lookup(&first).is_empty());
        assert_eq!(index.cache.lock().unwrap().len(), MAX_CACHE_ENTRIES);
        let second: Vec<String> = (3000..3000 + MAX_LOOKUP_IDS)
            .map(|value| uuid::Uuid::from_u128(value as u128).to_string())
            .collect();
        assert!(index.lookup(&second).is_empty());
        let cache = index.cache.lock().unwrap();
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        assert!(cache.contains_key(&second[0]));
        assert!(!cache.contains_key(&first[0]));
    }

    #[test]
    fn queries_multiple_bounded_batches() {
        let home = tempdir().unwrap();
        let mut db = make_db(home.path(), 5);
        let requested: Vec<String> = (1..=QUERY_BATCH_SIZE * 2 + 1)
            .map(|value| uuid::Uuid::from_u128(value as u128).to_string())
            .collect();
        let transaction = db.transaction().unwrap();
        for id in &requested {
            transaction
                .execute(
                    "INSERT INTO threads (id, title) VALUES (?1, '批量标题')",
                    [id],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        let result = ConversationIndex::new(home.path().to_path_buf()).lookup(&requested);
        assert_eq!(result.len(), requested.len());
        assert!(
            requested
                .iter()
                .all(|id| result.get(id).map(String::as_str) == Some("批量标题"))
        );
    }
}
