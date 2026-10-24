//! Bounded diagnostics for a fixed synthetic probe. Normal requests never use this collector.
use crate::monitor::{self, Observation, SseObserver};
use serde::Serialize;
use serde_json::Value;

pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const REQUEST_TEXT: &str = "Reply only OK.";
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_EVENT_BYTES: usize = 2 * 1024;
const MAX_EVENTS: usize = 100;
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 2 * 1024;

fn token_end(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len()
        && !bytes[index].is_ascii_whitespace()
        && !b"\"'<>[]{}(),;".contains(&bytes[index])
    {
        index += 1;
    }
    index
}

fn quoted_value_end(bytes: &[u8], start: usize) -> usize {
    let Some(&quote) = bytes
        .get(start)
        .filter(|byte| matches!(byte, b'\"' | b'\''))
    else {
        return token_end(bytes, start);
    };
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == quote {
            break;
        }
        if bytes[index] == b'\\' {
            index += 1;
        }
        index += 1;
    }
    index.min(bytes.len())
}

/// Removes exact locally known secrets and common credential representations.
/// Unknown tokens without a credential marker cannot be identified reliably.
/// Callers must pass every locally loaded credential and only bounded input.
pub fn redact_sensitive<'a>(text: &str, secrets: impl IntoIterator<Item = &'a str>) -> String {
    let lower = text.to_ascii_lowercase();
    let mut ranges = Vec::new();
    let bytes = text.as_bytes();
    // Collect ranges in the original input. Repeated string replacements could
    // modify earlier [REDACTED] markers when a credential is short or overlaps
    // another credential, expanding the output and hiding useful diagnostics.
    for secret in secrets.into_iter().filter(|secret| !secret.is_empty()) {
        ranges.extend(
            text.match_indices(secret)
                .map(|(start, _)| (start, start + secret.len())),
        );
    }
    for prefix in ["bearer", "basic", "sk-", "http://", "https://"] {
        for (start, _) in lower.match_indices(prefix) {
            let mut end = start + prefix.len();
            if matches!(prefix, "bearer" | "basic") {
                // The authentication scheme must be followed by a separator.
                // A model name such as "basic-model" is ordinary metadata.
                if !bytes.get(end).is_some_and(u8::is_ascii_whitespace) {
                    continue;
                }
                while end < bytes.len() && bytes[end].is_ascii_whitespace() {
                    end += 1;
                }
            }
            end = quoted_value_end(bytes, end);
            // URLs are omitted entirely, including credentials, query strings and fragments.
            ranges.push((start, end));
        }
    }
    // Cover headers, key=value text and quoted JSON fields without retaining
    // unrecognized token values. This is deliberately independent of spelling
    // and prefixes of the token itself.
    for field in [
        "authorization",
        "proxy-authorization",
        "x-api-key",
        "api_key",
        "api-key",
        "apikey",
        "access_token",
        "refresh_token",
        "password",
        "token",
    ] {
        for (start, _) in lower.match_indices(field) {
            let mut value_start = start + field.len();
            if bytes
                .get(value_start)
                .is_some_and(|byte| matches!(byte, b'\"' | b'\''))
            {
                value_start += 1;
            }
            while bytes.get(value_start).is_some_and(u8::is_ascii_whitespace) {
                value_start += 1;
            }
            if !bytes
                .get(value_start)
                .is_some_and(|byte| matches!(byte, b':' | b'='))
            {
                continue;
            }
            value_start += 1;
            while bytes.get(value_start).is_some_and(u8::is_ascii_whitespace) {
                value_start += 1;
            }
            let mut end = quoted_value_end(bytes, value_start);
            if field.ends_with("authorization")
                && matches!(lower.get(value_start..end), Some("bearer" | "basic"))
            {
                while bytes.get(end).is_some_and(u8::is_ascii_whitespace) {
                    end += 1;
                }
                end = quoted_value_end(bytes, end);
            }
            ranges.push((start, end));
        }
    }
    ranges.sort_unstable();
    let mut sanitized = String::new();
    let mut cursor = 0;
    for (start, end) in ranges {
        if end <= cursor {
            continue;
        }
        if start >= cursor {
            sanitized.push_str(&text[cursor..start]);
            sanitized.push_str("[REDACTED]");
        }
        cursor = end;
    }
    sanitized.push_str(&text[cursor..]);
    sanitized
}

