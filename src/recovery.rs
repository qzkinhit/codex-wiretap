//! Bounded error classification and explicit request recovery helpers.
//! The caller owns retry limits, endpoint checks and successful-response checks.

use std::{collections::HashMap, sync::Mutex, time::Instant};

use http::{HeaderMap, HeaderValue};
use serde_json::Value;

const MAX_ERROR_BYTES: usize = 256 * 1024;
const MAX_ERROR_TEXT: usize = 8192;
const MAX_SESSION_REMAPS: usize = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryKind {
    /// This describes one rejection; the caller must establish persistence.
    RateLimited,
    SessionResourceUnavailable,
    IncompatibleEncryptedContent,
}

fn error_text(value: &Value) -> Option<String> {
    let value = value.as_str()?;
    (value.len() <= MAX_ERROR_TEXT).then(|| value.to_ascii_lowercase())
}

fn normalized_words(value: &str) -> String {
    value.replace(['_', '-'], " ")
}

fn classify_record(status: u16, value: &Value, event_error: bool) -> Option<RecoveryKind> {
    let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    let explicit_error = event_error
        || matches!(event_type, "error" | "response.failed" | "response.error")
        || value.get("error").is_some_and(Value::is_object)
        || value
            .pointer("/response/error")
            .is_some_and(Value::is_object);
    if status < 400 && !explicit_error {
        return None;
    }
    let error = value
        .get("error")
        .filter(|v| v.is_object())
        .or_else(|| value.pointer("/response/error").filter(|v| v.is_object()))
        .unwrap_or(value);
    let code = error.get("code").and_then(error_text).unwrap_or_default();
    let kind = error.get("type").and_then(error_text).unwrap_or_default();
    let message = error
        .get("message")
        .and_then(error_text)
        .or_else(|| value.get("error").and_then(error_text))
        .unwrap_or_default();
    let event_status = [
        error.get("status"),
        error.get("status_code"),
        value.get("status"),
        value.get("status_code"),
    ]
    .into_iter()
    .flatten()
    .find_map(|v| v.as_u64().and_then(|v| u16::try_from(v).ok()));
    let effective_status = if status >= 400 {
        status
    } else {
        event_status.unwrap_or(status)
    };
    // Explicit HTTP/event status takes precedence over a possibly generic code.
    let status_unspecified = status < 400 && event_status.is_none();
    if effective_status == 429
        || (status_unspecified
            && explicit_error
            && [code.as_str(), kind.as_str()]
                .iter()
                .any(|v| matches!(*v, "rate_limit_exceeded" | "too_many_requests")))
    {
        return Some(RecoveryKind::RateLimited);
    }
    let words = normalized_words(&message);
    if (effective_status == 502 || (status_unspecified && explicit_error))
        && ([code.as_str(), kind.as_str()].iter().any(|v| {
            matches!(
                *v,
                "resource_not_found" | "deploymentnotfound" | "deployment_not_found"
            )
        }) || message.contains("resource_not_found")
            || words.contains("failed to find upstream resource")
            || words.contains("upstream resource not found")
            || words.contains("deploymentnotfound")
            || words.contains("deployment not found"))
    {
        return Some(RecoveryKind::SessionResourceUnavailable);
    }
    if effective_status == 400 {
        let encrypted_code = [code.as_str(), kind.as_str()].iter().any(|v| {
            matches!(
                *v,
                "invalid_encrypted_content"
                    | "incompatible_encrypted_content"
                    | "encrypted_content_invalid"
                    | "invalid_encrypted_reasoning_content"
                    | "encrypted_content_decryption_failed"
                    | "invalid_encrypted_content_signature"
            )
        });
        let encrypted_subject =
            words.contains("encrypted content") || words.contains("encrypted reasoning");
        let encrypted_message = encrypted_subject
            && [
                "invalid",
                "incompatible",
                "decrypt",
                "signature",
                "could not be verified",
                "cannot be verified",
            ]
            .iter()
            .any(|word| words.contains(word));
        let decryption_message = [
            "failed to decrypt",
            "unable to decrypt",
            "decryption failed",
        ]
        .iter()
        .any(|word| words.contains(word));
        // A failed credential, transport or tool-result decryption is unrelated
        // to the opaque state carried by Responses reasoning/compaction items.
        let state_subject =
            encrypted_subject || words.contains("reasoning") || words.contains("compaction");
        let state_decryption = state_subject
            && (decryption_message || code == "decryption_failed" || kind == "decryption_failed");
        if encrypted_code || encrypted_message || state_decryption {
            return Some(RecoveryKind::IncompatibleEncryptedContent);
        }
    }
    None
}

