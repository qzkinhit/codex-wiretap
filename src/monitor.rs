//! 元数据与错误观察。成功正文和工具参数不进入日志；错误消息须由调用方脱敏。
use anyhow::{Context, Result};
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

const MAX_FRAME: usize = 4 * 1024 * 1024;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AttemptRecord {
    pub key_id: String,
    pub status: Option<u16>,
    pub reason: String,
    pub error_message: Option<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayAction {
    pub action: String,
    pub attempt: usize,
    pub elapsed_ms: u64,
    pub count: usize,
    pub before_bytes: Option<u64>,
    pub after_bytes: Option<u64>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestRecord {
    pub id: String,
    pub started_at_ms: u64,
    pub conversation_id: Option<String>,
    pub transport: String,
    /// Empty in older records; callers interpret that as the pool route.
    pub route_mode: String,
    pub requested_model: Option<String>,
    pub effective_model: Option<String>,
    pub response_model: Option<String>,
    pub requested_effort: Option<String>,
    pub effective_effort: Option<String>,
    pub response_effort: Option<String>,
    pub requested_service_tier: Option<String>,
    pub response_service_tier: Option<String>,
    /// 调度器生成的非秘密标识，不得填写 API key。
    pub key_id: Option<String>,
    pub status: Option<u16>,
    pub duration_ms: u64,
    pub ttfb_ms: Option<u64>,
    pub first_output_ms: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    /// Billed cache writes. Disjoint from cached_tokens, both inside input_tokens.
    pub cache_write_tokens: Option<u64>,
    pub cache_write_30m_tokens: Option<u64>,
    pub cache_write_12h_tokens: Option<u64>,
    /// Reported by the upstream as input_tokens + output_tokens. Consistency check only.
    pub total_tokens: Option<u64>,
    /// Already counted inside output_tokens. Never add it again when pricing.
    pub reasoning_tokens: Option<u64>,
    /// The tier the upstream named when it answered with less depth than
    /// the request asked for, kept so the panel can show what was served.
    pub effort_mismatch: Option<String>,
    pub request_bytes: u64,
    pub upstream_request_bytes: Option<u64>,
    pub response_bytes: u64,
    pub has_previous_response_id: bool,
    pub input_items: Option<usize>,
    pub store: Option<bool>,
    pub outcome: String,
    pub attempts: Vec<AttemptRecord>,
    pub gateway_actions: Vec<GatewayAction>,
}

#[derive(Debug, Default, Clone)]
pub struct Observation {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
    pub response_id: Option<String>,
    pub error_code: Option<String>,
    /// Transient upstream error text. Never serialize before credential redaction.
    pub raw_error_message: Option<String>,
    pub retryable: bool,
    pub meaningful_output: bool,
    pub terminal: bool,
    pub completed: bool,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cache_write_30m_tokens: Option<u64>,
    pub cache_write_12h_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub dropped_frames: u64,
    pub malformed_frames: u64,
}

/// Cache writes split by lifetime. The two counts do not overlap and sum to
/// cache_write_tokens, so a missing split must not be inferred from the total.
fn cache_write_ttl(usage: &Value, field: &str) -> Option<u64> {
    usage["input_tokens_details"]["cache_write_tokens_details"][field]
        .as_u64()
        .or_else(|| usage["prompt_tokens_details"]["cache_write_tokens_details"][field].as_u64())
}

fn label(value: &Value) -> Option<String> {
    let s = value.as_str()?;
    if s.is_empty()
        || s.len() > 180
        || s.to_ascii_lowercase().contains("sk-")
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:/@+-".contains(&b))
    {
        return None;
    }
    Some(s.to_owned())
}

fn allowed_error(value: &Value) -> Option<String> {
    let code = value.as_str()?;
    // 未知错误只保留分类，不记录上游任意 message/code 字符串。
    Some(
        match code {
            "rate_limit_exceeded"
            | "too_many_requests"
            | "server_error"
            | "internal_error"
            | "internal_server_error"
            | "overloaded"
            | "overloaded_error"
            | "service_unavailable"
            | "temporarily_unavailable"
            | "upstream_error"
            | "timeout"
            | "request_timeout"
            | "invalid_api_key"
            | "authentication_error"
            | "unauthorized"
            | "permission_denied"
            | "insufficient_quota"
            | "quota_exceeded"
            | "invalid_request_error"
            | "invalid_request"
            | "context_length_exceeded"
            | "model_not_found"
            | "unsupported_model"
            | "previous_response_not_found"
            | "response_not_found"
            | "invalid_encrypted_content"
            | "incompatible_encrypted_content"
            | "resource_not_found"
            | "deployment_not_found"
            | "first_output_timeout"
            | "content_filter"
            | "max_output_tokens"
            | "incomplete" => code,
            _ => "unknown_error",
        }
        .to_owned(),
    )
}

fn retryable(code: &str) -> bool {
    matches!(
        code,
        "rate_limit_exceeded"
            | "too_many_requests"
            | "server_error"
            | "internal_error"
            | "internal_server_error"
            | "overloaded"
            | "overloaded_error"
            | "service_unavailable"
            | "temporarily_unavailable"
            | "upstream_error"
            | "timeout"
            | "request_timeout"
    )
}

fn nonempty(value: &Value) -> bool {
    value.as_str().is_some_and(|v| !v.is_empty())
}

fn meaningful_item(item: &Value) -> bool {
    match item["type"].as_str().unwrap_or("") {
        "function_call" | "custom_tool_call" => {
            nonempty(&item["name"]) || nonempty(&item["arguments"]) || nonempty(&item["input"])
        }
        "computer_call"
        | "web_search_call"
        | "file_search_call"
        | "code_interpreter_call"
        | "image_generation_call"
        | "local_shell_call"
        | "shell_call"
        | "apply_patch_call"
        | "mcp_call"
        | "mcp_approval_request" => true,
        "message" | "reasoning" => {
            item["content"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .any(|p| nonempty(&p["text"]) || nonempty(&p["refusal"]))
            }) || item["summary"]
                .as_array()
                .is_some_and(|items| items.iter().any(|p| nonempty(&p["text"])))
        }
        _ => false,
    }
}

/// 只提取模型、用量、错误与流状态，不保留成功正文或工具参数。
pub fn inspect_json(data: &Value) -> Observation {
    let response = data
        .get("response")
        .filter(|v| v.is_object())
        .unwrap_or(data);
    let kind = data["type"].as_str().unwrap_or("");
    let status = response["status"].as_str().unwrap_or("");
    let error = response
        .get("error")
        .filter(|v| v.is_object())
        .or_else(|| data.get("error").filter(|v| v.is_object()))
        .or_else(|| matches!(kind, "error" | "response.error").then_some(data));
    let error_code = error
        .map(|e| {
            ["code", "type"]
                .iter()
                .filter_map(|k| allowed_error(&e[k]))
                .find(|v| v != "unknown_error")
                .unwrap_or_else(|| "unknown_error".into())
        })
        .or_else(|| {
            (status == "incomplete" || kind == "response.incomplete").then(|| {
                allowed_error(&response["incomplete_details"]["reason"])
                    .unwrap_or_else(|| "incomplete".into())
            })
        })
        .or_else(|| {
            (status == "failed" || kind == "response.failed").then(|| "response_failed".into())
        })
        .or_else(|| (status == "cancelled").then(|| "response_cancelled".into()));
    let terminal = matches!(
        kind,
        "response.completed"
            | "response.done"
            | "response.failed"
            | "response.incomplete"
            | "error"
            | "response.error"
    ) || matches!(status, "completed" | "failed" | "incomplete" | "cancelled")
        || error.is_some();
    let failed = matches!(
        kind,
        "response.failed" | "response.incomplete" | "error" | "response.error"
    ) || matches!(status, "failed" | "incomplete" | "cancelled")
        || error.is_some();
    let completed = !failed
        && (matches!(kind, "response.completed" | "response.done") || status == "completed");
    let is_delta = matches!(
        kind,
        "response.output_text.delta"
            | "response.text.delta"
            | "response.refusal.delta"
            | "response.reasoning_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning.delta"
            | "response.function_call_arguments.delta"
            | "response.custom_tool_call_input.delta"
            | "response.custom_tool_call_arguments.delta"
            | "response.audio.delta"
            | "response.output_audio.delta"
    );
    let meaningful_output = (is_delta && nonempty(&data["delta"]))
        || (matches!(
            kind,
            "response.output_text.done"
                | "response.reasoning_text.done"
                | "response.reasoning_summary_text.done"
        ) && nonempty(&data["text"]))
        || (matches!(
            kind,
            "response.function_call_arguments.done" | "response.custom_tool_call_input.done"
        ) && (nonempty(&data["arguments"]) || nonempty(&data["input"])))
        || meaningful_item(&data["item"])
        || response["output"]
            .as_array()
            .is_some_and(|items| items.iter().any(meaningful_item));
    let usage = &response["usage"];
    let retryable_error = error.is_some_and(|e| {
        ["code", "type"]
            .iter()
            .any(|k| e[k].as_str().is_some_and(retryable))
    });
    Observation {
        model: label(&response["model"]),
        service_tier: label(&response["service_tier"]),
        effort: label(&response["reasoning"]["effort"])
            .or_else(|| label(&response["reasoning_effort"])),
        response_id: label(&response["id"]).or_else(|| label(&data["response_id"])),
        error_code,
        raw_error_message: error
            .and_then(|value| value["message"].as_str())
            .filter(|text| !text.trim().is_empty())
            .map(|text| {
                if text.len() <= 16 * 1024 {
                    text.to_owned()
                } else {
                    "上游错误详情超过观察上限，已省略。".into()
                }
            }),
        retryable: retryable_error,
        meaningful_output,
        terminal,
        completed,
        input_tokens: usage["input_tokens"]
            .as_u64()
            .or_else(|| usage["prompt_tokens"].as_u64()),
        output_tokens: usage["output_tokens"]
            .as_u64()
            .or_else(|| usage["completion_tokens"].as_u64()),
        cached_tokens: usage["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .or_else(|| usage["prompt_tokens_details"]["cached_tokens"].as_u64()),
        cache_write_tokens: usage["input_tokens_details"]["cache_write_tokens"]
            .as_u64()
            .or_else(|| usage["prompt_tokens_details"]["cache_write_tokens"].as_u64()),
        cache_write_30m_tokens: cache_write_ttl(usage, "ttl_30m_tokens"),
        cache_write_12h_tokens: cache_write_ttl(usage, "ttl_12h_tokens"),
        total_tokens: usage["total_tokens"].as_u64(),
        reasoning_tokens: usage["output_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .or_else(|| usage["completion_tokens_details"]["reasoning_tokens"].as_u64()),
        ..Observation::default()
    }
}

impl Observation {
    fn merge(&mut self, update: Self) {
        macro_rules! some { ($($field:ident),*) => { $(if update.$field.is_some() { self.$field = update.$field; })* }; }
        some!(
            service_tier,
            model,
            effort,
            response_id,
            error_code,
            raw_error_message,
            input_tokens,
            output_tokens,
            cached_tokens,
            cache_write_tokens,
            cache_write_30m_tokens,
            cache_write_12h_tokens,
            total_tokens,
            reasoning_tokens
        );
        self.retryable |= update.retryable;
        self.meaningful_output |= update.meaningful_output;
        if update.terminal {
            self.terminal = true;
            self.completed = update.completed;
        }
        self.dropped_frames += update.dropped_frames;
        self.malformed_frames += update.malformed_frames;
    }
}

/// 每条 SSE 消息独立限长，跳过超大消息后继续观察下一条消息。
#[derive(Default)]
pub struct SseObserver {
    line: Vec<u8>,
    data: Vec<u8>,
    event: Vec<u8>,
    saw_data: bool,
    frame_bytes: usize,
    dropping: bool,
    previous_cr: bool,
    line_nonempty: bool,
    observation: Observation,
}

impl SseObserver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observation(&self) -> &Observation {
        &self.observation
    }

    pub fn push(&mut self, chunk: &[u8]) {
        for &b in chunk {
            if self.previous_cr {
                self.previous_cr = false;
                if b == b'\n' {
                    continue;
                }
            }
            if b == b'\r' || b == b'\n' {
                self.end_line();
                self.previous_cr = b == b'\r';
                continue;
            }
            self.line_nonempty = true;
            self.frame_bytes = self.frame_bytes.saturating_add(1);
            if self.frame_bytes > MAX_FRAME && !self.dropping {
                self.dropping = true;
                self.line.clear();
                self.data.clear();
                self.event.clear();
                self.observation.dropped_frames += 1;
            }
            if !self.dropping {
                self.line.push(b);
            }
        }
    }

    fn end_line(&mut self) {
        if !self.line_nonempty {
            self.dispatch();
            return;
        }
        self.line_nonempty = false;
        // Include separators in the observation bound, even for millions of short lines.
        self.frame_bytes = self.frame_bytes.saturating_add(1);
        if self.frame_bytes > MAX_FRAME && !self.dropping {
            self.dropping = true;
            self.observation.dropped_frames += 1;
            self.data.clear();
            self.event.clear();
        }
        if !self.dropping {
            let colon = self.line.iter().position(|&b| b == b':');
            let (field, value) = match colon {
                Some(n) => (&self.line[..n], &self.line[n + 1..]),
                None => (self.line.as_slice(), &[][..]),
            };
            let value = value.strip_prefix(b" ").unwrap_or(value);
            match field {
                b"data" => {
                    self.saw_data = true;
                    self.data.extend_from_slice(value);
                    self.data.push(b'\n');
                }
                b"event" => {
                    self.event.clear();
                    self.event.extend_from_slice(value);
                }
                _ => {}
            }
        }
        self.line.clear();
    }

    fn dispatch(&mut self) {
        if !self.dropping && self.saw_data {
            if self.data.last() == Some(&b'\n') {
                self.data.pop();
            }
            if self.data != b"[DONE]" {
                match serde_json::from_slice::<Value>(&self.data) {
                    Ok(mut data) => {
                        if data.is_object()
                            && data.get("type").is_none()
                            && let Ok(kind) = std::str::from_utf8(&self.event)
                            && !kind.is_empty()
                        {
                            data["type"] = Value::String(kind.to_owned());
                        }
                        self.observation.merge(inspect_json(&data));
                    }
                    Err(_) => self.observation.malformed_frames += 1,
                }
            }
        }
        self.line.clear();
        self.data.clear();
        self.event.clear();
        self.line_nonempty = false;
        self.saw_data = false;
        self.frame_bytes = 0;
        self.dropping = false;
    }

    /// EOF 不会把没有终止事件的流标为成功。
    pub fn finish(&mut self) {
        if self.line_nonempty {
            self.end_line();
        }
        self.dispatch();
        self.previous_cr = false;
    }
}

fn valid_uuid(value: &str) -> Option<String> {
    if value.len() != 36 {
        return None;
    }
    uuid::Uuid::parse_str(value)
        .ok()
        .map(|id| id.hyphenated().to_string())
}

/// 仅使用显式合法的会话标识。同一级出现冲突时不再退回较低优先级。
pub fn conversation_id(headers: &HeaderMap, body: &Value) -> Option<String> {
    let body = body
        .get("response")
        .filter(|v| v.is_object())
        .unwrap_or(body);
    for (field, header) in [("thread_id", "thread-id"), ("session_id", "session-id")] {
        let mut ids = Vec::new();
        if let Some(id) = body["client_metadata"][field].as_str().and_then(valid_uuid) {
            ids.push(id);
        }
        for v in headers.get_all(header) {
            if let Some(id) = v.to_str().ok().and_then(valid_uuid) {
                ids.push(id);
            }
        }
        if let Some(first) = ids.first() {
            return ids.iter().all(|id| id == first).then(|| first.clone());
        }
    }
    None
}

#[derive(Clone)]
pub struct RecordStore {
    inner: Arc<Mutex<StoreState>>,
}

struct StoreState {
    path: Option<PathBuf>,
    rows: VecDeque<RequestRecord>,
    limit: usize,
}

impl RecordStore {
    pub fn new(path: Option<PathBuf>, limit: usize) -> Result<Self> {
        let limit = limit.clamp(1, 10_000);
        let mut rows = VecDeque::new();
        if let Some(path) = &path {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                let mut builder = fs::DirBuilder::new();
                builder.recursive(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(parent).context("无法创建监控目录")?;
            }
            match fs::File::open(path) {
                Ok(file) => {
                    let mut reader = BufReader::new(file);
                    let mut line = Vec::new();
                    loop {
                        line.clear();
                        // Old or foreign files are not trusted to have bounded JSONL lines.
                        let mut oversized = false;
                        let mut consumed = false;
                        loop {
                            let buf = reader.fill_buf().context("无法读取监控记录")?;
                            if buf.is_empty() {
                                break;
                            }
                            consumed = true;
                            let n = buf
                                .iter()
                                .position(|&b| b == b'\n')
                                .map(|n| n + 1)
                                .unwrap_or(buf.len());
                            let end = buf[n - 1] == b'\n';
                            if line.len() + n > 128 * 1024 {
                                oversized = true;
                            }
                            if !oversized {
                                line.extend_from_slice(&buf[..n]);
                            }
                            reader.consume(n);
                            if end {
                                break;
                            }
                        }
                        if !consumed {
                            break;
                        }
                        if !oversized
                            && let Ok(row) = serde_json::from_slice::<RequestRecord>(&line)
                        {
                            rows.push_back(sanitize(row));
                            while rows.len() > limit {
                                rows.pop_front();
                            }
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => anyhow::bail!("无法读取监控文件"),
            }
        }
        let state = StoreState { path, rows, limit };
        state.persist()?;
        Ok(Self {
            inner: Arc::new(Mutex::new(state)),
        })
    }

    /// 内存更新不依赖磁盘成功；磁盘错误由调用者处理，不影响代理流。
    pub fn record(&self, record: RequestRecord) -> Result<()> {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.rows.push_back(sanitize(record));
        while state.rows.len() > state.limit {
            state.rows.pop_front();
        }
        state.persist()
    }

    pub fn records(&self) -> Vec<RequestRecord> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rows
            .iter()
            .rev()
            .cloned()
            .collect()
    }

    pub fn clear(&self) -> Result<()> {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.rows.clear();
        state.persist()
    }
}

fn clean(s: String) -> String {
    label(&Value::String(s)).unwrap_or_else(|| "redacted".into())
}
fn clean_option(v: Option<String>) -> Option<String> {
    v.and_then(|s| label(&Value::String(s)))
}
fn sanitize(mut row: RequestRecord) -> RequestRecord {
    row.route_mode = if row.route_mode == "legacy" {
        "legacy"
    } else {
        "pool"
    }
    .into();
    row.id = clean(row.id);
    row.conversation_id = row.conversation_id.and_then(|id| valid_uuid(&id));
    row.transport = clean(row.transport);
    row.requested_model = clean_option(row.requested_model);
    row.effective_model = clean_option(row.effective_model);
    row.response_model = clean_option(row.response_model);
    row.requested_effort = clean_option(row.requested_effort);
    row.effective_effort = clean_option(row.effective_effort);
    row.response_effort = clean_option(row.response_effort);
    row.effort_mismatch = clean_option(row.effort_mismatch);
    row.requested_service_tier = clean_option(row.requested_service_tier);
    row.response_service_tier = clean_option(row.response_service_tier);
    row.key_id = clean_option(row.key_id);
    row.outcome = clean(row.outcome);
    row.attempts.truncate(100);
    row.gateway_actions.retain(|action| {
        matches!(
            action.action.as_str(),
            "session_reset"
                | "session_reused"
                | "compaction_repair"
                | "image_compression"
                | "first_output_timeout"
                | "header_timeout_retry"
                | "hedged_attempt"
                | "hedged_win"
                | "model_backoff"
                | "model_alias"
                | "downgrade_retry"
        )
    });
    row.gateway_actions.truncate(32);
    let mut remaining_error_bytes = 16 * 1024;
    for attempt in &mut row.attempts {
        attempt.key_id = clean(std::mem::take(&mut attempt.key_id));
        attempt.reason = clean(std::mem::take(&mut attempt.reason));
        attempt.error_message = attempt.error_message.take().and_then(|message| {
            let mut message = crate::probe::bounded_error_message(&message, std::iter::empty())?;
            let mut end = message.len().min(remaining_error_bytes);
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
            remaining_error_bytes -= end;
            (!message.is_empty()).then_some(message)
        });
    }
    row
}

impl StoreState {
    fn persist(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let temporary = parent.join(format!(".monitor-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary).context("无法写入监控文件")?;
            for row in &self.rows {
                serde_json::to_writer(&mut file, row).context("无法编码监控记录")?;
                file.write_all(b"\n").context("无法写入监控记录")?;
            }
            file.flush().context("无法刷新监控记录")?;
            fs::rename(&temporary, path).context("无法替换监控文件")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_splits_input_into_ordinary_cached_and_written_tokens() {
        // Numbers from the official prompt caching guide: the three input parts
        // are disjoint and 12000 - 4000 - 7000 leaves 1000 ordinary tokens.
        let frame = json!({"response":{"status":"completed","model":"gpt-6-astra","usage":{
            "input_tokens":12000,
            "output_tokens":1035,
            "total_tokens":13035,
            "input_tokens_details":{
                "cached_tokens":4000,
                "cache_write_tokens":7000,
                "cache_write_tokens_details":{"ttl_30m_tokens":3000,"ttl_12h_tokens":4000}
            },
            "output_tokens_details":{"reasoning_tokens":832}
        }}});
        let mut parser = SseObserver::new();
        parser.push(format!("event: response.completed\ndata: {frame}\n\n").as_bytes());
        parser.finish();
        let o = parser.observation();
        assert_eq!(o.input_tokens, Some(12000));
        assert_eq!(o.cached_tokens, Some(4000));
        assert_eq!(o.cache_write_tokens, Some(7000));
        assert_eq!(o.cache_write_30m_tokens, Some(3000));
        assert_eq!(o.cache_write_12h_tokens, Some(4000));
        assert_eq!(o.total_tokens, Some(13035));
        // Reasoning is already inside output_tokens; it is read for display only.
        assert_eq!(o.output_tokens, Some(1035));
        assert_eq!(o.reasoning_tokens, Some(832));
    }

    #[test]
    fn usage_accepts_chat_completions_naming_and_leaves_absent_splits_unset() {
        let frame = json!({"response":{"status":"completed","model":"gpt-6.1-sol","usage":{
            "prompt_tokens":90,
            "completion_tokens":12,
            "prompt_tokens_details":{"cached_tokens":64,"cache_write_tokens":16},
            "completion_tokens_details":{"reasoning_tokens":4}
        }}});
        let mut parser = SseObserver::new();
        parser.push(format!("event: response.completed\ndata: {frame}\n\n").as_bytes());
        parser.finish();
        let o = parser.observation();
        assert_eq!(o.input_tokens, Some(90));
        assert_eq!(o.output_tokens, Some(12));
        assert_eq!(o.cached_tokens, Some(64));
        assert_eq!(o.cache_write_tokens, Some(16));
        // A missing lifetime split must stay unknown rather than be inferred.
        assert_eq!(o.cache_write_30m_tokens, None);
        assert_eq!(o.cache_write_12h_tokens, None);
        assert_eq!(o.total_tokens, None);
        assert_eq!(o.reasoning_tokens, Some(4));
    }

    #[test]
    fn sse_arbitrary_chunks_crlf_and_multiline() {
        let bytes = b": keepalive\r\nevent: response.completed\r\ndata: {\"response\": {\r\ndata: \"status\":\"completed\",\"model\":\"gpt-6-astra\",\"usage\":{\"input_tokens\":9}}}\r\n\r\n";
        for size in 1..20 {
            let mut parser = SseObserver::new();
            for chunk in bytes.chunks(size) {
                parser.push(chunk);
            }
            parser.finish();
            assert!(parser.observation().completed);
            assert_eq!(parser.observation().model.as_deref(), Some("gpt-6-astra"));
            assert_eq!(parser.observation().input_tokens, Some(9));
            assert_eq!(parser.observation().malformed_frames, 0);
        }
    }

    #[test]
    fn failure_inside_http_200_is_terminal_and_retryable() {
        let mut p = SseObserver::new();
        p.push(b"data: {\"type\":\"response.created\",\"response\":{\"status\":\"in_progress\"}}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"sk-SUPERSECRET private prompt\"}}}\n\n");
        assert!(p.observation().terminal);
        assert!(p.observation().retryable);
        assert!(!p.observation().completed);
        assert!(!p.observation().meaningful_output);
        assert_eq!(
            p.observation().error_code.as_deref(),
            Some("rate_limit_exceeded")
        );
        let top =
            inspect_json(&json!({"type":"error","code":"overloaded","message":"sk-SUPERSECRET"}));
        assert!(top.terminal && top.retryable && !top.completed);
        assert_eq!(top.error_code.as_deref(), Some("overloaded"));
        let typed = inspect_json(
            &json!({"error":{"code":"unknown_gateway_code","type":"too_many_requests"}}),
        );
        assert_eq!(typed.error_code.as_deref(), Some("too_many_requests"));
        for payload in [
            json!({"type":"response.failed"}),
            json!({"status":"failed"}),
        ] {
            let failed = inspect_json(&payload);
            assert!(failed.terminal && !failed.completed && !failed.retryable);
            assert_eq!(failed.error_code.as_deref(), Some("response_failed"));
        }
    }

    #[test]
    fn meaningful_output_boundaries_and_terminal_usage() {
        for kind in [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
        ] {
            assert!(!inspect_json(&json!({"type":kind,"response":{"status":"in_progress"},"item":{"type":"message","content":[]}})).meaningful_output);
        }
        for kind in [
            "response.output_text.delta",
            "response.reasoning_summary_text.delta",
            "response.function_call_arguments.delta",
            "response.custom_tool_call_input.delta",
        ] {
            assert!(inspect_json(&json!({"type":kind,"delta":"x"})).meaningful_output);
            assert!(!inspect_json(&json!({"type":kind,"delta":""})).meaningful_output);
        }
        assert!(inspect_json(&json!({"type":"response.output_item.added","item":{"type":"function_call","name":"exec","arguments":""}})).meaningful_output);
        let r = inspect_json(
            &json!({"type":"response.completed","response":{"status":"completed","id":"resp_123","model":"gpt-6-astra","reasoning":{"effort":"high"},"usage":{"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":80},"output_tokens_details":{"reasoning_tokens":10}}}}),
        );
        assert!(r.terminal && r.completed);
        assert_eq!(
            (
                r.input_tokens,
                r.output_tokens,
                r.cached_tokens,
                r.reasoning_tokens
            ),
            (Some(100), Some(20), Some(80), Some(10))
        );
        assert_eq!(r.effort.as_deref(), Some("high"));
    }

    #[test]
    fn oversized_and_malformed_frames_recover() {
        let mut p = SseObserver::new();
        p.push(b"data: ");
        p.push(&vec![b'x'; MAX_FRAME + 1]);
        p.push(b"\r\n\r\ndata: not-json\r\n\r\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\r\n\r\n");
        assert_eq!(p.observation().dropped_frames, 1);
        assert_eq!(p.observation().malformed_frames, 1);
        assert!(p.observation().completed);
    }

    #[test]
    fn eof_and_done_do_not_invent_success() {
        let mut p = SseObserver::new();
        p.push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\ndata: [DONE]\n\n");
        p.finish();
        assert!(p.observation().meaningful_output);
        assert!(!p.observation().terminal);
        assert!(!p.observation().completed);
        let mut incomplete = SseObserver::new();
        incomplete.push(
            b"data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\"}}",
        );
        incomplete.finish();
        assert!(incomplete.observation().terminal);
        assert!(!incomplete.observation().completed);
    }

    #[test]
    fn service_tier_keeps_latest_reported_value_without_inventing_success() {
        let mut parser = SseObserver::new();
        parser.push(b"data: {\"type\":\"response.created\",\"response\":{\"service_tier\":\"priority\",\"status\":\"in_progress\"}}\n\n");
        parser.push(
            b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        );
        assert_eq!(
            parser.observation().service_tier.as_deref(),
            Some("priority")
        );
        assert!(parser.observation().completed);
        let mut failed = SseObserver::new();
        failed.push(b"data: {\"type\":\"response.created\",\"response\":{\"service_tier\":\"priority\"}}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"service_tier\":\"fast\",\"error\":{\"code\":\"rate_limit_exceeded\"}}}\n\n");
        assert_eq!(failed.observation().service_tier.as_deref(), Some("fast"));
        assert!(failed.observation().terminal && !failed.observation().completed);
        assert!(
            inspect_json(&json!({"service_tier":"tier/sk-SECRET"}))
                .service_tier
                .is_none()
        );
        let store = RecordStore::new(None, 2).unwrap();
        store
            .record(RequestRecord {
                requested_service_tier: Some("sk-SECRET".into()),
                response_service_tier: Some("fast/sk-SECRET".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            !serde_json::to_string(&store.records())
                .unwrap()
                .contains("SECRET")
        );
    }

    #[test]
    fn conversation_priority_conflicts_and_uuid_validation() {
        let first = "01a166ee-ea86-7d02-b7fa-ab476d5b2388";
        let second = "01a10c74-536b-7b50-8e33-1acbad307821";
        let mut headers = HeaderMap::new();
        headers.insert("session-id", second.parse().unwrap());
        assert_eq!(
            conversation_id(&headers, &json!({"client_metadata":{"thread_id":first}})).as_deref(),
            Some(first)
        );
        headers.insert("thread-id", second.parse().unwrap());
        assert_eq!(
            conversation_id(&headers, &json!({"client_metadata":{"thread_id":first}})),
            None
        );
        assert_eq!(
            conversation_id(&HeaderMap::new(), &json!({"prompt_cache_key":first})),
            None
        );
        assert_eq!(
            conversation_id(
                &HeaderMap::new(),
                &json!({"client_metadata":{"thread_id":"private title"}})
            ),
            None
        );
    }

    #[test]
    fn no_secret_serialization_and_bounded_private_disk() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records.jsonl");
        let store = RecordStore::new(Some(path.clone()), 2).unwrap();
        let observation = inspect_json(
            &json!({"type":"response.failed","response":{"model":"model/sk-SUPERSECRET","error":{"code":"sk-SUPERSECRET","message":"PRIVATE PROMPT"},"output":[{"type":"message","content":[{"text":"PRIVATE OUTPUT"}]}]}}),
        );
        assert!(label(&json!("model/SK-secret")).is_none());
        assert_eq!(observation.error_code.as_deref(), Some("unknown_error"));
        for i in 0..3 {
            store
                .record(RequestRecord {
                    id: format!("record{i}"),
                    response_model: observation.model.clone(),
                    outcome: observation.error_code.clone().unwrap(),
                    key_id: Some("sk-SUPERSECRET".into()),
                    attempts: vec![AttemptRecord {
                        reason: "PRIVATE PROMPT".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .unwrap();
        }
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        for secret in ["SUPERSECRET", "PRIVATE PROMPT", "PRIVATE OUTPUT"] {
            assert!(!text.contains(secret));
        }
        assert_eq!(store.records()[0].id, "record2");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            RecordStore::new(Some(path.clone()), 2)
                .unwrap()
                .records()
                .len(),
            2
        );
        store.clear().unwrap();
        assert!(store.records().is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
    }
}
