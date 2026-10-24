"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const { compareReleaseVersions, safeReleaseUrl, updateActionState, conversationInfo, matchesConversationFilter, classifyRequestOutcome, manualAccountPayload, requestRouteMode, classifyServiceTier, recordServiceTier, matchesServiceTierFilter, sourceProviderRows, draftFromConfig, routingConfigPayload, accountModels, filterAccounts, filterModelHealth, syncConfigPatch, normalizeProbeResult, gatewayActionRows, attemptErrorDetails, effectiveOutputWait } = require("./app.js");

test("retry settings distinguish fifteen retries from sixteen total attempts and support both boundaries", () => {
  const defaults = draftFromConfig({});
  assert.equal(defaults.max_retries, 15);
  assert.equal(routingConfigPayload(defaults, {}).max_attempts, 16);
  for (const [attempts, retries] of [[1, 0], [16, 15], [100, 99]]) {
    const draft = draftFromConfig({ max_attempts: attempts });
    assert.equal(draft.max_retries, retries);
    assert.equal(routingConfigPayload(draft, {}).max_attempts, attempts);
  }
  const edited = { ...defaults, max_retries: 0 };
  assert.equal(routingConfigPayload(edited, {}).max_attempts, 1);
  assert.equal(Object.hasOwn(routingConfigPayload(edited, {}), "max_retries"), false);
});

test("attempt errors preserve actual upstream diagnostics and support older records", () => {
  assert.deepEqual(attemptErrorDetails({ reason: "previous_response_not_found", error_message: "Previous response could not be found." }), {
    reason: "previous_response_not_found", message: "Previous response could not be found.",
  });
  assert.deepEqual(attemptErrorDetails({ reason: "completed" }), { reason: "completed", message: "" });
  assert.deepEqual(attemptErrorDetails({}), { reason: "", message: "" });
  const details = attemptErrorDetails({ reason: "upstream_error", error_message: "<img src=x onerror=alert(1)> SK-secret12345\nAuthorization: Bearer private-token\n" + "x".repeat(3000) });
  assert.match(details.message, /^<img/);
  assert.equal(details.message.includes("secret12345"), false);
  assert.equal(details.message.includes("private-token"), false);
  assert.equal(details.message.length, 2048);
});

test("gateway action metadata keeps preparation distinct from numbered attempts", () => {
  const rows = gatewayActionRows({ gateway_actions: [
    { action: "image_compression", attempt: 0, count: 2, before_bytes: 3000, after_bytes: 1000 },
    { action: "session_reset", attempt: 1, elapsed_ms: 700 },
    { action: "compaction_repair", attempt: 2, elapsed_ms: 1100 },
    { action: "future_action" },
  ] });
  assert.equal(rows[0].attempt, 0);
  assert.equal(rows[0].before_bytes, 3000);
  assert.equal(rows[1].label, "重置会话");
  assert.equal(rows[1].attempt, 1);
  assert.equal(rows[2].label, "修复压缩上下文");
  assert.equal(rows[3].label, "其他网关处理");
  assert.equal(rows[3].attempt, null);
  assert.deepEqual(gatewayActionRows({}), []);
});

test("first effective output time never falls back to transport or total duration", () => {
  assert.equal(effectiveOutputWait({ first_output_ms: 0, ttfb_ms: 100, duration_ms: 1200 }), 0);
  assert.equal(effectiveOutputWait({ first_output_ms: 850, ttfb_ms: 100, duration_ms: 1200 }), 850);
  for (const first_output_ms of [undefined, null, -1, NaN, Infinity]) {
    assert.equal(effectiveOutputWait({ first_output_ms, ttfb_ms: 100, duration_ms: 1200 }), null);
  }
});

