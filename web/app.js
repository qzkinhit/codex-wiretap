"use strict";

function classifyRequestOutcome(record) {
  const result = String(record.outcome || "").toLowerCase();
  if (result === "completed") return { type: "success", label: "成功", style: "green" };
  if (["active", "pending", "running", "streaming", "in_progress"].includes(result)) return { type: "active", label: "处理中", style: "blue" };
  if (["client_cancelled", "cancelled", "canceled", "disconnected"].includes(result)) return { type: "failure", label: "已取消", style: "neutral" };
  if (["pool_exhausted", "no_candidate", "affinity_unavailable"].includes(result)) return { type: "failure", label: "号池暂不可用", style: "amber" };
  if (result === "transport_uncertain") return { type: "failure", label: "传输状态不确定", style: "amber" };
  const failures = {
    unknown_response: "响应状态未知",
    connect_failed: "连接失败",
    header_timeout: "响应超时",
    first_output_timeout: "首段输出超时",
    response_read_failed: "响应读取失败",
    upstream_failed: "上游失败",
    stream_read_failed: "流读取失败",
    invalid_upstream_response: "上游响应无效",
  };
  if (Object.hasOwn(failures, result)) return { type: "failure", label: failures[result], style: "red" };
  // Unknown terminal outcomes must never inherit success from HTTP response headers.
  if (result) return { type: "failure", label: "未确认成功", style: "red" };
  const hasFinished = typeof record.duration_ms === "number" && Number.isFinite(record.duration_ms);
  if (hasFinished || (typeof record.status === "number" && record.status >= 400)) return { type: "failure", label: "结果未确认", style: "amber" };
  return { type: "active", label: "处理中", style: "blue" };
}

function conversationInfo(record = {}) {
  const id = typeof record?.conversation_id === "string" ? record.conversation_id : "";
  const title = typeof record?.conversation_title === "string" ? record.conversation_title.trim() : "";
  const href = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(id) ? `codex://threads/${id}` : null;
  return { id, title, label: title || id || "未提供", href };
}

function matchesConversationFilter(record, query = "") {
  const term = String(query).trim().toLowerCase();
  const conversation = conversationInfo(record);
  return !term || [conversation.title, conversation.id].some((value) => value.toLowerCase().includes(term));
}

function compareReleaseVersions(left, right) {
  const parse = (value) => {
    if (typeof value !== "string" || value.length > 64) return null;
    const match = /^v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/.exec(value);
    return match ? match.slice(1).map(BigInt) : null;
  };
  const a = parse(left), b = parse(right);
  if (!a || !b) return null;
  for (let index = 0; index < 3; index++) {
    if (a[index] !== b[index]) return a[index] > b[index] ? 1 : -1;
  }
  return 0;
}

function safeReleaseUrl(value) {
  if (typeof value !== "string" || value !== value.trim()) return null;
  try {
    const url = new URL(value);
    if (url.protocol !== "https:" || url.hostname !== "github.com" || url.port || url.username || url.password || url.search || url.hash) return null;
    return /^\/qzkinhit\/codex-wiretap\/releases(?:\/latest|\/tag\/v?(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))?\/?$/.test(url.pathname) ? url.href : null;
  } catch (_) { return null; }
}

function updateActionState(info, phase = "idle") {
  const busy = phase === "checking" || phase === "downloading";
  const newer = compareReleaseVersions(info?.current_version, info?.latest_version) === -1;
  return {
    busy,
    canDownload: !busy && phase !== "opened" && newer && info?.update_available === true && info?.download_supported === true,
    releaseUrl: safeReleaseUrl(info?.release_url),
  };
}

function manualAccountPayload(fields) {
  const key = String(fields.key || "").trim();
  const models = [...new Set(String(fields.models || "").split(/[,，\n]+/).map((model) => model.trim()).filter(Boolean))];
  const maxConcurrency = Number(fields.max_concurrency ?? 1);
  if (!key) throw new Error("请输入要添加或更新的上游 Key。");
  if (!models.length || models.some((model) => !/^[a-zA-Z0-9._:/-]+$/.test(model))) throw new Error("请输入有效模型 ID，多个模型用逗号分隔。");
  if (!Number.isInteger(maxConcurrency) || maxConcurrency < 1 || maxConcurrency > 128) throw new Error("每模型并发上限必须是 1 到 128 的整数。");
  const payload = { key, models, max_concurrency: maxConcurrency };
  const label = String(fields.label || "").trim();
  if (label) payload.label = label;
  return payload;
}

function requestRouteMode(record) {
  return record.route_mode === "legacy" ? { mode: "legacy", label: "旧方式监控" } : { mode: "pool", label: "号池调度" };
}

function classifyServiceTier(value) {
  const raw = value == null ? null : String(value);
  const normalized = (raw || "").trim().toLowerCase();
  if (normalized === "fast" || normalized === "priority") return { raw, group: "fast", label: "Fast", known: true };
  if (normalized === "default") return { raw, group: "normal", label: "普通", known: true };
  if (["ultrafast", "flex", "scale"].includes(normalized)) return { raw, group: normalized, label: normalized, known: true };
  if (normalized === "auto") return { raw, group: "auto", label: "auto", known: false };
  return { raw, group: "unknown", label: normalized ? "未知" : "未提供", known: false };
}

function recordServiceTier(record) {
  const requested = classifyServiceTier(record.requested_service_tier);
  const response = classifyServiceTier(record.response_service_tier);
  const completed = record.outcome === "completed";
  const comparable = completed && requested.known && response.known;
  return {
    requested, response,
    responseConfirmed: completed && response.known,
    comparison: comparable ? requested.group === response.group ? "same" : "different" : "unconfirmed",
    fastToNormal: completed && requested.group === "fast" && response.group === "normal",
    unknown: !completed || !requested.known || !response.known,
  };
}

function matchesServiceTierFilter(record, filter) {
  const tier = recordServiceTier(record);
  if (filter === "fast") return tier.requested.group === "fast";
  if (filter === "fast_to_normal") return tier.fastToNormal;
  if (filter === "unknown") return tier.unknown;
  return true;
}

function accountModels(currentState) {
  return [...new Set([
    ...(Array.isArray(currentState?.models) ? currentState.models : []),
    ...(currentState?.accounts || []).flatMap((account) => account.models || []),
    ...(currentState?.source?.providers || []).map((provider) => provider.model),
  ].filter((model) => typeof model === "string" && model))].sort();
}

function filterAccounts(accounts, model = "") {
  return (accounts || []).filter((account) => account.present !== false && (!model || account.models?.includes(model)));
}

function filterModelHealth(account, model = "") {
  return (Array.isArray(account.model_health) ? account.model_health : []).filter((health) => !model || health.model === model);
}

function syncActionLabel(config) {
  if (config?.source_mode === "manual") return "↻ 刷新手动池";
  return config?.biwrite_import_all === true ? "↻ 同步全部 Key" : "↻ 同步并验证保活";
}

function syncResultText(sync) {
  if (!sync || sync.last_completed_ms == null) return "";
  return `验证 ${Number(sync.total) || 0} 项：成功 ${Number(sync.healthy) || 0} 项，剔除 ${Number(sync.removed) || 0} 项，暂时失败或未完成 ${Number(sync.temporary) || 0} 项。`;
}

function syncProgressText(sync) {
  if (!sync?.running) return "";
  return `正在实时验证 ${Number(sync.completed) || 0} / ${Number(sync.total) || 0} 项，每项只尝试一次。`;
}

function syncProgressBrief(sync) {
  if (!sync?.running) return "";
  return `验证 ${Number(sync.completed) || 0} / ${Number(sync.total) || 0}`;
}

/// Keeps small amounts readable: a tenth of a cent must not render as 0.00.
/// Requests that never reached the upstream because the pool has no account for
/// that model are reported apart from real failures.
function requestOutcomeSummary(stats) {
  const successes = Number(stats?.successes) || 0;
  const failures = Number(stats?.failures) || 0;
  const unassigned = Number(stats?.unassigned) || 0;
  const parts = [];
  if (failures) parts.push(`失败 ${failures} 次`);
  if (unassigned) parts.push(`${unassigned} 次请求的模型号池里没有`);
  return {
    successes,
    failures,
    unassigned,
    total: successes + failures,
    text: parts.length ? `成功 ${successes} 次 · ${parts.join(" · ")}` : `全部 ${successes} 次都成功`,
  };
}

/// Per-model availability, so the panel can say which model is actually usable.
function modelReadiness(accounts) {
  const rows = Array.isArray(accounts) ? accounts : [];
  const models = new Map();
  for (const account of rows) {
    if (account?.present === false) continue;
    const health = Array.isArray(account?.model_health) ? account.model_health : [];
    for (const entry of health) {
      const model = typeof entry?.model === "string" ? entry.model : "";
      if (!model) continue;
      const seen = models.get(model) || { model, total: 0, ready: 0 };
      seen.total += 1;
      const cooling = Number(entry?.cooldown_until_ms) || 0;
      const disabled = account?.enabled === false || entry?.enabled === false;
      if (!disabled && cooling <= 0) seen.ready += 1;
      models.set(model, seen);
    }
  }
  return [...models.values()].sort((left, right) => left.model.localeCompare(right.model));
}

/// One sentence for the top of the panel: can I use it right now, and if not,
/// what do I do next. Written for someone who does not know the vocabulary.
function poolReadiness(state) {
  if (!state) return { tone: "down", headline: "正在连接本地服务", detail: "", action: "" };
  const models = modelReadiness(state.accounts);
  const usable = models.filter((row) => row.ready > 0);
  const empty = models.filter((row) => row.ready === 0);
  const manual = state.config?.source_mode === "manual";
  if (!models.length) {
    return {
      tone: "warn",
      headline: "还没有可用的账号",
      detail: manual ? "手动池里还没有账号。" : "Pool Router 没有从 BiWrite 读到可用的号。",
      action: manual ? "到「路由设置」里添加账号。" : "先在 BiWrite 里把号保活好，再点右上角「同步并验证保活」。",
    };
  }
  if (!usable.length) {
    return {
      tone: "warn",
      headline: "账号都在冷却或已停用，暂时用不了",
      detail: `共 ${models.length} 个模型通道，现在没有一个可以分配。`,
      action: "等几分钟自动恢复，或点「同步并验证保活」重新检查。",
    };
  }
  const summary = usable.map((row) => `${row.model} ${row.ready} 个`).join("、");
  return {
    tone: empty.length ? "partial" : "ok",
    headline: empty.length ? "部分模型可以用" : "可以用了",
    detail: `现在可分配：${summary}。`,
    action: empty.length
      ? `${empty.map((row) => row.model).join("、")} 还没有可用的号，需要先在 BiWrite 保活再同步。`
      : "在 Codex 里正常使用即可，请求会自动分配到空闲账号。",
  };
}

function usageFormatAmount(value) {
  const amount = Number(value);
  if (!Number.isFinite(amount)) return "—";
  if (amount === 0) return "0.00";
  const magnitude = Math.abs(amount);
  if (magnitude >= 100) return amount.toFixed(0);
  if (magnitude >= 1) return amount.toFixed(2);
  if (magnitude >= 0.01) return amount.toFixed(3);
  return amount > 0 ? "<0.01" : ">-0.01";
}

function usageMoney(usd, pricing) {
  const rate = Number(pricing?.usd_to_display);
  const amount = Number(usd) || 0;
  const display = Number.isFinite(rate) && rate > 0 ? amount * rate : null;
  return {
    usd: amount,
    display,
    currency: typeof pricing?.display_currency === "string" ? pricing.display_currency : "",
  };
}

function usageTokenTotal(tokens) {
  return (Number(tokens?.input) || 0) + (Number(tokens?.output) || 0);
}

/// Cached reads as a share of the input. Writes are billed, so they are not hits.
function usageCacheHitRate(tokens) {
  const input = Number(tokens?.input) || 0;
  if (input <= 0) return null;
  return (Number(tokens?.cached) || 0) / input;
}

/// What the cached part would have cost at the ordinary input price, minus what
/// it did cost. Only meaningful when the model has a configured price.
function usageCacheSavingUsd(tokens, prices, tierMultiplier = 1, rate = 1) {
  if (!prices) return null;
  const cached = Number(tokens?.cached) || 0;
  const full = Number(prices.input);
  const discounted = Number(prices.cached_input);
  if (!Number.isFinite(full) || !Number.isFinite(discounted)) return null;
  return (cached * (full - discounted)) / 1e6 * (Number(tierMultiplier) || 1) * (Number(rate) || 1);
}