/// Inspect only bounded JSON error fields or complete SSE error frames.
/// No message, credential or arbitrary upstream code is returned or logged.
/// A 429 is one rate-limit observation, not proof that session recovery will help.
pub fn classify_error(status: u16, body: &[u8]) -> Option<RecoveryKind> {
    if body.len() > MAX_ERROR_BYTES {
        return None;
    }
    if status == 429 {
        return Some(RecoveryKind::RateLimited);
    }
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return classify_record(status, &value, false);
    }
    let text = std::str::from_utf8(body).ok()?;
    let mut data = String::new();
    let mut event_error = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            if !data.is_empty()
                && let Ok(value) = serde_json::from_str::<Value>(&data)
                && let Some(kind) = classify_record(status, &value, event_error)
            {
                return Some(kind);
            }
            data.clear();
            event_error = false;
        } else if let Some(value) = line.strip_prefix("event:") {
            event_error = matches!(value.trim(), "error" | "response.failed" | "response.error");
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    // A trailing frame without the blank event delimiter may be incomplete.
    None
}

fn has_text(value: &Value) -> bool {
    if value.as_str().is_some_and(|v| !v.trim().is_empty()) {
        return true;
    }
    value.as_array().is_some_and(|parts| {
        parts.iter().any(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
        })
    })
}

/// Used only after a confirmed encrypted-content 400 on `/v1/responses/compact`.
/// Remove opaque reasoning/compaction items, retaining plaintext reasoning when
/// available. Messages, function calls and their outputs keep their order and data.
/// Returns the number of affected items; the caller permits at most one repair.
pub fn strip_incompatible_encrypted_items(body: &mut Value) -> usize {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut changed = 0;
    items.retain_mut(|item| {
        let Some(object) = item.as_object_mut() else {
            return true;
        };
        let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(kind, "reasoning" | "compaction" | "compaction_summary")
            || !object
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|v| !v.is_empty())
        {
            return true;
        }
        changed += 1;
        if kind != "reasoning" {
            return false;
        }
        let keep_plaintext = object.get("summary").is_some_and(has_text)
            || object.get("content").is_some_and(has_text);
        if keep_plaintext {
            object.remove("encrypted_content");
            if object.get("content").is_some_and(Value::is_null) {
                object.remove("content");
            }
        }
        keep_plaintext
    });
    changed
}

fn request_body(body: &Value) -> &Value {
    body.get("response")
        .filter(|v| v.is_object())
        .unwrap_or(body)
}

fn has_previous_response(body: &Value) -> bool {
    body.get("previous_response_id")
        .is_some_and(|value| !value.is_null() && !value.as_str().is_some_and(str::is_empty))
        || request_body(body)
            .get("previous_response_id")
            .is_some_and(|value| !value.is_null() && !value.as_str().is_some_and(str::is_empty))
}