test("model filters include models without kept keys and preserve account labels and input state", () => {
  const state = { accounts: [
    { id: "a", label: "示例账号 A", present: true, models: ["gpt-6-astra", "gpt-6.1-sol"], model_health: [{ model: "gpt-6-astra", in_flight: 1 }, { model: "gpt-6.1-sol", in_flight: 0 }] },
    { id: "b", label: "示例账号 B", present: true, models: ["gpt-6-astra"] },
    { id: "old", present: false, models: ["gpt-6.1-sol"] },
  ], source: { providers: [{ model: "custom-model", kept_keys: 0 }] }, config: { policy: { strategy: "sticky" } } };
  const before = JSON.stringify(state);
  assert.deepEqual(accountModels(state), ["custom-model", "gpt-6-astra", "gpt-6.1-sol"]);
  const rows = filterAccounts(state.accounts, "gpt-6.1-sol");
  assert.equal(rows.length, 1);
  assert.equal(rows[0].label, "示例账号 A");
  assert.deepEqual(filterModelHealth(rows[0], "gpt-6.1-sol"), [{ model: "gpt-6.1-sol", in_flight: 0 }]);
  assert.equal(filterAccounts(state.accounts).length, 2);
  assert.equal(filterAccounts(state.accounts, "custom-model").length, 0);
  assert.equal(JSON.stringify(state), before);
});

test("all-key mode reports imported Sol keys even when its kept candidate set is empty", () => {
  const [row] = sourceProviderRows({ providers: [{ model: "gpt-6.1-sol", total_keys: 11, kept_keys: 0, imported_keys: 11 }] }, true);
  assert.equal(row.imported_keys, 11);
  assert.equal(row.kept_keys, 0);
  assert.equal(row.description, "已同步全部 Key，可用性待请求确认");
});

test("sync controls preserve independent pause and scope settings", () => {
  assert.deepEqual(syncConfigPatch("pause", { sync_paused: false, biwrite_import_all: true }), { sync_paused: true });
  assert.deepEqual(syncConfigPatch("pause", { sync_paused: true }), { sync_paused: false });
  assert.deepEqual(syncConfigPatch("scope", { source_mode: "biwrite", sync_paused: true, biwrite_import_all: false }), { source_mode: "biwrite", biwrite_import_all: true });
  assert.deepEqual(syncConfigPatch("scope", { source_mode: "biwrite", biwrite_import_all: true }), { source_mode: "biwrite", biwrite_import_all: false });
  assert.deepEqual(syncConfigPatch("scope", { source_mode: "manual", biwrite_import_all: true }), { source_mode: "biwrite", biwrite_import_all: true });
});

test("probe failures retain diagnostics and usage without becoming HTTP-only successes", () => {
  const result = normalizeProbeResult({ ok: false, status: 200, message: "上游失败", error_code: "resource_exhausted", details: { requested_model: "gpt-6-astra", request_text: "Reply only OK.", response_text: "", error_message: "资源暂不可用", elapsed_ms: 350, events: [{ type: "upstream_error", elapsed_ms: 340, detail: "资源暂不可用" }], usage: { input_tokens: 0 } } }, "gpt-6.1-sol");
  assert.equal(result.ok, false);
  assert.equal(result.status, 200);
  assert.equal(result.details.requested_model, "gpt-6-astra");
  assert.equal(result.details.elapsed_ms, 350);
  assert.equal(result.details.usage.input_tokens, 0);
  assert.equal(result.details.usage.output_tokens, null);
  assert.equal(result.details.events[0].detail, "资源暂不可用");
  assert.equal(normalizeProbeResult({ status: 200 }, "model").ok, false);
});

test("probe diagnostics allow only display fields and redact credentials before retaining them", () => {
  const secret = "local-private-value";
  const result = normalizeProbeResult({ ok: false, headers: { Authorization: secret }, message: `Bearer ${secret}`, details: { response_text: `<img src=x onerror=alert(1)> sk-sensitive123456 ${secret}`, error_message: `Authorization: Basic secret123`, events: [{ type: "error", elapsed_ms: 1, detail: `api_key=some-private-value` }], raw_request: secret } }, "gpt-6-astra", 1, [secret]);
  const encoded = JSON.stringify(result);
  for (const value of [secret, "sk-sensitive123456", "secret123", "some-private-value", "raw_request", "headers"]) assert.equal(encoded.includes(value), false, value);
  // HTML stays ordinary text. Rendering uses textContent, never HTML insertion.
  assert.match(result.details.response_text, /^<img/);
});