/// Return only a bounded, display-safe error message. Extract error.message
/// before calling; never pass a successful response body or request payload.
pub fn bounded_error_message<'a>(
    text: &str,
    secrets: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let display_safe = display_safe_text(text);
    let sanitized = redact_sensitive(&display_safe, secrets);
    let text = sanitized.trim();
    if text.is_empty() {
        return None;
    }
    Some(bounded(text, MAX_ERROR_BYTES, &mut false))
}

fn display_safe_text(text: &str) -> String {
    text.chars().filter_map(|character| {
        if matches!(character, '\n' | '\r' | '\t') {
            Some(' ')
        } else if character.is_control()
            || matches!(character, '\u{00ad}' | '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
        {
            None
        } else {
            Some(character)
        }
    }).collect()
}

fn bounded(text: &str, limit: usize, truncated: &mut bool) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    *truncated = true;
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[derive(Serialize)]
pub struct Event {
    #[serde(rename = "type")]
    pub kind: String,
    pub elapsed_ms: u64,
    pub detail: String,
}

#[derive(Serialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

#[derive(Serialize)]
pub struct Details {
    pub request_id: Option<String>,
    pub requested_model: String,
    pub requested_effort: String,
    pub request_text: &'static str,
    pub elapsed_ms: u64,
    pub response_text: String,
    pub error_message: Option<String>,
    pub events: Vec<Event>,
    pub usage: Usage,
    pub truncated: bool,
}

pub fn error_message(code: Option<&str>) -> &'static str {
    match code {
        Some("invalid_api_key" | "authentication_error" | "unauthorized") => "上游拒绝了账号认证。",
        Some("permission_denied") => "账号没有访问所请求模型的权限。",
        Some("rate_limit_exceeded" | "too_many_requests") => "上游限制了请求频率，请稍后重试。",
        Some("insufficient_quota" | "quota_exceeded") => "账号可用额度不足。",
        Some("model_not_found" | "unsupported_model") => "上游不支持所请求的模型。",
        Some("invalid_request_error" | "invalid_request") => "上游未接受此次合成验证请求。",
        Some("timeout" | "request_timeout" | "probe_timeout") => "验证超过时间限制，已停止等待。",
        Some("pool_exhausted") => "账号持续忙碌、冷却或重试未成功，验证队列已到期。",
        Some("no_candidate") => "指定账号当前不在可用号池中，或不支持所请求模型。",
        Some("header_timeout") => "等待上游响应头超时。",
        Some("connect_failed") => "未能建立上游连接。",
        Some("transport_uncertain") => "上游连接中断，无法确认请求是否已被处理。",
        Some("response_read_failed") => "上游响应未完整收到。",
        Some("invalid_upstream_response" | "probe_incomplete") => {
            "上游未返回有效、完整的 Responses 结果。"
        }
        Some("probe_response_too_large") => "验证响应超过大小限制，已停止读取。",
        Some("probe_stream_failed") => "验证响应传输中断。",
        Some(
            "server_error"
            | "internal_error"
            | "internal_server_error"
            | "overloaded"
            | "overloaded_error"
            | "service_unavailable"
            | "temporarily_unavailable"
            | "upstream_error",
        ) => "上游暂时无法完成此次验证。",
        _ => "此次验证未完整成功，请查看上游错误详情和尝试记录。",
    }
}

pub struct Collector {
    sse: bool,
    observer: SseObserver,
    json_observation: Observation,
    line: Vec<u8>,
    data: Vec<u8>,
    event: String,
    frame_bytes: usize,
    dropping: bool,
    previous_cr: bool,
    line_nonempty: bool,
    received_bytes: usize,
    size_exceeded: bool,
    diagnostic_error: Option<&'static str>,
    last_attempt_error: Option<String>,
    json: Vec<u8>,
    response_text: String,
    saw_delta: bool,
    events: Vec<Event>,
    truncated: bool,
}

impl Collector {
    pub fn new(sse: bool) -> Self {
        Self {
            sse,
            observer: SseObserver::new(),
            json_observation: Observation::default(),
            line: Vec::new(),
            data: Vec::new(),
            event: String::new(),
            frame_bytes: 0,
            dropping: false,
            previous_cr: false,
            line_nonempty: false,
            received_bytes: 0,
            size_exceeded: false,
            diagnostic_error: None,
            last_attempt_error: None,
            json: Vec::new(),
            response_text: String::new(),
            saw_delta: false,
            events: Vec::new(),
            truncated: false,
        }
    }