/// Apply an already established mapping after the caller has checked account
/// affinity. This also applies to continuations with `previous_response_id`.
/// Only fields already present in the supported request shape are rewritten.
pub fn apply_known_upstream_session(
    headers: &mut HeaderMap,
    body: &mut Value,
    new_id: &str,
) -> bool {
    if new_id.len() != 36 {
        return false;
    }
    let Ok(parsed) = uuid::Uuid::parse_str(new_id) else {
        return false;
    };
    if parsed.is_nil() {
        return false;
    }
    let new_id = parsed.hyphenated().to_string();
    let Ok(header_value) = HeaderValue::from_str(&new_id) else {
        return false;
    };
    let mut changed = false;
    for name in ["thread-id", "session-id"] {
        if headers.contains_key(name) {
            changed |= headers.get_all(name).iter().count() != 1
                || headers.get(name) != Some(&header_value);
            headers.insert(name, header_value.clone());
        }
    }
    let body = if body.get("response").is_some_and(Value::is_object) {
        body.get_mut("response").expect("checked response object")
    } else {
        body
    };
    if let Some(metadata) = body
        .get_mut("client_metadata")
        .and_then(Value::as_object_mut)
    {
        for field in ["thread_id", "session_id"] {
            if let Some(value) = metadata.get_mut(field)
                && value.is_string()
            {
                changed |= value.as_str() != Some(new_id.as_str());
                *value = Value::String(new_id.clone());
            }
        }
    }
    if let Some(value) = body.get_mut("prompt_cache_key")
        && value.is_string()
    {
        changed |= value.as_str() != Some(new_id.as_str());
        *value = Value::String(new_id);
    }
    changed
}

/// Apply a newly rotated session only to a self-contained request. A request
/// referring to an earlier server response may only use its established mapping.
pub fn rewrite_upstream_session(headers: &mut HeaderMap, body: &mut Value, new_id: &str) -> bool {
    if has_previous_response(body) {
        return false;
    }
    apply_known_upstream_session(headers, body, new_id)
}

struct Remap {
    session: String,
    created: Instant,
}

/// The scope is a caller-generated digest including conversation, account,
/// model and upstream. Raw keys and conversation bodies must never be stored.
#[derive(Default)]
pub struct SessionRemaps {
    entries: Mutex<HashMap<String, Remap>>,
}

fn valid_scope(scope: &str) -> bool {
    !scope.is_empty() && scope.len() <= 128 && scope.bytes().all(|c| c.is_ascii_graphic())
}

impl SessionRemaps {
    pub fn lookup(&self, scope: &str, ttl_ms: u64) -> Option<String> {
        if !valid_scope(scope) {
            return None;
        }
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let expired = entries.get(scope).is_some_and(|entry| {
            now.saturating_duration_since(entry.created).as_millis() >= u128::from(ttl_ms)
        });
        if expired {
            entries.remove(scope);
            return None;
        }
        entries.get(scope).map(|entry| entry.session.clone())
    }