test("probe logs enforce text and event limits and report truncation", () => {
  const result = normalizeProbeResult({ ok: true, details: { response_text: "x".repeat(70000), events: Array.from({ length: 120 }, () => ({ type: "delta", elapsed_ms: 0, detail: "y".repeat(3000) })) } }, "model");
  assert.equal(result.details.response_text.length, 65536);
  assert.equal(result.details.events.length, 100);
  assert.equal(result.details.events[0].detail.length, 2048);
  assert.equal(result.details.truncated, true);
});

test("gateway controls round-trip explicit values without resetting immediate sync controls", () => {
  const config = { session_recovery_enabled: false, compaction_repair_enabled: true, compress_images: true, first_output_timeout_ms: 45000, sync_paused: true, biwrite_import_all: true };
  const payload = routingConfigPayload(draftFromConfig(config), config);
  assert.equal(payload.session_recovery_enabled, false);
  assert.equal(payload.compaction_repair_enabled, true);
  assert.equal(payload.compress_images, true);
  assert.equal(payload.first_output_timeout_ms, 45000);
  assert.equal(Object.hasOwn(payload, "sync_paused"), false);
  assert.equal(Object.hasOwn(payload, "biwrite_import_all"), false);
});

test("source synchronization retains Sol with zero candidates despite historical success", () => {
  const rows = sourceProviderRows({ providers: [
    { model: "gpt-6-astra", total_keys: 4, kept_keys: 2, imported_keys: 2 },
    { model: "gpt-6.1-sol", total_keys: 4, kept_keys: 0, imported_keys: 0, last_ok_ms: Date.now() },
    { model: "gpt-6.1-sol", total_keys: 0, kept_keys: 0, imported_keys: 0 },
  ] });
  assert.equal(rows.length, 3);
  assert.deepEqual(rows[1], { model: "gpt-6.1-sol", total_keys: 4, kept_keys: 0, imported_keys: 0, description: "已读取配置，当前无保活候选" });
  assert.equal(rows[2].total_keys, 0);
  assert.equal(rows[0].imported_keys, 2);
  assert.match(rows[0].description, /可用性待请求确认/);
});

test("source synchronization distinguishes excluded candidates and unknown counts", () => {
  const [excluded, unknown] = sourceProviderRows({ providers: [
    { model: "gpt-6-astra", total_keys: 3, kept_keys: 1, imported_keys: 0 },
    { model: "gpt-6.1-sol" },
  ] });
  assert.equal(excluded.description, "候选未导入，请检查来源状态");
  assert.equal(unknown.kept_keys, null);
  assert.equal(unknown.description, "等待来源同步结果");
  assert.deepEqual(sourceProviderRows({}), []);
});

test("routing settings preserve an explicit upstream and unedited policy fields", () => {
  const config = { upstream_base_url: "https://api.example.com/v1", source_mode: "biwrite", policy: { strategy: "sticky", model_specific_limit: 3 }, queue_timeout_ms: 120000 };
  const draft = draftFromConfig(config);
  draft.strategy = "round_robin";
  const payload = routingConfigPayload(draft, config);
  assert.equal(payload.upstream_base_url, config.upstream_base_url);
  assert.equal(payload.policy.strategy, "round_robin");
  assert.equal(payload.policy.model_specific_limit, 3);
  assert.equal(payload.queue_timeout_ms, config.queue_timeout_ms);
  assert.equal(Object.hasOwn(payload, "local_token"), false);
});

test("clearing the upstream address explicitly restores automatic discovery", () => {
  const config = { upstream_base_url: "https://api.example.com/v1" };
  const draft = draftFromConfig(config);
  draft.upstream_base_url = "   ";
  assert.equal(routingConfigPayload(draft, config).upstream_base_url, null);
  assert.equal(draftFromConfig({ upstream_base_url: null }).upstream_base_url, "");
  assert.equal(routingConfigPayload(draftFromConfig({}), {}).upstream_base_url, null);
});