    pub fn add_event(&mut self, kind: &str, elapsed_ms: u64, detail: &str) {
        if self.events.len() == MAX_EVENTS {
            self.truncated = true;
            return;
        }
        self.events.push(Event {
            kind: if kind.len() > 128 {
                self.truncated = true;
                "other_event".into()
            } else {
                kind.into()
            },
            elapsed_ms,
            // Do not retain a prefix before redaction: the cut could bisect a
            // known credential and leave an unrecognizable secret fragment.
            detail: if detail.len() > MAX_EVENT_BYTES {
                self.truncated = true;
                "事件说明超过长度限制，已省略。".into()
            } else {
                detail.into()
            },
        });
    }

    pub fn record_events(&mut self, record: &monitor::RequestRecord) {
        for action in &record.gateway_actions {
            let (kind, description) = match action.action.as_str() {
                "session_reset" => ("session_reset", "重建会话并清除失效的续接标识"),
                "session_reused" => ("session_reused", "使用已恢复的会话关联"),
                "compaction_repair" => ("compaction_repair", "修复压缩上下文的协议兼容字段"),
                "image_compression" => ("image_compression", "压缩请求中的图片"),
                "first_output_timeout" => ("first_output_timeout", "等待首个有效输出超时"),
                "header_timeout_retry" => ("header_timeout_retry", "等待响应头超时后重试"),
                _ => continue,
            };
            self.add_event(
                kind,
                action.elapsed_ms,
                &format!(
                    "第 {} 次尝试，{}，处理项数 {}。",
                    action.attempt, description, action.count
                ),
            );
        }
        for (index, attempt) in record.attempts.iter().enumerate() {
            let status = attempt
                .status
                .map(|status| format!("HTTP {status}"))
                .unwrap_or_else(|| "未收到 HTTP 响应".into());
            let completed = attempt.reason == "completed"
                && !attempt.status.is_some_and(|status| status >= 400);
            let result = if completed {
                "响应完成。"
            } else {
                error_message(Some(&attempt.reason))
            };
            let summary = format!(
                "第 {} 次尝试，{}，耗时 {} ms。{}",
                index + 1,
                status,
                attempt.duration_ms,
                result
            );
            // Records carry already redacted error text. Sanitize once more
            // with the probe's captured credential set in details().
            let detail = if !completed {
                let message = attempt
                    .error_message
                    .as_deref()
                    .and_then(|message| bounded_error_message(message, []));
                let combined = message
                    .map(|message| format!("{summary} {message}"))
                    .unwrap_or_else(|| summary.clone());
                let combined = bounded(&combined, MAX_EVENT_BYTES, &mut self.truncated);
                self.last_attempt_error = Some(combined.clone());
                combined
            } else {
                summary
            };
            self.add_event("upstream_attempt", record.duration_ms, &detail);
        }
    }

    pub fn push(&mut self, bytes: &[u8], elapsed_ms: u64) {
        self.received_bytes = self.received_bytes.saturating_add(bytes.len());
        if self.received_bytes > MAX_RESPONSE_BYTES {
            self.size_exceeded = true;
            self.truncated = true;
            self.response_text.clear();
            self.json.clear();
            self.line.clear();
            self.data.clear();
            return;
        }
        if !self.sse {
            if self.json.len().saturating_add(bytes.len()) <= MAX_RESPONSE_BYTES {
                self.json.extend_from_slice(bytes);
            } else {
                self.truncated = true;
            }
            return;
        }
        self.observer.push(bytes);
        for &byte in bytes {
            if self.previous_cr {
                self.previous_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.end_line(elapsed_ms);
                self.previous_cr = byte == b'\r';
            } else {
                self.line_nonempty = true;
                self.frame_bytes = self.frame_bytes.saturating_add(1);
                if self.frame_bytes > MAX_FRAME_BYTES {
                    self.dropping = true;
                    self.truncated = true;
                    self.line.clear();
                    self.data.clear();
                } else if !self.dropping {
                    self.line.push(byte);
                }
            }
        }
    }