    pub fn rotate(&self, scope: &str, ttl_ms: u64) -> String {
        let session = uuid::Uuid::new_v4().hyphenated().to_string();
        if !valid_scope(scope) || ttl_ms == 0 {
            return session;
        }
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, entry| {
            now.saturating_duration_since(entry.created).as_millis() < u128::from(ttl_ms)
        });
        if entries.len() >= MAX_SESSION_REMAPS && !entries.contains_key(scope) {
            let oldest = entries
                .iter()
                .min_by_key(|(_, entry)| entry.created)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
        entries.insert(
            scope.to_owned(),
            Remap {
                session: session.clone(),
                created: now,
            },
        );
        session
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{sync::Arc, time::Duration};

    const OLD: &str = "4b3892b5-57b8-45f2-8219-cc7f0a05107a";
    const NEW: &str = "e1b795d0-2766-4290-9cb0-3cf45b86cb10";

    #[test]
    fn classifies_only_explicit_resource_502_and_encrypted_400() {
        for code in [
            "resource_not_found",
            "DeploymentNotFound",
            "deployment_not_found",
        ] {
            let body = json!({"error":{"code":code}}).to_string();
            assert_eq!(
                classify_error(502, body.as_bytes()),
                Some(RecoveryKind::SessionResourceUnavailable)
            );
            assert_eq!(classify_error(500, body.as_bytes()), None);
            assert_eq!(classify_error(400, body.as_bytes()), None);
        }
        for message in [
            "Failed to find upstream resource",
            "upstream resource not found",
            "resource_not_found",
        ] {
            let body = json!({"error":{"message":message}}).to_string();
            assert_eq!(
                classify_error(502, body.as_bytes()),
                Some(RecoveryKind::SessionResourceUnavailable)
            );
        }
        for error in [
            json!({"code":"invalid_encrypted_content"}),
            json!({"code":"incompatible_encrypted_content"}),
            json!({"message":"Encrypted content could not be verified"}),
            json!({"message":"invalid encrypted_content signature"}),
            json!({"message":"Failed to decrypt reasoning state"}),
            json!({"code":"decryption_failed","message":"compaction state"}),
        ] {
            let body = json!({"error":error}).to_string();
            assert_eq!(
                classify_error(400, body.as_bytes()),
                Some(RecoveryKind::IncompatibleEncryptedContent)
            );
            assert_eq!(classify_error(502, body.as_bytes()), None);
        }
        for message in [
            "Bad gateway",
            "Resource exhausted",
            "Signature verification failed",
            "Invalid API key",
            "Unknown model",
            "encrypted content is supported",
            "Failed to decrypt authentication token",
            "Unable to decrypt tool result",
        ] {
            let body = json!({"error":{"message":message}}).to_string();
            assert_eq!(classify_error(400, body.as_bytes()), None);
            assert_eq!(classify_error(502, body.as_bytes()), None);
        }
        assert_eq!(
            classify_error(400, br#"{"error":{"code":"decryption_failed"}}"#),
            None
        );
    }

    #[test]
    fn explicit_status_cannot_be_overridden_by_a_recovery_code() {
        for (status, code) in [(401, "rate_limit_exceeded"), (404, "resource_not_found")] {
            let body = json!({"error":{"code":code}}).to_string();
            assert_eq!(classify_error(status, body.as_bytes()), None);
            let frame = format!(
                "event: error\ndata: {{\"status\":{status},\"error\":{{\"code\":\"{code}\"}}}}\n\n"
            );
            assert_eq!(classify_error(200, frame.as_bytes()), None);
        }
    }

    #[test]
    fn parses_complete_sse_errors_without_treating_generated_text_as_errors() {
        let body = b"event: error\r\ndata: {\"status\":502,\"error\":{\"code\":\"resource_not_found\"}}\r\n\r\n";
        assert_eq!(
            classify_error(200, body),
            Some(RecoveryKind::SessionResourceUnavailable)
        );
        let body = b"event: error\ndata: {\"error\":{\"code\":\"rate_limit_exceeded\"}}\n\n";
        assert_eq!(classify_error(200, body), Some(RecoveryKind::RateLimited));
        for event in ["response.failed", "response.error"] {
            let body = format!(
                "data: {{\"type\":\"{event}\",\"response\":{{\"error\":{{\"code\":\"resource_not_found\"}}}}}}\n\n"
            );
            assert_eq!(
                classify_error(200, body.as_bytes()),
                Some(RecoveryKind::SessionResourceUnavailable)
            );
            let body = format!("event: {event}\ndata: {{\"code\":\"rate_limit_exceeded\"}}\n\n");
            assert_eq!(
                classify_error(200, body.as_bytes()),
                Some(RecoveryKind::RateLimited)
            );
        }
        let body = b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"status_code\":400,\"code\":\"invalid_encrypted_content\"}}}\n\n";
        assert_eq!(
            classify_error(200, body),
            Some(RecoveryKind::IncompatibleEncryptedContent)
        );
        let body =
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"resource_not_found\"}\n\n";
        assert_eq!(classify_error(502, body), None);
        assert_eq!(
            classify_error(200, br#"{"message":"Failed to find upstream resource"}"#),
            None
        );
        assert_eq!(
            classify_error(
                200,
                b"event: error\ndata: {\"status\":502,\"code\":\"resource_not_found\"}"
            ),
            None
        );
        assert_eq!(
            classify_error(502, b"<html>resource_not_found</html>"),
            None
        );
        assert_eq!(classify_error(502, &[0xff]), None);
        assert_eq!(classify_error(400, &vec![b' '; MAX_ERROR_BYTES + 1]), None);
        assert_eq!(classify_error(429, b""), Some(RecoveryKind::RateLimited));
    }

    #[test]
    fn strips_only_encrypted_state_and_keeps_plaintext_and_tool_dependencies() {
        let message = json!({"type":"message","role":"user","content":"keep all text"});
        let call = json!({"type":"function_call","call_id":"call_a","name":"tool","arguments":"{}","encrypted_content":"unrelated"});
        let output = json!({"type":"function_call_output","call_id":"call_a","output":"result"});
        let mut body = json!({"model":"model-a","reasoning":{"effort":"high"},"input":[message.clone(),{"type":"reasoning","encrypted_content":"opaque","summary":[]},call.clone(),{"type":"compaction","encrypted_content":"opaque-2"},output.clone(),{"type":"reasoning","encrypted_content":"opaque-3","content":null,"summary":[{"type":"summary_text","text":"keep summary"}]},{"type":"reasoning","summary":[]},{"type":"compaction_summary","encrypted_content":"opaque-4"}]});
        assert_eq!(strip_incompatible_encrypted_items(&mut body), 4);
        assert_eq!(body["input"][0], message);
        assert_eq!(body["input"][1], call);
        assert_eq!(body["input"][2], output);
        assert_eq!(
            body["input"][3],
            json!({"type":"reasoning","summary":[{"type":"summary_text","text":"keep summary"}]})
        );
        assert_eq!(body["input"][4], json!({"type":"reasoning","summary":[]}));
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(strip_incompatible_encrypted_items(&mut body), 0);
        for mut body in [
            json!({"input":"keep text"}),
            json!({"input":[{"type":"reasoning","encrypted_content":null}]}),
            Value::Null,
        ] {
            let before = body.clone();
            assert_eq!(strip_incompatible_encrypted_items(&mut body), 0);
            assert_eq!(body, before);
        }
    }

    #[test]
    fn rewrite_only_existing_supported_fields_and_preserve_other_parameters() {
        let mut headers = HeaderMap::new();
        headers.append("thread-id", OLD.parse().unwrap());
        headers.append("thread-id", "other".parse().unwrap());
        headers.insert("session-id", OLD.parse().unwrap());
        headers.insert("authorization", "Bearer fixture-secret".parse().unwrap());
        let mut body = json!({"client_metadata":{"thread_id":OLD,"session_id":OLD,"tag":"keep"},"prompt_cache_key":"cache-original","reasoning":{"effort":"ultra"},"input":"keep"});
        assert!(rewrite_upstream_session(&mut headers, &mut body, NEW));
        assert_eq!(headers.get_all("thread-id").iter().count(), 1);
        assert_eq!(headers["thread-id"], NEW);
        assert_eq!(headers["session-id"], NEW);
        assert_eq!(headers["authorization"], "Bearer fixture-secret");
        assert_eq!(body["client_metadata"]["thread_id"], NEW);
        assert_eq!(body["client_metadata"]["session_id"], NEW);
        assert_eq!(body["prompt_cache_key"], NEW);
        assert_eq!(body["reasoning"]["effort"], "ultra");
        assert_eq!(body["client_metadata"]["tag"], "keep");
        assert!(!rewrite_upstream_session(&mut headers, &mut body, NEW));
        let mut untouched = json!({"input":"keep"});
        let mut no_headers = HeaderMap::new();
        assert!(!rewrite_upstream_session(
            &mut no_headers,
            &mut untouched,
            NEW
        ));
        assert_eq!(untouched, json!({"input":"keep"}));
        assert!(no_headers.is_empty());
    }

    #[test]
    fn rotation_rejects_previous_response_but_known_mapping_preserves_continuation() {
        for wrapped in [false, true] {
            let original = json!({"previous_response_id":"resp_existing","prompt_cache_key":OLD});
            let mut body = if wrapped {
                json!({"response":original})
            } else {
                original
            };
            let before = body.clone();
            let mut headers = HeaderMap::new();
            headers.insert("session-id", OLD.parse().unwrap());
            assert!(!rewrite_upstream_session(&mut headers, &mut body, NEW));
            assert_eq!(body, before);
            assert_eq!(headers["session-id"], OLD);
            assert!(apply_known_upstream_session(&mut headers, &mut body, NEW));
            assert_eq!(headers["session-id"], NEW);
            assert_eq!(request_body(&body)["previous_response_id"], "resp_existing");
            assert_eq!(request_body(&body)["prompt_cache_key"], NEW);
        }
        let mut headers = HeaderMap::new();
        let mut body = json!({"prompt_cache_key":OLD});
        assert!(!apply_known_upstream_session(
            &mut headers,
            &mut body,
            "invalid"
        ));
        assert!(!apply_known_upstream_session(
            &mut headers,
            &mut body,
            "00000000-0000-0000-0000-000000000000"
        ));
        assert_eq!(body["prompt_cache_key"], OLD);
    }

    #[test]
    fn remaps_are_scoped_expire_and_rotate_without_exposing_request_data() {
        let mappings = SessionRemaps::default();
        assert_eq!(mappings.lookup("scope-a", 1000), None);
        let first = mappings.rotate("scope-a", 1000);
        assert_eq!(uuid::Uuid::parse_str(&first).unwrap().get_version_num(), 4);
        assert_eq!(mappings.lookup("scope-a", 1000), Some(first.clone()));
        assert_eq!(mappings.lookup("scope-b", 1000), None);
        let second = mappings.rotate("scope-a", 1000);
        assert_ne!(first, second);
        mappings
            .entries
            .lock()
            .unwrap()
            .get_mut("scope-a")
            .unwrap()
            .created = Instant::now() - Duration::from_secs(2);
        assert_eq!(mappings.lookup("scope-a", 1000), None);
        assert!(mappings.entries.lock().unwrap().is_empty());
        mappings.rotate("scope-a", 0);
        mappings.rotate(&"a".repeat(129), 1000);
        assert!(mappings.entries.lock().unwrap().is_empty());
    }

    #[test]
    fn remaps_enforce_capacity_and_allow_concurrent_access() {
        let mappings = Arc::new(SessionRemaps::default());
        {
            let now = Instant::now();
            let mut entries = mappings.entries.lock().unwrap();
            for index in 0..MAX_SESSION_REMAPS {
                entries.insert(
                    format!("scope-{index}"),
                    Remap {
                        session: OLD.into(),
                        created: now,
                    },
                );
            }
            entries.get_mut("scope-0").unwrap().created = now - Duration::from_secs(1);
        }
        mappings.rotate("fresh", 60_000);
        assert_eq!(mappings.entries.lock().unwrap().len(), MAX_SESSION_REMAPS);
        assert_eq!(mappings.lookup("scope-0", 60_000), None);
        std::thread::scope(|scope| {
            for index in 0..8 {
                let mappings = Arc::clone(&mappings);
                scope.spawn(move || {
                    let key = format!("thread-{index}");
                    let session = mappings.rotate(&key, 60_000);
                    assert_eq!(mappings.lookup(&key, 60_000), Some(session));
                });
            }
        });
        assert_eq!(mappings.entries.lock().unwrap().len(), MAX_SESSION_REMAPS);
    }
}