test("HTTP 200 with upstream_failed is a failure, never a success", () => {
  const status = classifyRequestOutcome({ status: 200, duration_ms: 1200, outcome: "upstream_failed" });
  assert.equal(status.type, "failure");
  assert.equal(status.label, "上游失败");
  assert.equal(status.style, "red");
});

test("all backend failure outcomes take precedence over HTTP 200", () => {
  const failures = [
    "pool_exhausted", "unknown_response", "affinity_unavailable", "no_candidate",
    "connect_failed", "transport_uncertain", "header_timeout", "first_output_timeout", "response_read_failed",
    "upstream_failed", "stream_read_failed", "client_cancelled", "invalid_upstream_response",
  ];
  for (const outcome of failures) {
    const status = classifyRequestOutcome({ status: 200, duration_ms: 1200, outcome });
    assert.equal(status.type, "failure", outcome);
    assert.notEqual(status.style, "green", outcome);
  }
});

test("only the explicit completed outcome confirms success", () => {
  const status = classifyRequestOutcome({ status: 200, duration_ms: 1200, outcome: "completed" });
  assert.equal(status.type, "success");
  assert.equal(status.style, "green");
  for (const outcome of ["success", "ok", "complete", "new_unknown_terminal_state"]) {
    assert.notEqual(classifyRequestOutcome({ status: 200, duration_ms: 1200, outcome }).type, "success", outcome);
  }
});

test("a successful HTTP status alone cannot confirm a completed response", () => {
  for (const status of [200, 201, 204]) {
    assert.notEqual(classifyRequestOutcome({ status, duration_ms: 1200 }).type, "success");
    assert.notEqual(classifyRequestOutcome({ status, duration_ms: 0, outcome: "" }).type, "success");
  }
});

test("cancelled requests and unavailable pools remain distinguishable", () => {
  assert.equal(classifyRequestOutcome({ status: 200, outcome: "client_cancelled" }).label, "已取消");
  for (const outcome of ["pool_exhausted", "no_candidate", "affinity_unavailable"]) {
    assert.equal(classifyRequestOutcome({ status: 503, outcome }).label, "号池暂不可用");
  }
});

test("unfinished requests remain active while waiting for their terminal outcome", () => {
  for (const outcome of [undefined, "", "active", "pending", "running", "streaming", "in_progress"]) {
    const status = classifyRequestOutcome({ status: 200, duration_ms: null, outcome });
    assert.equal(status.type, "active", String(outcome));
    assert.notEqual(status.style, "green", String(outcome));
  }
});

test("manual accounts normalize model lists and omit an empty optional label", () => {
  const payload = manualAccountPayload({ key: "  mock-key  ", models: "gpt-6-astra, gpt-6.1-sol，gpt-6-astra\n", max_concurrency: "2", label: " " });
  assert.deepEqual(payload, { key: "mock-key", models: ["gpt-6-astra", "gpt-6.1-sol"], max_concurrency: 2 });
});

test("manual accounts reject empty keys, invalid models and invalid limits without echoing keys", () => {
  const key = "private-test-value";
  for (const fields of [
    { key: "", models: "gpt-6-astra" },
    { key, models: "" },
    { key, models: "gpt-6-astra gpt-6.1-sol" },
    { key, models: "gpt-6-astra", max_concurrency: 0 },
    { key, models: "gpt-6-astra", max_concurrency: 1.5 },
    { key, models: "gpt-6-astra", max_concurrency: 129 },
  ]) {
    assert.throws(() => manualAccountPayload(fields), (error) => !error.message.includes(key));
  }
});

test("manual payload only sends the documented account fields and defaults concurrency to one", () => {
  assert.deepEqual(manualAccountPayload({ key: "mock-key", models: "gpt-6-astra", label: " 备用 ", source_mode: "manual", strategy: "pinned" }), { key: "mock-key", models: ["gpt-6-astra"], label: "备用", max_concurrency: 1 });
});