    fn end_line(&mut self, elapsed_ms: u64) {
        if !self.line_nonempty {
            if !self.dropping && !self.data.is_empty() && self.data != b"[DONE]\n" {
                match serde_json::from_slice::<Value>(&self.data) {
                    Ok(mut value) => {
                        if value.is_object()
                            && value.get("type").is_none()
                            && !self.event.is_empty()
                        {
                            value["type"] = Value::String(self.event.clone());
                        }
                        self.observe_value(&value, elapsed_ms);
                    }
                    Err(_) => self.add_event("parse_error", elapsed_ms, "收到无法解析的响应事件。"),
                }
            }
            self.data.clear();
            self.event.clear();
            self.frame_bytes = 0;
            self.dropping = false;
            return;
        }
        self.line_nonempty = false;
        self.frame_bytes = self.frame_bytes.saturating_add(1);
        if self.frame_bytes > MAX_FRAME_BYTES {
            self.dropping = true;
            self.truncated = true;
        }
        if !self.dropping {
            if let Some(data) = self.line.strip_prefix(b"data:") {
                self.data
                    .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
                self.data.push(b'\n');
            } else if let Some(event) = self.line.strip_prefix(b"event:") {
                self.event =
                    String::from_utf8_lossy(event.strip_prefix(b" ").unwrap_or(event)).into_owned();
            }
        }
        self.line.clear();
    }

    fn observe_value(&mut self, value: &Value, elapsed_ms: u64) {
        let raw_kind = value["type"].as_str().unwrap_or("json_response");
        let kind = match raw_kind {
            "response.created"
            | "response.queued"
            | "response.in_progress"
            | "response.output_item.added"
            | "response.output_item.done"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.delta"
            | "response.output_text.done"
            | "response.completed"
            | "response.done"
            | "response.failed"
            | "response.incomplete"
            | "error"
            | "json_response" => raw_kind,
            _ => "other_event",
        };
        let observation = monitor::inspect_json(value);
        let response = value.get("response").unwrap_or(value);
        let raw_error = value.get("error").or_else(|| response.get("error"));
        if let Some(code) = raw_error.and_then(|error| error["code"].as_str()) {
            self.diagnostic_error = match code {
                "pool_exhausted" => Some("pool_exhausted"),
                "no_candidate" => Some("no_candidate"),
                "header_timeout" => Some("header_timeout"),
                "connect_failed" => Some("connect_failed"),
                "transport_uncertain" => Some("transport_uncertain"),
                "response_read_failed" => Some("response_read_failed"),
                "invalid_upstream_response" => Some("invalid_upstream_response"),
                _ => self.diagnostic_error,
            };
        }
        let detail = if observation.error_code.is_some() {
            error_message(self.diagnostic_error.or(observation.error_code.as_deref())).to_owned()
        } else if kind == "response.output_text.delta" {
            let delta = value["delta"].as_str().unwrap_or_default();
            self.saw_delta = true;
            self.append_text(delta);
            format!("收到可见文本片段，共 {} 个字符。", delta.chars().count())
        } else if observation.completed {
            "响应已完成。".into()
        } else {
            "收到响应事件。".into()
        };
        if kind == "response.output_text.done" && !self.saw_delta {
            self.append_text(value["text"].as_str().unwrap_or_default());
        }
        if let Some(items) = response["output"].as_array() {
            let mut text = String::new();
            for item in items {
                if item["type"].as_str() != Some("message") {
                    continue;
                }
                if let Some(content) = item["content"].as_array() {
                    for part in content {
                        if part["type"].as_str() == Some("output_text")
                            && let Some(part) = part["text"].as_str()
                        {
                            if text.len().saturating_add(part.len()) <= MAX_RESPONSE_BYTES {
                                text.push_str(part);
                            } else {
                                self.truncated = true;
                            }
                        }
                    }
                }
            }
            if !text.is_empty() {
                self.response_text = text;
            }
        }
        self.add_event(kind, elapsed_ms, &detail);
    }

    fn append_text(&mut self, text: &str) {
        if self.response_text.len().saturating_add(text.len()) <= MAX_RESPONSE_BYTES {
            self.response_text.push_str(text);
        } else {
            self.truncated = true;
        }
    }

    pub fn finish(&mut self, elapsed_ms: u64) {
        if self.sse {
            if self.line_nonempty {
                self.end_line(elapsed_ms);
            }
            self.end_line(elapsed_ms);
            self.observer.finish();
        } else {
            match serde_json::from_slice::<Value>(&self.json) {
                Ok(value) => {
                    self.json_observation = monitor::inspect_json(&value);
                    self.observe_value(&value, elapsed_ms);
                }
                Err(_) => self.add_event("parse_error", elapsed_ms, "响应不是有效的 JSON。"),
            }
        }
    }

    pub fn observation(&self) -> &Observation {
        if self.sse {
            self.observer.observation()
        } else {
            &self.json_observation
        }
    }

    pub fn error_code(&self) -> Option<&str> {
        self.diagnostic_error
            .or(self.observation().error_code.as_deref())
    }

