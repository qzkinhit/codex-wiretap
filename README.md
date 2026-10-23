# Codex Wiretap (Rust)

**简体中文** | [English](README.en.md)

Codex Wiretap 的 Rust 版本提供本地号池调度和请求监控。它默认只读同步 BiWrite 的 Anyrouter 候选，让 Codex 直接连接 `http://127.0.0.1:10813/v1`，按账号与模型选择可用容量，并在本地面板中查看请求状态和用量。日常默认流程只需要 BiWrite 与本软件，不依赖 CC Switch。

桌面启动器名称为 **Pool Router**，命令行程序为 **`pool-router`**。macOS 启动器打开浏览器面板，实际转发由本机后台服务完成。项目同时保留可选的 CC Switch 兼容监控路径，供仍在 CC Switch 中维护 Key 的用户使用。

[下载预编译版本](https://github.com/qzkinhit/codex-wiretap/releases/latest) · [历史 Python 实现](legacy/python/)

![Codex Wiretap 本地控制台示例](docs/dashboard.png)

图中为模拟数据，不包含真实账号、密钥或对话。

## 安装与启动

### 预编译版本

从 [最新 Release](https://github.com/qzkinhit/codex-wiretap/releases/latest) 下载匹配系统和处理器架构的压缩包。macOS 安装包解压后，在包根目录执行：

```bash
bash scripts/macos-install.sh --no-build
```

此命令使用包内 `target/release/pool-router`，不需要现场编译。安装脚本需要 `python3` 来生成 macOS plist，会安装当前用户的后台服务和 `~/Applications/Pool Router.app`，然后打开面板。安装本身不会修改 Codex 或 CC Switch 的接入配置。

### 从源码构建

需要当前稳定版 Rust、Cargo 和 C 编译工具链。macOS 可直接构建并安装：

```bash
git clone https://github.com/qzkinhit/codex-wiretap.git
cd codex-wiretap
bash scripts/macos-install.sh
```

安装选项包括 `--no-build`、`--no-open` 和 `--no-start`。其中 `--no-start` 只安装文件，不启动服务或打开面板。重新安装会重启本软件自己的后台服务。

也可以只运行命令行程序，不安装后台服务：

```bash
cargo build --release --locked
./target/release/pool-router init
./target/release/pool-router serve
```

保持上述服务运行，在另一终端打开面板：

```bash
./target/release/pool-router open
```

所有命令支持 `--config /absolute/path/config.json`。`open` 从私有配置读取本地令牌，通过 URL fragment 交给面板，不在命令输出中打印令牌。前台运行时使用 `Ctrl+C` 停止服务。

### macOS 安装位置

| 内容 | 位置 |
| --- | --- |
| 程序 | `~/Library/Application Support/Pool Router/bin/pool-router` |
| 配置 | `~/Library/Application Support/Pool Router/config.json` |
| 手动号池 | `~/Library/Application Support/Pool Router/manual-keys.json` |
| 请求元数据 | `~/Library/Application Support/Pool Router/records.jsonl` |
| 服务日志 | `~/Library/Application Support/Pool Router/logs/service.log` |
| 启动器 | `~/Applications/Pool Router.app` |
| 后台服务 | `~/Library/LaunchAgents/local.pool-router.plist` |

其他系统可使用命令行构建与运行方式；当前仓库提供的后台服务安装脚本仅适用于 macOS。

## 默认流程：BiWrite 与 Codex 直连

1. 在 BiWrite 中配置 Anyrouter 的 Responses 供应商，并对所需模型运行保活。启用本地密钥文件模式，使密钥存于 `keys.json`。
2. 启动 Pool Router，确认面板显示了预期模型的候选。可按需运行账号与模型探测；探测是真实上游请求，可能消耗额度。
3. 在面板执行 Codex 直连接入，或运行：

```bash
"$HOME/Library/Application Support/Pool Router/bin/pool-router" connect
```

接入会备份 `~/.codex/config.toml`，新增独立的 `pool_router` 供应商，并切换 Codex 的活动供应商。它使用本地入口、本软件生成的本地令牌，以及以下协议设置：

```toml
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false
```

原供应商和其他配置保留，`auth.json` 不改动。默认直连接入不读取或修改 CC Switch 数据库，也不会停止 CC Switch。存在本软件旧版接入备份时，程序会先检查并恢复自己的旧改动，再建立新备份。

首次接入后，已经打开的 Codex 会话可能需要重新打开或重载一次。请发出真实请求并确认面板出现记录。此后，来源切换、候选同步、路由策略及模型和推理强度覆盖在代理内生效，不需要为这些变化重启 Codex。在途请求继续使用已选中的账号和模型。

对相同地址和令牌重复接入保持幂等；接入后被用户修改的受管字段会触发冲突检查，不会被静默覆盖。

## 号池来源与调度方式

配置字段 `source_mode` 决定账号从哪里来，调度方式决定一次请求使用哪个账号。这两项独立设置。

| 号池来源 | 行为 |
| --- | --- |
| `biwrite`，默认 | 每 1.5 秒只读同步 BiWrite 的本地 `settings.json` 和 `keys.json`，无需复制 Key。 |
| `manual` | 只使用用户在面板中添加的 Key、模型、昵称和并发配置。 |

自动来源只导入 Anyrouter Responses 的 `keptKeys` 候选，并排除对应供应商下被忽略或停止的密钥。macOS 默认 BiWrite 目录为 `~/Library/Application Support/app.biwrite.desktop`。该适配器要求 `keysInFile` 开启，不读取系统钥匙串，也不修改 BiWrite 文件。

`keptKeys` 表示保活及等待重新进入的候选集合，并不等同于此刻严格“已挤成功”的状态。`keyThrough` 是历史成功时间，可能按小时粒度持久化。代理实际观察到的成功、失败、冷却和并发另行记录。读取失败时短暂保留此前快照；最后一次成功读取超过 30 秒后，停止用该旧快照分配新请求，待文件恢复后继续同步。

手动来源可先添加账号，再保存来源切换。自动来源下维护手动列表不会改变当前 BiWrite 号池；手动池为空时会拒绝切换。相同 Key 重复添加会合并模型，已保存的 Key 不在列表中回显。手动密钥存于本机私有 `manual-keys.json`，自动来源密钥只读入内存。

| 调度方式 | 行为 |
| --- | --- |
| `sticky`，默认 | 尽量保持同一会话使用原账号；允许切换的请求可在原账号不可用时选择其他候选。 |
| `random` | 从当前可调度候选中随机选择。 |
| `round_robin` | 在当前可调度候选间轮流选择。 |
| `pinned` | 只使用指定账号，不因失败而自动换号。 |

因此，BiWrite 自动导入的账号可以手动指定，手动维护的多个账号也可以自动随机或轮询调度。

并发与健康状态以 **Key + 模型** 为单位。同一个 Key 的 Astra 失败不会把 Sol 一并标为失败。默认每个 Key、每个模型最多有 1 个本代理请求在途，并结合来源中保存的模型并发上限。可额外设置跨模型的账号总并发上限。BiWrite 保活与其他客户端的请求不计入本代理的并发数，仍可能与本代理请求一起触发上游限制。

模型和推理强度默认跟随 Codex。面板覆盖只作用于后续号池请求；携带 `previous_response_id` 的请求保留原账号与模型关联，不能跨模型覆盖或跨账号续接。

## 可选流程：兼容 CC Switch 监控

仍希望在 CC Switch 中换 Key 时，可展开面板的“兼容 CC Switch 监控”，执行“接入旧方式监控”。这会将 **CC Switch 当前 Codex 供应商** 的 Anyrouter 地址改为本地入口，并关闭该供应商的 WebSocket 支持。它不修改 Key、模型、推理强度或服务档位，也不切换 CC Switch 的当前供应商。

后续仍在 CC Switch 修改 Key，不需要把 Key 添加到 Pool Router 的手动池。若切换到另一个尚未接入本地入口的 CC Switch 供应商，它的请求不会自动出现在监控中。

| 请求路径 | 认证凭证 | 处理方式 |
| --- | --- | --- |
| Codex → Pool Router → Anyrouter | 本软件的本地令牌 | `route_mode = "pool"`，使用当前号池来源和调度策略。 |
| Codex → CC Switch → Pool Router → Anyrouter | CC Switch 转发的受支持 `sk-` 上游 Key | `route_mode = "legacy"`，只使用该请求携带的 Key。 |

`legacy` 路径不会选择或回退到已存号池，不应用面板中的模型或推理强度覆盖，不修改请求的 `service_tier`，也不自动重试或更换 Key。上游请求体按收到的字节转发，监控解析与转发分离。

兼容接入与恢复分别对应 `POST /api/legacy/connect` 和 `POST /api/legacy/restore`，两个管理接口都要求本地令牌认证。兼容接入有独立恢复备份；恢复只还原本软件修改的地址和协议字段，保留接入后在 CC Switch 更换的新 Key 以及其他非受管设置。

`legacy_monitor_enabled` 默认为 `true`，只表示允许这类传入 Key 的监控请求，不会改变默认 `biwrite` 来源或本地令牌请求的号池调度。关闭后会拒绝兼容 Key 请求，本地令牌路径继续工作。

## 协议、重试与上下文

当前 Rust 版本支持 `POST /v1/responses` 的 HTTP/SSE，以及 `POST /v1/responses/compact`；另有本地 `GET /v1/models`。不支持 Responses WebSocket、Chat Completions 或任意路径转发，客户端需设置 `supports_websockets = false`。上游固定为 `https://anyrouter.top/v1`。

号池路径同时检查 HTTP 状态与 Responses/SSE 事件。HTTP 200 中的失败事件不会计为成功。有效内容输出前，明确可重试的拒绝或已确认未建立连接的失败可以更换候选；输出文本、推理或工具调用之后不自动重放。结果不确定的传输中断也不透明重试。

号池默认最多尝试 20 次，排队和等待有效输出的前导阶段共用 120 秒期限，单次上游响应头等待上限为 45 秒，流式空闲上限为 180 秒。重试次数不保证成功，也不能消除上游整体故障。兼容监控路径始终只提交一次上游请求。

号池客户端发送 `Accept: text/event-stream` 时，代理立即发出 SSE 等待注释，并在等待期间每 5 秒发送心跳。注释不含响应 ID 或模型内容，不阻止前导失败时换号。响应头发出后的错误通过 SSE `error` 事件返回；记录仍保留实际处理状态。客户端断开会取消未完成的等待和转发。

携带 `previous_response_id` 的号池请求必须使用已记录的原账号与模型。绑定未知、过期或不可用时会明确返回错误。绑定保存在内存中，默认有效期为 24 小时；重启代理会丢失绑定，客户端可能需要重新发送上下文。兼容监控路径按原样转发客户端的续接字段，不利用号池绑定替换账号。

同一 Anyrouter 地址不证明不同账号共享服务端会话或提示词缓存。每次请求发送多少上下文取决于 Codex 和协议。缓存命中与网络上是否发送请求体是不同问题，代理不会自动补齐被省略的历史。

## 监控记录与数据边界

只监控**经过本地入口**的请求，不读取其他进程的 HTTPS 流量，也不执行系统级抓包或中间人解密。默认保存最近 1000 条白名单元数据，可暂停记录而继续转发。

记录包括路由类型 `route_mode`、匿名账号标识、可识别的会话 UUID、请求与服务器声明的模型和推理强度、请求与响应的 `service_tier`、状态与失败分类、重试次数、耗时、字节数，以及上游报告的 token、缓存和推理 token 用量。号池覆盖后的实际发送模型与推理强度单独显示。没有报告的字段保持未知，不推算为已确认信息。

面板将 `fast` 与 `priority` 放在同一服务档位组中方便查看，同时保留请求和响应的原始值。这些字段只说明请求参数或服务器声明，不能证明实际后端模型、服务档位执行情况或最终计费。

监控不保存 Key、提示词、回复正文或任意上游错误消息。BiWrite 密钥只读入内存；手动密钥存于私有文件；兼容请求的 Key 只用于该请求转发。私有配置保存本地访问令牌，接入恢复备份也可能包含原配置中的凭证。这些文件使用 `0600` 权限，不应随源码、截图或诊断包分享。

服务仅监听回环地址，并检查 Host 和浏览器 Origin。管理接口始终要求本地令牌。兼容的真实 Key 只被模型转发接口接受，不能作为管理令牌使用。

## 维护、恢复与卸载

查看脱敏状态或重新打开面板：

```bash
"$HOME/Library/Application Support/Pool Router/bin/pool-router" status
"$HOME/Library/Application Support/Pool Router/bin/pool-router" open
```

更新程序时重新运行安装脚本。重启代理可能影响在途请求和内存中的续接绑定，宜在当前请求结束后更新。一般的面板设置变化不需要重启服务或 Codex。

已接入 Codex 或 CC Switch 时，停用或卸载前先恢复接入：

```bash
"$HOME/Library/Application Support/Pool Router/bin/pool-router" restore
```

该命令检查并恢复本软件待恢复的直连接入和兼容监控接入。只恢复 CC Switch 监控时，也可使用面板对应恢复按钮。恢复保留无关的后续修改；受管字段发生冲突时拒绝覆盖并保留备份。恢复后可能需要重新打开 Codex 会话，确认请求已采用恢复后的入口。

仅停用 macOS 后台服务，不卸载文件：

```bash
launchctl bootout "gui/$(id -u)/local.pool-router"
```

重新启用：

```bash
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/local.pool-router.plist"
```

卸载后台服务与启动器：

```bash
bash "$HOME/Library/Application Support/Pool Router/uninstall.sh"
```

卸载不停止 CC Switch 或其他 wiretap 服务。配置、手动密钥、日志和归档恢复备份默认保留；确认恢复完成且不再需要数据后，可手动删除本软件的数据目录。不要在仍有客户端指向本地入口时直接删除程序。

## 历史版本与开发

此前的 Python 实现保存在 [`legacy/python/`](legacy/python/)，用于保留历史源码和功能参考。旧版支持的 WebSocket 等能力不代表当前 Rust 版本已实现，也不会由 Rust 安装器自动启用。

本地验证使用临时夹具和 localhost mock，不需要真实 Anyrouter Key。前端测试需要 Node.js 22 或更新版本。

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
node --test web/app.test.cjs
bash -n scripts/macos-install.sh scripts/macos-uninstall.sh
```

CI 在 Linux 与 macOS 执行检查，不修改真实 Codex 接入配置。测试不代替实际供应商验证。

以下官方文档用于理解配置和协议概念，不代表 Anyrouter 的实现与官方服务完全一致：

- [Codex 配置参考](https://learn.chatgpt.com/docs/config-file/config-reference)
- [Responses WebSocket 模式](https://developers.openai.com/api/docs/guides/websocket-mode)
- [提示词缓存](https://developers.openai.com/api/docs/guides/prompt-caching)

本项目使用 [MIT 许可证](LICENSE)。

旧方式监控不计入号池调度的并发额度。若同时通过两条路径使用同一 Key，还需要考虑上游的总并发限制。已回到接入前供应商时，可再次点击“直接接入”返回号池模式，原始备份及之后更新的非受管配置会保留。