test("legacy request records are explicitly identified as legacy monitoring", () => {
  assert.deepEqual(requestRouteMode({ route_mode: "legacy" }), { mode: "legacy", label: "旧方式监控" });
});

test("old records without route_mode keep the pool routing label", () => {
  for (const record of [{}, { route_mode: "" }, { route_mode: null }, { route_mode: "pool" }]) {
    assert.deepEqual(requestRouteMode(record), { mode: "pool", label: "号池调度" });
  }
});

test("Fast and priority share a display group while preserving both original tier values", () => {
  const record = { outcome: "completed", requested_service_tier: "fast", response_service_tier: "priority" };
  const tier = recordServiceTier(record);
  assert.equal(tier.comparison, "same");
  assert.equal(tier.fastToNormal, false);
  assert.equal(tier.requested.raw, "fast");
  assert.equal(tier.response.raw, "priority");
  assert.equal(matchesServiceTierFilter(record, "fast"), true);
  assert.equal(matchesServiceTierFilter(record, "fast_to_normal"), false);
});

test("completed Fast to default records report a different upstream declaration", () => {
  const record = { outcome: "completed", requested_service_tier: "priority", response_service_tier: "default" };
  const tier = recordServiceTier(record);
  assert.equal(tier.comparison, "different");
  assert.equal(tier.response.label, "普通");
  assert.equal(matchesServiceTierFilter(record, "fast_to_normal"), true);
});

test("failed and unfinished responses cannot confirm an ordinary response tier", () => {
  for (const outcome of ["upstream_failed", "stream_read_failed", "client_cancelled", "running", "", undefined]) {
    const record = { outcome, status: 200, requested_service_tier: "fast", response_service_tier: "default" };
    const tier = recordServiceTier(record);
    assert.equal(tier.responseConfirmed, false, String(outcome));
    assert.equal(tier.comparison, "unconfirmed", String(outcome));
    assert.equal(matchesServiceTierFilter(record, "fast_to_normal"), false, String(outcome));
    assert.equal(matchesServiceTierFilter(record, "unknown"), true, String(outcome));
  }
});

test("auto, absent and unknown tiers never become ordinary tiers", () => {
  for (const value of ["auto", "", undefined, null, "vendor_future_tier"]) {
    assert.notEqual(classifyServiceTier(value).group, "normal");
    const record = { outcome: "completed", requested_service_tier: "fast", response_service_tier: value };
    assert.equal(recordServiceTier(record).responseConfirmed, false);
    assert.equal(matchesServiceTierFilter(record, "unknown"), true);
  }
});

test("ultrafast, flex and scale retain independent groups", () => {
  for (const value of ["ultrafast", "flex", "scale"]) {
    const tier = classifyServiceTier(value);
    assert.equal(tier.group, value);
    assert.equal(tier.raw, value);
    assert.equal(tier.known, true);
    assert.equal(matchesServiceTierFilter({ outcome: "completed", requested_service_tier: value, response_service_tier: value }, "fast"), false);
  }
});


test("conversation names take precedence while older records retain their exact IDs", () => {
  const id = "01a16b50-e1f9-71d3-b9e2-24b065002660";
  assert.equal(conversationInfo({ conversation_id: id, conversation_title: "  安装与发布 DMG  " }).label, "安装与发布 DMG");
  for (const title of [undefined, null, "", " \n ", 42]) {
    const info = conversationInfo({ conversation_id: id, conversation_title: title });
    assert.equal(info.label, id);
    assert.equal(info.href, `codex://threads/${id}`);
  }
  assert.equal(conversationInfo({ conversation_title: "仅有名称" }).label, "仅有名称");
  assert.equal(conversationInfo({}).label, "未提供");
  assert.equal(conversationInfo(null).href, null);
});

test("conversation filtering searches Chinese titles and IDs independently of case", () => {
  const id = "01a16b50-e1f9-71d3-b9e2-24b065002660";
  const record = { conversation_id: id, conversation_title: "修复 Codex DMG 安装" };
  for (const query of ["修复", "  CODEX dmg ", "E1F9-71D3", "", "  "]) assert.equal(matchesConversationFilter(record, query), true, query);
  assert.equal(matchesConversationFilter(record, "未出现的名称"), false);
  assert.equal(matchesConversationFilter({ conversation_id: id }, "E1F9"), true);
  assert.equal(matchesConversationFilter({ conversation_id: id }, "Codex"), false);
  assert.equal(matchesConversationFilter({}, "未提供"), false);
});

