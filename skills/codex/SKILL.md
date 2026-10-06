---
name: codex
description: 把画图、定理审查、理论审查和规格明确的执行任务派给本机 Codex CLI 在后台运行，Claude 负责拆分任务、写任务说明、审核与验收。写作类任务（论文正文、摘要、图注、审稿回复、PPT 与文档文字）一律不派发。用户说「交给 codex」「让 codex 画」「让 codex 审一下」或输入 /codex 时使用。Claude 判断某项工作属于上述可派发类型且工作量明显大于几次编辑时，也主动使用。
argument-hint: <任务描述>
---

# codex

本 skill 让 Codex CLI 充当 Claude Code 的子代理。Claude 决定做什么、写任务说明、审核并验收结果。Codex 按任务说明画图、审查定理与理论推导、修改代码和运行命令。Codex 的输入只有任务说明文件，不包含当前对话，所以任务说明必须自包含。

## 分派范围

派给 Codex 的工作：
- 画图。按给定数据与样式写画图脚本并出图
- 定理审查与理论审查。逐步核对证明，检查假设是否完整、记号是否一致、定理表述与证明是否对应，必要时做数值验证
- 规格明确的执行任务。按规格写代码、批量修改、运行脚本、编译、修复原因明确的报错

由 Claude 自己完成的工作：
- 一切写作，包括论文正文、摘要、图注、表注、审稿回复、PPT 与文档文字。图中出现的文字由 Claude 在任务说明里逐字给定
- 方法设计、定理的提出与改写、实验结论判断
- 一两处的小改动。Codex 单次调用需要数分钟，小改动由 Claude 直接完成更快

## 任务粒度

单个任务的会话上下文保持在 256k tokens 以内，上下文过长时 Codex 响应明显变慢。拆分原则如下。
- 一个任务只做一件事，例如一张图或一组同源的图、一条定理连同它依赖的定义与引理、一个模块的改动
- 任务说明给出文件路径与行号范围，不让 Codex 自行通读仓库
- 大文件、日志与数据表只给路径和需要的行列，不把全文粘进任务说明
- 每轮结束时脚本报告上下文占用。达到上限的 75% 后脚本拒绝续接，此时新开任务，把需要延续的结论写进新的任务说明

## 步骤

1. 在 scratchpad 目录写任务说明文件（Markdown），包含以下四项。
   - 目标：完成后应当出现的结果
   - 范围：允许修改的文件或目录，禁止触碰的文件。审查任务写明被审查的文件与行号范围
   - 输入：相关文件路径、规格、可参照的现有代码或图。需要运行 Python 时写明解释器路径，Codex 登录 shell 中的 python3 不一定装有所需的库
   - 验收：验证所用的命令与期望输出。审查任务写明需要回答的问题
2. 用 Bash 后台运行，run_in_background 设为 true。
   ```
   python3 ~/.claude/skills/codex/scripts/codex_run.py new <工作目录> <任务说明.md> [额外可写目录...]
   python3 ~/.claude/skills/codex/scripts/codex_run.py review <审查说明.md>
   ```
   `new` 用于画图与执行任务，Codex 可以写工作目录和额外可写目录。`review` 用于定理与理论审查，Codex 在运行目录下的独立 workspace 中工作，可以读取任意文件，只能写 workspace，被审查的文件不会被改动。需要联网（安装依赖、下载数据）时在命令前加 `CODEX_NETWORK=1`。
3. Codex 运行期间 Claude 继续做别的工作，不轮询。脚本结束时 Claude 会收到通知。
4. 审核。脚本输出依次是运行信息（模型、思考强度、沙箱、上下文占用）、Codex 的最终报告、执行过的命令（失败的以 ! 标记）、本轮修改过的文件、git status 新增条目。
   - 执行任务：对每个改动文件看 git diff，未跟踪文件直接读，核对是否符合任务说明、是否越界。亲自运行验收命令，不以 Codex 的自述为准。越界改动由 Claude 回退
   - 画图任务：用 Read 查看输出图片，核对数值与数据源一致、样式符合要求、图中文字与任务说明给定的原文一致
   - 审查任务：Codex 的意见是待核实的输入。Claude 逐条复核依据（反例、推导、数值验证），确认成立的问题由 Claude 修改定理或证明
5. 不合格时续接同一个 Codex 会话返工，续接沿用上一轮的会话记录。
   ```
   python3 ~/.claude/skills/codex/scripts/codex_run.py resume <运行目录> <返工说明.md>
   ```
   返工说明写明哪里不对、期望结果是什么。两轮返工仍不合格，由 Claude 自己完成。
6. 向用户汇报时写明哪些部分由 Codex 完成、Claude 审核了什么、验收结果如何。

## 并行

修改的文件互不重叠的任务可以同时后台运行。审查任务各自使用独立 workspace，可以按定理拆开并行。可能改到同一文件的任务串行执行。

## 配置

脚本读取以下环境变量，也读取 skill 目录下的 `.env` 文件（格式见 `.env.example`），环境变量优先。

| 变量 | 作用 | 默认值 |
| --- | --- | --- |
| CODEX_MODEL | 模型 | 沿用 Codex 的 config.toml |
| CODEX_EFFORT | 思考强度 | 沿用 Codex 的 config.toml |
| CODEX_SANDBOX | new 模式的沙箱 | workspace-write |
| CODEX_NETWORK | 设为 1 时沙箱内命令可以联网 | 不联网 |
| CODEX_CONTEXT_LIMIT | 上下文上限，单位 tokens | 256000 |
| CODEX_RUNS_ROOT | 运行记录目录 | ~/.claude/codex-runs |
| CODEX_BIN | codex 可执行文件 | 依次查找 ChatGPT.app 与 Codex.app 内置版本、PATH 中的 codex |

每次运行一个目录，每轮一个子目录，含 prompt.md、events.jsonl、last_message.md、stderr.log。模型、思考强度与上下文占用读自 Codex 本地会话记录 `$CODEX_HOME/sessions`，反映客户端设置，不能证明上游实际执行的模型。
