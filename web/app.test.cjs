"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const { classifyRequestOutcome, manualAccountPayload, requestRouteMode, classifyServiceTier, recordServiceTier, matchesServiceTierFilter } = require("./app.js");

test("HTTP 200 with upstream_failed is a failure, never a success", () => {
  const status = classifyRequestOutcome({ status: 200, duration_ms: 1200, outcome: "upstream_failed" });
  assert.equal(status.type, "failure");
  assert.equal(status.label, "上游失败");
  assert.equal(status.style, "red");
});

test("all backend failure outcomes take precedence over HTTP 200", () => {
  const failures = [
    "pool_exhausted", "unknown_response", "affinity_unavailable", "no_candidate",
    "connect_failed", "transport_uncertain", "header_timeout", "response_read_failed",
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
