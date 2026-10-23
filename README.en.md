# Codex Wiretap (Rust)

[简体中文](README.md) | **English**

The Rust edition of Codex Wiretap provides local account pool routing and request monitoring. By default, it reads Anyrouter candidates from BiWrite and connects Codex directly to `http://127.0.0.1:10813/v1`. It schedules requests by account and model, and displays request status and usage in a local dashboard. The default workflow requires BiWrite and this application; CC Switch is not a dependency.

The desktop launcher is named **Pool Router**, and the executable is **`pool-router`**. On macOS, the launcher opens a browser dashboard while a local background service handles requests. An optional CC Switch monitoring path is also available for users who continue to manage their keys in CC Switch.

[Download a prebuilt release](https://github.com/qzkinhit/codex-wiretap/releases/latest) · [Historical Python implementation](legacy/python/)

![Example Codex Wiretap dashboard](docs/dashboard.png)

The screenshot uses simulated data and contains no real accounts, keys, or conversations.

## Installation and startup

### Prebuilt release

Download a package matching your operating system and processor architecture from the [latest release](https://github.com/qzkinhit/codex-wiretap/releases/latest). After extracting a macOS installation package, run this from its root directory:

```bash
bash scripts/macos-install.sh --no-build
```

This uses the included `target/release/pool-router` binary without compiling it locally. The installer requires `python3` to generate macOS plist files. It installs a user background service and `~/Applications/Pool Router.app`, then opens the dashboard. Installation itself does not change Codex or CC Switch connection settings.

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

1. Configure Anyrouter Responses providers in BiWrite and run keep-alive for the models you need. Enable local key storage so that keys are available in `keys.json`.
2. Start Pool Router and confirm that the expected model candidates appear. Optional account/model probes send real upstream requests and may consume quota.
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

The automatic source imports only Anyrouter Responses `keptKeys` candidates, excluding keys ignored or stopped under the corresponding provider. On macOS, the default BiWrite directory is `~/Library/Application Support/app.biwrite.desktop`. The adapter requires `keysInFile` to be enabled. It does not read the system keychain or modify BiWrite files.

`keptKeys` is a keep-alive and re-entry candidate set, not an exact assertion that an account is currently admitted. `keyThrough` records historical success and may be persisted only at hourly intervals. The proxy tracks its own observed success, failure, cooldown, and concurrency separately. If reading fails, it briefly retains the previous snapshot. Once the last successful read is more than 30 seconds old, it stops assigning new requests from that snapshot until the files become readable again.

For manual mode, add accounts before saving the source change. Editing the manual list while the automatic source is active does not change the BiWrite pool. Switching to an empty manual pool is rejected. Adding the same key again merges its models; saved keys are not returned in the account list. Manual keys are kept in the private local `manual-keys.json`; automatic-source keys are read into memory only.

| Routing policy | Behavior |
| --- | --- |
| `sticky`, default | Tries to keep a conversation on its previous account. Requests that permit switching may use another candidate if that account becomes unavailable. |
| `random` | Randomly selects an eligible candidate with capacity. |
| `round_robin` | Rotates through eligible candidates with capacity. |
| `pinned` | Uses only the selected account; failures do not trigger a switch to another key. |

An automatically imported BiWrite account can therefore be pinned, while a manually maintained pool can use automatic random or round-robin scheduling.

Concurrency and health are tracked per **key + model**. An Astra failure does not mark Sol as failed for the same key. The default is one in-flight proxy request per key and model, also constrained by the model limit stored in the source. An optional account-wide limit can additionally cap concurrency across models. BiWrite keep-alives and requests from other clients are outside this proxy's counters, so combined upstream traffic can still hit an account limit.

Model and reasoning effort follow Codex by default. Dashboard overrides affect subsequent pool requests only. Requests containing `previous_response_id` preserve the original account and model association and cannot be moved across accounts or overridden to another model.

## Optional CC Switch monitoring

If you prefer changing keys in CC Switch, expand the dashboard's CC Switch compatibility section and enable legacy monitoring. The action changes the Anyrouter URL of **CC Switch's currently selected Codex provider** to the local endpoint and disables WebSocket support for that provider. It does not change its key, model, reasoning effort, or service tier, and does not select a different CC Switch provider.

Continue changing keys in CC Switch afterward. There is no need to copy those keys into Pool Router's manual store. If you select a different CC Switch provider that does not point to this local endpoint, its traffic will not automatically appear in the dashboard.

| Request path | Authentication credential | Behavior |
| --- | --- | --- |
| Codex → Pool Router → Anyrouter | The application's local token | `route_mode = "pool"`; uses the selected account source and routing policy. |
| Codex → CC Switch → Pool Router → Anyrouter | A supported upstream `sk-` key forwarded by CC Switch | `route_mode = "legacy"`; uses only the key supplied with that request. |

The legacy path never selects or falls back to the stored pool. It does not apply dashboard model or reasoning overrides, does not modify the requested `service_tier`, and does not automatically retry or change keys. The upstream request body is forwarded as received, with monitoring parsing kept separate from forwarding.

The integration actions are `POST /api/legacy/connect` and `POST /api/legacy/restore`. Both management endpoints require the local token. Compatibility integration has a separate recovery backup. Restoration reverts only the URL and transport fields managed by this application, preserving keys subsequently changed in CC Switch and other unrelated settings.

`legacy_monitor_enabled` defaults to `true`. It permits requests carrying an upstream key; it does not change the default `biwrite` source or the pool behavior of local-token requests. Disabling it rejects compatibility-key requests while leaving the local-token path available.

## Protocol, retries, and context

The Rust version supports HTTP/SSE on `POST /v1/responses`, plus `POST /v1/responses/compact` and local `GET /v1/models`. Responses WebSocket, Chat Completions, and arbitrary path forwarding are not supported. Set `supports_websockets = false` in the client. The upstream is fixed to `https://anyrouter.top/v1`.

The pool path checks both HTTP status and Responses/SSE events. A failure event inside HTTP 200 is not counted as success. Before meaningful output, an explicit retryable rejection or a confirmed failure to establish a connection can select another candidate. Once text, reasoning, or tool-call output appears, the request is not automatically replayed. Transport interruptions with an uncertain outcome are also not retried transparently.

The pool defaults to at most 20 attempts, a shared 120-second deadline for queueing and waiting for meaningful output, a 45-second upstream response-header timeout, and a 180-second stream idle timeout. More attempts do not guarantee success or eliminate an upstream-wide outage. Legacy monitoring submits each upstream request once.

When a pool client sends `Accept: text/event-stream`, the proxy immediately emits an SSE waiting comment and sends a heartbeat every five seconds while waiting. Comments contain no response ID or model output, so pre-output failover remains possible. Errors after response headers have been sent are returned as SSE `error` events; records retain the actual processing status. Disconnecting the client cancels unfinished waiting and forwarding.

A pool request containing `previous_response_id` must use its recorded original account and model. Unknown, expired, or unavailable bindings return an explicit error. Bindings are held in memory and expire after 24 hours by default. Restarting the proxy loses them, so the client may need to resend context. Legacy monitoring forwards the client's continuation fields unchanged and does not substitute an account using pool bindings.

A shared Anyrouter URL does not establish that accounts share server-side conversation state or prompt caches. The amount of context sent depends on Codex and its protocol. Cache hits and transmitting a request body are separate matters; the proxy does not reconstruct omitted history.

## Monitoring and local data

Only requests **passing through the local endpoint** are monitored. This is not system-wide packet capture or HTTPS interception. The dashboard keeps the latest 1,000 allowlisted metadata records by default. Recording can be paused while forwarding continues.

Records include `route_mode`, an anonymous account identifier, recognizable conversation UUIDs, requested and server-declared model/reasoning values, request and response `service_tier`, status and failure classifications, attempt count, timing, byte counts, and upstream-reported token, cache, and reasoning-token usage. The effective model and reasoning effort after pool overrides are shown separately. Missing upstream fields remain unknown rather than being inferred as confirmed values.

The dashboard groups `fast` and `priority` together for service-tier viewing while retaining the original request and response values. These fields describe request parameters or server declarations; they do not prove the actual backend model, execution tier, or final billing.

Monitoring does not store keys, prompts, response bodies, or arbitrary upstream error messages. BiWrite keys are read into memory, manual keys use a private file, and a compatibility key is used only to forward its request. Private configuration contains the local access token, and recovery backups may contain credentials from previous configuration. These files use `0600` permissions and should not be included in source archives, screenshots, or diagnostic bundles.

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

Local checks use temporary fixtures and localhost mocks without real Anyrouter keys. Frontend tests require Node.js 22 or later.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
node --test web/app.test.cjs
bash -n scripts/macos-install.sh scripts/macos-uninstall.sh
```

CI runs checks on Linux and macOS without changing real Codex connection settings. These checks do not replace validation against an actual provider.

The following official documentation explains client configuration and protocol concepts. It does not establish that Anyrouter behaves identically to the official service:

- [Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference)
- [Responses WebSocket mode](https://developers.openai.com/api/docs/guides/websocket-mode)
- [Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)

This project is licensed under the [MIT License](LICENSE).

Legacy monitoring does not consume pool scheduler slots. If both paths use the same key at once, account for the upstream total concurrency limit. After returning to the original provider, Direct connect can reapply the pool connection while preserving the first backup and later changes to unmanaged settings.