/// Bars for one series, scaled to the largest value in the window.
function usageBars(daily, pick) {
  const rows = Array.isArray(daily) ? daily : [];
  const values = rows.map((row) => Number(pick(row)) || 0);
  const peak = Math.max(0, ...values);
  return rows.map((row, index) => ({
    date: typeof row?.date === "string" ? row.date : "",
    value: values[index],
    // A zero peak would divide by zero; an all-zero window draws no height.
    ratio: peak > 0 ? values[index] / peak : 0,
  }));
}

function usageShare(rows, pick) {
  const list = Array.isArray(rows) ? rows : [];
  const total = list.reduce((sum, row) => sum + (Number(pick(row)) || 0), 0);
  return list.map((row) => ({
    name: typeof row?.name === "string" ? row.name : "",
    title: typeof row?.title === "string" ? row.title : "",
    value: Number(pick(row)) || 0,
    share: total > 0 ? (Number(pick(row)) || 0) / total : 0,
    requests: Number(row?.requests) || 0,
    unpriced: Number(row?.unpriced_requests) || 0,
  }));
}

/// A day label short enough for an axis: "10/25", and the month when it changes.
function usageDayLabel(date, previous) {
  const parts = String(date || "").split("-");
  if (parts.length !== 3) return "";
  const [, month, day] = parts;
  const previousMonth = String(previous || "").split("-")[1];
  return previousMonth && previousMonth === month ? day : `${month}/${day}`;
}

function verifiesCandidates(config) {
  return config?.source_mode === "biwrite" && config?.biwrite_import_all !== true;
}

function syncBusyLabel(config) {
  return verifiesCandidates(config) ? "正在验证…" : "正在读取…";
}

function sourceTimeState(state) {
  const progress = syncProgressText(state?.sync);
  if (progress) return { mode: "progress", text: progress };
  const verifiedAt = Number(state?.sync?.last_completed_ms);
  const readAt = Number(state?.source?.read_at_ms);
  const verified = verifiesCandidates(state?.config) && Number.isFinite(verifiedAt) && verifiedAt > 0;
  const read = Number.isFinite(readAt) && readAt > 0;
  // A missing timestamp arrives as JSON null, which compares as zero.
  if (verified && (!read || verifiedAt >= readAt)) return { mode: "verified", at: verifiedAt };
  if (read) return verified ? { mode: "read", at: readAt, verifiedAt } : { mode: "read", at: readAt };
  return { mode: "waiting" };
}

function syncConfigPatch(action, config) {
  if (action === "pause") return { sync_paused: config?.sync_paused !== true };
  if (action === "scope") return { source_mode: "biwrite", biwrite_import_all: config?.source_mode !== "biwrite" || config?.biwrite_import_all !== true };
  throw new Error("未知同步操作");
}

function redactSensitiveText(value, secrets = []) {
  let text = typeof value === "string" ? value : value == null ? "" : String(value);
  for (const secret of secrets) if (typeof secret === "string" && secret) text = text.split(secret).join("[已隐藏凭证]");
  return text.replace(/sk-[a-zA-Z0-9_-]{8,}/gi, "[已隐藏密钥]")
    .replace(/\bBearer\s+[a-zA-Z0-9._~+\/-]+/gi, "[已隐藏授权信息]")
    .replace(/\b(?:authorization|api[_-]?key|access[_-]?token)\s*[:=]\s*[^\r\n,;]+/gi, "[已隐藏授权信息]");
}

function normalizeProbeResult(result, model, elapsedMs = 0, secrets = []) {
  const data = result && typeof result === "object" ? result : {};
  const details = data.details && typeof data.details === "object" ? data.details : {};
  const clean = (value, limit = 2048) => redactSensitiveText(value, secrets).slice(0, limit);
  const number = (value) => typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : null;
  const usage = details.usage || {};
  return {
    pending: false,
    ok: data.ok === true,
    status: number(data.status),
    message: clean(data.message || (data.ok === true ? "本次验证成功" : "本次验证未成功")),
    response_model: clean(data.response_model, 200),
    error_code: clean(data.error_code, 200),
    details: {
      request_id: clean(details.request_id, 200),
      requested_model: clean(details.requested_model || model, 200),
      requested_effort: clean(details.requested_effort || "low", 40),
      request_text: clean(details.request_text || "Reply only OK.", 4096),
      elapsed_ms: number(details.elapsed_ms) ?? elapsedMs,
      response_text: clean(details.response_text, 65536),
      error_message: clean(details.error_message),
      events: (Array.isArray(details.events) ? details.events : []).slice(0, 100).map((event) => ({ type: clean(event.type, 100), elapsed_ms: number(event.elapsed_ms), detail: clean(event.detail) })),
      usage: { input_tokens: number(usage.input_tokens), output_tokens: number(usage.output_tokens), cached_tokens: number(usage.cached_tokens), reasoning_tokens: number(usage.reasoning_tokens) },
      truncated: details.truncated === true || (typeof details.response_text === "string" && details.response_text.length > 65536) || (Array.isArray(details.events) && details.events.length > 100),
    },
  };
}

function gatewayActionRows(record) {
  const names = { session_reset: "重置会话", session_reused: "复用恢复后的会话", compaction_repair: "修复压缩上下文", image_compression: "压缩图片", first_output_timeout: "等待首段输出超时", header_timeout_retry: "响应头超时后重试", hedged_attempt: "同时试另一个号", hedged_win: "另一个号先返回" };
  return (Array.isArray(record.gateway_actions) ? record.gateway_actions : []).map((action) => ({
    label: Object.hasOwn(names, action.action) ? names[action.action] : "其他网关处理",
    attempt: Number.isInteger(action.attempt) && action.attempt >= 0 ? action.attempt : null,
    elapsed_ms: action.elapsed_ms,
    count: action.count,
    before_bytes: action.before_bytes,
    after_bytes: action.after_bytes,
  }));
}

function attemptErrorDetails(attempt) {
  return {
    reason: redactSensitiveText(attempt.reason).slice(0, 180),
    message: redactSensitiveText(attempt.error_message).slice(0, 2048),
  };
}

function effectiveOutputWait(record) {
  return typeof record.first_output_ms === "number" && Number.isFinite(record.first_output_ms) && record.first_output_ms >= 0 ? record.first_output_ms : null;
}

function sourceProviderRows(source, importAll = false) {
  const providers = Array.isArray(source?.providers) ? source.providers : [];
  const keyCount = (value) => Number.isInteger(value) && value >= 0 ? value : null;
  return providers.map((provider) => {
    const kept = keyCount(provider.kept_keys);
    const imported = keyCount(provider.imported_keys);
    return {
      model: provider.model || "未提供模型",
      total_keys: keyCount(provider.total_keys),
      kept_keys: kept,
      imported_keys: imported,
      description: importAll ? imported > 0 ? "已同步全部 Key，可用性待请求确认" : imported === 0 ? "已读取配置，当前无可导入 Key" : "等待来源同步结果" : kept === 0 ? "已读取配置，当前无保活候选" : imported === 0 ? "候选未导入，请检查来源状态" : imported > 0 ? "已导入候选，实时可用性待请求确认" : "等待来源同步结果",
    };
  });
}

function draftFromConfig(config) {
  return {
    source_mode: config?.source_mode === "manual" ? "manual" : "biwrite",
    upstream_base_url: config?.upstream_base_url || "",
    strategy: config?.policy?.strategy || "sticky",
    pinned_key: config?.policy?.pinned_key || "",
    max_concurrency_per_key: config?.policy?.max_concurrency_per_key ?? 1,
    shared_account_concurrency: config?.policy?.shared_account_concurrency ?? 0,
    model_override: config?.model_override || "",
    reasoning_override: config?.reasoning_override || "",
    max_retries: (config?.max_attempts ?? 16) - 1,
    queue_timeout: (config?.queue_timeout_ms ?? 600000) / 1000,
    recording_enabled: config?.recording_enabled !== false,
    legacy_monitor_enabled: config?.legacy_monitor_enabled !== false,
    session_recovery_enabled: config?.session_recovery_enabled !== false,
    compaction_repair_enabled: config?.compaction_repair_enabled !== false,
    compress_images: config?.compress_images === true,
    first_output_timeout: (config?.first_output_timeout_ms ?? 120000) / 1000,
    hedge_after: (config?.hedge_after_ms ?? 20000) / 1000,
  };
}

function routingConfigPayload(draft, latestConfig) {
  return {
    source_mode: draft.source_mode,
    upstream_base_url: String(draft.upstream_base_url || "").trim() || null,
    policy: { ...latestConfig?.policy, strategy: draft.strategy, pinned_key: draft.pinned_key || null, max_concurrency_per_key: draft.max_concurrency_per_key, shared_account_concurrency: draft.shared_account_concurrency || null },
    model_override: draft.model_override || null,
    reasoning_override: draft.reasoning_override || null,
    max_attempts: draft.max_retries + 1,
    queue_timeout_ms: Math.round(draft.queue_timeout * 1000),
    recording_enabled: draft.recording_enabled,
    legacy_monitor_enabled: draft.legacy_monitor_enabled,
    session_recovery_enabled: draft.session_recovery_enabled,
    compaction_repair_enabled: draft.compaction_repair_enabled,
    compress_images: draft.compress_images,
    first_output_timeout_ms: Math.round(draft.first_output_timeout * 1000),
    hedge_after_ms: Math.round(draft.hedge_after * 1000),
  };
}

if (typeof module !== "undefined" && module.exports) module.exports = { compareReleaseVersions, safeReleaseUrl, updateActionState, conversationInfo, matchesConversationFilter, classifyRequestOutcome, manualAccountPayload, requestRouteMode, classifyServiceTier, recordServiceTier, matchesServiceTierFilter, sourceProviderRows, draftFromConfig, routingConfigPayload, accountModels, filterAccounts, filterModelHealth, syncConfigPatch, syncActionLabel, syncResultText, syncProgressText, syncProgressBrief, requestOutcomeSummary, modelReadiness, poolReadiness, usageMoney, usageFormatAmount, usageTokenTotal, usageCacheHitRate, usageCacheSavingUsd, usageBars, usageShare, usageDayLabel, verifiesCandidates, syncBusyLabel, sourceTimeState, redactSensitiveText, normalizeProbeResult, gatewayActionRows, attemptErrorDetails, effectiveOutputWait };

