# Codex Wiretap (Rust)

[简体中文](README.md) | **English**

The Rust edition of Codex Wiretap provides local account pool routing and request monitoring. By default, it reads upstream candidates and key names from BiWrite and connects Codex directly to `http://127.0.0.1:10813/v1`. It schedules requests by account and model, and displays request status and usage in a local dashboard. The default workflow requires BiWrite and this application; CC Switch is not a dependency.

The desktop launcher is named **Pool Router**, and the executable is **`pool-router`**. On macOS, the launcher opens a browser dashboard while a local background service handles requests. An optional CC Switch monitoring path is also available for users who continue to manage their keys in CC Switch.

[Download a prebuilt release](https://github.com/qzkinhit/codex-wiretap/releases/latest) · [Historical Python implementation](legacy/python/)

![Example Codex Wiretap dashboard](docs/dashboard.png)

The screenshot uses simulated data and contains no real accounts, keys, or conversations.

## Installation and startup

### Prebuilt release

On Apple Silicon Macs running macOS 11 or later, download **`PoolRouter-macOS-arm64.dmg`** from the [latest release](https://github.com/qzkinhit/codex-wiretap/releases/latest). Open the disk image and double-click **`安装.command`**. The DMG installer uses tools included with macOS and requires neither compilation nor a separate Python installation. Eject the image after installation and use `~/Applications/Pool Router.app` afterward.

The installer uses the included `target/release/pool-router` binary, installs a user background service and `~/Applications/Pool Router.app`, and then opens the dashboard. Installation itself does not change Codex or CC Switch connection settings.

The existing v0.1.0 `PoolRouter-macOS-arm64.zip` remains available. Its older installer still requires `python3` to generate macOS plist files. Extract the ZIP and double-click `安装.command`, or run this from its root directory:

```bash
bash scripts/macos-install.sh --no-build
```

The DMG and ZIP contain the same executable and dependency licenses; the DMG includes updated installation scripts. The DMG is read-only and has not been notarized by Apple. If macOS blocks the first opening after download, verify the download source, find the corresponding message in System Settings under Privacy & Security, choose Open Anyway, and confirm. On older macOS versions, use Security & Privacy in System Preferences.

Reproduce the DMG from an existing binary ZIP by running the following command in the current repository. Packaging requires Python 3; installing the DMG does not.

```bash
python3 scripts/macos-package-dmg.py /path/PoolRouter-macOS-arm64.zip /path/PoolRouter-macOS-arm64.dmg
```

### Build from source

Install a current stable Rust toolchain, Cargo, and a C compiler toolchain. On macOS, build and install with:

```bash
git clone https://github.com/qzkinhit/codex-wiretap.git
cd codex-wiretap
bash scripts/macos-install.sh
```

Installer options are `--no-build`, `--no-open`, and `--no-start`. The last option installs files without starting the service or opening the dashboard. Reinstalling restarts this application's own background service.

To run without installing a background service:

```bash
cargo build --release --locked
./target/release/pool-router init
./target/release/pool-router serve
```

Keep that service running, then open the dashboard from another terminal:

```bash
./target/release/pool-router open
```

All commands accept `--config /absolute/path/config.json`. The `open` command reads the private local token and supplies it to the dashboard through a URL fragment without printing it. Stop a foreground service with `Ctrl+C`.

### macOS installation paths

| Item | Location |
| --- | --- |
| Executable | `~/Library/Application Support/Pool Router/bin/pool-router` |
| Configuration | `~/Library/Application Support/Pool Router/config.json` |
| Manual account store | `~/Library/Application Support/Pool Router/manual-keys.json` |
| Request metadata | `~/Library/Application Support/Pool Router/records.jsonl` |
| Service log | `~/Library/Application Support/Pool Router/logs/service.log` |
| Launcher | `~/Applications/Pool Router.app` |
| Background service | `~/Library/LaunchAgents/local.pool-router.plist` |

Other systems can use the command-line build and runtime. The background service installer included in this repository is specific to macOS.

## Default workflow: BiWrite and direct Codex connection

1. Configure the required Responses providers in BiWrite. Enable local key storage so that keys are available in `keys.json`. The default scope imports keep-alive candidates only, so run keep-alive for the required models when using that scope.
2. Start Pool Router. A single eligible upstream address in BiWrite is selected automatically; if several addresses exist, enter the desired Base URL in routing settings and save it. Choose continuous all-key synchronization to include keys outside the keep-alive set. Check the models and candidate counts in the source synchronization summary. Optional account/model probes send real upstream requests and may consume quota; importing alone does not verify availability.
3. Use the dashboard's direct Codex connection action, or run:

```bash
"$HOME/Library/Application Support/Pool Router/bin/pool-router" connect
```

The connection action backs up `~/.codex/config.toml`, adds a separate `pool_router` provider, and selects it as Codex's active provider. It uses the local endpoint, the application's generated local token, and these protocol settings:

```toml
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false
```

Existing providers and unrelated settings are preserved; `auth.json` is unchanged. The default direct connection does not read or modify the CC Switch database and does not stop CC Switch. If a backup from this application's older integration exists, its own changes are checked and restored before a new backup is created.

Already-open Codex conversations may need to be reopened or reloaded once after initial connection. Send a real request and confirm that it appears in the dashboard. After that, source selection, candidate synchronization, routing policies, and model/reasoning overrides take effect inside the proxy without restarting Codex. In-flight requests retain their selected account and model.

Repeating connection with the same endpoint and token is idempotent. Later edits to managed fields trigger conflict checks instead of being silently overwritten.

## Account source and routing policy

The `source_mode` configuration field determines where accounts come from. The routing policy determines which account handles a request. These are independent settings.

| Account source | Behavior |
| --- | --- |
| `biwrite`, default | Reads BiWrite's local `settings.json` and `keys.json` every 1.5 seconds without copying keys manually. |
| `manual` | Uses only the keys, models, labels, and concurrency limits entered in the dashboard. |

By default, the automatic source imports only `keptKeys` candidates from Responses providers at the selected upstream, excluding ignored or stopped keys. The dashboard’s continuous all-key sync action immediately selects the BiWrite source and enables `biwrite_import_all`. Subsequent automatic reads keep including all keys at that upstream, including keys outside the keep-alive set and keys marked ignored or stopped. This does not create a manual pool. You can switch back to keep-alive candidates at any time. The public default is `false`; imported accounts remain unverified until an actual request provides evidence. On macOS, the default BiWrite directory is `~/Library/Application Support/app.biwrite.desktop`. The adapter requires `keysInFile` to be enabled. It does not read the system keychain or modify BiWrite files.

The dashboard reads key labels from BiWrite’s `keyNames` for account cards and account references in request records. If no usable label exists, it displays an anonymous account identifier. Saved keys are never returned for display.

The source synchronization summary includes every configured model at the selected upstream and its total, keep-alive candidate, and imported key counts, including zeros. In keep-alive-only mode, a model such as Sol with an empty `keptKeys` set is reported as configured with no keep-alive candidates and is not added to schedulable accounts. All-key mode does not depend on the keep-alive candidate count, but import does not establish current availability. Historical success is never treated as real-time health.

The pool view can be filtered by full model ID. Cards retain key labels and show the selected model’s health observations and concurrency. Filtering changes only the view, leaving routing policies, model overrides, and account configuration unchanged.

`keptKeys` is a keep-alive and re-entry candidate set, not an exact assertion that an account is currently admitted. `keyThrough` records historical success and may be persisted only at hourly intervals. The proxy tracks its own observed success, failure, cooldown, and concurrency separately. During normal automatic synchronization, a failed read briefly retains the previous snapshot. Once the last successful read is more than 30 seconds old, new assignments stop until the files become readable again.

The pause control sets `sync_paused = true` and stops only scheduled BiWrite reads. The current pool remains schedulable and does not expire merely because synchronization was deliberately paused. Every application startup reads the current source once to initialize the pool, including when a saved pause is active; subsequent periodic synchronization respects the pause setting. A single sync action reads the current scope once without resuming automatic reads. The resume control restarts scheduled reads. Pause and all-key scope are saved independently; both default to `false`.

The application has no built-in provider address. `upstream_base_url` defaults to `null`, which discovers the unique HTTPS Responses upstream in BiWrite when using the automatic source. If multiple addresses exist, you must specify the desired Base URL in the dashboard; the application does not choose one arbitrarily. An explicit address restricts imports to that upstream. When switching from the automatic source to manual mode without an explicit address, the most recently resolved upstream is saved to `upstream_base_url`. Manual mode then retains its upstream selection if BiWrite is closed or its directory moves. If no previously resolved address is available, an explicit address is required. Starting directly in manual mode requires an explicit `upstream_base_url`; manual mode does not follow changes in BiWrite and rejects forwarding when this address is absent. Only HTTPS public domain names on the default port are accepted, without user information, query parameters, fragments, or other paths. Root and `/v1` paths are normalized to `/v1`.

For manual mode, add accounts before saving the source change. Manual keys are sent to the same selected upstream. Editing the manual list while the automatic source is active does not change the BiWrite pool. Switching to an empty manual pool is rejected. Adding the same key again merges its models; saved keys are not returned in the account list. Manual keys are kept in the private local `manual-keys.json`; automatic-source keys are read into memory only.

| Routing policy | Behavior |
| --- | --- |
| `sticky`, default | Tries to keep a conversation on its previous account. Requests that permit switching may use another candidate if that account becomes unavailable. |
| `random` | Randomly selects an eligible candidate with capacity. |
| `round_robin` | Rotates through eligible candidates with capacity. |
| `pinned` | Uses only the selected account; failures do not trigger a switch to another key. |

An automatically imported BiWrite account can therefore be pinned, while a manually maintained pool can use automatic random or round-robin scheduling.

Concurrency and health are tracked per **key + model**. An Astra failure does not mark Sol as failed for the same key. The default is one in-flight proxy request per key and model, also constrained by the model limit stored in the source. An optional account-wide limit can additionally cap concurrency across models. BiWrite keep-alives and requests from other clients are outside this proxy's counters, so combined upstream traffic can still hit an account limit.

Model and reasoning effort follow Codex by default. Dashboard overrides affect subsequent pool requests only. Requests containing `previous_response_id` preserve the original account and model association and cannot be moved across accounts or overridden to another model.

The account probe sends a fixed `Reply only OK.` request with `low` reasoning effort and no user conversation. Its dialog retains status, timing, requested and declared models, the fixed request, actual visible response text, safe error classifications, usage, and event details. Successful response text can be inspected directly; the dashboard does not invent an “OK” when no visible text was returned. Each account’s latest probe can be reopened from its log action. Results remain in the current page’s memory, disappear on refresh, and are never written to persistent browser storage. A successful probe establishes only that this request succeeded; text and events may be truncated at their display limits.

## Optional CC Switch monitoring

If you prefer changing keys in CC Switch, expand the dashboard's CC Switch compatibility section and enable legacy monitoring. The action changes the upstream URL of **CC Switch's currently selected Codex provider** to the local endpoint and disables WebSocket support for that provider. It does not change its key, model, reasoning effort, or service tier, and does not select a different CC Switch provider.

Continue changing keys in CC Switch afterward. There is no need to copy those keys into Pool Router's manual store. If you select a different CC Switch provider that does not point to this local endpoint, its traffic will not automatically appear in the dashboard.

| Request path | Authentication credential | Behavior |
| --- | --- | --- |
| Codex → Pool Router → upstream provider | The application's local token | `route_mode = "pool"`; uses the selected account source and routing policy. |
| Codex → CC Switch → Pool Router → upstream provider | A supported upstream `sk-` key forwarded by CC Switch | `route_mode = "legacy"`; uses only the key supplied with that request. |

The legacy path uses only the original provider address stored in the private backup created during compatibility integration. Forwarding is rejected if integration is incomplete or the backup is invalid; the application does not guess the destination for the caller’s key. It never selects or falls back to the stored pool. It does not apply dashboard model or reasoning overrides, does not modify the requested `service_tier`, and does not automatically retry or change keys. The upstream request body is forwarded as received, with monitoring parsing kept separate from forwarding. Session recovery, compaction repair, and image compression do not apply to this path.

The integration actions are `POST /api/legacy/connect` and `POST /api/legacy/restore`. Both management endpoints require the local token. Compatibility integration has a separate recovery backup. Restoration reverts only the URL and transport fields managed by this application, preserving keys subsequently changed in CC Switch and other unrelated settings.

`legacy_monitor_enabled` defaults to `true`. It permits requests carrying an upstream key; it does not change the default `biwrite` source or the pool behavior of local-token requests. Disabling it rejects compatibility-key requests while leaving the local-token path available.

Legacy monitoring does not consume pool scheduler slots. If both paths use the same key at once, account for the upstream total concurrency limit. After returning to the original provider, Direct connect can reapply the pool connection while preserving the first backup and later changes to unmanaged settings.

## Protocol, retries, and context

The Rust version supports HTTP/SSE on `POST /v1/responses`, plus `POST /v1/responses/compact` and local `GET /v1/models`. Responses WebSocket, Chat Completions, and arbitrary path forwarding are not supported. Set `supports_websockets = false` in the client. The upstream address is discovered from BiWrite or explicitly selected in routing settings.

The pool path checks both HTTP status and Responses/SSE events. A failure event inside HTTP 200 is not counted as success. Before meaningful output, an explicit retryable rejection or a confirmed failure to establish a connection can select another candidate. Once text, reasoning, or tool-call output appears, the request is not automatically replayed. Transport interruptions with an uncertain outcome are also not retried transparently.

The pool defaults to at most **15 retries**, or **16 attempts including the first request**. The backend setting `max_attempts = 16` includes the first request; the dashboard displays and edits the retry count. Requests created by session recovery or compaction repair count against the same limit. Queueing and the pre-output phase share a 600-second deadline. The upstream response-header timeout is 120 seconds, the first meaningful output deadline is 120 seconds from the start of each attempt, and the stream idle timeout is 180 seconds. The time deadline can therefore expire before all retries are used. Legacy monitoring submits each upstream request once.

The default `first_output_timeout_ms` is `120000`. If a pool request has no `previous_response_id` and has produced no meaningful content, reaching that deadline can release the current attempt and schedule another. SSE heartbeats and status-only events do not count as meaningful content. Once text, reasoning, or tool-call output appears, the request is not automatically replayed. A timeout does not prove that the upstream never executed the request, and more retries do not guarantee success or eliminate an upstream-wide outage.

When a pool client sends `Accept: text/event-stream`, the proxy immediately emits an SSE waiting comment and sends a heartbeat every five seconds while waiting. Comments contain no response ID or model output, so pre-output failover remains possible. Errors after response headers have been sent are returned as SSE `error` events; records retain the actual processing status. Disconnecting the client cancels unfinished waiting and forwarding.

A pool request containing `previous_response_id` must use its recorded original account and model. Each response ID also binds the upstream session mapping actually used for that response, or its original unmodified state. Continuations preserve that state instead of applying a mapping subsequently rebuilt for the same scope. Unknown, expired, or unavailable bindings return an explicit error. Bindings are held in memory and expire after 24 hours by default. Restarting the proxy loses them, so the client may need to resend context. Legacy monitoring forwards the client's continuation fields unchanged and does not substitute an account using pool bindings.

Session recovery is controlled by `session_recovery_enabled` and is enabled by default. Before meaningful output, persistent 429 responses or a 502 explicitly identifying an unavailable bound resource can trigger an upstream session identifier reset for a request without `previous_response_id`, at most once per request. The replacement is scoped to the original conversation, account, model, and upstream, and later matching requests without `previous_response_id` can reuse it. This does not reconstruct omitted history or make `previous_response_id` portable across accounts. A generic 502 does not trigger this recovery; whether a compatible upstream accepts a replacement identifier depends on its implementation.

Compaction repair is controlled by `compaction_repair_enabled` and is enabled by default. Only an HTTP 400 from `/v1/responses/compact` that explicitly identifies incompatible, invalid, or undecryptable encrypted content permits removal of the affected opaque encrypted reasoning or compaction items and one repair retry. Available plaintext, messages, and tool-call relationships are preserved. Ordinary 400 errors and successful responses do not trigger removal. Removing encrypted items can lose context available only through those items, so the client still needs to supply the history required to continue. This is bounded handling of compatible upstream failures, not an official protocol guarantee.

Image compression is controlled by `compress_images` and is disabled by default. When enabled, it processes only inline Base64 PNG/JPEG images in request messages and does not download external image URLs. Eligible images may be reduced to a 2048-pixel longest edge and converted to lossy JPEG, with transparent areas composited onto white. A replacement is used only when it reduces the payload sufficiently. Small images, unsupported formats, animations, images exceeding processing budgets, and decoding failures remain unchanged. Compression can affect small text, fine lines, and other details; keep it disabled when those details must be preserved.

A shared upstream URL does not establish that accounts share server-side conversation state or prompt caches. The amount of context sent depends on Codex and its protocol. Cache hits and transmitting a request body are separate matters; the proxy does not reconstruct omitted history.

## Monitoring and local data

Only requests **passing through the local endpoint** are monitored. This is not system-wide packet capture or HTTPS interception. The dashboard keeps the latest 1,000 allowlisted metadata records by default. Recording can be paused while forwarding continues.

Records also include `gateway_actions`, the actual request body size sent in the last upstream attempt as `upstream_request_bytes`, and the wait for meaningful model output as `first_output_ms`. The dashboard distinguishes session resets and reuse, compaction repair, image compression, and timeout retries. It never substitutes SSE heartbeats, first-byte timing, or total request duration for meaningful output timing; missing values remain unknown.

Records include `route_mode`, an anonymous account identifier, recognizable conversation UUIDs, requested and server-declared model/reasoning values, request and response `service_tier`, status and failure classifications, attempt count, timing, byte counts, and upstream-reported token, cache, and reasoning-token usage. The effective model and reasoning effort after pool overrides are shown separately. The dashboard maps anonymous account identifiers to key labels from the current source. Missing upstream fields remain unknown rather than being inferred as confirmed values.

The dashboard groups `fast` and `priority` together for service-tier viewing while retaining the original request and response values. These fields describe request parameters or server declarations; they do not prove the actual backend model, execution tier, or final billing.

Ordinary gateway logs retain only allowlisted metadata and bounded, redacted per-attempt error messages in `AttemptRecord.error_message`. They do not store complete requests or successful response bodies. Error summaries explain individual failures; missing messages remain unknown, and complete upstream error responses are not stored. Summaries contain upstream diagnostic text and should still be reviewed before sharing. Limited text from fixed probes is used only in the current page’s probe details, separately from ordinary gateway logs. BiWrite keys are read into memory, manual keys use a private file, and a compatibility key is used only to forward its request. Private configuration contains the local access token, and recovery backups may contain credentials from previous configuration. These files use `0600` permissions and should not be included in source archives, screenshots, or diagnostic bundles.

The service binds only to loopback and checks Host and browser Origin. Management endpoints always require the local token. Compatibility upstream keys are accepted only by model-forwarding endpoints and cannot authorize administration.

## Maintenance, restoration, and removal

Inspect sanitized status or reopen the dashboard:

```bash
"$HOME/Library/Application Support/Pool Router/bin/pool-router" status
"$HOME/Library/Application Support/Pool Router/bin/pool-router" open
```

Rerun the installer to update. Restarting the proxy can affect in-flight requests and in-memory continuation bindings, so update after current requests finish. Ordinary dashboard changes do not require restarting the service or Codex.

If Codex or CC Switch has been connected, restore the connection before disabling or removing the service:

```bash
"$HOME/Library/Application Support/Pool Router/bin/pool-router" restore
```

This checks and restores pending direct and compatibility-monitor integrations managed by the application. To restore only CC Switch monitoring, use its dashboard restore button. Unrelated later edits are preserved; conflicting changes to managed fields stop restoration and retain the backup. You may need to reopen Codex conversations afterward and verify the restored request path.

Disable the macOS background service without removing files:

```bash
launchctl bootout "gui/$(id -u)/local.pool-router"
```

Start it again:

```bash
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/local.pool-router.plist"
```

Remove the background service and launcher:

```bash
bash "$HOME/Library/Application Support/Pool Router/uninstall.sh"
```

Uninstallation does not stop CC Switch or other wiretap services. Configuration, manual keys, logs, and archived recovery backups are retained by default. After verifying restoration and deciding the data is no longer needed, you can remove this application's data directory manually. Do not delete the executable while clients still depend on its local endpoint.

## Historical version and development

The earlier Python implementation is preserved in [`legacy/python/`](legacy/python/) as historical source and a feature reference. Features supported there, including WebSocket handling, are not automatically available in the Rust version and are not enabled by its installer.

Local checks use temporary fixtures and localhost mocks without real upstream keys. Frontend tests require Node.js 22 or later.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
node --test web/app.test.cjs
bash -n scripts/macos-install.sh scripts/macos-uninstall.sh
```

CI runs checks on Linux and macOS without changing real Codex connection settings. These checks do not replace validation against an actual provider.

The following official documentation explains client configuration and protocol concepts. It does not establish that an upstream provider behaves identically to the official service:

- [Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference)
- [Responses WebSocket mode](https://developers.openai.com/api/docs/guides/websocket-mode)
- [Streaming Responses](https://developers.openai.com/api/docs/guides/streaming-responses)
- [Compaction](https://developers.openai.com/api/docs/guides/compaction)
- [Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)

This project is licensed under the [MIT License](LICENSE).