test("conversation links only use complete UUIDs and never derive URLs from titles", () => {
  const id = "01A16B50-E1F9-71D3-B9E2-24B065002660";
  const title = '<img src=x onerror="alert(1)"> javascript:alert(1)';
  assert.deepEqual(conversationInfo({ conversation_id: id, conversation_title: title }), { id, title, label: title, href: `codex://threads/${id}` });
  for (const invalid of ["javascript:alert(1)", `codex://threads/${id}`, `${id}/other`, `${id}?action=delete`, `${id}#fragment`, ` ${id}`, `${id}\n`, '<img src=x onerror="alert(1)">', { toString: () => id }]) {
    assert.equal(conversationInfo({ conversation_id: invalid, conversation_title: "正常名称" }).href, null);
  }
});


test("release versions compare numeric components and reject ambiguous or prerelease formats", () => {
  assert.equal(compareReleaseVersions("1.9.9", "v1.10.0"), -1);
  assert.equal(compareReleaseVersions("v1.0.0", "1.0.0"), 0);
  assert.equal(compareReleaseVersions("2.0.0", "1.99.99"), 1);
  assert.equal(compareReleaseVersions("1.0.9007199254740993", "1.0.9007199254740992"), 1);
  for (const value of [undefined, null, "", "1.0", "01.0.0", "1.0.1-beta", "1.0.1+build", " 1.0.1", "1.0.1/../../", "v" + "1".repeat(70) + ".0.0"]) assert.equal(compareReleaseVersions("1.0.0", value), null, String(value));
});

test("update actions need both a newer version and server approval and suppress duplicate downloads", () => {
  const info = { current_version: "1.0.0", latest_version: "1.1.0", update_available: true, download_supported: true };
  assert.equal(updateActionState(info, "ready").canDownload, true);
  assert.equal(updateActionState(info, "error").canDownload, true);
  for (const phase of ["checking", "downloading", "opened"]) assert.equal(updateActionState(info, phase).canDownload, false, phase);
  assert.equal(updateActionState(info, "downloading").busy, true);
  for (const patch of [{ current_version: "1.1.0" }, { current_version: "2.0.0" }, { latest_version: null }, { latest_version: "1.1.0-rc.1" }, { update_available: false }, { download_supported: false }, { download_supported: "true" }]) assert.equal(updateActionState({ ...info, ...patch }, "ready").canDownload, false, JSON.stringify(patch));
  assert.equal(updateActionState(null).canDownload, false);
});

test("update release links stay on this repository without credentials or token-bearing URL parts", () => {
  const root = "https://github.com/qzkinhit/codex-wiretap/releases";
  for (const suffix of ["", "/", "/latest", "/tag/v1.0.1", "/tag/2.10.0"]) assert.equal(safeReleaseUrl(root + suffix), root + suffix);
  for (const value of ["http://github.com/qzkinhit/codex-wiretap/releases", "https://github.com.evil.test/qzkinhit/codex-wiretap/releases", "https://github.com/other/repo/releases", "https://user:secret@github.com/qzkinhit/codex-wiretap/releases", root + "?token=local-secret", root + "#local-secret", root + "/../issues", root + "/download/v1.0.1/installer.exe", root + "/tag/v1.0.1%2f..", "javascript:alert(1)", "//github.com/qzkinhit/codex-wiretap/releases", null]) assert.equal(safeReleaseUrl(value), null, String(value));
  const state = updateActionState({ current_version: "1.0.0", latest_version: "1.1.0", update_available: true, download_supported: false, release_url: root + "/tag/v1.1.0" });
  assert.equal(state.canDownload, false);
  assert.equal(state.releaseUrl, root + "/tag/v1.1.0");
});