if (typeof document !== "undefined") (() => {
  const $ = (id) => document.getElementById(id);
  const tokenStorageKey = "pool-router-session";
  let sessionToken = "";
  try {
    const fragment = new URLSearchParams(window.location.hash.slice(1));
    const suppliedToken = fragment.get("token");
    sessionToken = suppliedToken || sessionStorage.getItem(tokenStorageKey) || "";
    if (suppliedToken) sessionStorage.setItem(tokenStorageKey, suppliedToken);
  } catch (_) {
    sessionToken = new URLSearchParams(window.location.hash.slice(1)).get("token") || "";
  }
  if (window.location.hash) history.replaceState(null, "", window.location.pathname + window.location.search);

  let state = null;
  let records = [];
  let manualAccounts = [];
  let manualPending = false;
  let manualSignature = "";
  let activeTab = "accounts";
  let formDirty = false;
  let baselineDraft = null;
  let statePending = null;
  let recordsPending = false;
  let accountSignature = "";
  let recordSignature = "";
  let mutationQueue = Promise.resolve();
  let probeAccountId = null;
  let accountModelFilter = "";
  const probeResults = new Map();
  let openedRecordId = null;
  let updateState = { info: null, phase: "idle", message: "" };
  const pendingActions = new Set();
  const dateFormatter = new Intl.DateTimeFormat("zh-CN", { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
  const timeFormatter = new Intl.DateTimeFormat("zh-CN", { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
  const numberFormatter = new Intl.NumberFormat("zh-CN");

  function safeText(value) {
    return redactSensitiveText(value, [$("manual-key")?.value, sessionToken]);
  }

  function node(tag, className, content) {
    const element = document.createElement(tag);
    if (className) element.className = className;
    if (content !== undefined && content !== null) element.textContent = safeText(content);
    return element;
  }

  function text(id, value) { $(id).textContent = safeText(value); }
  function showAlert(id, message) { text(id, message); $(id).hidden = !message; }
  function asNumber(value) { return typeof value === "number" && Number.isFinite(value) ? value : null; }
  function count(value) { const n = asNumber(value); return n === null ? "—" : numberFormatter.format(n); }
  function shortId(value) { const id = safeText(value); return id ? id.slice(0, 12) : "—"; }
  function timestamp(value) { return asNumber(value) !== null && value > 0 ? dateFormatter.format(new Date(value)) : "—"; }
  function clockTime(value) { return asNumber(value) !== null && value > 0 ? timeFormatter.format(new Date(value)) : "—"; }
  function duration(value) {
    const n = asNumber(value);
    if (n === null) return "—";
    if (n < 1000) return `${Math.round(n)} ms`;
    if (n < 60000) return `${(n / 1000).toFixed(1)} s`;
    return `${Math.floor(n / 60000)} 分 ${Math.round((n % 60000) / 1000)} 秒`;
  }
  function bytes(value) {
    const n = asNumber(value);
    if (n === null) return "—";
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    return `${(n / (1024 * 1024)).toFixed(2)} MB`;
  }
  function relativeTime(value) {
    const n = asNumber(value);
    if (n === null || n <= 0) return "尚无记录";
    const seconds = Math.max(0, Math.floor((Date.now() - n) / 1000));
    if (seconds < 60) return "刚刚";
    if (seconds < 3600) return `${Math.floor(seconds / 60)} 分钟前`;
    if (seconds < 86400) return `${Math.floor(seconds / 3600)} 小时前`;
    return `${Math.floor(seconds / 86400)} 天前`;
  }

  function toast(message, kind = "") {
    if (!message) return;
    const element = node("div", `toast ${kind}`, message);
    $("toast-region").append(element);
    while ($("toast-region").children.length > 3) $("toast-region").firstChild.remove();
    window.setTimeout(() => element.remove(), kind === "error" ? 6500 : 4200);
  }

  async function api(path, options = {}) {
    const controller = new AbortController();
    const timeoutMs = options.timeout === 0 ? null : options.timeout || 15000;
    const timeout = timeoutMs === null ? null : setTimeout(() => controller.abort(), timeoutMs);
    const headers = { Accept: "application/json", Authorization: `Bearer ${sessionToken}` };
    if (options.body !== undefined) headers["Content-Type"] = "application/json";
    try {
      const response = await fetch(path, {
        method: options.method || "GET", headers, signal: controller.signal,
        credentials: "same-origin", cache: "no-store",
        body: options.body === undefined ? undefined : JSON.stringify(options.body),
      });
      let data = null;
      try { data = await response.json(); } catch (_) { /* Empty response is permitted. */ }
      if (!response.ok) {
        if (options.probeDetails && data) return { ...data, ok: false, status: data.status ?? response.status, error_code: data.error_code || data.error?.code, message: data.message || (typeof data.error === "string" ? data.error : data.error?.message) || `本地服务返回 HTTP ${response.status}` };
        if (response.status === 401 || response.status === 403) throw new Error("控制台授权已失效。请使用本地服务提供的带令牌地址重新打开控制台。");
        const detail = typeof data?.error === "string" ? data.error : data?.error?.message || data?.message;
        throw new Error(detail ? safeText(detail).slice(0, 220) : `本地服务返回 HTTP ${response.status}`);
      }
      return data || {};
    } catch (error) {
      if (error.name === "AbortError") throw new Error("本地服务响应超时。正在继续同步状态，请稍后检查结果。");
      if (error instanceof TypeError) throw new Error("无法连接本地服务，请检查 Pool Router 是否正在运行。");
      throw error;
    } finally { clearTimeout(timeout); }
  }

  function serializeMutation(operation) {
    const result = mutationQueue.then(operation, operation);
    mutationQueue = result.catch(() => {});
    return result;
  }

  function allModels(currentState = state) {
    return [...new Set([
      ...(Array.isArray(currentState?.models) ? currentState.models : []),
      ...(currentState?.accounts || []).flatMap((account) => Array.isArray(account.models) ? account.models : []),
    ].filter((model) => typeof model === "string" && model.length > 0))].sort();
  }

  function updateOptions(select, items, initialLabel, desiredValue) {
    const wanted = desiredValue === undefined ? select.value : String(desiredValue ?? "");
    const normalized = items.map((item) => typeof item === "string" ? { value: item, label: item } : item);
    if (wanted && !normalized.some((item) => item.value === wanted)) normalized.push({ value: wanted, label: `${safeText(wanted)}（当前设置）` });
    const fingerprint = JSON.stringify([initialLabel, normalized]);
    if (select.dataset.options !== fingerprint) {
      const children = [];
      if (initialLabel !== null) { const first = node("option", "", initialLabel); first.value = ""; children.push(first); }
      for (const item of normalized) { const option = node("option", "", item.label); option.value = item.value; children.push(option); }
      select.replaceChildren(...children);
      select.dataset.options = fingerprint;
    }
    select.value = wanted;
    if (!select.value && initialLabel === null && select.options.length) select.selectedIndex = 0;
  }

  function accountStatus(account) {
    if (account.present === false || account.status === "removed") return { label: "源中已移除", style: "neutral", ready: false };
    if (account.enabled === false || account.status === "disabled") return { label: "已停用", style: "neutral", ready: false };
    if ((asNumber(account.cooldown_until_ms) || 0) > Date.now()) return { label: `冷却 ${Math.ceil((account.cooldown_until_ms - Date.now()) / 1000)}s`, style: "amber", ready: false };
    const limit = account.max_concurrency ?? state?.config?.policy?.max_concurrency_per_key ?? 1;
    if ((account.in_flight || 0) >= limit || account.status === "busy") return { label: "繁忙", style: "blue", ready: false };
    if (account.last_ok_ms || account.status === "healthy") return { label: "曾成功", style: "neutral", ready: true };
    return { label: "待验证", style: "neutral", ready: true };
  }

  function modelStatus(account, health) {
    return accountStatus({ ...health, present: account.present, enabled: account.enabled });
  }

  function availableModelHealth(account) {
    return Array.isArray(account.model_health) ? account.model_health : [];
  }

  function badge(label, style) { return node("span", `badge badge-${style}`, label); }
  function accountLabel(id) { return state?.accounts?.find((account) => account.id === id)?.label || shortId(id); }

  function renderStats() {
    if (!state) return;
    const accounts = state.accounts || [];
    const present = accounts.filter((account) => account.present !== false);
    const ready = accounts.filter((account) => availableModelHealth(account).length ? availableModelHealth(account).some((health) => modelStatus(account, health).ready) : accountStatus(account).ready);
    const verified = present.flatMap((account) => availableModelHealth(account).length ? availableModelHealth(account).filter((health) => health.last_ok_ms || health.status === "healthy") : (account.last_ok_ms || account.status === "healthy" ? [account] : []));
    const readiness = poolReadiness(state);
    text("readiness-headline", readiness.headline);
    text("readiness-detail", readiness.detail || "Codex → Pool Router → 上游服务。按会话分配。");
    text("readiness-action", readiness.action);
    $("readiness-headline").className = `readiness-${readiness.tone}`;
    text("stat-ready", ready.length);
    text("stat-total", `/ ${present.length} 个`);
    text("stat-verified", `${verified.length} 个模型通道有成功记录`);
    text("stat-active", count(state.stats?.active ?? 0));
    const outcome = requestOutcomeSummary(state.stats);
    text("stat-requests", count(outcome.total));
    text("stat-results", outcome.text);
    const uptime = Math.max(0, Math.floor(state.uptime_seconds || 0));
    text("stat-uptime", uptime >= 86400 ? `${Math.floor(uptime / 86400)} 天 ${Math.floor(uptime % 86400 / 3600)} 时` : uptime >= 3600 ? `${Math.floor(uptime / 3600)} 时 ${Math.floor(uptime % 3600 / 60)} 分` : `${Math.floor(uptime / 60)} 分 ${uptime % 60} 秒`);
    text("stat-version", state.version ? `本地服务 v${state.version}` : "本地服务");
    text("account-count", present.length);
    text("base-url", state.connection?.base_url || "未提供接入地址");
    $("copy-url").disabled = !state.connection?.base_url;
    text("connect-button", state.connection?.configured ? "检查接入" : "直接接入 Codex");
    $("disconnect-button").hidden = !state.connection?.configured;
    $("disconnect-button").title = "把 Codex 配置还原到接入之前，之后可在 CC Switch 或 Codex 里自行选择供应商";
    $("connect-button").title = state.connection?.configured ? "检查 Codex → Pool Router → 上游服务 的直接接入配置" : "直接配置 Codex 使用 Pool Router";
    showAlert("connection-note", !state.connection?.configured ? "点击「直接接入 Codex」完成本地配置，后续由 Pool Router 自动分配号池。" : "");
    const legacyEnabled = state.config?.legacy_monitor_enabled !== false;
    $("legacy-status").className = `badge badge-${legacyEnabled ? "green" : "neutral"}`;
    text("legacy-status", legacyEnabled ? "允许旧方式监控" : "旧方式监控已关闭");
    text("legacy-scope", legacyEnabled ? "仅能监控经过本地入口的请求，无法查看未经过代理的 HTTPS 流量。" : "旧方式监控已关闭，可在路由设置开启并保存。本地令牌号池模式不受影响。");
    const syncProgress = syncProgressText(state.sync);
    const sourceTime = sourceTimeState(state);
    text("source-time", sourceTime.mode === "progress" ? sourceTime.text
      : sourceTime.mode === "verified" ? `最近验证 ${timestamp(sourceTime.at)}`
      : sourceTime.mode === "read" ? `最近读取 ${timestamp(sourceTime.at)}${sourceTime.verifiedAt ? ` · 最近验证 ${timestamp(sourceTime.verifiedAt)}` : ""}`
      : "等待首次读取");
    const manualMode = state.config?.source_mode === "manual";
    const importAll = state.config?.biwrite_import_all === true;
    const paused = state.config?.sync_paused === true;
    const syncBusy = state.sync?.running === true || pendingActions.has("sync");
    $("sync-button").disabled = syncBusy;
    text("sync-button", syncProgressBrief(state.sync) || (syncBusy ? syncBusyLabel(state.config) : syncActionLabel(state.config)));
    text("pause-sync-button", paused ? "恢复同步" : "暂停同步");
    $("pause-sync-button").disabled = manualMode || syncBusy || pendingActions.has("sync-config");
    $("pause-sync-button").title = manualMode ? "当前使用手动来源，未自动读取 BiWrite" : "仅控制 BiWrite 的自动读取，当前号池仍可使用";
    $("sync-scope-button").disabled = syncBusy || pendingActions.has("sync-config");
    text("sync-scope-button", !manualMode && importAll ? "仅同步保活候选" : "持续同步全部 Key");
    text("sync-mode-status", manualMode ? "手动来源" : `${paused ? "自动同步已暂停" : "自动同步中"} · ${importAll ? "全部 Key" : "保活候选"}`);
    $("sync-mode-status").className = `badge badge-${!manualMode && paused ? "amber" : "neutral"}`;
    const lastVerification = verifiesCandidates(state.config) ? syncResultText(state.sync) : "";
    text("sync-mode-description", `${manualMode ? "可切换为 BiWrite 持续同步全部 Key，切换后立即生效。" : `${paused ? "暂停只停止自动读取，当前号池仍可使用；手动同步不会恢复自动同步。" : "每 1.5 秒只读取来源，不发送验证请求。"}${importAll ? "当前持续包含全部 Key，包括未保活、忽略和停止的 Key。" : "点击顶部按钮会实时验证保活候选，明确失效的模型退出候选，暂时失败仅冷却。"}`}${lastVerification ? ` 上次${lastVerification}` : ""}`);
    text("source-heading", manualMode ? "手动维护号池" : importAll ? "BiWrite 全部 Key" : "BiWrite 保活号池");
    text("source-description", manualMode ? "当前使用本地自定义池。可在路由设置中管理账号，模型健康与并发分别计算。" : importAll ? "同步全部 Key 只读取配置，不执行实时验证。" : "手动同步会逐项验证当前 BiWrite 候选。后续自动读取不会重新加入已确认失效的模型，下次手动同步会重新验证完整来源。");
    text("accounts-empty-title", manualMode ? "当前手动池没有可用账号" : sourceProviderRows(state.source).length ? "当前没有可导入候选" : "还没有同步到号池");
    text("accounts-empty-description", manualMode ? "可在路由设置的手动池管理中添加账号，或保存切换为自动读取 BiWrite。" : importAll ? "当前上游尚无可读取的 Key，请检查 BiWrite 配置和来源同步状态。" : "可在 BiWrite 运行保活后点击「同步并验证保活」，或选择持续同步全部 Key。来源同步中可查看尚无候选的模型。");
    text("manual-effect-note", manualMode ? "当前已使用手动来源，添加、更新和删除会影响后续请求。相同 Key 再次提交会更新该账号。" : "当前使用自动来源，增删手动池不会改变当前路由。相同 Key 再次提交会更新该账号。");
    const warnings = [];
    if (state.source?.last_error) warnings.push(state.source.last_error);
    if (Array.isArray(state.source?.warnings)) warnings.push(...state.source.warnings.filter((warning) => typeof warning === "string"));
    const warningText = warnings.join("；");
    $("source-warning").hidden = !warningText;
    text("source-warning-summary", state.source?.last_error
      ? "读取来源时出了问题，点开查看"
      : `来源说明（${warnings.length} 条，点开查看）`);
    text("source-warning-body", warningText);
    $("recording-status").className = `badge badge-${state.config?.recording_enabled ? "green" : "neutral"}`;
    text("recording-status", state.config?.recording_enabled ? "正在记录" : "记录已暂停");
  }

  function renderSourceProviders() {
    const importAll = state.config?.biwrite_import_all === true;
    const providers = state.config?.source_mode === "manual" ? [] : sourceProviderRows(state.source, importAll);
    $("source-sync").hidden = providers.length === 0;
    text("source-sync-description", importAll ? "当前持续同步全部 Key，保活数量仅供参考。已导入不等于本次请求可用。" : "当前只同步保活候选。保活候选与历史成功均不保证实时可用。");
    const rows = providers.map((provider) => {
      const row = node("tr");
      for (const value of [provider.model, provider.total_keys, provider.kept_keys, provider.imported_keys]) row.append(node("td", "", value ?? "—"));
      row.append(node("td", (importAll ? provider.imported_keys === 0 : provider.kept_keys === 0) ? "source-no-candidates" : "", provider.description));
      return row;
    });
    $("source-provider-list").replaceChildren(...rows);
  }

  function renderAccountFilters() {
    const models = accountModels(state);
    if (accountModelFilter && !models.includes(accountModelFilter)) models.push(accountModelFilter);
    const signature = JSON.stringify([models, accountModelFilter]);
    const container = $("account-model-filter");
    if (container.dataset.signature === signature) return;
    container.dataset.signature = signature;
    container.replaceChildren(...["", ...models].map((model) => {
      const button = node("button", "model-filter-button", model || "全部模型");
      button.type = "button";
      button.setAttribute("aria-pressed", String(model === accountModelFilter));
      button.addEventListener("click", () => {
        accountModelFilter = model;
        renderAccountFilters();
        renderAccounts(true);
        [...container.children].find((element) => element.getAttribute("aria-pressed") === "true")?.focus({ preventScroll: true });
      });
      return button;
    }));
  }

  function accountButton(label, action, accountId, extraClass = "", model = null) {
    const button = node("button", `button button-light button-small ${extraClass}`, label);
    button.type = "button";
    button.dataset.action = action;
    button.dataset.account = accountId;
    if (model) button.dataset.model = model;
    button.disabled = pendingActions.has(accountId);
    button.addEventListener("click", () => handleAccountAction(action, accountId, model));
    return button;
  }

  function renderAccounts(force = false) {
    const allAccounts = filterAccounts(state.accounts);
    const accounts = filterAccounts(allAccounts, accountModelFilter);
    const signature = JSON.stringify([accounts, accountModelFilter, state.config?.policy, Math.floor(Date.now() / 15000), accounts.some((a) => (a.cooldown_until_ms || 0) > Date.now() || availableModelHealth(a).some((health) => (health.cooldown_until_ms || 0) > Date.now())) ? Math.floor(Date.now() / 1000) : 0, [...pendingActions], [...probeResults.keys()]]);
    if (!force && signature === accountSignature) return;
    accountSignature = signature;
    const focusedAction = document.activeElement?.dataset?.action;
    const focusedAccount = document.activeElement?.dataset?.account;
    const focusedModel = document.activeElement?.dataset?.model;
    const cards = accounts.map((account, index) => {
      const card = node("article", `account-card${account.enabled === false || account.present === false ? " is-disabled" : ""}`);
      const header = node("div", "account-card-header");
      const title = node("div", "account-title");
      const heading = node("h3", "", account.label || `号池 ${index + 1}`);
      heading.title = safeText(account.label || `号池 ${index + 1}`);
      const subtitle = node("div", "account-subtitle");
      subtitle.append(node("span", "", shortId(account.id)));
      if (state.config?.policy?.strategy === "pinned" && state.config.policy.pinned_key === account.id) subtitle.append(node("span", "pin-marker", "· 已指定"));
      title.append(heading, subtitle);
      const healthRows = filterModelHealth(account, accountModelFilter);
      const readyModels = healthRows.filter((health) => modelStatus(account, health).ready).length;
      const status = account.present === false || account.enabled === false || !healthRows.length ? accountStatus(account) : { label: `${readyModels}/${healthRows.length} 模型可调度`, style: readyModels ? "blue" : "amber" };
      header.append(node("div", "account-avatar", String(index + 1).padStart(2, "0")), title, badge(status.label, status.style));
      const models = node("div", "account-models");
      if (healthRows.length) {
        models.classList.add("with-health");
        for (const health of healthRows) {
          const row = node("div", "model-health-row");
          const description = node("div", "model-health-description");
          const modelName = node("span", "model-health-name", health.model);
          const modelState = modelStatus(account, health);
          const limit = health.max_concurrency ?? state.config?.policy?.max_concurrency_per_key ?? 1;
          const extra = node("span", "model-health-meta", `并发 ${health.in_flight || 0}/${limit} · 成功 ${count(health.successes ?? 0)} · 失败 ${count(health.failures ?? 0)}`);
          description.append(modelName, extra);
          const controls = node("div", "model-health-controls");
          controls.append(badge(modelState.label, modelState.style));
          if ((health.cooldown_until_ms || 0) > Date.now()) controls.append(accountButton("解除", "reset", account.id, "model-reset", health.model));
          row.append(description, controls);
          if (health.last_error) row.title = safeText(`${health.model}：${health.last_error}`);
          else row.title = health.last_ok_ms ? `此模型上次成功 ${timestamp(health.last_ok_ms)}` : "此模型尚无代理请求成功记录";
          models.append(row);
        }
      } else if (Array.isArray(account.models) && account.models.length) {
        for (const model of account.models.filter((model) => !accountModelFilter || model === accountModelFilter)) models.append(node("span", "model-tag", model));
      } else models.append(node("span", "model-tag", "暂无模型信息"));
      const metrics = node("div", "account-metrics");
      const values = [
        ["账号总并发", `${account.in_flight || 0}${state.config?.policy?.shared_account_concurrency ? ` / ${state.config.policy.shared_account_concurrency}` : ""}`],
        ["请求成功", count(account.successes ?? 0)],
        ["请求失败", count(account.failures ?? 0)],
      ];
      for (const [label, value] of values) { const metric = node("div"); metric.append(node("span", "mini-metric-label", label), node("span", "mini-metric-value", value)); metrics.append(metric); }
      const observation = node("div", `account-observation${account.last_error ? " account-error" : ""}`);
      if (account.last_error) { observation.textContent = safeText(account.last_error); observation.title = safeText(account.last_error); }
      else if (account.last_ok_ms) observation.textContent = `上次请求成功 · ${relativeTime(account.last_ok_ms)}`;
      else if (account.source_last_ok_ms) observation.textContent = `BiWrite 保活记录 · ${relativeTime(account.source_last_ok_ms)}`;
      else observation.textContent = "尚无代理请求记录，可发送小请求验证";
      const actions = node("div", "account-actions");
      const probe = accountButton("验证", "probe", account.id, "", accountModelFilter || null);
      probe.title = "向上游发送一个小请求以验证此号池";
      probe.disabled = probe.disabled || account.present === false;
      const pin = accountButton("固定使用", "pin", account.id);
      pin.disabled = pin.disabled || account.present === false || account.enabled === false;
      actions.append(probe, pin);
      if (probeResults.has(account.id)) {
        const log = accountButton("查看验证日志", "probe-log", account.id);
        log.disabled = false;
        actions.append(log);
      }
      if (!healthRows.length && (account.cooldown_until_ms || 0) > Date.now()) actions.append(accountButton("解除冷却", "reset", account.id));
      const toggle = accountButton(account.enabled === false ? "启用" : "已启用", "toggle", account.id, "account-toggle");
      toggle.setAttribute("aria-pressed", String(account.enabled !== false));
      toggle.setAttribute("aria-label", `${account.enabled === false ? "启用" : "停用"}${safeText(account.label || "此号池")}`);
      toggle.disabled = toggle.disabled || account.present === false;
      actions.append(toggle);
      card.append(header, models, metrics, observation, actions);
      return card;
    });
    $("account-list").replaceChildren(...cards);
    $("accounts-empty").hidden = allAccounts.length > 0;
    $("accounts-filter-empty").hidden = !allAccounts.length || accounts.length > 0;
    text("account-filter-summary", `显示 ${accounts.length} / ${allAccounts.length} 个号池${accountModelFilter ? ` · ${accountModelFilter}` : ""}。筛选仅影响视图，不改变路由。`);
    if (focusedAction && focusedAccount) {
      const replacement = [...$("account-list").querySelectorAll("button")].find((button) => button.dataset.action === focusedAction && button.dataset.account === focusedAccount && button.dataset.model === focusedModel);
      replacement?.focus({ preventScroll: true });
    }
  }

  function readDraft() {
    return {
      source_mode: document.querySelector('input[name="source_mode"]:checked')?.value || "biwrite",
      upstream_base_url: $("upstream-base-url").value,
      strategy: document.querySelector('input[name="strategy"]:checked')?.value || "sticky",
      pinned_key: $("pinned-key").value,
      max_concurrency_per_key: Number($("max-concurrency").value),
      shared_account_concurrency: Number($("shared-concurrency").value),
      model_override: $("model-override").value,
      reasoning_override: $("reasoning-override").value,
      max_retries: Number($("max-retries").value),
      queue_timeout: Number($("queue-timeout").value),
      recording_enabled: $("recording-enabled").checked,
      legacy_monitor_enabled: $("legacy-monitor-enabled").checked,
      session_recovery_enabled: $("session-recovery-enabled").checked,
      compaction_repair_enabled: $("compaction-repair-enabled").checked,
      compress_images: $("compress-images").checked,
      first_output_timeout: Number($("first-output-timeout").value),
      hedge_after: Number($("hedge-after").value),
    };
  }

  function setDirty(dirty) {
    formDirty = dirty;
    $("dirty-label").hidden = !dirty;
    $("save-settings").disabled = !dirty || !state || pendingActions.has("save");
    $("discard-settings").disabled = !dirty || pendingActions.has("save");
    text("save-hint", dirty ? "有未保存的修改，自动刷新会保留当前输入。" : "设置由本地服务保存。后续请求使用最新配置。");
  }

  function draftAccounts() {
    const mode = document.querySelector('input[name="source_mode"]:checked')?.value || "biwrite";
    if (mode === "manual") return manualAccounts;
    return state?.config?.source_mode === "manual" ? [] : (state?.accounts || []).filter((account) => account.present !== false);
  }

  function updateFormOptions() {
    const accounts = draftAccounts();
    const models = [...new Set(accounts.flatMap((account) => account.models || []))].sort();
    updateOptions($("model-override"), models, "跟随 Codex");
    updateOptions($("pinned-key"), accounts.map((account) => ({ value: account.id, label: `${account.label || "号池"} · ${shortId(account.id)}${account.enabled === false ? "（已停用）" : ""}` })), "请选择号池");
    const draft = readDraft();
    const unavailablePin = draft.strategy === "pinned" && draft.pinned_key && !accounts.some((account) => account.id === draft.pinned_key);
    $("source-mode-hint").classList.toggle("is-warning", !!unavailablePin);
    text("source-mode-hint", unavailablePin ? "当前指定账号不在所选来源的已知列表中；不会自动改用其他账号。请确认来源或重新指定账号后保存。" : draft.source_mode === "manual" ? "手动来源只改变账号的维护方式，仍可使用会话固定、随机或轮流使用。请先添加账号，再保存来源设置。" : "来源切换在保存设置后生效。自动来源也可以使用「指定号池」。");
  }

  function fillForm() {
    const draft = draftFromConfig(state.config);
    for (const radio of document.querySelectorAll('input[name="source_mode"]')) radio.checked = radio.value === draft.source_mode;
    for (const radio of document.querySelectorAll('input[name="strategy"]')) radio.checked = radio.value === draft.strategy;
    const accounts = draftAccounts();
    updateOptions($("pinned-key"), accounts.map((account) => ({ value: account.id, label: `${account.label || "号池"} · ${shortId(account.id)}${account.enabled === false ? "（已停用）" : ""}` })), "请选择号池", draft.pinned_key);
    updateOptions($("model-override"), [...new Set(accounts.flatMap((account) => account.models || []))].sort(), "跟随 Codex", draft.model_override);
    $("upstream-base-url").value = draft.upstream_base_url;
    $("reasoning-override").value = draft.reasoning_override;
    $("max-concurrency").value = draft.max_concurrency_per_key;
    $("shared-concurrency").value = draft.shared_account_concurrency;
    $("max-retries").value = draft.max_retries;
    $("queue-timeout").value = draft.queue_timeout;
    $("recording-enabled").checked = draft.recording_enabled;
    $("legacy-monitor-enabled").checked = draft.legacy_monitor_enabled;
    $("session-recovery-enabled").checked = draft.session_recovery_enabled;
    $("compaction-repair-enabled").checked = draft.compaction_repair_enabled;
    $("compress-images").checked = draft.compress_images;
    $("first-output-timeout").value = draft.first_output_timeout;
    $("hedge-after").value = draft.hedge_after;
    $("pinned-field").hidden = draft.strategy !== "pinned";
    baselineDraft = readDraft();
    setDirty(false);
    updateFormOptions();
  }

  function renderState(nextState) {
    if (!nextState || !Array.isArray(nextState.accounts) || !nextState.config) throw new Error("本地服务返回了无法识别的状态数据。");
    state = nextState;
    $("service-status").className = "live-status online";
    text("service-status-text", "服务运行中");
    showAlert("connection-error", "");
    text("last-update", `状态更新于 ${clockTime(Date.now())}`);
    renderStats();
    renderSourceProviders();
    renderAccountFilters();
    renderAccounts();
    if ($("probe-dialog").open && probeResults.get(probeAccountId)?.pending) renderProbeResult();
    if (!formDirty) fillForm();
    else updateFormOptions();
  }

  function refreshState() {
    if (statePending) return statePending;
    statePending = (async () => {
      try { renderState(await api("/api/state")); }
      catch (error) {
        $("service-status").className = "live-status offline";
        text("service-status-text", "连接中断");
        showAlert("connection-error", error.message);
      } finally { statePending = null; }
    })();
    return statePending;
  }

  function renderManualAccounts() {
    const signature = JSON.stringify([manualAccounts, state?.config?.policy?.pinned_key, [...pendingActions]]);
    if (signature === manualSignature) return;
    manualSignature = signature;
    const rows = manualAccounts.map((account) => {
      const row = node("div", "manual-account-row");
      const meta = node("div", "manual-account-meta");
      const title = node("div", "manual-account-title", account.label || `账号 · ${shortId(account.id)}`);
      if (state?.config?.policy?.pinned_key === account.id) title.append(badge("当前指定", "blue"));
      meta.append(title, node("div", "manual-account-details", `${shortId(account.id)} · ${(account.models || []).join("、")} · 每模型并发 ${account.max_concurrency ?? 1}`));
      const remove = node("button", "button button-secondary button-small manual-account-delete", "删除");
      remove.type = "button";
      remove.setAttribute("aria-label", `删除手动账号 ${safeText(account.label || shortId(account.id))}`);
      remove.disabled = pendingActions.has(`manual:${account.id}`);
      remove.addEventListener("click", () => deleteManualAccount(account.id));
      row.append(meta, remove);
      return row;
    });
    $("manual-list").replaceChildren(...rows);
    text("manual-count", manualAccounts.length);
    $("manual-empty").hidden = manualAccounts.length > 0;
  }

  async function refreshManualAccounts() {
    if (manualPending || !sessionToken) return false;
    manualPending = true;
    try {
      const data = await api("/api/manual");
      if (!Array.isArray(data.accounts)) throw new Error("本地服务返回了无法识别的手动池数据。");
      manualAccounts = data.accounts.map((account) => ({ id: String(account.id), label: typeof account.label === "string" ? account.label : "", models: Array.isArray(account.models) ? account.models.filter((model) => typeof model === "string") : [], max_concurrency: account.max_concurrency ?? 1 }));
      showAlert("manual-error", "");
      renderManualAccounts();
      if (state) updateFormOptions();
      return true;
    } catch (error) { showAlert("manual-error", error.message); return false; }
    finally { manualPending = false; }
  }

  async function deleteManualAccount(id) {
    const pendingId = `manual:${id}`;
    if (pendingActions.has(pendingId)) return;
    pendingActions.add(pendingId);
    renderManualAccounts();
    try {
      await serializeMutation(() => api(`/api/manual/${encodeURIComponent(id)}`, { method: "DELETE" }));
      toast("手动池账号已删除，指定号池设置不会自动改为其他账号。", "success");
      await refreshManualAccounts();
      await refreshState();
    } catch (error) { toast(error.message, "error"); }
    finally { pendingActions.delete(pendingId); renderManualAccounts(); }
  }

  function manualFormHasInput() {
    return !!($("manual-key").value || $("manual-models").value || $("manual-label").value || $("manual-concurrency").value !== "1");
  }

  async function handleAccountAction(action, id, model = null) {
    const account = state?.accounts?.find((item) => item.id === id);
    if (account && action === "probe-log") { openProbe(account, probeResults.get(id)?.details?.requested_model); return; }
    if (!account || pendingActions.has(id)) return;
    if (action === "probe") { openProbe(account, model); return; }
    pendingActions.add(id);
    renderAccounts(true);
    try {
      await serializeMutation(async () => {
        if (action === "reset") await api(`/api/accounts/${encodeURIComponent(id)}/reset`, { method: "POST", body: model ? { model } : {} });
        else {
          const latest = await api("/api/state");
          const policy = { ...latest.config.policy };
          if (action === "pin") { policy.strategy = "pinned"; policy.pinned_key = id; }
          if (action === "toggle") {
            const latestAccount = latest.accounts.find((item) => item.id === id);
            if (!latestAccount) throw new Error("此号池已从源中移除，请刷新后重试。");
            const overrides = { ...(policy.account_overrides || {}) };
            overrides[id] = { ...(overrides[id] || {}), enabled: latestAccount.enabled === false };
            policy.account_overrides = overrides;
          }
          await api("/api/config", { method: "POST", body: { policy } });
        }
      });
      toast(action === "reset" ? model ? `已解除 ${model} 的冷却。` : "已解除冷却，下次请求可重新尝试此号池。" : action === "pin" ? "已指定优先使用此号池。" : account.enabled === false ? "号池已启用。" : "号池已停用，进行中的请求继续完成。", "success");
      if (formDirty && action === "pin") toast("路由表单仍保留未保存的设置，保存时会覆盖当前路由策略。");
      await refreshState();
    } catch (error) { toast(error.message, "error"); }
    finally { pendingActions.delete(id); if (state) renderAccounts(true); }
  }

  function openProbe(account, preferredModel = null) {
    probeAccountId = account.id;
    text("probe-description", `验证 ${account.label || "号池"}（${shortId(account.id)}）的当前可用性。`);
    const models = Array.isArray(account.models) && account.models.length ? account.models : allModels();
    updateOptions($("probe-model"), models, null, models.includes(preferredModel) ? preferredModel : models[0] || "");
    $("submit-probe").disabled = !models.length || pendingActions.has(account.id);
    $("probe-model").disabled = pendingActions.has(account.id);
    text("submit-probe", pendingActions.has(account.id) ? "验证中…" : probeResults.has(account.id) ? "再次发送验证请求" : "发送验证请求");
    renderProbeResult();
    if (!$("probe-dialog").open) $("probe-dialog").showModal();
  }

  function renderProbeResult() {
    const result = probeResults.get(probeAccountId);
    $("probe-result").hidden = !result;
    if (!result) { $("probe-result-content").replaceChildren(); return; }
    const details = result.details || {};
    const label = result.pending ? "验证进行中" : result.ok ? "本次验证成功" : "本次验证失败";
    text("probe-result-status", `${label}${result.status ? ` · HTTP ${result.status}` : ""}`);
    $("probe-result-status").className = `badge badge-${result.pending ? "blue" : result.ok ? "green" : "red"}`;
    const grid = node("div", "detail-grid");
    grid.append(
      detail("验证模型", details.requested_model || "未提供"),
      detail("推理强度", details.requested_effort || "low"),
      detail("耗时", duration(result.pending ? Date.now() - result.started_at_ms : details.elapsed_ms)),
      detail("返回模型（上游声明）", result.response_model || "未提供"),
      detail("请求 ID", details.request_id || "未提供"),
      detail("错误类型", result.error_code || "无"),
    );
    const blocks = [grid];
    const addBlock = (title, content) => {
      const block = node("section", "probe-text-block");
      block.append(node("h4", "", title), node("pre", "", content));
      blocks.push(block);
    };
    addBlock("本次固定验证请求", details.request_text || "Reply only OK.");
    addBlock("返回内容", details.response_text || (result.pending ? "正在等待上游响应。关闭窗口后仍可从账号卡片查看本次结果。" : "上游未返回可显示的文本。"));
    if (result.message || details.error_message) addBlock(result.ok ? "结果说明" : "状态与错误详情", [result.message, details.error_message].filter(Boolean).filter((value, index, array) => array.indexOf(value) === index).join("\n"));
    if (!result.pending) {
      const usage = details.usage || {};
      const tokens = node("div", "detail-grid");
      tokens.append(detail("输入 / 输出 token", `${count(usage.input_tokens)} / ${count(usage.output_tokens)}`), detail("缓存 token", count(usage.cached_tokens)), detail("推理 token", count(usage.reasoning_tokens)));
      blocks.push(tokens);
    }
    const events = node("section", "probe-text-block");
    events.append(node("h4", "", "请求事件"));
    const list = node("ol", "probe-events");
    for (const event of details.events || []) {
      const item = node("li", "probe-event");
      const content = node("div");
      content.append(node("strong", "", event.type || "事件"), node("p", "", event.detail || ""));
      item.append(node("time", "", duration(event.elapsed_ms)), content);
      list.append(item);
    }
    events.append(list);
    if (!list.children.length) events.append(node("p", "subtle-note", result.pending ? "完整事件将在请求结束后显示。" : "没有返回事件明细。"));
    blocks.push(events);
    if (details.truncated) blocks.push(node("p", "subtle-note", "返回内容或事件达到显示上限，已截断。"));
    $("probe-result-content").replaceChildren(...blocks);
  }

  function conversationNode(record) {
    const conversation = conversationInfo(record);
    const element = node(conversation.href ? "a" : "span", "conversation-name", conversation.label);
    element.title = safeText([conversation.label, conversation.title && conversation.id, conversation.href && "在 Codex 中打开此对话"].filter(Boolean).join("\n"));
    if (conversation.href) {
      element.href = conversation.href;
      element.addEventListener("click", (event) => event.stopPropagation());
    }
    return element;
  }

  function conversationCell(record) {
    const cell = node("td", "conversation-cell");
    const { id } = conversationInfo(record);
    const summary = node("span", "cell-sub", `${clockTime(record.started_at_ms)}${id ? ` · ${shortId(id)}` : ""}`);
    summary.title = safeText([timestamp(record.started_at_ms), id].filter(Boolean).join(" · "));
    cell.append(conversationNode(record), summary);
    return cell;
  }

  function stackedCell(main, sub) {
    const cell = node("td");
    const mainNode = node("span", "cell-main", main);
    mainNode.title = safeText(main);
    cell.append(mainNode);
    if (sub !== undefined && sub !== null) {
      const subNode = node("span", "cell-sub");
      if (sub instanceof Node) subNode.append(sub); else subNode.textContent = safeText(sub);
      cell.append(subNode);
    }
    return cell;
  }

  let usageData = null;
  let usageSignature = "";

  function usageAmount(usd, pricing, withUsd = true) {
    const money = usageMoney(usd, pricing);
    const display = money.display === null ? null : `${money.currency} ${usageFormatAmount(money.display)}`;
    const usdText = `$${usageFormatAmount(money.usd)}`;
    if (!display) return usdText;
    return withUsd ? `${display} · ${usdText}` : display;
  }

  /// Bars are drawn as elements with a height class step, because the content
  /// security policy forbids inline styles.
  function renderUsageChart(id, bars, label) {
    const host = $(id);
    host.replaceChildren();
    if (!bars.length) {
      host.append(node("p", "usage-chart-empty", "所选范围还没有记录。"));
      return;
    }
    const track = node("div", "usage-chart-track");
    bars.forEach((bar, index) => {
      const column = node("div", "usage-chart-column");
      column.title = `${bar.date} · ${label(bar.value)}`;
      const fill = node("div", "usage-chart-fill");
      // Ten steps keep the stylesheet small while staying readable.
      const step = bar.value > 0 ? Math.max(1, Math.round(bar.ratio * 10)) : 0;
      fill.classList.add(`usage-step-${step}`);
      column.append(fill);
      const caption = node("span", "usage-chart-label", usageDayLabel(bar.date, bars[index - 1]?.date));
      column.append(caption);
      track.append(column);
    });
    host.append(track);
    const peak = bars.reduce((max, bar) => Math.max(max, bar.value), 0);
    host.append(node("p", "usage-chart-scale", `最高一天 ${label(peak)}`));
  }

  function renderUsageList(id, rows, pricing, nameOf) {
    const host = $(id);
    host.replaceChildren();
    if (!rows.length) {
      host.append(node("p", "usage-chart-empty", "今天还没有记录。"));
      return;
    }
    for (const row of rows.slice(0, 8)) {
      const line = node("div", "usage-bar-row");
      const head = node("div", "usage-bar-head");
      head.append(node("span", "usage-bar-name", nameOf ? nameOf(row) : row.name));
      head.append(node("span", "usage-bar-value", usageAmount(row.value, pricing, false)));
      line.append(head);
      const track = node("div", "usage-bar-track");
      const fill = node("div", "usage-bar-fill");
      fill.classList.add(`usage-step-${Math.max(1, Math.round(row.share * 10))}`);
      track.append(fill);
      line.append(track);
      const foot = `${count(row.requests)} 次请求${row.unpriced ? ` · ${count(row.unpriced)} 次无价格` : ""}`;
      line.append(node("span", "usage-bar-foot", foot));
      host.append(line);
    }
  }

  function renderUsage() {
    if (!usageData) return;
    const pricing = usageData.pricing || {};
    const today = usageData.today || {};
    const range = usageData.range || {};
    const signature = JSON.stringify([today, range, usageData.by_model, usageData.by_key, usageData.by_effort, usageData.by_conversation, pricing, $("usage-range").value]);
    if (signature === usageSignature) return;
    usageSignature = signature;

    text("usage-today-cost", usageAmount(today.cost_usd, pricing, false));
    $("usage-today-cost").title = usageAmount(today.cost_usd, pricing);
    text("usage-today-cost-foot", `${count(today.requests)} 次计入统计${today.unpriced_requests ? ` · ${count(today.unpriced_requests)} 次模型无价格` : ""}`);
    text("usage-today-tokens", count(usageTokenTotal(today.tokens)));
    text("usage-today-tokens-foot", `输入 ${count(today.tokens?.input)} · 输出 ${count(today.tokens?.output)} · 其中思考 ${count(today.tokens?.reasoning)}`);
    text("usage-range-cost", usageAmount(range.cost_usd, pricing, false));
    text("usage-range-foot", `${count(range.days)} 天合计 ${count(usageTokenTotal(range.tokens))} token`);

    const hit = usageCacheHitRate(today.tokens);
    text("usage-cache-rate", hit === null ? "—" : `${Math.round(hit * 100)}%`);
    const modelName = (usageData.by_model || [])[0]?.name;
    const prices = pricing.models?.[modelName]?.short;
    const saving = usageCacheSavingUsd(today.tokens, prices, pricing.tier_multipliers?.standard ?? 1, pricing.rate_multiplier ?? 1);
    text("usage-cache-foot", saving && saving > 0
      ? `缓存读取比普通输入省下约 ${usageAmount(saving, pricing, false)}`
      : "缓存读取占输入的比例");

    const uncounted = Number(range.uncounted_requests) || 0;
    showAlert("usage-warning", uncounted
      ? `所选范围内有 ${numberFormatter.format(uncounted)} 次请求没有收到上游用量，未计入统计。中途断流或上游报错时会出现这种情况。`
      : "");

    renderUsageChart("usage-cost-chart", usageBars(usageData.daily, (row) => row.cost_usd), (value) => usageAmount(value, pricing, false));
    renderUsageChart("usage-token-chart", usageBars(usageData.daily, (row) => usageTokenTotal(row.tokens)), (value) => `${numberFormatter.format(Math.round(value))} token`);

    renderUsageList("usage-by-model", usageShare(usageData.by_model, (row) => row.cost_usd), pricing);
    renderUsageList("usage-by-effort", usageShare(usageData.by_effort, (row) => row.cost_usd), pricing);
    renderUsageList("usage-by-key", usageShare(usageData.by_key, (row) => row.cost_usd), pricing, (row) => accountLabel(row.name));
    renderUsageList("usage-by-conversation", usageShare(usageData.by_conversation, (row) => row.cost_usd), pricing, conversationLabel);

    text("pricing-source", pricing.source_note || "");
    if (!$("usage-pricing").open) fillPricingForm(pricing);
  }

  /// Account ids are digests; show the nickname the pool already displays.
  function accountLabel(id) {
    const account = (state?.accounts || []).find((row) => row.id === id);
    return account?.label ? `${account.label}` : `${String(id).slice(0, 8)}…`;
  }

  function conversationLabel(row) {
    const title = typeof row?.title === "string" ? row.title.trim() : "";
    return title || `${String(row?.name || "").slice(0, 8)}…`;
  }

  function fillPricingForm(pricing) {
    $("pricing-rate").value = pricing.rate_multiplier ?? 1;
    $("pricing-currency").value = pricing.display_currency ?? "";
    $("pricing-fx").value = pricing.usd_to_display ?? "";
    $("pricing-offset").value = pricing.day_offset_minutes ?? 0;
    const host = $("usage-price-rows");
    host.replaceChildren();
    for (const [model, prices] of Object.entries(pricing.models || {})) {
      for (const bucket of ["short", "long"]) {
        const row = node("tr");
        const head = node("th", null, bucket === "short" ? model : "");
        head.scope = "row";
        row.append(head);
        row.append(node("td", null, bucket === "short" ? "≤ 272K" : "> 272K"));
        for (const field of ["input", "cached_input", "cache_write_30m", "cache_write_12h", "output"]) {
          const cell = node("td");
          const input = document.createElement("input");
          input.type = "number";
          input.step = "0.01";
          input.min = "0";
          input.max = "10000";
          input.value = prices?.[bucket]?.[field] ?? 0;
          input.dataset.model = model;
          input.dataset.bucket = bucket;
          input.dataset.field = field;
          input.className = "usage-price-input";
          input.setAttribute("aria-label", `${model} ${bucket === "short" ? "短上下文" : "长上下文"} ${field}`);
          cell.append(input);
          row.append(cell);
        }
        host.append(row);
      }
    }
  }

  function pricingFromForm(base) {
    const pricing = JSON.parse(JSON.stringify(base || {}));
    pricing.rate_multiplier = Number($("pricing-rate").value);
    pricing.display_currency = $("pricing-currency").value.trim();
    pricing.usd_to_display = Number($("pricing-fx").value);
    pricing.day_offset_minutes = Number($("pricing-offset").value);
    for (const input of document.querySelectorAll(".usage-price-input")) {
      const { model, bucket, field } = input.dataset;
      if (!pricing.models?.[model]?.[bucket]) continue;
      pricing.models[model][bucket][field] = Number(input.value);
    }
    return pricing;
  }

  let configFilesLoaded = false;

  async function refreshConfigFiles() {
    const host = $("config-file-list");
    try {
      const result = await api("/api/config/files");
      host.replaceChildren();
      for (const file of result?.files || []) {
        const block = node("section", "config-file");
        const head = node("div", "config-file-head");
        head.append(node("h4", null, file.name));
        const path = node("code", "config-file-path", file.path);
        head.append(path);
        block.append(head);
        if (file.note) block.append(node("p", "config-file-note", file.note));
        block.append(node("pre", "config-file-body", file.content || "（文件为空或无法读取）"));
        host.append(block);
      }
      configFilesLoaded = true;
    } catch (error) {
      host.replaceChildren(node("p", "config-file-note", error.message));
    }
  }

  $("config-files").addEventListener("toggle", () => {
    if ($("config-files").open && !configFilesLoaded) refreshConfigFiles();
  });

  async function refreshUsage() {
    try {
      usageData = await api(`/api/usage?days=${encodeURIComponent($("usage-range").value)}`);
      renderUsage();
    } catch (error) {
      showAlert("usage-warning", error.message);
    }
  }

  function renderRecords(force = false) {
    const selectedModel = $("filter-model").value;
    const conversation = $("filter-conversation").value.trim().toLowerCase();
    const selectedStatus = $("filter-status").value;
    const selectedTier = $("filter-tier").value;
    const signature = JSON.stringify([records, selectedModel, conversation, selectedStatus, selectedTier, state?.accounts?.map((account) => [account.id, account.label])]);
    if (!force && signature === recordSignature) return;
    recordSignature = signature;
    const models = [...new Set(records.flatMap((record) => [record.requested_model, record.effective_model, record.response_model]).filter((value) => typeof value === "string" && value))].sort();
    updateOptions($("filter-model"), models, "全部模型");
    const filtered = records.filter((record) => (!selectedModel || [record.requested_model, record.effective_model, record.response_model].includes(selectedModel)) && matchesConversationFilter(record, conversation) && (!selectedStatus || classifyRequestOutcome(record).type === selectedStatus) && matchesServiceTierFilter(record, selectedTier));
    const rows = filtered.map((record) => {
      const row = node("tr");
      row.tabIndex = 0;
      row.setAttribute("aria-label", safeText(`查看 ${conversationInfo(record).label} 于 ${clockTime(record.started_at_ms)} 的请求详情`));
      row.addEventListener("click", () => openRecord(record));
      row.addEventListener("keydown", (event) => { if (event.target === row && (event.key === "Enter" || event.key === " ")) { event.preventDefault(); openRecord(record); } });
      const status = classifyRequestOutcome(record);
      const route = requestRouteMode(record);
      const tier = recordServiceTier(record);
      const tierCell = stackedCell(`${tier.requested.label} → ${tier.responseConfirmed ? tier.response.label : "未确认"}`, tier.comparison === "different" ? "上游声明不同" : tier.comparison === "same" ? "上游声明分组一致" : "上游声明未确认");
      tierCell.classList.add("tier-cell");
      if (tier.comparison === "different") tierCell.classList.add("tier-different");
      const statusCell = node("td");
      statusCell.append(badge(status.label, status.style));
      if (Array.isArray(record.attempts) && record.attempts.length > 1) statusCell.append(node("span", "cell-sub", `${record.attempts.length} 次尝试`));
      row.append(
        conversationCell(record),
        stackedCell(record.effective_model || record.requested_model || "—", record.effective_effort || record.requested_effort || "强度未提供"),
        stackedCell(route.label, `${accountLabel(record.key_id)} · ${record.transport || "—"}`),
        tierCell,
        statusCell,
        stackedCell(duration(record.duration_ms), `有效输出 ${duration(effectiveOutputWait(record))}`),
        stackedCell(`${count(record.input_tokens)} / ${count(record.output_tokens)}`, "token"),
        stackedCell(count(record.cached_tokens)),
      );
      return row;
    });
    $("records-body").replaceChildren(...rows);
    $("records-empty").hidden = filtered.length > 0;
    $("records-empty").querySelector("h3").textContent = records.length ? "没有符合筛选条件的记录" : "尚无请求记录";
    $("records-empty").querySelector("p").textContent = records.length ? "调整模型、对话名称或 ID、状态或档位后重试。" : state?.config?.recording_enabled === false ? "记录已暂停，可在路由设置中启用请求监控。" : "请求经过 Pool Router 后，路由情况与诊断信息会显示在这里。";
    text("record-count", `${filtered.length} 条记录${filtered.length !== records.length ? ` / 共 ${records.length} 条` : ""}`);
    if (openedRecordId && $("record-dialog").open) {
      const updated = records.find((record) => String(record.id) === openedRecordId);
      if (updated) renderRecordDetail(updated);
    }
  }

  async function refreshRecords() {
    if (recordsPending) return;
    recordsPending = true;
    try {
      const data = await api("/api/records");
      records = (Array.isArray(data.records) ? data.records : []).slice().sort((a, b) => (b.started_at_ms || 0) - (a.started_at_ms || 0));
      showAlert("records-error", "");
      renderRecords();
    } catch (error) { showAlert("records-error", error.message); }
    finally { recordsPending = false; }
  }

  function detail(label, value, wide = false) {
    const element = node("dl", `detail-item${wide ? " wide" : ""}`);
    const content = node("dd");
    if (value instanceof Node) content.append(value); else content.textContent = safeText(value ?? "—");
    element.append(node("dt", "", label), content);
    return element;
  }

  function renderRecordDetail(record) {
    const status = classifyRequestOutcome(record);
    const route = requestRouteMode(record);
    const tier = recordServiceTier(record);
    const grid = node("div", "detail-grid");
    grid.append(
      detail("请求时间", timestamp(record.started_at_ms)),
      detail("请求结果", badge(`${status.label}${record.status ? ` · HTTP ${record.status}` : ""}`, status.style)),
      detail("传输方式", record.transport || "未提供"),
      detail("转发方式", route.label),
      detail("账号处理", route.mode === "legacy" ? "按 CC Switch 传来的 Key 和模型转发，不自动换号。" : "使用当前来源的号池，按保存的路由策略分配请求。", true),
      detail("对话名称", conversationNode(record), true),
      detail("对话 ID", conversationNode({ conversation_id: record.conversation_id }), true),
      detail("Codex 请求模型", record.requested_model || "未提供"),
      detail("代理转发模型", record.effective_model || "未提供"),
      detail("返回模型（供应商声明）", record.response_model || "未提供"),
      detail("Codex 请求强度", record.requested_effort || "未提供"),
      detail("代理转发强度", record.effective_effort || "未提供"),
      detail("返回强度", record.response_effort || "未提供"),
      detail("请求档位", `${tier.requested.label}（原值：${tier.requested.raw == null || tier.requested.raw === "" ? "未提供" : tier.requested.raw}）`),
      detail("响应档位（上游声明）", `${tier.responseConfirmed ? tier.response.label : "未确认"}（原值：${tier.response.raw == null || tier.response.raw === "" ? "未提供" : tier.response.raw}）`),
      detail("档位比较", tier.comparison === "different" ? "上游声明不同" : tier.comparison === "same" ? "上游声明分组一致" : "未确认"),
      detail("档位说明", "Fast 与 priority 归为同一展示组，原始字段分别保留。仅完成的请求可比较上游声明；档位字段不能独立证明实际调度或服务质量。", true),
      detail(route.mode === "legacy" ? "上游账号标识" : "使用号池", `${accountLabel(record.key_id)} · ${shortId(record.key_id)}`),
      detail("总耗时", duration(record.duration_ms)),
      detail("首段有效输出等待", duration(effectiveOutputWait(record))),
      detail("首个响应数据耗时", duration(record.ttfb_ms)),
      detail("输入 token", count(record.input_tokens)),
      detail("输出 token", count(record.output_tokens)),
      detail("缓存 token", count(record.cached_tokens)),
      detail("推理 token", count(record.reasoning_tokens)),
      detail("请求 / 响应体积", `${bytes(record.request_bytes)} / ${bytes(record.response_bytes)}`),
      detail("最后一次上游请求体积", bytes(record.upstream_request_bytes)),
      detail("输出耗时说明", "有效输出单独记录，不使用 SSE 心跳、首包或累计请求耗时代替。未记录时显示未知。", true),
      detail("输入项数量", count(record.input_items)),
      detail("携带 previous_response_id", record.has_previous_response_id == null ? "未提供" : record.has_previous_response_id ? "是" : "否"),
      detail("store 字段", record.store == null ? "未提供" : String(record.store)),
      detail("请求记录 ID", record.id == null ? "未提供" : String(record.id)),
    );
    const attemptSection = node("section", "attempt-section");
    const attempts = Array.isArray(record.attempts) ? record.attempts : [];
    attemptSection.append(node("h3", "", `路由尝试与上游错误 · ${attempts.length} 次尝试`));
    if (attempts.length) {
      const list = node("ol", "attempt-list");
      attempts.forEach((attempt, index) => {
        const item = node("li", "attempt-item");
        const content = node("div", "attempt-content");
        const title = node("div", "attempt-title");
        title.append(node("span", "", accountLabel(attempt.key_id)), node("span", "", attempt.status == null ? "无 HTTP 状态" : `HTTP ${attempt.status}`));
        content.append(title);
        const error = attemptErrorDetails(attempt);
        if (error.reason) content.append(node("div", "attempt-reason", error.reason === "completed" ? "本次尝试完成" : `错误类型 · ${error.reason}`));
        if (error.message) content.append(node("div", "attempt-error-message", error.message));
        item.append(node("span", "attempt-number", index + 1), content, node("span", "attempt-duration", duration(attempt.duration_ms)));
        list.append(item);
      });
      attemptSection.append(list);
    } else attemptSection.append(node("div", "subtle-note", "尚无尝试明细。"));
    const gatewaySection = node("section", "attempt-section");
    gatewaySection.append(node("h3", "", "恢复与网关处理"));
    const gatewayActions = gatewayActionRows(record);
    if (gatewayActions.length) {
      const list = node("ol", "attempt-list");
      for (const action of gatewayActions) {
        const item = node("li", "attempt-item");
        const content = node("div", "attempt-content");
        content.append(node("div", "attempt-title", `${action.label}${action.attempt === 0 ? " · 首次发送前" : action.attempt !== null ? ` · 第 ${action.attempt} 次尝试` : ""}`));
        const note = [];
        if (asNumber(action.count) !== null) note.push(`处理数量 ${count(action.count)}`);
        if (asNumber(action.before_bytes) !== null || asNumber(action.after_bytes) !== null) note.push(`体积 ${bytes(action.before_bytes)} → ${bytes(action.after_bytes)}`);
        if (note.length) content.append(node("div", "attempt-reason", note.join(" · ")));
        item.append(content, node("span", "attempt-duration", duration(action.elapsed_ms)));
        list.append(item);
      }
      gatewaySection.append(list);
    } else gatewaySection.append(node("p", "subtle-note", "没有记录网关处理动作。"));
    $("record-details").replaceChildren(grid, gatewaySection, attemptSection);
  }

  function openRecord(record) {
    openedRecordId = String(record.id);
    renderRecordDetail(record);
    if (!$("record-dialog").open) $("record-dialog").showModal();
  }

  function switchTab(tabName, focus = false) {
    activeTab = tabName;
    for (const button of document.querySelectorAll("[data-tab]")) {
      const selected = button.dataset.tab === tabName;
      button.classList.toggle("active", selected);
      button.setAttribute("aria-selected", String(selected));
      button.tabIndex = selected ? 0 : -1;
      $(`panel-${button.dataset.tab}`).hidden = !selected;
      if (selected && focus) button.focus();
    }
    if (tabName === "monitor") { renderRecords(true); refreshRecords(); }
    if (tabName === "usage") refreshUsage();
  }

  document.querySelectorAll("[data-tab]").forEach((button) => {
    button.addEventListener("click", () => switchTab(button.dataset.tab));
    button.addEventListener("keydown", (event) => {
      const tabs = ["accounts", "routing", "usage", "monitor"];
      const index = tabs.indexOf(activeTab);
      let next = null;
      if (event.key === "ArrowRight") next = tabs[(index + 1) % tabs.length];
      if (event.key === "ArrowLeft") next = tabs[(index + tabs.length - 1) % tabs.length];
      if (event.key === "Home") next = tabs[0];
      if (event.key === "End") next = tabs[tabs.length - 1];
      if (next) { event.preventDefault(); switchTab(next, true); }
    });
  });

  function onRoutingInput(event) {
    $("pinned-field").hidden = readDraft().strategy !== "pinned";
    if (state) updateFormOptions();
    setDirty(JSON.stringify(readDraft()) !== JSON.stringify(baselineDraft));
    if (event.target.name === "source_mode" && readDraft().source_mode === "manual") refreshManualAccounts();
  }
  $("routing-form").addEventListener("input", onRoutingInput);
  $("routing-form").addEventListener("change", onRoutingInput);
  $("discard-settings").addEventListener("click", () => { if (state) fillForm(); });
  $("routing-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    if (!state || pendingActions.has("save") || !$("routing-form").reportValidity()) return;
    const draft = readDraft();
    if (draft.strategy === "pinned" && !draft.pinned_key) { toast("请先选择要指定的号池。", "error"); $("pinned-key").focus(); return; }
    pendingActions.add("save");
    setDirty(formDirty);
    const submitLabel = $("save-settings").textContent;
    text("save-settings", "正在保存…");
    try {
      await serializeMutation(async () => {
        const latest = await api("/api/state");
        await api("/api/config", { method: "POST", body: routingConfigPayload(draft, latest.config) });
      });
      if (JSON.stringify(readDraft()) === JSON.stringify(draft)) setDirty(false);
      else { baselineDraft = draft; setDirty(true); }
      toast("设置已保存，后续请求将使用最新配置。", "success");
      await refreshState();
    } catch (error) { toast(error.message, "error"); }
    finally { pendingActions.delete("save"); text("save-settings", submitLabel); setDirty(formDirty); }
  });

  $("open-manual-editor").addEventListener("click", () => {
    $("manual-editor").open = true;
    $("manual-editor").scrollIntoView({ behavior: "auto", block: "start" });
    refreshManualAccounts();
  });
  $("manage-sources").addEventListener("click", () => switchTab("routing"));
  $("manual-editor").addEventListener("toggle", () => { if ($("manual-editor").open) refreshManualAccounts(); });
  $("manual-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    if (!sessionToken || pendingActions.has("manual-save") || !$("manual-form").reportValidity()) return;
    let payload;
    try { payload = manualAccountPayload({ key: $("manual-key").value, models: $("manual-models").value, label: $("manual-label").value, max_concurrency: $("manual-concurrency").value }); }
    catch (error) { toast(error.message, "error"); return; }
    pendingActions.add("manual-save");
    const fields = [...$("manual-form").querySelectorAll("input,button")];
    fields.forEach((field) => { field.disabled = true; });
    text("save-manual-account", "正在保存…");
    try {
      await serializeMutation(() => api("/api/manual", { method: "POST", body: payload }));
      $("manual-form").reset();
      $("manual-key").value = "";
      toast(state?.config?.source_mode === "manual" ? "手动池已保存，后续请求将使用最新账号信息。" : "手动池已保存。当前仍使用自动来源，保存来源设置后才会切换。", "success");
      await refreshManualAccounts();
      await refreshState();
    } catch (error) { toast(error.message, "error"); }
    finally {
      payload.key = "";
      fields.forEach((field) => { field.disabled = false; });
      pendingActions.delete("manual-save");
      text("save-manual-account", "添加或更新账号");
    }
  });

  async function updateSyncConfig(action) {
    if (!state || pendingActions.has("sync-config")) return;
    const patch = syncConfigPatch(action, state.config);
    pendingActions.add("sync-config");
    renderStats();
    try {
      await serializeMutation(() => api("/api/config", { method: "POST", body: patch }));
      await refreshState();
      toast(action === "pause" ? patch.sync_paused ? "自动同步已暂停，当前号池仍可使用。" : "自动同步已恢复。" : patch.biwrite_import_all ? `已切换为持续同步全部 Key。${state?.config?.sync_paused ? "自动同步仍处于暂停状态，可手动读取或恢复。" : ""}` : "已切换为仅同步保活候选。", "success");
      if (action === "scope" && formDirty) toast("路由表单仍有未保存的修改，请在保存前检查来源选择。");
    } catch (error) { toast(error.message, "error"); }
    finally { pendingActions.delete("sync-config"); if (state) renderStats(); }
  }
  $("pause-sync-button").addEventListener("click", () => updateSyncConfig("pause"));
  $("sync-scope-button").addEventListener("click", () => updateSyncConfig("scope"));

  $("usage-range").addEventListener("change", () => { usageSignature = ""; refreshUsage(); });

  $("usage-pricing-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const button = $("pricing-save");
    button.disabled = true;
    try {
      const result = await api("/api/usage/pricing", { method: "POST", body: pricingFromForm(usageData?.pricing) });
      if (usageData && result?.pricing) usageData.pricing = result.pricing;
      usageSignature = "";
      await refreshUsage();
      toast("价目表已保存，金额按新单价重新折算。", "success");
    } catch (error) { toast(error.message, "error"); }
    finally { button.disabled = false; }
  });

  $("pricing-reset").addEventListener("click", () => {
    if (usageData?.pricing) fillPricingForm(usageData.pricing);
  });

  $("sync-button").addEventListener("click", async () => {
    if (pendingActions.has("sync") || state?.sync?.running) return;
    pendingActions.add("sync");
    try {
      renderStats();
      const result = await api("/api/sync", { method: "POST", body: {}, timeout: 0 });
      if (state && result?.sync) state.sync = result.sync;
      await refreshState();
      if (state && result?.sync) state.sync = result.sync;
      toast(state?.config?.source_mode === "manual" ? "已重新读取当前手动池。" : verifiesCandidates(state?.config) ? syncResultText(result?.sync || state?.sync) : "已重新读取 BiWrite 全部 Key，未执行实时验证。", "success");
      if (state?.config?.sync_paused) toast("自动同步仍处于暂停状态。");
    } catch (error) { toast(error.message, "error"); }
    finally { pendingActions.delete("sync"); if (state) renderStats(); }
  });

  $("connect-button").addEventListener("click", async () => {
    const button = $("connect-button");
    button.disabled = true;
    text("connect-button", "正在接入…");
    try {
      const result = await serializeMutation(() => api("/api/connect", { method: "POST", body: {}, timeout: 30000 }));
      if (result.ok === false) throw new Error(safeText(result.message || "接入配置未完成。"));
      toast(result.message || "Codex 直接接入配置已更新。", "success");
      await refreshState();
    } catch (error) { toast(error.message, "error"); }
    finally { button.disabled = false; text("connect-button", state?.connection?.configured ? "检查接入" : "直接接入 Codex"); }
  });

  $("disconnect-button").addEventListener("click", async () => {
    const button = $("disconnect-button");
    button.disabled = true;
    text("disconnect-button", "正在断开…");
    try {
      const result = await serializeMutation(() => api("/api/restore", { method: "POST", body: {}, timeout: 30000 }));
      if (result.ok === false) throw new Error(safeText(result.message || "断开接入未完成。"));
      toast(result.message || "已把 Codex 配置还原到接入之前。重新打开 Codex 会话后生效。", "success");
      await refreshState();
    } catch (error) { toast(error.message, "error"); }
    finally { button.disabled = false; text("disconnect-button", "断开接入"); }
  });

  async function updateLegacyConnection(action) {
    if (!sessionToken || pendingActions.has("legacy-connection")) return;
    pendingActions.add("legacy-connection");
    const connect = $("legacy-connect-button");
    const restore = $("legacy-restore-button");
    connect.disabled = true;
    restore.disabled = true;
    const active = action === "connect" ? connect : restore;
    active.textContent = action === "connect" ? "正在接入…" : "正在恢复…";
    try {
      const result = await serializeMutation(() => api(`/api/legacy/${action}`, { method: "POST", body: {}, timeout: 30000 }));
      if (result.ok === false) throw new Error(safeText(result.message || "旧方式接入配置未完成。"));
      toast(result.message || (action === "connect" ? "旧方式监控已接入，后续照旧在 CC Switch 修改 Key。" : "已恢复旧方式接入前配置。"), "success");
      await refreshState();
    } catch (error) { toast(error.message, "error"); }
    finally {
      pendingActions.delete("legacy-connection");
      connect.disabled = false;
      restore.disabled = false;
      text("legacy-connect-button", "接入旧方式监控");
      text("legacy-restore-button", "恢复旧方式接入前配置");
    }
  }
  $("legacy-connect-button").addEventListener("click", () => updateLegacyConnection("connect"));
  $("legacy-restore-button").addEventListener("click", () => updateLegacyConnection("restore"));

  function renderUpdate() {
    const { info, phase, message } = updateState;
    const actions = updateActionState(info, phase);
    text("update-current-version", info?.current_version || state?.version || "未知");
    text("update-latest-version", info?.latest_version || "尚未取得");
    text("update-asset-name", info ? info.asset_name || "此版本未提供当前平台安装包" : "检查后显示适合本机的安装包");
    text("update-message", message || info?.message || "检查 GitHub 最新正式 Release，确认是否有适合本机的更新。");
    $("update-message").classList.toggle("update-error", phase === "error");
    $("update-progress").hidden = !actions.busy;
    $("update-progress").setAttribute("aria-label", phase === "downloading" ? "正在下载并校验安装包" : "正在检查更新");
    $("update-recheck").disabled = actions.busy || !sessionToken;
    $("update-download").disabled = !actions.canDownload || !sessionToken;
    const link = $("update-release-link");
    link.hidden = !actions.releaseUrl;
    if (actions.releaseUrl) link.href = actions.releaseUrl;
    else link.removeAttribute("href");
  }

  async function checkUpdate() {
    if (!sessionToken || updateActionState(updateState.info, updateState.phase).busy) return;
    updateState = { info: null, phase: "checking", message: "正在检查 GitHub 最新正式 Release…" };
    renderUpdate();
    try {
      const info = await api("/api/update/check", { method: "POST", body: {}, timeout: 310000 });
      updateState = { info, phase: "ready", message: "" };
    } catch (error) {
      updateState = { info: null, phase: "error", message: `${error.message} 请检查本机服务与 GitHub 网络连接，然后点击“检查更新”重试。` };
    }
    renderUpdate();
  }

  $("check-updates-button").addEventListener("click", () => {
    if (!$("update-dialog").open) $("update-dialog").showModal();
    renderUpdate();
    checkUpdate();
  });
  $("update-recheck").addEventListener("click", checkUpdate);
  $("close-update-dialog").addEventListener("click", () => $("update-dialog").close());
  $("update-download").addEventListener("click", async () => {
    if (!sessionToken || !updateActionState(updateState.info, updateState.phase).canDownload) return;
    const info = updateState.info;
    updateState = { info, phase: "downloading", message: "正在下载并校验安装包，请稍候。完成后会打开本机安装包。" };
    renderUpdate();
    try {
      const result = await api("/api/update/download", { method: "POST", body: { version: info.latest_version }, timeout: 930000 });
      if (result.ok !== true || result.opened !== true || compareReleaseVersions(result.version, info.latest_version) !== 0) throw new Error(result.message || "尚未确认安装包已打开，请检查更新后重试。");
      const instruction = result.filename === "PoolRouter-macOS-arm64.dmg" ? "请双击 DMG 中的“安装.command”完成安装。" : result.filename === "PoolRouter-Windows-x64-Setup.exe" ? "请在打开的 Windows 安装向导中完成安装。" : "请按已打开安装包的提示完成安装。";
      updateState = { info, phase: "opened", message: `安装包已校验并打开。${instruction}现有配置会保留；安装完成后刷新此页面查看版本。` };
    } catch (error) {
      updateState = { info, phase: "error", message: `${error.message} 请检查网络后重试；如果安装包已经打开，请先完成安装。` };
    }
    renderUpdate();
  });

  $("copy-url").addEventListener("click", async () => {
    const url = state?.connection?.base_url;
    if (!url) return;
    try { await navigator.clipboard.writeText(url); toast("本地接入地址已复制。", "success"); }
    catch (_) { toast("浏览器未允许自动复制，请选中接入地址后复制。", "error"); }
  });

  $("probe-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const id = probeAccountId;
    const model = $("probe-model").value;
    if (!id || !model || pendingActions.has(id)) return;
    pendingActions.add(id);
    const startedAt = Date.now();
    probeResults.set(id, { pending: true, started_at_ms: startedAt, details: { requested_model: model, requested_effort: "low", request_text: "Reply only OK.", events: [{ type: "请求已提交", elapsed_ms: 0, detail: "使用所选账号和模型发送固定验证请求。" }] } });
    $("submit-probe").disabled = true;
    $("probe-model").disabled = true;
    text("submit-probe", "验证中…");
    renderProbeResult();
    renderAccounts(true);
    try {
      const result = await api("/api/probe", { method: "POST", body: { key_id: id, model }, timeout: Math.max(180000, (state?.config?.queue_timeout_ms ?? 600000) + 180000), probeDetails: true });
      probeResults.set(id, normalizeProbeResult(result, model, Date.now() - startedAt, [sessionToken, $("manual-key")?.value]));
    } catch (error) {
      probeResults.set(id, normalizeProbeResult({ ok: false, message: error.message, error_code: "local_request_failed", details: { error_message: "未取得完整验证结果，不能据此确认上游未执行。" } }, model, Date.now() - startedAt, [sessionToken, $("manual-key")?.value]));
    } finally {
      pendingActions.delete(id);
      if (probeAccountId === id) {
        $("submit-probe").disabled = false;
        $("probe-model").disabled = false;
        text("submit-probe", "再次发送验证请求");
        renderProbeResult();
      }
      await refreshState();
      await refreshRecords();
      if (state) renderAccounts(true);
    }
  });

  $("close-record-dialog").addEventListener("click", () => $("record-dialog").close());
  $("record-dialog").addEventListener("close", () => { openedRecordId = null; });
  $("close-probe-dialog").addEventListener("click", () => $("probe-dialog").close());
  $("cancel-probe").addEventListener("click", () => $("probe-dialog").close());
  for (const id of ["record-dialog", "probe-dialog", "update-dialog"]) {
    $(id).addEventListener("click", (event) => { if (event.target !== $(id)) return; const rect = $(id).getBoundingClientRect(); if (event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom) $(id).close(); });
  }
  for (const id of ["filter-model", "filter-status", "filter-tier"]) $(id).addEventListener("change", () => renderRecords(true));
  $("filter-conversation").addEventListener("input", () => renderRecords(true));
  $("refresh-records").addEventListener("click", async () => { $("refresh-records").disabled = true; await refreshRecords(); $("refresh-records").disabled = false; });
  document.addEventListener("visibilitychange", () => { if (!document.hidden) { refreshState(); if (activeTab === "monitor") refreshRecords(); if (activeTab === "usage") refreshUsage(); } });
  window.addEventListener("beforeunload", (event) => { if (formDirty || manualFormHasInput()) { event.preventDefault(); event.returnValue = ""; } });

  if (!sessionToken) {
    $("service-status").className = "live-status offline";
    text("service-status-text", "需要授权");
    showAlert("connection-error", "此页面缺少控制台令牌。请使用 Pool Router 启动时提供的控制台地址打开，令牌会仅保存在当前浏览器会话中。");
    $("check-updates-button").disabled = true;
    $("sync-button").disabled = true;
    $("pause-sync-button").disabled = true;
    $("sync-scope-button").disabled = true;
    $("connect-button").disabled = true;
    $("refresh-records").disabled = true;
    $("save-manual-account").disabled = true;
    $("legacy-connect-button").disabled = true;
    $("legacy-restore-button").disabled = true;
  } else {
    refreshState();
    refreshRecords();
    refreshManualAccounts();
    window.setInterval(() => { if (!document.hidden) { refreshState(); if (activeTab === "monitor") refreshRecords(); if (activeTab === "usage") refreshUsage(); if ($("manual-editor").open || document.querySelector('input[name="source_mode"]:checked')?.value === "manual") refreshManualAccounts(); } }, 2000);
  }
})();
