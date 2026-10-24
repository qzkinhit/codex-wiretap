# Pool Router

**简体中文** | [English](README.en.md)

**Pool Router 让 Codex 使用 BiWrite 中的账号池，并在本机查看每次请求的账号、模型、重试和用量。** 当前版本为 **1.0.0**，属于 `codex-wiretap` 仓库的 Rust 实现，程序名为 `pool-router`。安装后从桌面启动器打开浏览器控制台，本机后台程序负责实际转发。

| 你的电脑 | 下载并安装 |
| --- | --- |
| Apple Silicon Mac，macOS 11 或更新版本 | [下载 PoolRouter-macOS-arm64.dmg](https://github.com/qzkinhit/codex-wiretap/releases/latest/download/PoolRouter-macOS-arm64.dmg) |
| Windows 10 / 11，x64 | [下载 PoolRouter-Windows-x64-Setup.exe](https://github.com/qzkinhit/codex-wiretap/releases/latest/download/PoolRouter-Windows-x64-Setup.exe) |

[全部版本与发行说明](https://github.com/qzkinhit/codex-wiretap/releases) · [BiWrite 公开仓库与安装说明](https://github.com/zzccppp/biwrite) · [历史 Python 实现](legacy/python/)

默认流程需要 Codex、BiWrite，以及可访问的 Responses 上游服务和对应 Key。三者分工如下。

| 软件 | 负责什么 |
| --- | --- |
| Codex | 发起你的对话和编程请求。 |
| [BiWrite](https://github.com/zzccppp/biwrite) | 保存上游地址、模型和 Key，运行保活并维护候选集合。 |
| Pool Router | 只读同步候选，按账号与模型分配请求，显示路由结果和监控记录。 |

默认路径为 **Codex → Pool Router → 上游服务**。BiWrite 提供账号来源，不处在每次请求的转发链路中；默认流程不需要 CC Switch。Pool Router 不提供上游账号或 Key。

![Pool Router 本地控制台示例](docs/dashboard.png)

图中为模拟数据，不包含真实账号、密钥或对话。

## 第一次使用

### 1. 下载适合电脑的安装包

从上表下载安装包。Mac 版本适用于 Apple Silicon；Windows 版本适用于 x64。安装这些预编译包无需 Rust、Cargo 或 Python。

**macOS：** 打开 DMG，双击其中的 **`安装.command`**，等待安装完成。安装器会安装当前用户的后台服务和 `~/Applications/Pool Router.app`，并打开浏览器面板。完成后可以推出 DMG，以后直接打开该 App。

**Windows：** 双击 **`PoolRouter-Windows-x64-Setup.exe`**，按向导完成当前用户安装，然后从开始菜单打开 **Pool Router**。启动器会启动本地程序并打开浏览器面板。程序安装在 `%LOCALAPPDATA%\Programs\Pool Router`，配置和记录保存在 `%LOCALAPPDATA%\Pool Router`。安装时可选择创建桌面快捷方式和登录时启动快捷方式。

安装本身不会修改 Codex 或 CC Switch 的接入配置。先完成下面的 BiWrite 准备，再执行直接接入。

当前版本未使用发布者代码签名，macOS DMG 尚未经过 Apple 公证。若 macOS 阻止首次打开，请确认下载来自本仓库，在“系统设置”的“隐私与安全性”中找到对应提示，选择“仍要打开”并确认；旧版 macOS 的入口为“系统偏好设置”的“安全性与隐私”。Windows 可能显示未知发布者提示，请核对下载来源。

### 2. 在 BiWrite 准备上游、模型和 Key

按照 [BiWrite 仓库说明](https://github.com/zzccppp/biwrite) 安装并打开 BiWrite，然后完成以下操作。

1. 配置 OpenAI 兼容的 **Responses** 供应商，填写你的上游 Base URL、模型 ID 和 Key。只配置 Chat Completions 的供应商不会被此适配器导入。
2. 启用**本地密钥文件模式**，即 `keysInFile = true`，使 Key 保存在 BiWrite 的 `keys.json` 中。Pool Router 不读取系统钥匙串，不需要你再次复制 Key。
3. 对实际要使用的模型运行保活，等待 BiWrite 保存对应的 `keptKeys` 候选。一个模型有候选，不代表同一 Key 在其他模型也已进入候选。

默认只同步保活候选，并排除 BiWrite 中被忽略或停止的 Key。候选表示允许参与后续请求，不保证此刻请求一定成功。

### 3. 打开 Pool Router，确认账号已导入

Mac 打开 `~/Applications/Pool Router.app`；Windows 从开始菜单打开 **Pool Router**。看到浏览器面板后，确认服务处于在线状态，并按下面的顺序检查。

1. 在“路由设置”中使用 **BiWrite** 来源。只有一个符合条件的上游地址时，程序会自动发现；有多个时，填写这次要使用的上游 Base URL 并保存。
2. 在“来源同步”中找到你要用的模型，查看“保活候选数”和“已导入数”。已导入数需要大于 0，首页才会出现相应账号。
3. 如果刚在 BiWrite 运行保活，点击“同步并验证保活”。Pool Router 会读取已保存候选，并对每个账号和模型发送一次小的真实验证请求。明确失效的模型会退出候选；网络错误、429 和超时等暂时失败只进入冷却，不会误删。全部 Key 范围下按钮显示“同步全部 Key”，只读取配置，不逐项验证。若自动同步已暂停，手动同步不会自动恢复它。

先保留默认的“仅同步保活候选”范围。**“持续同步全部 Key”是可选操作**，会把该上游未保活、已忽略和已停止的 Key 也纳入首页和路由候选，并不代表这些 Key 已验证可用。

### 4. 让 Codex 直接接入

点击面板右上角的 **“直接接入 Codex”**。成功后，按钮会显示 **“检查接入”**。此步骤会先备份 Codex 配置，再将活动供应商指向本机 `http://127.0.0.1:10813/v1`，并配置所需认证和 Responses 协议参数；无需手工填写本地令牌。

第一次接入后，重新打开或重载已打开的 Codex 会话，让它采用新配置。“检查接入”表示配置已识别为直接接入，仍需下一步确认实际请求经过该入口。

### 5. 发出请求，在监控中确认结果

在 Codex 中发送一条短请求，例如“只回复 OK”。这是实际上游请求，会使用你的账号额度。回到 Pool Router 的 **“请求监控”**，找到最新记录，确认结果和转发方式。

列表优先显示对话名称，可以按**对话名称或 ID**搜索。点击名称可打开对应 Codex 对话；点击记录其他位置可查看请求模型、实际使用账号、每次尝试的状态和错误，以及上游报告的用量。缺少名称时显示对话 ID。

对话名称只读自本机 Codex 数据库中的 `title` 或 `name`，不读取对话正文。名称缓存为 5 秒，到期后的监控刷新会重新读取，因此改名不会立即出现在面板。名称只用于显示，不写入 `records.jsonl`；无法读取时回退到 ID。

### 6. 确认使用的是哪条路径

以刚发出的请求记录为准。

| 看到什么 | 说明 |
| --- | --- |
| 按钮显示“检查接入” | 当前 Codex 配置已被识别为直接接入；已有会话是否采用它仍要看新请求。 |
| 新记录显示“号池调度” | 本次请求使用 Pool Router 当前来源中的候选账号和路由策略。 |
| 新记录显示“旧方式监控” | 本次请求携带上游 Key，通过兼容路径转发，不从当前号池选账号。 |
| “允许旧方式监控”已开启 | 只表示允许兼容请求，不代表当前请求就在使用旧方式。 |

首次使用按上述默认直连流程即可。仍需要由 CC Switch 维护 Key 时，再阅读后面的“可选流程：兼容 CC Switch 监控”。

## 检查并安装更新

1. 在 Pool Router 面板顶部点击 **“检查更新”**，查看当前版本和 GitHub 最新正式 Release 版本。
2. 确认有适合本机的新版本后，点击 **“下载并打开安装包”**。本地服务会从本仓库的正式 Release 下载安装包，校验 SHA256 后打开它；下载期间不要重复提交。
3. Mac 在打开的 DMG 中双击 `安装.command`；Windows 完成打开的安装向导。现有配置保留，安装完成后刷新面板确认版本。更新会重启本软件，宜等当前请求完成后再安装。

此功能检查正式 Release，不会把仓库 `main` 分支中的源码改动直接安装到本机，也不会静默替换程序。当前版本等于或高于最新正式版时不提供降级安装。没有当前平台安装包或缺少可靠校验信息时，可从窗口中的 GitHub 发行页面查看说明。GitHub 不可达时，请检查网络后重新点击“检查更新”。

## 常见问题

### 为什么没有账号，或者某个模型没有账号？

先确认 BiWrite 已启用本地密钥文件模式、供应商使用 Responses、上游地址与 Pool Router 选择的一致。再查看该模型的保活候选数和已导入数。`keptKeys` 缺失或为空时，默认不会导入该模型；历史成功时间 `keyThrough` 不能替代候选集合。在 BiWrite 对该模型运行保活后，点击“同步并验证保活”。候选存在但导入为 0 时，还要检查 Key 是否被忽略、停止、已从当前配置移除，或在实时验证中被确认失效。

### 为什么首页显示全部账号，而不只是保活候选？

查看同步范围是否为“全部 Key”。如果是，点击 **“仅同步保活候选”**，再点击“同步并验证保活”。已有在途请求继续执行。验证结果只代表本次请求，实际成功、失败和冷却仍由后续代理请求持续更新。

### 为什么同步后仍有失效账号？

`keptKeys` 包含保活及等待重新进入的候选，普通保活失败后 Key 仍可能留在集合中；`keyThrough` 是可能按小时保存的历史成功时间。BiWrite 没有向 Pool Router 提供窗口内的实时状态，因此 Pool Router 在手动同步保活候选时独立发送真实验证请求，而不是只读取历史时间。

验证完成后，面板显示成功、剔除和暂时失败或未完成的数量。认证、权限、额度和模型不支持等明确失败会剔除对应模型，并在后续自动读取中继续排除；下次手动同步会重新验证完整来源。429、5xx、网络错误、超时和未在本次完成的验证可能只是上游繁忙，因此保留账号，不作为失效依据。验证会使用账号额度，成功也不保证之后一直可用。

### 关掉浏览器以后，怎么重新打开？

Mac 打开 `~/Applications/Pool Router.app`，Windows 从开始菜单打开 **Pool Router**。浏览器只是控制面板，关闭页面不会停止后台转发。不要只手工输入本地地址，启动器会正确传递面板所需的本地认证令牌。

### 怎么判断还在使用旧接入方式？

发送一条新请求，在“请求监控”查看标签。“号池调度”表示当前号池路径，“旧方式监控”表示兼容路径。右上角“检查接入”反映配置状态，“允许旧方式监控”反映权限开关，两者都不能替代这条实际请求记录。想回到默认号池路径时，点击“直接接入 Codex”（已接入时显示“检查接入”），按提示重新打开会话，再用新请求确认。

### 名称没显示，或者监控完全没有新记录？

仅名称缺失时，仍可通过 ID 查找。程序只查询本机可读 Codex 数据库中的明确对话 ID；其他设备的对话或缺失的本地索引可能没有名称。如果新请求完全没有记录，先确认服务在线、记录已启用，以及发出请求的 Codex 会话已加载本地接入配置。此程序只监控经过本地入口的请求。

## 安装与命令行参考

### 维护者构建安装包

以下工具仅用于从源码构建发行包，用户安装 DMG 或 EXE 不需要运行这些命令。macOS 在仓库目录依次执行：

```bash
cargo build --release --locked --bin pool-router
python3 scripts/package-release.py --output dist/macos
python3 scripts/macos-package-dmg.py dist/macos/PoolRouter-macOS-arm64.zip dist/macos/PoolRouter-macOS-arm64.dmg
```

Windows 在具备 Rust 和安装器构建工具的 PowerShell 环境中执行：

```powershell
./scripts/windows/build.ps1 -ReleaseTag v1.0.0
```

也可运行 GitHub Actions 的 **Build Windows installer**，填写 `release_tag`。发布标签已创建时，留空 `source_ref` 即从该标签构建；标签创建前需显式填写要构建的提交或分支，例如 `main`。构建结果应对应最终发布标签，不应把未经核对的分支结果当作已发布版本。

### 旧版 ZIP 与 DMG 复现

现有 v0.1.0 的 `PoolRouter-macOS-arm64.zip` 继续保留，其旧安装器仍需要 `python3` 来生成 macOS plist。新用户优先使用 DMG。使用旧 ZIP 时，解压后双击 `安装.command`，或在包根目录执行：

```bash
bash scripts/macos-install.sh --no-build
```

v0.1.0 DMG 与对应 ZIP 包含相同的程序二进制和依赖许可证，DMG 更新了安装脚本。可在当前仓库中从已有二进制 ZIP 打包 DMG；**打包需要 Python 3，安装 DMG 不需要**。

```bash
python3 scripts/macos-package-dmg.py /path/PoolRouter-macOS-arm64.zip /path/PoolRouter-macOS-arm64.dmg
```

### Windows 安装位置

| 内容 | 位置 |
| --- | --- |
| 程序与启动器 | `%LOCALAPPDATA%\Programs\Pool Router` |
| 配置、手动号池和请求记录 | `%LOCALAPPDATA%\Pool Router` |
| 打开控制台 | 开始菜单 → Pool Router |
| 默认 BiWrite 数据目录 | `%APPDATA%\app.biwrite.desktop` |

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

Linux 或未提供预编译包的架构可使用命令行构建与运行方式。以上 `macos-install.sh` 和 LaunchAgent 仅适用于 macOS。

## Codex 直接接入的配置细节

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

自动来源默认只导入所选上游 Responses 供应商的 `keptKeys` 候选，并排除对应供应商下被忽略或停止的密钥。面板中的“持续同步全部 Key”会立即切换到 BiWrite 来源并启用 `biwrite_import_all`，之后自动同步持续包含该上游全部 Key，包括未保活、忽略和停止的 Key。该操作不创建手动池；可随时切回“仅同步保活候选”。公开默认值为 `false`，新增账号未经实际请求验证时显示“待验证”。macOS 默认 BiWrite 目录为 `~/Library/Application Support/app.biwrite.desktop`，Windows 为 `%APPDATA%\app.biwrite.desktop`。该适配器要求 `keysInFile` 开启，不读取系统钥匙串，也不修改 BiWrite 文件。

面板从 BiWrite 的 `keyNames` 读取 Key 昵称，并用于账号卡片和请求记录中的账号显示。缺少可用昵称时显示匿名账号标识，不回显密钥。

“来源同步”显示所选上游中每个模型的配置 Key 数、保活候选数和已导入数，零值也保留。在仅同步保活候选模式下，若 Sol 等模型的 `keptKeys` 为空，面板显示“已读取配置，当前无保活候选”，该模型不会进入可调度账号列表。全部 Key 模式不受保活候选数量限制，但已导入不等于当前请求可用。历史成功记录不会被当作实时健康状态。

号池上方可按完整模型 ID 快速筛选，卡片保留 Key 昵称并显示所选模型的健康记录和并发。筛选只影响当前视图，不更改路由策略、模型覆盖或账号配置。

`keptKeys` 表示保活及等待重新进入的候选集合，并不等同于此刻严格“已挤成功”的状态。`keyThrough` 是历史成功时间，可能按小时粒度持久化。代理实际观察到的成功、失败、冷却和并发另行记录。正常自动同步时，读取失败会短暂保留此前快照；最后一次成功读取超过 30 秒后，停止用该旧快照分配新请求，待文件恢复后继续同步。

顶部“暂停同步”保存 `sync_paused = true`，只暂停定时读取 BiWrite，当前号池仍可调度，不因主动暂停而过期。程序每次启动时会先读取一次当前来源以初始化号池，即使之前保存了暂停状态也会执行；之后的周期同步遵守暂停设置。顶部“同步并验证保活”会重新读取候选并逐项实时验证，验证最多六路并发、每项单次尝试且有 60 秒超时。明确失败排除在当前进程内持续生效，下次手动验证会重新检查完整来源。“同步全部 Key”只读取配置，不逐项验证。两种手动同步都不恢复自动同步；“恢复同步”才重新开始定时读取。暂停状态与“全部 Key / 保活候选”范围独立保存，默认均为 `false`。

本软件不内置供应商地址。`upstream_base_url` 默认为 `null`，自动来源模式下从 BiWrite 发现唯一的 HTTPS Responses 上游；存在多个地址时必须在面板填写所需上游的 Base URL，不会自动任选。填写后只导入该地址的候选。从自动来源切换为手动来源且未填写上游地址时，会将最近成功读取的上游地址保存到 `upstream_base_url`；之后关闭 BiWrite 或移动其目录不会影响手动来源的上游选择。没有可沿用的地址时，需明确填写上游地址。直接以手动来源启动时必须已有明确的 `upstream_base_url`；手动来源不会重新跟随 BiWrite 选择上游，地址为空时拒绝转发。只接受公网域名的 HTTPS 默认端口，不接受用户信息、查询参数、片段或其他路径；根路径与 `/v1` 统一为 `/v1`。

手动来源可先添加账号，再保存来源切换。手动 Key 的请求发送到同一个所选上游。自动来源下维护手动列表不会改变当前 BiWrite 号池；手动池为空时会拒绝切换。相同 Key 重复添加会合并模型，已保存的 Key 不在列表中回显。手动密钥存于本机私有 `manual-keys.json`，自动来源密钥只读入内存。

| 调度方式 | 行为 |
| --- | --- |
| `sticky`，默认 | 尽量保持同一会话使用原账号；允许切换的请求可在原账号不可用时选择其他候选。 |
| `random` | 从当前可调度候选中随机选择。 |
| `round_robin` | 在当前可调度候选间轮流选择。 |
| `pinned` | 只使用指定账号，不因失败而自动换号。 |

因此，BiWrite 自动导入的账号可以手动指定，手动维护的多个账号也可以自动随机或轮询调度。

并发与健康状态以 **Key + 模型** 为单位。同一个 Key 的 Astra 失败不会把 Sol 一并标为失败。默认每个 Key、每个模型最多有 1 个本代理请求在途，并结合来源中保存的模型并发上限。可额外设置跨模型的账号总并发上限。BiWrite 保活与其他客户端的请求不计入本代理的并发数，仍可能与本代理请求一起触发上游限制。

模型和推理强度默认跟随 Codex。面板覆盖只作用于后续号池请求；携带 `previous_response_id` 的请求保留原账号与模型关联，不能跨模型覆盖或跨账号续接。

账号“验证”会发送固定的 `Reply only OK.` 请求，推理强度为 `low`，不发送用户对话。窗口保留本次状态、耗时、请求模型、返回模型、固定请求、实际返回的可见文本、错误分类、用量及事件明细。成功时可以直接查看返回文本；没有可见文本时不会生成或补写“OK”。每个账号的最近一次验证可从“查看验证日志”重新打开，仅保留在当前页面内存，刷新后清除，不写入浏览器持久存储。验证成功只代表本次请求成功；返回文本和事件可能按上限截断。

## 可选流程：兼容 CC Switch 监控

仍希望在 CC Switch 中换 Key 时，可展开面板的“兼容 CC Switch 监控”，执行“接入旧方式监控”。这会将 **CC Switch 当前 Codex 供应商** 的上游地址改为本地入口，并关闭该供应商的 WebSocket 支持。它不修改 Key、模型、推理强度或服务档位，也不切换 CC Switch 的当前供应商。

后续仍在 CC Switch 修改 Key，不需要把 Key 添加到 Pool Router 的手动池。若切换到另一个尚未接入本地入口的 CC Switch 供应商，它的请求不会自动出现在监控中。

| 请求路径 | 认证凭证 | 处理方式 |
| --- | --- | --- |
| Codex → Pool Router → 上游服务 | 本软件的本地令牌 | `route_mode = "pool"`，使用当前号池来源和调度策略。 |
| Codex → CC Switch → Pool Router → 上游服务 | CC Switch 转发的受支持 `sk-` 上游 Key | `route_mode = "legacy"`，只使用该请求携带的 Key。 |

`legacy` 路径仅使用接入监控时的私有备份中保存的原供应商地址。未完成兼容接入或备份无效时拒绝转发，不猜测调用者 Key 的目标。它不会选择或回退到已存号池，不应用面板中的模型或推理强度覆盖，不修改请求的 `service_tier`，也不自动重试或更换 Key。上游请求体按收到的字节转发，监控解析与转发分离。会话恢复、压缩上下文修复和图片压缩也不作用于此路径。

兼容接入与恢复分别对应 `POST /api/legacy/connect` 和 `POST /api/legacy/restore`，两个管理接口都要求本地令牌认证。兼容接入有独立恢复备份；恢复只还原本软件修改的地址和协议字段，保留接入后在 CC Switch 更换的新 Key 以及其他非受管设置。

`legacy_monitor_enabled` 默认为 `true`，只表示允许这类传入 Key 的监控请求，不会改变默认 `biwrite` 来源或本地令牌请求的号池调度。关闭后会拒绝兼容 Key 请求，本地令牌路径继续工作。

旧方式监控不计入号池调度的并发额度。若同时通过两条路径使用同一 Key，还需要考虑上游的总并发限制。已回到接入前供应商时，可再次点击“直接接入”返回号池模式，原始备份及之后更新的非受管配置会保留。

## 协议、重试与上下文

当前 Rust 版本支持 `POST /v1/responses` 的 HTTP/SSE，以及 `POST /v1/responses/compact`；另有本地 `GET /v1/models`。不支持 Responses WebSocket、Chat Completions 或任意路径转发，客户端需设置 `supports_websockets = false`。上游地址由 BiWrite 自动发现或用户在路由设置中指定。

号池路径同时检查 HTTP 状态与 Responses/SSE 事件。HTTP 200 中的失败事件不会计为成功。有效内容输出前，明确可重试的拒绝或已确认未建立连接的失败可以更换候选；输出文本、推理或工具调用之后不自动重放。结果不确定的传输中断也不透明重试。

号池默认最多重试 **15 次**，加上首次请求共 **16 次尝试**。后端配置 `max_attempts = 16` 包含首次请求，面板显示和编辑的是重试次数。会话恢复和压缩上下文修复产生的请求也计入同一总次数上限。排队和等待有效输出的前导阶段共用 600 秒期限，单次上游响应头等待上限为 120 秒，每次尝试从发送开始等待首段有效内容的上限为 120 秒，流式空闲上限为 180 秒。因此，达到时间期限时可能尚未用完重试次数。兼容监控路径始终只提交一次上游请求。

默认 `first_output_timeout_ms = 120000`。号池请求没有 `previous_response_id` 且尚未产生有效内容时，等待首段有效内容超时可以释放当前尝试并重新调度；SSE 心跳和纯状态事件不算有效内容。已经产生文本、推理或工具调用时不自动重放。超时不能证明上游从未执行，增加重试次数也不保证成功或消除上游整体故障。

号池客户端发送 `Accept: text/event-stream` 时，代理立即发出 SSE 等待注释，并在等待期间每 5 秒发送心跳。注释不含响应 ID 或模型内容，不阻止前导失败时换号。响应头发出后的错误通过 SSE `error` 事件返回；记录仍保留实际处理状态。客户端断开会取消未完成的等待和转发。

携带 `previous_response_id` 的号池请求必须使用已记录的原账号与模型。响应 ID 同时绑定该次响应实际使用的上游会话映射，或原始未重写状态；后续续接沿用这一状态，不会套用后来为同一范围重建的会话映射。绑定未知、过期或不可用时会明确返回错误。绑定保存在内存中，默认有效期为 24 小时；重启代理会丢失绑定，客户端可能需要重新发送上下文。兼容监控路径按原样转发客户端的续接字段，不利用号池绑定替换账号。

路由设置中的“会话恢复”对应 `session_recovery_enabled`，默认开启。有效输出前持续收到 429，或收到明确指向绑定资源缺失的 502 时，代理可为不含 `previous_response_id` 的请求重建上游会话标识，每个请求最多一次。恢复后的标识按原会话、账号、模型和上游分别保存，后续不含 `previous_response_id` 的匹配请求可复用。该操作不重建被省略的历史，也不能让 `previous_response_id` 跨账号续接。一般 502 不会自动触发这种恢复；兼容上游是否接受新标识取决于其实现。

“压缩上下文修复”对应 `compaction_repair_enabled`，默认开启。仅当 `/v1/responses/compact` 返回 HTTP 400 且明确指出加密内容不兼容、无效或无法解密时，代理才清除相关不透明加密推理或压缩块，并在保留可用明文、消息和工具调用关系的前提下修复重试一次。普通 400 不触发清除，成功响应不触发修复。清除加密块可能丢失只能从这些块恢复的上下文；客户端仍需提供继续处理所需的历史。这是对兼容上游错误的有限处理，不是官方协议保证。

“压缩图片”对应 `compress_images`，默认关闭。开启后仅处理请求消息中内联的 PNG/JPEG Base64 图片，不下载外部图片 URL。符合处理条件的图片可能缩小至最长边 2048 像素并转为有损 JPEG，透明区域合成为白色；仅在体积有足够下降时替换。小图片、不支持的格式、动画、超出处理预算或解码失败的图片保留原样。压缩可能影响小字、细线和其他细节，要求保留这些细节时应关闭此项。

同一上游地址不证明不同账号共享服务端会话或提示词缓存。每次请求发送多少上下文取决于 Codex 和协议。缓存命中与网络上是否发送请求体是不同问题，代理不会自动补齐被省略的历史。

## 监控记录与数据边界

监控接口按记录中已有的对话 UUID，只读查询本机 Codex 数据库的 `title` 或 `name`，将结果作为可空的 `conversation_title` 返回。它优先使用 `CODEX_HOME`；未设置时使用当前用户的 `.codex` 目录，Windows 支持 `USERPROFILE`。标题正、负查询结果均缓存 5 秒，到期后随监控查询更新；不读取对话正文，不向请求 JSONL 追加名称。无名称时界面和搜索继续支持原始 ID。

只监控**经过本地入口**的请求，不读取其他进程的 HTTPS 流量，也不执行系统级抓包或中间人解密。默认保存最近 1000 条白名单元数据，可暂停记录而继续转发。

记录另含 `gateway_actions` 网关处理动作、最后一次实际发往上游的请求体积 `upstream_request_bytes`，以及有效模型输出等待时间 `first_output_ms`。面板将会话重置与复用、压缩上下文修复、图片压缩及超时重试分别展示。有效输出耗时不使用 SSE 心跳、首包时间或累计请求耗时代替，缺失时保持未知。

记录包括路由类型 `route_mode`、匿名账号标识、可识别的会话 UUID、请求与服务器声明的模型和推理强度、请求与响应的 `service_tier`、状态与失败分类、重试次数、耗时、字节数，以及上游报告的 token、缓存和推理 token 用量。号池覆盖后的实际发送模型与推理强度单独显示。面板按当前来源将匿名账号标识映射为 Key 昵称。没有报告的字段保持未知，不推算为已确认信息。

面板将 `fast` 与 `priority` 放在同一服务档位组中方便查看，同时保留请求和响应的原始值。这些字段只说明请求参数或服务器声明，不能证明实际后端模型、服务档位执行情况或最终计费。

日常网关日志仅保存白名单元数据，以及每次尝试中经过长度限制和脱敏的错误消息 `AttemptRecord.error_message`，不保存完整请求或成功回复正文。错误摘要用于解释具体失败，缺失时保持未知，不保存整段上游错误响应。摘要来自上游诊断文本，分享前仍应检查其中的内容。固定验证请求的有限返回文本仅用于当前页面的验证详情，与日常网关日志分开处理。BiWrite 密钥只读入内存；手动密钥存于私有文件；兼容请求的 Key 只用于该请求转发。私有配置保存本地访问令牌，接入恢复备份也可能包含原配置中的凭证。这些文件不应随源码、截图或诊断包分享；在 macOS 和 Linux 上创建时使用 `0600` 权限。

服务仅监听回环地址，并检查 Host 和浏览器 Origin。管理接口始终要求本地令牌。兼容的真实 Key 只被模型转发接口接受，不能作为管理令牌使用。

## 维护、恢复与卸载

更新到新版本时重新运行对应系统的安装包。Windows 可从系统设置的应用列表或开始菜单卸载 Pool Router。Windows 卸载器会先恢复本软件管理的接入配置；出现受管字段冲突时中止卸载，保留配置供处理。用户数据默认保留。

以下命令和 LaunchAgent 操作适用于 macOS。查看脱敏状态或重新打开面板：

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

macOS 卸载脚本不停止 CC Switch 或其他 wiretap 服务。配置、手动密钥、日志和归档恢复备份默认保留；确认恢复完成且不再需要数据后，可手动删除本软件的数据目录。不要在仍有客户端指向本地入口时直接删除程序。

## 历史版本与开发

此前的 Python 实现保存在 [`legacy/python/`](legacy/python/)，用于保留历史源码和功能参考。旧版支持的 WebSocket 等能力不代表当前 Rust 版本已实现，也不会由 Rust 安装器自动启用。

本地验证使用临时夹具和 localhost mock，不需要真实上游 Key。前端测试需要 Node.js 22 或更新版本。

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
node --test web/app.test.cjs
bash -n scripts/macos-install.sh scripts/macos-uninstall.sh
```

CI 在 Linux 与 macOS 执行检查，不修改真实 Codex 接入配置。测试不代替实际供应商验证。

以下官方文档用于理解配置和协议概念，不代表上游服务的实现与官方服务完全一致：

- [Codex 配置参考](https://learn.chatgpt.com/docs/config-file/config-reference)
- [Responses WebSocket 模式](https://developers.openai.com/api/docs/guides/websocket-mode)
- [Responses 流式响应](https://developers.openai.com/api/docs/guides/streaming-responses)
- [上下文压缩](https://developers.openai.com/api/docs/guides/compaction)
- [提示词缓存](https://developers.openai.com/api/docs/guides/prompt-caching)

本项目使用 [MIT 许可证](LICENSE)。