    pub fn details(
        mut self,
        request_id: Option<String>,
        model: &str,
        elapsed_ms: u64,
        failure: Option<&str>,
        sanitize: impl Fn(&str) -> String,
    ) -> Details {
        let observation = self.observation().clone();
        let complete = !self.size_exceeded
            && failure.is_none()
            && observation.completed
            && observation.error_code.is_none();
        let response_text = if complete {
            bounded(
                &sanitize(&self.response_text),
                MAX_TEXT_BYTES,
                &mut self.truncated,
            )
        } else {
            String::new()
        };
        let error_message = (!complete).then(|| {
            let message = self
                .last_attempt_error
                .as_deref()
                .or(observation.raw_error_message.as_deref());
            message
                .and_then(|message| {
                    // Normalize first, so removing invisible characters cannot
                    // reconstruct a credential after exact-secret redaction.
                    let sanitized = sanitize(&display_safe_text(message));
                    if sanitized.len() > MAX_ERROR_BYTES {
                        self.truncated = true;
                    }
                    bounded_error_message(&sanitized, [])
                })
                .unwrap_or_else(|| error_message(failure.or(self.error_code())).into())
        });
        let mut events: Vec<_> = self
            .events
            .into_iter()
            .map(|event| Event {
                kind: bounded(&sanitize(&event.kind), 128, &mut self.truncated),
                elapsed_ms: event.elapsed_ms,
                detail: bounded(
                    &sanitize(&event.detail),
                    MAX_EVENT_BYTES,
                    &mut self.truncated,
                ),
            })
            .collect();
        events.sort_by_key(|event| event.elapsed_ms);
        Details {
            request_id: request_id.map(|id| bounded(&sanitize(&id), 128, &mut self.truncated)),
            requested_model: bounded(&sanitize(model), 128, &mut self.truncated),
            requested_effort: "low".into(),
            request_text: REQUEST_TEXT,
            elapsed_ms,
            response_text,
            error_message,
            events,
            usage: Usage {
                input_tokens: observation.input_tokens,
                output_tokens: observation.output_tokens,
                cached_tokens: observation.cached_tokens,
                reasoning_tokens: observation.reasoning_tokens,
            },
            truncated: self.truncated || failure == Some("probe_response_too_large"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn completed(text: &str) -> Value {
        json!({"id":"resp_fixture","object":"response","model":"test-model","status":"completed",
            "output":[{"type":"reasoning","encrypted_content":"encrypted-do-not-display"},
                {"type":"message","content":[{"type":"output_text","text":text}]}],
            "usage":{"input_tokens":5,"output_tokens":2,"input_tokens_details":{"cached_tokens":1},
                "output_tokens_details":{"reasoning_tokens":1}}})
    }

    #[test]
    fn json_success_collects_only_visible_text_usage_and_fixed_request() {
        let mut collector = Collector::new(false);
        collector.push(completed("OK").to_string().as_bytes(), 42);
        collector.finish(42);
        assert!(collector.observation().completed);
        let detail = collector.details(
            Some("local-id".into()),
            "test-model",
            42,
            None,
            str::to_owned,
        );
        assert_eq!(detail.response_text, "OK");
        assert_eq!(detail.request_text, REQUEST_TEXT);
        assert_eq!(detail.usage.input_tokens, Some(5));
        assert_eq!(detail.usage.output_tokens, Some(2));
        assert_eq!(detail.usage.cached_tokens, Some(1));
        assert_eq!(detail.usage.reasoning_tokens, Some(1));
        assert!(detail.error_message.is_none());
        assert!(
            !serde_json::to_string(&detail)
                .unwrap()
                .contains("encrypted-do-not-display")
        );
    }

    #[test]
    fn sse_chunk_boundaries_event_field_and_final_output_do_not_duplicate_text() {
        let final_value = json!({"response": completed("OK")});
        let stream = format!(
            "event: response.output_text.delta\r\ndata: {{\"delta\":\"O\"}}\r\n\r\n\
             data: {{\"type\":\"response.output_text.delta\",\"delta\":\"K\"}}\n\n\
             event: response.completed\r\ndata: {final_value}\r\n\r\ndata: [DONE]\n\n"
        );
        let mut collector = Collector::new(true);
        for (index, chunk) in stream.as_bytes().chunks(3).enumerate() {
            collector.push(chunk, index as u64);
        }
        collector.finish(999);
        assert!(collector.observation().completed);
        let detail = collector.details(None, "test-model", 999, None, str::to_owned);
        assert_eq!(detail.response_text, "OK");
        assert_eq!(detail.events.len(), 3);
        assert!(
            detail
                .events
                .windows(2)
                .all(|events| events[0].elapsed_ms <= events[1].elapsed_ms)
        );
        assert!(
            !serde_json::to_string(&detail.events)
                .unwrap()
                .contains("\"delta\"")
        );
    }

    #[test]
    fn redaction_covers_plain_secrets_bearer_tokens_urls_and_output_across_deltas() {
        let secret = "opaque-token-without-prefix";
        let local = "local-access-fixture";
        let mut collector = Collector::new(true);
        for delta in [
            "opaque-token-",
            "without-prefix",
            " local-access-fixture sk-third-secret Bearer fourth-token https://user:password@example.com/path?key=hidden",
        ] {
            let event = json!({"type":"response.output_text.delta","delta":delta});
            collector.push(format!("data: {event}\n\n").as_bytes(), 1);
        }
        collector.push(
            b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
            2,
        );
        collector.finish(3);
        let detail = collector.details(None, "test-model", 3, None, |text| {
            redact_sensitive(text, [secret, local])
        });
        let json = serde_json::to_string(&detail).unwrap();
        for forbidden in [
            secret,
            local,
            "sk-third-secret",
            "fourth-token",
            "user:password",
            "hidden",
        ] {
            assert!(!json.contains(forbidden), "leaked {forbidden}");
        }
        assert!(detail.response_text.contains("[REDACTED]"));
    }

    #[test]
    fn explicit_error_text_is_useful_but_credentials_and_unknown_event_names_stay_private() {
        let key = "opaque-known-fixture-secret";
        let mut collector = Collector::new(true);
        let error = json!({"error":{"code":"opaque-unknown-code", "message":format!(
            "账号暂不允许模型访问。{key} Authorization: Bearer hidden https://user:password@example.com/private"
        )}});
        collector.push(
            format!("event: opaque-unknown-event\ndata: {error}\n\n").as_bytes(),
            5,
        );
        collector.finish(6);
        assert_eq!(collector.error_code(), Some("unknown_error"));
        let detail = collector.details(None, "test-model", 6, None, |text| {
            redact_sensitive(text, [key])
        });
        assert!(
            detail
                .error_message
                .as_deref()
                .unwrap()
                .contains("账号暂不允许模型访问。")
        );
        let serialized = serde_json::to_string(&detail).unwrap();
        for secret in [
            key,
            "opaque-unknown-code",
            "opaque-unknown-event",
            "Authorization",
            "hidden",
            "user:password",
            "example.com",
        ] {
            assert!(!serialized.contains(secret));
        }
        assert_eq!(detail.events[0].kind, "other_event");
    }

    #[test]
    fn local_errors_preserve_useful_safe_classification() {
        let mut collector = Collector::new(false);
        collector.push(br#"{"error":{"code":"no_candidate"}}"#, 10);
        collector.finish(10);
        assert_eq!(collector.error_code(), Some("no_candidate"));
        let detail = collector.details(None, "test-model", 10, None, str::to_owned);
        assert!(detail.error_message.unwrap().contains("不在可用号池"));
    }

    #[test]
    fn last_failed_attempt_summary_takes_precedence_and_survives_the_event_limit() {
        let key = "opaque-record-fixture-secret";
        let mut collector = Collector::new(false);
        collector.push(
            br#"{"error":{"code":"quota_exceeded","message":"fallback-body-error"}}"#,
            30,
        );
        collector.finish(30);
        for _ in 0..MAX_EVENTS {
            collector.add_event("fixture", 1, "fixture");
        }
        collector.record_events(&monitor::RequestRecord {
            duration_ms: 30,
            attempts: vec![
                monitor::AttemptRecord {
                    reason: "rate_limit_exceeded".into(),
                    status: Some(429),
                    error_message: Some("earlier-error".into()),
                    duration_ms: 10,
                    ..Default::default()
                },
                monitor::AttemptRecord {
                    reason: "quota_exceeded".into(),
                    status: Some(403),
                    error_message: Some(format!(
                        "本周期余额不足。{key} api_key=unknown-fixture-token"
                    )),
                    duration_ms: 20,
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        let details = collector.details(None, "test-model", 30, None, |text| {
            redact_sensitive(text, [key])
        });
        let message = details.error_message.unwrap();
        assert!(message.contains("第 2 次尝试"));
        assert!(message.contains("HTTP 403"));
        assert!(message.contains("本周期余额不足"));
        for forbidden in [
            key,
            "unknown-fixture-token",
            "earlier-error",
            "fallback-body-error",
        ] {
            assert!(!message.contains(forbidden));
        }
        assert!(details.truncated);
    }

    #[test]
    fn attempts_keep_safe_details_but_success_does_not_return_an_old_error() {
        let mut collector = Collector::new(false);
        collector.push(completed("OK").to_string().as_bytes(), 30);
        collector.finish(30);
        collector.record_events(&monitor::RequestRecord {
            duration_ms: 30,
            attempts: vec![
                monitor::AttemptRecord {
                    reason: "rate_limit_exceeded".into(),
                    status: Some(429),
                    error_message: Some("当前额度需要等待恢复".into()),
                    ..Default::default()
                },
                monitor::AttemptRecord {
                    reason: "completed".into(),
                    status: Some(200),
                    error_message: Some("unexpected-success-field".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        let details = collector.details(None, "test-model", 30, None, str::to_owned);
        assert!(details.error_message.is_none());
        assert!(
            details
                .events
                .iter()
                .any(|event| event.detail.contains("当前额度需要等待恢复"))
        );
        assert!(
            !serde_json::to_string(&details)
                .unwrap()
                .contains("unexpected-success-field")
        );
    }

    #[test]
    fn raw_error_without_request_record_is_sanitized_before_its_display_limit() {
        let key = "opaque-boundary-probe-error-fixture";
        let mut collector = Collector::new(false);
        let message = format!("{}{}", "x".repeat(MAX_ERROR_BYTES - 4), key);
        collector.push(
            json!({"error":{"code":"fixture-code","message":message}})
                .to_string()
                .as_bytes(),
            1,
        );
        collector.finish(1);
        let details = collector.details(None, "test-model", 1, None, |text| {
            redact_sensitive(text, [key])
        });
        assert!(details.truncated);
        let message = details.error_message.unwrap();
        assert!(message.len() <= MAX_ERROR_BYTES);
        assert!(!message.contains("opaque"));

        let mut collector = Collector::new(false);
        collector.push(
            json!({"error":{"code":"fixture-code", "message":"opaque-huge-fixture".repeat(2000)}})
                .to_string()
                .as_bytes(),
            1,
        );
        collector.finish(1);
        let details = collector.details(None, "test-model", 1, None, str::to_owned);
        assert!(
            !details
                .error_message
                .unwrap()
                .contains("opaque-huge-fixture")
        );
    }

    #[test]
    fn recorded_gateway_steps_and_attempts_use_only_allowlisted_diagnostics() {
        let mut collector = Collector::new(false);
        collector.push(completed("OK").to_string().as_bytes(), 30);
        collector.finish(30);
        collector.record_events(&monitor::RequestRecord {
            duration_ms: 30,
            gateway_actions: vec![
                monitor::GatewayAction {
                    action: "session_reset".into(),
                    attempt: 2,
                    elapsed_ms: 12,
                    count: 1,
                    ..monitor::GatewayAction::default()
                },
                monitor::GatewayAction {
                    action: "opaque-secret-action".into(),
                    ..monitor::GatewayAction::default()
                },
            ],
            attempts: vec![monitor::AttemptRecord {
                reason: "opaque-secret-reason".into(),
                status: Some(400),
                duration_ms: 10,
                ..monitor::AttemptRecord::default()
            }],
            ..monitor::RequestRecord::default()
        });
        let details = collector.details(None, "test-model", 30, None, str::to_owned);
        assert_eq!(details.events[0].kind, "session_reset");
        assert!(
            details
                .events
                .iter()
                .any(|event| event.kind == "upstream_attempt")
        );
        assert!(
            !serde_json::to_string(&details)
                .unwrap()
                .contains("opaque-secret")
        );
    }

    #[test]
    fn output_events_and_details_are_bounded_after_redaction() {
        let secret = "opaque-boundary-secret";
        let text = format!(
            "{}{}{}",
            "x".repeat(MAX_TEXT_BYTES - 5),
            secret,
            "测".repeat(30)
        );
        let mut collector = Collector::new(false);
        for _ in 0..150 {
            collector.add_event("fixture", 1, &"测".repeat(1_000));
        }
        collector.push(completed(&text).to_string().as_bytes(), 2);
        collector.finish(2);
        let detail = collector.details(None, "test-model", 2, None, |value| {
            redact_sensitive(value, [secret])
        });
        assert!(detail.truncated);
        assert!(detail.response_text.len() <= MAX_TEXT_BYTES);
        assert!(!detail.response_text.contains("opaque"));
        assert_eq!(detail.events.len(), MAX_EVENTS);
        assert!(
            detail
                .events
                .iter()
                .all(|event| event.detail.len() <= MAX_EVENT_BYTES)
        );
    }

    #[test]
    fn oversized_input_and_failed_transport_never_return_partial_output() {
        let mut collector = Collector::new(false);
        collector.push(&vec![b'x'; MAX_RESPONSE_BYTES + 1], 1);
        collector.finish(2);
        let detail = collector.details(
            None,
            "test-model",
            2,
            Some("probe_response_too_large"),
            str::to_owned,
        );
        assert!(detail.truncated);
        assert!(detail.response_text.is_empty());
        let mut collector = Collector::new(false);
        collector.push(completed("partial-secret").to_string().as_bytes(), 1);
        collector.finish(2);
        let detail = collector.details(None, "test-model", 2, Some("probe_timeout"), str::to_owned);
        assert!(detail.response_text.is_empty());
        assert!(detail.error_message.unwrap().contains("超过时间限制"));
    }

    #[test]
    fn credential_headers_and_json_fields_hide_unknown_tokens() {
        for message in [
            "Authorization: Basic dXNlcjpwYXNzd29yZA==",
            "Proxy-Authorization = Bearer fixture-opaque-token",
            r#"{"Authorization": "opaque fixture token", "error": "denied"}"#,
            r#"{"api_key":"fixture-opaque-token"}"#,
            "X-API-Key: fixture-opaque-token",
            "password='fixture opaque password'",
            "refresh_token = fixture-opaque-token",
            "Bearer \"fixture opaque token\"",
        ] {
            let safe = redact_sensitive(message, std::iter::empty());
            assert!(safe.contains("[REDACTED]"));
            assert!(!safe.contains("fixture"));
            assert!(!safe.contains("dXNlcjpwYXNzd29yZA"));
        }
        assert_eq!(
            redact_sensitive("basic-model returned 2 tokens", []),
            "basic-model returned 2 tokens"
        );
    }

    #[test]
    fn repeated_and_overlapping_secrets_do_not_modify_redaction_markers() {
        assert_eq!(
            redact_sensitive("fixture-secret", ["fixture-secret", "E", "E", "D"]),
            "[REDACTED]"
        );
        assert_eq!(redact_sensitive("abcdef", ["abcd", "cdef"]), "[REDACTED]");
        assert_eq!(
            redact_sensitive("Bearer opaque-fixture", ["opaque-fixture"]),
            "[REDACTED]"
        );
    }

    #[test]
    fn bounded_error_messages_sanitize_before_truncation_and_remove_hidden_characters() {
        let secret = "opaque-boundary-error-fixture";
        let message = format!("{}{} trailing", "x".repeat(MAX_ERROR_BYTES - 3), secret);
        let result = bounded_error_message(&message, [secret]).unwrap();
        assert_eq!(result.len(), MAX_ERROR_BYTES);
        assert!(!result.contains("opaque"));
        let result =
            bounded_error_message("\n拒绝\u{202e}\u{0000} 请求\tBearer fixture-secret\r", [])
                .unwrap();
        assert!(!result.contains("fixture-secret"));
        assert!(!result.chars().any(char::is_control));
        assert!(!result.contains('\u{202e}'));
        assert!(bounded_error_message("\n \t\u{202e}", []).is_none());
        let unicode = bounded_error_message(&"测".repeat(MAX_ERROR_BYTES), []).unwrap();
        assert!(unicode.len() <= MAX_ERROR_BYTES);
    }

    #[test]
    fn oversized_event_details_never_expose_a_cut_secret_and_metadata_is_bounded() {
        let secret = "opaque-cut-event-fixture";
        let mut collector = Collector::new(false);
        collector.add_event(
            "fixture",
            0,
            &format!("{}{}", "x".repeat(MAX_EVENT_BYTES - 5), secret),
        );
        collector.push(completed("OK").to_string().as_bytes(), 1);
        collector.finish(1);
        let detail = collector.details(
            Some(format!("{secret}{}", "x".repeat(256))),
            &format!("{secret}{}", "x".repeat(256)),
            1,
            None,
            |text| redact_sensitive(text, [secret]),
        );
        let serialized = serde_json::to_string(&detail).unwrap();
        assert!(!serialized.contains("opaque"));
        assert!(detail.truncated);
        assert!(detail.request_id.unwrap().len() <= 128);
        assert!(detail.requested_model.len() <= 128);
    }
}
