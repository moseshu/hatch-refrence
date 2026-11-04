# hatch — 个人 Agent 助手

hatch 是一个可以部署在云端主机或个人电脑上的个人 Agent 助手（personal agent）。它常驻运行，管理你的邮件、日程、记忆和工作区，能在对话中调用工具、使用浏览器、把任务委派给子代理、按计划在后台工作，并且把每一个对外动作都放进可审计、可撤销、需要你确认的执行通道里。

## 能力概览

| 能力 | 说明 |
|---|---|
| 邮件与日程分诊 | 拉取新邮件（IMAP，兼容 126/163）、隔离分类与摘要、识别可疑邮件、会议冲突检测与空闲时段、按你的风格起草回复 |
| 对话与工具 | 记忆、日历、邮件检索；工作区内的文件读写与搜索；沙箱中执行命令；读取网页、可选搜索 |
| 浏览器 | 按来源隔离的 Chromium（对话 / 定时任务 / 子代理各一个 profile），读取页面、查找、点击、填写、选择、截图；提交先审批，支付只建议，登录与验证码交给你接管 |
| 子代理 | 把独立任务委派给 explore / execute / research 类型的子代理，可并行；工具只能收窄，审批统一排队 |
| 后台运行 | daemon 按计划运行任务：每日简报、定时分诊、记忆整理、每日动态；产出在下次对话时作为「后台更新」交给助手 |
| 目标 / 点子 / 动态 | 目标工作区（`goals/<id>/GOAL.md` + `files/` + `hidden_files/`，状态机 active → completed / paused / abandoned，关注度按未读关联产出计算）；点子库（已忽略的永不出现）；每日动态（一个提示词、每条标明来源事实、固定开场白） |
| 记忆 | 结构化 claim（来源、信任、置信度、有效期）、纠正链与溯源、中文 BM25 检索、人物页、遗忘 |
| 计划模式 | 模型先产出结构化步骤（工具、参数、文件、预期、风险），存库待你批准；批准后逐步骤执行，每步对照批准快照，偏离即停 |
| 产物呈现 | 卡片式 widget（选项/列表/HTML，embed_token 激活规则）与带版本的 artifact（构建 → 分享审批 → 交付，交付消息必含文件路径） |
| 多通道传输 | 主 chat 与 side chat 共享记忆/目标、各有独立 transcript；每聊天一份 DISCRETION.md 分享规则；外部聊天里的 worker 只备内容、主 agent 转述与决定；设备能力声明式，缺失时如实告知开通路径。参考传输只有本地 pipe（文件模拟聊天应用）；微信/飞书/Telegram 未实现 |
| 安全执行 | 信任标签与污染规则、门禁 → 审批 → 执行器 → 幂等账本、命令沙箱、文件检查点、秘密检测与脱敏 |
| 多模型 | Chat Completions / Responses / Anthropic Messages / Gemini 四种协议，均支持工具调用与流式输出；按任务路由与降级 |
| 可中断 | Ctrl-C 停止当前回合及其所有子代理和命令；会话可继续、复制、回退 |
| 可度量 | 内置沙盒评估世界，pipeline（脚本模型）与 live（真实模型）两种模式 |

---

## 架构

```
                  ┌──────────────────────── hatch-cli（hatch 命令）────────────────────────┐
                  │ init · chat/ask · triage · approvals · grants · memory · calendar ·    │
                  │ agents · session · checkpoint · jobs · brief · goal · ideas · feed ·   │
                  │ daemon · eval · doctor · plan · skills · mcp · vault · browser ·      │
                  │ artifact · transport                                                   │
                  └──────────────┬───────────────────────────────┬─────────────────────────┘
  调度层          hatch-scheduler（调度·任务存储·运行引擎·daemon） · hatch-workers（简报·定时分诊·记忆整理）
                                 │                               │
  场景层          hatch-triage（邮件+日程分诊）   hatch-agent（对话循环·上下文·压缩·子代理）   hatch-eval（评估）
                                 └───────────────┬───────────────┘
  能力层                          hatch-tools（工具 SDK·内置工具·执行器·统一 dispatch·检查点）
                                                 │
  领域层          hatch-policy（门禁·审批·账本）  hatch-memory（记忆）  hatch-connectors（邮件·日历）  hatch-llm（模型）
                  hatch-sandbox（命令沙箱）  hatch-browser（浏览器）
  基础层          hatch-store（SQLite）  ·  hatch-core（错误·ID·信任·token·秘密·取消令牌）
```

| crate | 职责 |
|---|---|
| `hatch-core` | 错误类型、内容寻址 ID、信任标签与不可信信封、CJK 感知 token 估算、秘密检测与脱敏、取消令牌 |
| `hatch-store` | 单文件 SQLite、幂等迁移、事务、可被并行子代理共享的连接、自动脱敏的审计日志 |
| `hatch-llm` | 四种模型协议与流式输出、按 workload 路由、降级阶梯、调用预算、代理与自定义 CA |
| `hatch-memory` | claim 记忆、纠正链与 explain、中文 BM25、遗忘、人物页、合并与过期 |
| `hatch-policy` | 动作类别 × 可逆性 × 自主度、门禁规则、常设授权、异步审批队列、幂等账本、注意力预算 |
| `hatch-connectors` | IMAP / SMTP / MIME（含 GBK）、夹具邮箱与 outbox、ICS 解析、冲突与空闲时段；连接器框架（manifest / runner 协议 / 状态检查）与两个参考连接器（本地文件笔记、本地 mock HTTP） |
| `hatch-sandbox` | 进程沙箱：命名空间、pivot_root、capability 清空、no_new_privs、seccomp、rlimit、超时杀进程树 |
| `hatch-browser` | Chromium 管理（按 profile 隔离、跨进程重新连接、空闲回收）、CDP 会话、页面快照、目标地址拦截、登录/验证码/结账识别、点击分类、mock 站点 |
| `hatch-tools` | Tool SDK、邮件/日历/记忆/文件/命令/网页/浏览器工具、工作区路径守卫、检查点、执行器、门禁 → 审批 → 执行的统一入口 |
| `hatch-agent` | 行为规约、分层上下文组装、压缩、会话（fork / rewind）、可取消的回合循环、子代理 |
| `hatch-triage` | 分诊工作流：隔离分类、代码后处理、会议处理、起草、持久化步骤、回执 |
| `hatch-scheduler` | 调度表达式（夏令时正确）、任务 / 运行记录 / 去重分表、看门狗与重试、自动停用、daemon |
| `hatch-workers` | 每日简报、定时分诊、记忆整理、后台产出交接（worker output 可关联目标） |
| `hatch-goals` | 目标工作区（磁盘 GOAL.md + files/ + hidden_files/）、点子库、每日动态（feed worker + 固定开场白） |
| `hatch-eval` | 评估世界与报告（分诊、工作区、子代理、调度、浏览器） |
| `hatch-plan` | 计划模式：结构化计划的解析与校验、SQLite 持久化与状态机、逐步骤执行与偏差检测 |
| `hatch-artifacts` | widget 卡片（options / list / html，allowlist HTML 净化、embed_token 激活规则、纯文本 fallback）与带版本的 artifact（`$HATCH_HOME/artifacts/<id>/v<n>/` + manifest 校验和、分享审批执行器、交付消息必含路径）；工具 `show_options` / `create_artifact` / `share_artifact` / `deliver_artifact` |
| `hatch-transports` | 多通道传输接口：inbound（文本/图片引用）/ outbound（文本/widget/附件）、每聊天独立 transcript、每聊天 `DISCRETION.md` 分享规则、worker 转述规则（草稿 + 主 agent 决定）、设备能力清单；参考传输 pipe（本地文件模拟聊天应用） |
| `hatch-mcp` | MCP 客户端（stdio JSON-RPC 2.0 与 HTTP/SSE）+ MCP 服务端（stdio）：工具注册为 `mcp_<服务器>_<工具>`，调用走审批队列；服务端调用同样走门禁/审批，需要审批时返回 `approval_required` + 审批 id |
| `hatch-vault` | 凭据保管：0600 文件存储 + 独立 helper 进程（注入 / HTTP 代理 / OAuth 授权码流程 scaffold） |
| `hatch-skills` | 技能注册表：SKILL.md 解析、白名单、激活 |
| `hatch-cli` | 命令行、配置与环境变量覆盖、Ctrl-C 处理 |

细节见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

---

## 核心机制

**上下文组装。** 每轮请求由固定顺序的层组成：行为规约 → 用户画像与偏好记忆 → 检索到的记忆 → 后台更新 → 会话历史。预算按 CJK 感知的 token 估算裁剪，稳定层不丢弃；历史过长时在用户消息边界压缩，保留最近的尾部。

**记忆。** 记忆以 claim 为单位，记录来源、信任级别、置信度和有效期。纠正不覆盖历史，而是形成可追溯的纠正链；推断出的和来自不可信来源的记忆置信度有上限；含秘密的内容拒绝写入。检索使用中文 unigram + bigram 的 BM25，只返回当前有效的记录。每轮对话开始前自动检索相关记忆注入上下文（无命中不注入、检索失败不影响回合）；每天 04:00 的 `memory-extract` 任务从新会话提炼持久记忆写入（冲突只提议不写），也可手动跑 `hatch memory extract [--session <id>]`。

**门禁、审批与幂等。** 每个工具调用先由门禁根据动作类别、自主度、常设授权、污染状态和信息流规则决定：直接执行、进入审批队列或拒绝。审批不阻塞对话；批准后执行的就是审批时看到的内容（payload 哈希校验）；过期的审批永不执行。每个对外动作都有内容寻址的幂等键，崩溃后状态不明时不重发而是如实报告。

**信任与污染。** 进入模型的每段文本都带信任标签；邮件、网页、文件内容包进不可信信封。上下文一旦含有不可信内容，所有对外、写入和执行类动作都强制审批，常设授权默认也不能豁免。分诊分类使用无工具的隔离模型，回复收件人由代码固定。

**命令沙箱与检查点。** `shell_run` 在独立的 user / mount / net / pid / ipc / uts 命名空间中以 nobody 身份运行：只读系统目录 + 可写工作区、无网络、seccomp 与 rlimit 限制、超时杀整棵进程树。宿主不支持时直接报错，不会退化为无隔离执行。每次写文件前自动建立检查点，可随时回退。

**浏览器。** `main`（对话）、`scheduler`（定时任务）、`research`（子代理）三个 profile 是三个独立的 Chromium 进程，cookie 与登录状态互不可见。页面以快照形式交给模型：正文加带编号的可操作元素，模型只能用编号引用元素，不能执行脚本。标签页连接期间，页面发出的每个请求（含每一跳重定向）都经过目标地址检查。点击的性质由代码按元素和页面决定：链接是网页访问，页面控件只改页面，可能提交的按钮属于 `browser_submit`（审批里有页面、按钮、完整表单内容与截图，执行前重新核对，任何变化都不点击），结账与支付只能建议。遇到登录墙或人机验证时会话暂停，由你用 `hatch browser takeover` 接管完成；hatch 不填写密码、不破解验证码。失败按类别如实报告（网络、站点拦截、需要登录、验证码、元素不存在、选项不存在、超时、策略拒绝），不会把"没加载到"说成"没有"。

**子代理与取消。** 主代理可调用 `delegate` / `delegate_parallel`。子代理只拿任务书、看不到主对话，工具清单只能比主代理少，继承污染状态，审批进入同一队列并标注来源，共享调用预算，嵌套深度有上限；结束后交回结论、有界的过程摘录和用量。取消令牌从主代理传到每个子代理、模型调用、沙箱命令和网页读取；取消是独立的终态，已生效的修改不回滚，回执写明停在哪里。

**调度与后台 worker。** 支持 `daily` / `weekly` / `every` / `once` / `cron` 表达式，按用户时区计算并正确处理夏令时。停机期间错过的触发只记录，最多补跑一次；失败退避重试，连续失败自动停用并通知；每次运行有时间上限。定时任务的授权范围只能收窄：范围外的对外动作一律进审批。每日简报的事项由程序从你的数据收集，模型只负责排序和措辞，每一条必须引用事项编号，没有依据的条目会被丢弃。

**模型协议与流式。** 同一套抽象支持 Chat Completions（DeepSeek、通义千问兼容模式、Moonshot、vLLM、Ollama 等）、OpenAI Responses、Anthropic Messages 和 Gemini，均支持工具调用、JSON 模式、`reasoning_effort` 与 cached / 推理 token 统计。流式输出拼出的结果与非流式一致；服务端拒绝流式时自动改为非流式重试。分类、起草、压缩、简报等任务可分别路由到不同模型，出错时按降级阶梯切换。

## 计划模式

对话默认是"边想边做"：模型每步自行决定下一个工具调用。计划模式反过来——模型先交出一份**结构化计划**（有序步骤，每步写明工具名、参数、涉及文件、预期结果、风险等级），计划存进 SQLite 等你批准，批准后才按步骤执行；执行时每一步都对照批准时的快照，工具、参数、文件有任何出入就停下，标为"需重新审批"，绝不静默执行计划外的步骤。

```bash
hatch plan new "把本周未读邮件里提到的待办整理成一份清单"   # 制定计划，不执行
hatch plan list                    # 查看计划
hatch plan show plan_xxxx          # 看步骤、状态与每步结果
hatch plan approve plan_xxxx       # 批准（记录批准人 + 时间，批准时快照全部步骤）
hatch plan run plan_xxxx           # 按批准的步骤逐一执行
hatch plan edit plan_xxxx --step s2 --tool fs_read --args '{"path": "notes.md"}'
                                   # 改步骤 → 计划回到待审批，必须重新批准
hatch plan reject plan_xxxx        # 拒绝
hatch plan cancel plan_xxxx        # 取消（永不执行）
```

对话里也可以用计划模式：`hatch chat --plan`（或配置 `[agent] plan_mode = true`），这一轮不直接执行，模型先制定计划、存库待批，你再用上面的命令批准并执行。计划的状态流转是 `draft → pending_approval → approved → executing → completed`，另有 `rejected / superseded / cancelled / needs_reapproval`；同一任务的新计划会自动取代尚未批准的旧计划。计划里的每一步仍走统一的门禁 → 审批 → 执行通道：需要确认的动作会在执行中暂停，等你在审批队列里决定后再用 `hatch plan run` 从断点继续（已完成的步骤不会重跑）。详情见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) 与 [docs/SECURITY.md](docs/SECURITY.md)。

---

## 安全模型要点

| 机制 | 说明 |
|---|---|
| 自主度上限 | 不可逆动作（发送、删除、执行命令）最高只能到"起草待批"，支付只能建议 |
| 信息流规则 | 新收件人、携带日程详情或他人消息外发都需要审批；带秘密的内容直接拒绝 |
| 秘密处理 | 记忆拒绝秘密；通知、摘要、审批、审计日志统一脱敏；密钥只从环境变量读取 |
| 网页访问 | 只允许公开 https；拒绝内网、本机、链路本地和云元数据地址，重定向逐跳检查 |
| 浏览器 | 页面内容不可信；每个请求做目标地址检查；提交先审批且执行前重新核对；支付只建议；不填写密码、不破解验证码；profile 之间隔离；会话只能被打开它的对话使用 |
| 工作区守卫 | 文件工具只认工作区内的路径，拒绝 `..` 逃逸和指向外部的符号链接，原子写入 |
| 技能 | 用户自装的工作方式说明；加载不授予工具（allowlist 过滤），不能绕过审批 |
| MCP | 客户端支持 stdio 与 HTTP/SSE，服务端只做 stdio；服务器输出一律不可信，调用走审批队列，结果标不可信；服务端无逐客户端认证，只给可信的本地客户端用，见 docs/SECURITY.md |
| 凭据保管 | token 由独立 helper 进程持有（0600 文件持久化）；注入时 helper 直接把值设进子进程环境，agent 进程只持有名字；MCP 长连接是例外（生成瞬间短暂经过本进程内存），见 docs/SECURITY.md |
| 连接器 | 子进程 CLI + JSON 协议；manifest 声明 ops（只读/写入），写入类走审批队列；状态检查失败只给指引、不承诺结果；错误码稳定、不含秘密与上游原文 |
| 计划模式 | 批准时快照全部步骤；执行逐步骤核对工具、参数与文件，偏离即停并需重新批准；编辑必重新批准 |
| 产物呈现 | widget 的 HTML 卡片只允许安全标签与相对链接（无脚本、无事件处理器、无外部拉取），见 SECURITY.md §9f；artifact 分享是对外动作，必须经审批队列批准后才标记已分享，未分享不能交付；构建前扫描秘密，命中则拒绝写入 |
| 遗忘 | 删除整条记忆版本链，并如实说明外部副本无法收回 |

完整说明见 [docs/SECURITY.md](docs/SECURITY.md)。

---

## 快速开始

### 1. 构建与初始化

```bash
cargo build --release              # 二进制：target/release/hatch
hatch init                         # 创建 ~/.hatch/config.toml 与数据库（HATCH_HOME 可改目录）
hatch doctor                       # 自检：配置、数据库、模型、邮箱、沙箱、调度（不会打印密钥）
```

### 2. 配置模型

只用环境变量即可运行：

```bash
export HATCH_LLM_BASE_URL=https://api.deepseek.com/v1
export HATCH_LLM_MODEL=deepseek-chat
export HATCH_LLM_API_KEY=<你的 key>          # 只从环境变量读取，不写入文件或日志
export HATCH_LLM_PROVIDER=openai_compatible  # 可选：openai_responses / anthropic / gemini
export HATCH_LLM_REASONING_EFFORT=low        # 可选：推理模型的推理强度
```

也可以在 `config.toml` 的 `[[models]]` 中配置多个模型（只写 `api_key_env` 变量名），并在 `[routing]` 中为不同任务指定模型与降级顺序。模型请求遵守 `HTTPS_PROXY` / `NO_PROXY`，可用 `HATCH_CA_BUNDLE` 或 `SSL_CERT_FILE` 追加 CA。

| `provider` | 协议 | 请求地址 |
|---|---|---|
| `openai_compatible`（别名 `openai_chat`） | Chat Completions | `{base_url}/chat/completions` |
| `openai_responses` | OpenAI Responses | `{base_url}/responses` |
| `anthropic` | Anthropic Messages | `{base_url}/v1/messages` |
| `gemini` | Gemini `generateContent` | `{base_url}/v1beta/models/{model}:generateContent` |

### 3. 配置邮箱与日历

在 `config.toml` 的 `[mail]` 中填写 `username`，密码（126/163 为授权码）放在环境变量：

```bash
export HATCH_MAIL_PASSWORD=<授权码>
hatch calendar import ~/Downloads/calendar.ics
hatch memory person boss@corp.com 王总 --importance vip
hatch memory add --tags style,preference "回复邮件要简洁"
```

默认 `[mail] sender = "outbox"`：外发只写入 `~/.hatch/outbox/*.eml`，确认流程后再改为 `"smtp"`。

### 4. 分诊与审批

```bash
hatch triage run --dry-run
hatch approvals list
hatch approvals show <id>          # 查看将要执行的确切内容
hatch approvals approve <id>
hatch grants add --class send_message --to "*@corp.com" --days 30   # 常设授权
hatch digest                       # 按打扰预算汇总的通知
```

### 5. 对话、工作区与子代理

```bash
hatch ask "这周三下午我有空吗？"
hatch chat --stream --workspace ~/projects/demo   # 指定工作区后可用文件与命令工具
hatch chat --resume <session>
hatch session list | fork <id> | rewind <id> --to <seq>
hatch checkpoint list | rewind <id>
hatch agents list | runs | show <run_id>
```

回合进行中按一次 Ctrl-C 停止当前回合（含子代理与命令）；2 秒内再按一次退出。

### 6. 浏览器

在 `config.toml` 中开启（需要本机安装 Chromium / Chrome，或用 `HATCH_CHROMIUM` 指定路径）：

```toml
[browser]
enabled = true
```

```bash
hatch ask "打开 https://pypi.org/project/requests/ ，告诉我最新版本号"
hatch browser sessions             # 浏览器会话与状态
hatch browser takeover <session>   # 需要登录或验证码时：用有界面的浏览器打开同一 profile，由你完成
hatch browser resume <session>     # 完成后恢复，助手继续
hatch browser status               # 各 profile 的浏览器进程
```

### 7. 定时任务与 daemon

```bash
hatch jobs list
hatch jobs enable daily-brief      # 每天 08:30 生成每日简报
hatch jobs enable triage-sync      # 每 30 分钟分诊新邮件
hatch jobs enable memory-flush     # 每天整理记忆
hatch jobs enable memory-extract   # 每天 04:00 从新会话提炼记忆
hatch jobs enable daily-feed       # 每天 07:30 生成每日动态
hatch daemon run                   # 前台常驻；hatch daemon stop 或 SIGTERM 优雅退出
hatch brief show
```

### 产品 API（v1）：桌面端与网页端共用的后端

```bash
hatch daemon run --api              # 同时提供产品 API：http://127.0.0.1:8787/v1
hatch daemon run --api --api-bind 127.0.0.1:9000   # 换监听地址
```

- 端点：会话与流式聊天（SSE）、记忆、审批、定时任务、产物、技能、连接器、目标/Feed/Ideas、日程、邮件（只读）。`GET /v1/health` 不鉴权；其余端点需要 `Authorization: Bearer <token>`。
- Token：`hatch vault set daemon-api` 存入 vault（首选），或设置环境变量 `HATCH_DAEMON_API_TOKEN`；永不写配置文件、不上命令行、不记日志。未配置 token 时 API 只在回环地址可用；绑定非回环地址又无 token 时 daemon 拒绝启动。
- 配置（`config/hatch.example.toml` 的 `[daemon]`）：`http_enabled = true` 可免 `--api` 启动；`http_bind` 改默认监听地址。服务本身不做 TLS，对外暴露请走反向代理；限流每 IP 20 请求/秒（429 + `Retry-After: 1`）。
- 明确不在 v1：支付（仍是 stub）、真实多端（微信/飞书/Telegram）、多租户隔离（单用户 dev token）、产物分享撤销的持久化（`POST /v1/artifacts/{id}/revoke` 目前返回 501）。

### 网页客户端（验证 API 用）

daemon 同时托管一个手写网页客户端（无构建工具链，`web/index.html` + `app.js` + `style.css`），证明桌面端与网页端共用同一套接口：

```bash
hatch vault set daemon-api          # 先存 token（或设 HATCH_DAEMON_API_TOKEN）
hatch daemon run --api              # 启动后打开 http://127.0.0.1:8787/
```

打开页面后把 token 粘贴进顶部的输入框保存（只存在浏览器 localStorage），即可用会话（SSE 流式）、审批、记忆检索、任务四个面。静态路由只做 fallback：`/v1/...` 与 `/mcp` 永远优先；daemon 启动时定位不到 `web/` 目录时静态返回 404（一句说明），API 不受影响。定位顺序：环境变量 `HATCH_WEB_DIR` → 从 daemon 二进制向上找 `web/index.html` → 当前目录的 `web/`。

### 目标、点子与动态

```bash
hatch goal create "学英语口语"     # 新建目标：goals/<id>/GOAL.md + files/ + hidden_files/
hatch goal list                    # 关注度：user_action_required（有未读关联产出）/ new_information（48 小时内更新过）/ does_not_need_attention
hatch goal show goal_xxxx
hatch goal complete|pause|resume|abandon goal_xxxx   # 完成/暂停不可逆；放弃是终态
hatch ideas add "每天跑步" --source user
hatch ideas list                   # 已忽略（dismiss）的点子永不出现
hatch ideas dismiss|act idea_xxxx
hatch feed list                    # 每日动态：每条标明来源事实；开场白是固定文本
hatch feed show
```

目标状态机与关注度规则是本仓库自己的设计（ORIGINAL），不是对 Muse 框架行为的主张。点子库用 SQLite（`ideas` 表，与 claims / briefs 同一惯例）；worker 产生的点子记为低置信度（0.3）建议、标明来源，永远不是用户决定。动态与简报共用同一份事实收集（邮件分诊结果、日程、冲突、待审批、提醒），模型只排序措辞、每条必须引用事实编号；没有事实就不生成条目（开场白除外）。动态经 WorkerOutputs 交接，不直接写进聊天。

### 模型路由：缓存与降级阶梯

```bash
hatch doctor    # 末尾有各 workload 的缓存统计（provider 上报的 cached_tokens / prompt_tokens）
```

- **缓存 TTL**：`[routing.cache_ttl_secs] brief = 300`（只接受 300 或 3600）。只有 Anthropic 会按 TTL 发送 `cache_control`（ephemeral，5m / 1h，挂在系统提示的最后一个块上；无系统提示时挂在最后一条 user 文本块上）。OpenAI 系（Chat Completions / Responses）自动缓存、不接受 TTL 参数；Gemini 的显式缓存未实现、TTL 被忽略——这两家上报的 `cached_tokens` 仍会统计。命中率 = `cached_tokens / prompt_tokens`，只统计成功调用上报的值，不推断缓存位置或过期情况。
- **降级阶梯**：`[[routing.fallback]] workload / trigger / action`，trigger 为 `refused_params` / `timeout` / `server_error` / `empty_answer`，action 为 `retry_same` / `next_model` / `stop`。默认阶梯（未配置时）：refused_params → 下一个模型；timeout / server_error → 同模型重试一次再下一个；empty_answer（无正文也无工具调用）→ 直接停止并报 `BackendError`，不记录用量、不烧下一个模型的调用。默认不跨 provider 切换——要换 provider 需显式配置。每次阶梯步骤都记入 router 事件日志（`ladder <workload> level=<n> <action>: …`）。trigger 的分类是对已有错误种类 + 报错文本的启发式判断，不是 provider 语义。

`hatch init` 创建的默认任务均为停用状态。systemd 示例见 [contrib/systemd/hatch-daemon.service](contrib/systemd/hatch-daemon.service)。

### 常用命令

| 命令 | 作用 |
|---|---|
| `hatch chat / ask [--stream] [--workspace DIR] [--resume ID]` | 对话 |
| `hatch triage run [--dry-run]` / `triage last` | 邮件分诊与回执 |
| `hatch approvals list/show/approve/reject/history` | 审批队列 |
| `hatch grants add/list/revoke` | 常设授权 |
| `hatch memory add/search/explain/correct/forget/people/person` | 记忆与联系人 |
| `hatch memory extract [--session <id>]` | 从会话记录提炼持久记忆（无模型时跳过；冲突只提议不写） |
| `hatch calendar import/list` | 日历：.ics 单向导入与列表 |
| `hatch calendar sync [--dry-run] [--chat ID]` | CalDAV 双向同步（冲突时服务器获胜） |
| `hatch calendar conflicts` / `resolve <uid> --keep local\|server` / `status` | 同步冲突处理与状态 |
| `hatch agents list/runs/show` | 子代理类型与运行记录 |
| `hatch session list/show/fork/rewind` | 会话 |
| `hatch checkpoint list/show/rewind` | 文件检查点 |
| `hatch jobs add/list/show/enable/disable/remove/run-now/history` | 定时任务 |
| `hatch brief show/list` | 每日简报 |
| `hatch goal create/list/show/complete/pause/resume/abandon` | 目标 |
| `hatch ideas list/add/dismiss/act` | 点子库 |
| `hatch feed list/show` | 每日动态 |
| `hatch daemon run/status/stop` | 常驻进程 |
| `hatch browser status/sessions/takeover/resume/close` | 浏览器进程与会话 |
| `hatch skills list/show/load/unload` | 技能：查看与激活 |
| `hatch mcp list/test/call` | MCP 服务器（stdio/HTTP）：测试与调用 |
| `hatch mcp serve`（`server` 同义） | 以 MCP 服务端运行（stdio/HTTP）：把 hatch 工具暴露给本地 MCP 客户端；HTTP 支持按客户端 token 与 in-process TLS |
| `hatch mcp token create/list/revoke` | 按客户端 token：创建（明文只显示一次）、列表、撤销（立即生效） |
| `hatch artifact build/list/show/share/deliver` | 产物：构建带版本的文档/HTML、查看、提交分享审批、交付（交付消息含文件路径） |
| `hatch vault set/list/delete/status/stop/oauth` | 凭据保管（helper 进程 + 0600 文件；list 只显示名字；oauth 为 mock 端点 scaffold） |
| `hatch connectors list/status/call` | 连接器：清单、状态检查与调用（写入类需 --approve） |
| `hatch eval [--world DIR] [--live]` | 沙盒评估 |
| `hatch doctor` | 自检 |

### 技能、MCP 与凭据保管

**技能**是放在 `$HATCH_HOME/skills/<名称>/SKILL.md`（或 `./skills/`）的
Markdown 文件：开头 `---` 围起的 frontmatter（`name` / `description` /
`tools` / `scope` / `version`，手写解析，不引 YAML 库），后面是正文。
`hatch skills list` 只看名称与说明；`hatch skills load <名称>` 激活后，
对话每轮会把正文放进上下文的一个稳定层（在记忆之后、后台更新之前），
并把本轮可用工具过滤到技能的 `tools` 列表（`list_skills` / `load_skill`
永远保留）。对话里也可以用 `load_skill` 工具现加载，allowlist 从当轮
起生效。技能是用户自己装的工作方式说明：它不能授予新工具，不能绕过
审批，也不能把正文里的要求当作用户的要求。示例见
`skills/example-brief/SKILL.md`。

**MCP 客户端**支持 stdio 与 HTTP/SSE 两种传输（JSON-RPC 2.0，协议版本
`2024-11-05`）：在 `config.toml` 里加 `[[mcp_servers]]`（`name` /
`transport`（`stdio` 默认 / `http`）/ `command` / `args` / `env`——只写
变量名——/`url`（http）/ `headers`（http，只写变量名）/ `token_ref` /
`timeout_secs` / `enabled`），对话启动时连接已启用的服务器，远程工具以
`mcp_<服务器>_<工具>` 的名字注册。HTTP 传输向 `url` POST JSON-RPC，接受
普通 JSON 回复或 SSE 流；`token_ref` 有值时作为 `Authorization: Bearer`
发送，header 值永不记入日志。服务器返回的内容一律视为不可信外部内容；
调用一律走审批队列（`mcp_call`），策略允许时自动执行并记入审计，执行
结果标为不可信。`hatch mcp test <服务器>` 做 ping + 列工具/资源/提示词；
`hatch mcp call` 只读工具直接执行，写入类工具需要 `--approve` 并在输出
里明确写出确认。token 经 vault helper `reveal` 在生成子进程的瞬间传入
（见上）；helper 起不来时回退到文件读取并在输出里明确警告。暂不支持
WebSocket 传输，也不支持以 HTTP 方式作为 MCP 服务端。

**MCP 服务端**（`hatch mcp serve`，`hatch mcp server --stdio` 同义）把
hatch 的工具通过 stdio 暴露给 MCP 客户端：内置工具 + 文件/命令/网页工具
+ 产物工具（`show_options` / `create_artifact` / `share_artifact`）
+ 连接器工具（`connector_*`）+ 已启用的浏览器工具；不暴露 `mcp_*` 客户端
工具（会嵌套审批）与技能管理工具（`list_skills` / `load_skill`，只属于当
轮对话）。每次 `tools/call` 都走与对话相同的门禁/审批路径，上下文标记为
已污染；需要审批的调用不执行，返回 `structuredContent.status ==
"approval_required"` + `approval_id` + 门禁原因，调用方用 `approval_status`
工具轮询（用户在 hatch 里用 `hatch approvals approve <id>` 批准后，轮询
会看到 `executed`）。只读调用直接执行。所有结果的
`structuredContent.trust` 标注信任级别（`system` / `untrusted`）；产生
widget 的工具还在 `structuredContent.widget` 里附上卡片 JSON（含
`embed_token`），客户端只在 token 出现在消息里时才渲染卡片，否则只显示
`content` 里的纯文本。资源
`hatch://people` 与 `hatch://memory` 是真实的联系人与记忆列表（各限 100
条）；`prompts/list` 诚实返回空列表。每个调用有超时（默认 60 秒，可用
`--timeout-secs` 改）；stdin 关闭或 Ctrl-C/SIGTERM 时干净退出。HTTP 传输另
有按客户端 token（`hatch mcp token create <名字> [--scope read|write]`，
注册表 `<home>/mcp-clients.json` 0600 只存哈希，撤销下一个请求立即生效；
审批 origin 为 `mcp:http:<客户端名>`，只读 scope 只能调读类工具——按
`ActionClass` 判定：`ReadPersonal`/`FetchWeb`/`BrowseLocal` 是读，其余是写）
与 in-process TLS（`--tls-cert/--tls-key`；非回环绑定默认要求 TLS + token，
或显式 `--allow-plain-http`）。旧的共享 token（vault `mcp-http` /
`HATCH_MCP_TOKEN`）仍可用，是过渡方案。威胁模型：
stdio 服务端没有逐客户端认证，任何能连上它 stdin/stdout 的客户端都能调
用已暴露的工具——只给可信的本地客户端用；读记忆/邮件靠门禁与审批挡住，
详见 [docs/SECURITY.md](docs/SECURITY.md)。

**凭据保管（vault helper 进程）**：token 以记录形式存在
`$HATCH_HOME/vault/tokens.json`（目录 0700，文件 0600），运行时由独立的
`hatch-vault-helper` 进程持有；agent 进程只持有 token 名字。`hatch vault set
<名字>` 从 stdin 或 `--from-env VAR` 读值，永不从命令行参数或配置文件读；
`hatch vault list` 只显示名字（附过期标记）。连接器 / MCP 需要 token 时，
helper 把值直接设进子进程的环境（`inject`），或自己替 agent 做 HTTP 调用
（`request`，Bearer 头由 helper 加）；MCP 长连接 stdio 需要父进程持有管道，
是例外：`reveal` 只在生成子进程的瞬间把值交给父进程，随后立即丢弃。
helper 随父进程死亡（PDEATHSIG + 父 pid 轮询）、空闲 5 分钟自动退出，
也可 `hatch vault stop` 手动停止；`hatch doctor` 检查 helper 状态与文件权限。
威胁模型见 [docs/SECURITY.md](docs/SECURITY.md)：helper 防的是 token 落盘明文
被随手读到、被日志/审批记录带走，不是沙箱——被攻破的 agent 进程仍可要求
helper *使用* token。

**连接器框架**：连接器是子进程 CLI，用 `$HATCH_HOME/connectors/<名字>.toml`
清单描述（`command` / `args` / `env` 变量名 / `token_ref` / `timeout_secs` /
ops 表：名字、说明、`read_only`、参数 schema、状态检查命令与连接指引）。
一次调用 = 一个进程：runner 写一行 JSON 请求（`{op, args, request_id}`），
读一行 JSON 响应（`{ok, result | error{code, message}}`），超时杀整个进程组。
操作以 `connector_<连接器>_<操作>` 注册进工具表，调用一律走审批队列
（`connector_call`），只读操作门禁放行后自动执行，写入类操作按策略排队；
结果一律标为不可信。`hatch connectors status <名字>` 做状态检查：未连接时
只给连接指引，绝不承诺结果。只带两个参考连接器：本地文件笔记
（`hatch-notes-connector`）与本地 mock HTTP（`hatch-mockhttp-connector`，
只连测试时启动的本地 mock 服务器）；不做真实供应商连接器。

### 产物呈现：widget 与 artifact

**widget** 是助手可以给出的小卡片，三种类型：`options`（选项列表，每项
有 id、label 与回复文本）、`list`（标题/副标题/链接的行）与 `html`
（HTML 卡片，allowlist 净化后才存）。每个 widget 带一个 `embed_token`：
卡片在 token 出现在用户可见的消息里之前是**惰性**的——没有 token 只渲染
纯文本版，有 token 才渲染卡片。CLI 没有界面，永远只渲染纯文本版；纯文本
版不丢信息（全部选项、回复、行与链接都在文本里）。MCP 客户端在
`structuredContent.widget` 里收到卡片 JSON（与已有的 `trust` 标签并列），
合规的客户端只在 token 在消息里时才渲染卡片。工具 `show_options` 产生
options 卡片；对话里工具结果的文本就是 fallback，模型可以直接引用它。

**artifact** 是带版本的交付物，只支持 `document`（markdown/文本）与
`html`（构建时同样净化）两种类型，其他类型不在范围。文件放在
`$HATCH_HOME/artifacts/<id>/v<n>/`：每个版本一个目录（内容文件 +
`manifest.json`：id、标题、类型、版本、创建时间、来源、SHA-256 校验和），
新版本永不覆盖旧版本；`meta.json` 记录标题、版本列表、是否已分享、是否已
交付。流程是构建 → 分享 → 批准 → 交付：

```bash
hatch artifact build "周报" --kind document --content "# 第一周"   # v1
hatch artifact build "周报" --kind document --content "# 第二周" --id art_xxx  # v2，v1 保留
hatch artifact list / hatch artifact show art_xxx [--version 1]
hatch artifact share art_xxx     # 只提交分享审批，不分享
hatch approvals approve apr_xxx  # 批准后才标记已分享
hatch artifact deliver art_xxx   # 打印含文件路径的交付消息，并标记已交付
```

规则：写文件不等于交付——只有交付步骤跑完（消息里含可用路径）`delivered`
才为真；未分享的 artifact 不能交付；分享是对外动作，永远走审批队列
（`share_artifact`，类别 `SendMessage`）。构建前用 `hatch-core` 的秘密
检测扫描内容，命中则拒绝写入且错误里只写秘密种类、不回显值；`--from`
读工作区文件时走 Workspace 守卫。助手侧的 `create_artifact` /
`share_artifact` 工具同样走门禁 → 审批 → 执行器；`create_artifact` 是
`WriteFile` 类，默认需审批。`deliver_artifact` 工具是 CLI `deliver` 的
审批版：批准后执行器调用同一个 `store.deliver`，返回的交付消息（含绝对
路径）由模型原样告诉用户。详情见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
与 [docs/SECURITY.md](docs/SECURITY.md) §9f。

### 多通道传输：主 chat、side chat 与 pipe 参考传输

设计依据是 Muse agent 框架（深挖文档 §9.4 多通道、§12 设备；路线图
R13 / RW-16）：主 chat 与 side chat 共享记忆与目标，但各有独立的
transcript；外部聊天里的 worker 只替用户准备内容，消息经主 agent 转述
给用户，worker 不能替用户做决定；每个外部聊天有一份 `DISCRETION.md`
分享规则（用户在对话中随口说的规则会被记录为该聊天的长期规则）；设备
能力是声明式的，缺失时如实告知开通路径、不假装能做。文件布局、pipe
传输的 JSON 形状、redact 的行模式、默认能力清单是本仓库自己的设计
（ORIGINAL），不是 Muse 的行为——见 `hatch-transports` 各模块的文档。

唯一实现的传输是 **pipe**（本地文件模拟一个聊天应用，无网络、无账号、
无真实 SDK）：收件箱 `$HATCH_HOME/transports/pipe/inbox/` 里一个 JSON
就是一条消息（`{"chat": "family", "sender": "alice", "text": "你好",
"image": "photo.jpg"}`），`hatch transport poll pipe` 为每条消息跑一轮
主 agent 回合、按该聊天的分享规则检查后把回复写进发件箱，处理过的文件
进 `processed/`，解析失败的进 `failed/`，每次发送记一行 `send.log`。
图片只以文件引用（文件名 + mime + 大小）交给 agent，agent 看不到图片
内容、被问到时必须如实说看不到；图片内容绝不是指令。

```bash
hatch transport list                                  # pipe + 未实现的真实通道（只给指引）
hatch transport poll pipe                             # 轮询一次
hatch transport run pipe [--interval-secs 5]          # 持续轮询，Ctrl-C 停
hatch transport chats                                 # 各聊天的规则与待转述草稿
hatch transport discretion show family
hatch transport discretion set family --rule "forbid: mail" --rule "forbid: other_people"
hatch transport device list / show local / check local computer.control
```

规则：秘密对任何聊天都不发送（规则文件写不写都一样）；带标签的违禁内容
（本轮用了邮件工具，回答就可能含邮件内容）整条拦截；未带标签的文本只
对可定位的行做 redact（引用的邮件头、`张三说：` 式转述），删完没剩可发
内容时拦截。worker 的草稿只存在 `chats/<chat-id>/drafts/` 里等主 agent
决定，没有决定就永远不发；主 agent 转述时可以改写，改写后的文本按主
agent 自己的话重新检查。**微信 / 飞书 / Telegram 本阶段未实现**，没有真实
接入、没有账号与凭据；未来实现时必须先满足 SECURITY.md §9g 的要求
（webhook 签名校验、按聊天鉴权、每聊天速率限制）。详情见
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) §4h 与
[docs/SECURITY.md](docs/SECURITY.md) §9g。

### CalDAV 双向同步

`hatch calendar sync` 把本地日历与 CalDAV 服务器双向同步：先按
sync-token 拉取服务器变化（没有 token 时全量拉取），再把本地新增 /
修改 / 删除推送上去；重跑一次没有变化时不发任何 PUT/DELETE（幂等）。
配置在 `[caldav]`（见 `config/hatch.example.toml`）：服务器 URL 与
用户名是明文配置，凭据只存 vault 里的 token 名字
（`hatch vault set caldav`），绝不在配置文件里写 token 值；真实服务器
必须用 `https://`，`http://` 只允许本地 mock（`allow_http = true` 且
127.0.0.1/localhost）。

**冲突规则（写死在代码里）：服务器获胜。** 同一个 UID 两边都改过时，
保留服务器版本，本地版本记进冲突记录（UID、两边的更新时间、保留了哪
边），绝不悄悄用本地版本覆盖服务器，也绝不悄悄丢掉本地修改。用户用
`hatch calendar resolve <uid> --keep local` 明确批准后，本地版本才在
下次同步时推送上去；`--keep server` 则接受已保留的服务器版本（服务器
删过、本地改过的，`--keep server` 会删掉本地副本）。删除冲突同样处理。

```bash
hatch calendar sync [--dry-run] [--chat family]   # --dry-run 只列计划，不写库、不发请求；--chat 把摘要经 Relay 发到指定聊天
hatch calendar status                              # 服务器、最近同步、sync-token、待推送的本地变化
hatch calendar conflicts                           # 未解决的冲突
hatch calendar resolve <uid> --keep local|server   # 处理一个冲突（本身不推送，下次同步推送）
hatch doctor                                       # 也会检查 CalDAV 配置 + token + 服务器可达性（只探活，不同步）
```

同步**不做**的事：不展开循环事件（拉取时只取 DTSTART/DTEND 生成一条
本地事件；本地编辑过循环事件再推送时，RRULE 会丢失——已知局限，
不要拿它同步重要的循环日程）、不同步参会人回复状态与附件（除非 .ics
里本身带有）、不做自动重试风暴（瞬时 5xx 最多重试一次）。`hatch calendar import` 仍是单向导入，不会
同步回服务器。详情见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) §4i
与 [docs/SECURITY.md](docs/SECURITY.md) §9h。

---

## 评估

```bash
hatch eval                                          # pipeline：脚本模型，不调用真实模型
hatch eval --live                                   # live：使用配置的模型（可加 --max-calls N）
hatch eval --world eval/worlds/workspace-basic      # 其他世界：delegate-basic、scheduler-basic、browser-basic
hatch eval --world eval/worlds/en-office            # 英文办公世界
hatch eval --world eval/worlds/thread-attachment    # 长线程 + 附件引用世界
hatch eval --repeat 3                               # 同一世界跑 3 次，报告取均值（仅邮件分诊世界）
```

| 世界 | 考察内容 | pipeline | live（`gpt-6-luna`，单次运行） |
|---|---|---|---|
| `zh-office` | 邮件分类、起草、注入邮件、验证码脱敏、冲突检测、幂等 | 11/11 | 10/11–11/11（分类 10–12/14） |
| `en-office` | 与 zh-office 同形状的英文邮件：分类、起草、注入、验证码脱敏、冲突检测、幂等 | 11/11 | 未跑 |
| `thread-attachment` | 长线程（7 封、中途改决定）、附件只作引用、最终决定准确、附件元数据指令未执行 | 15/15 | 未跑 |
| `workspace-basic` | 文件问答与修改、沙箱命令、文档中的注入指令 | 11/11 | 11/11 |
| `delegate-basic` | 并行委派、取消、深度上限、子代理遇到注入 | 13/13 | 10/10 |
| `scheduler-basic` | 夏令时、停机补跑、失败与超时、简报事实一致性、定时外发授权 | 17/17 | 17/17 |
| `browser-basic` | 页面问答、选项存在性、登录墙、验证码、表单提交审批、结账、页面注入、内网重定向（真实 Chromium + 本地 mock 站点） | 16/16 | 16/16 |

pipeline 模式的脚本模型会在对抗样本上故意"上当"，用来验证结构性防线；它不代表真实模型的能力。详细数字与解读见 [docs/EVALUATION.md](docs/EVALUATION.md)。

分类边界（notification / fyi 与 action / reply）：判断标准不是"有没有日期、动词或'请'字"，而是"有没有点名要你做的具体事项、需不需要你回应"——系统通知、明确无需回复的知会、订阅推送、纯知会不判成待办，带明确提问/确认请求的邮件不被降级。定义与例子在提示词（`prompts/triage_classify_zh.md`）里，确定性规则在 `crates/hatch-triage/src/boundary.rs`（R1–R6），zh-office 的期望逐封核对过，无需改动。

---

## TODO

- [x] 最终阶段 live 评估（全部世界，`gpt-6-luna`）：2026-11-03 尝试时被环境 blocker 拦住——本 VM 的 egress 代理与 rustls 握手不兼容，zh-office live `--repeat 3` 全部传输失败（6/11，**不是模型成绩**）；其余世界未跑、不记为通过。见 docs/EVALUATION.md §7。待在无该代理的环境（或代理修复后）用 `scripts/run_live_eval.py` 补跑。附带修复：`--repeat` 曾累计上报调用/token（96/192/288），已改为每轮差值 + 回归测试。2026-11-04 用户在 Colab 跑 zh-office live 又得 6/11，这次是网关 400 拒收 `temperature` 参数（`gpt-6-luna` 推理模型不支持）；provider 层已修（400 点名 temperature 时重发一次不带该参数，`chat`/`chat_stream` 均覆盖，stub 网关回归测试已过），但 live 重跑验证尚未做——下一次在 Colab/Mac 上的重跑才是真正的验证。2026-11-04 Colab 重跑仍是 6/11，这次是网关 400 拒收 `text.format json_object`（"input messages must contain the word 'json'"：词 "JSON" 只在系统提示词里，而 Responses API 把系统消息放进 `instructions`、不在 `input` 里）；provider 层已修（`json_mode` 时若 input 里没有 "json"，把 "请以 JSON 对象回复。" 追加到最后一条 user 消息，`build_body` 回归测试 5 项已过），待下一次 Colab/Mac 重跑验证。2026-11-04 晚用户在 Colab 跑完全量：7 个世界 89/91（zh-office 11/11 且 repeat 3 次稳定、en-office 11/11、workspace-basic 11/11、delegate-basic 10/10、browser-basic 16/16；thread-attachment 14/15、scheduler-basic 16/17，各一处未过，均为检查侧/引用精度问题，见 docs/EVALUATION.md §7）。
- [x] 连接器框架：连接器 manifest、状态检查、OAuth 授权流程（Phase 2：框架 + 两个参考连接器 + vault helper 进程；OAuth 只对 mock 端点）
- [x] MCP 服务端：把 hatch 的工具通过 MCP 暴露出去（stdio + streamable HTTP）
- [x] MCP 服务端 over HTTP（streamable HTTP）：与 stdio 服务端共用门禁/审批路径（`hatch mcp serve --transport http`；协议 2025-03-26；默认只绑 127.0.0.1，非回环绑定必须配 bearer token + TLS（或显式 `--allow-plain-http`）；每 IP 限流 20 请求/秒，会话上限 32）；按客户端 token（`hatch mcp token create/list/revoke`，注册表 `<home>/mcp-clients.json` 0600 只存哈希，撤销下一个请求立即生效，审批 origin 为 `mcp:http:<客户端名>`，scope 按 `ActionClass` 分只读/可写）；旧的共享 token（vault `mcp-http` / `HATCH_MCP_TOKEN`）仍可用，过渡方案
- [x] 计划模式：先出计划、批准后执行（Phase 1 已落地；Phase 2 把连接器工具也提供给计划器）
- [x] 产物呈现：卡片式结果（widget）、产物的构建 → 版本 → 分享审批 → 交付
- [x] 多通道入口（Phase 5）：transport 接口 + pipe 参考传输 + 每聊天 DISCRETION.md + worker 转述规则 + 设备能力清单；微信 / 飞书 / Telegram 的真实接入仍未实现（OUT OF SCOPE）
- [ ] 多通道入口：企业微信 / 飞书 / Telegram 的真实接入（需先满足 SECURITY.md §9g：webhook 签名校验、按聊天鉴权）
- [x] CalDAV 双向同步（Phase 6）：PROPFIND 发现、sync-token 增量同步、PUT/DELETE 推送、幂等重跑、服务器获胜的冲突规则 + `resolve --keep local` 明确批准；凭据只经 vault helper；只在本地 mock 服务器上测过，没有真实 CalDAV 账号
- [ ] 支付（明确延期，见 SECURITY.md §9i）：无钱包、无结账、无提交动作；`ActionClass::Payment` 上限 Suggest，gatekeeper 一律 Deny（只建议、不执行）；浏览器结账点击已映射到 `ActionClass::Payment`；扩展点是 `crates/hatch-connectors/src/payment.rs`（stub，调用即拒绝）——接真实支付商时经连接器框架 + vault `token_ref` 接入，不动 agent loop / gatekeeper
- [ ] 其它应用真实接入（明确延期）：除两个参考连接器（notes、mockhttp）、CalDAV（Phase 6）与 pipe 参考传输外，一律未接入；扩展点见 ARCHITECTURE.md §7（连接器 manifest + runner + 状态检查、传输接口、MCP 注册）；微信 / 飞书 / Telegram 等按需再接，`hatch doctor` 如实报告未接入状态
- [x] 分类提示词的类别边界（notification / fyi 与 action）——Phase 7：提示词加"类别边界"一节（定义 + 例子），代码后处理 `crates/hatch-triage/src/boundary.rs` 规则 R1–R6；zh-office 期望逐封核对，无需改动
- [x] 扩充评估世界：更多语言风格、长线程、附件，多次运行取均值——Phase 7：新增 `en-office`（英文办公）与 `thread-attachment`（长线程 + 附件引用）两个世界，`hatch eval --repeat N` 报告均值
- [x] 目标 / 点子 / 动态产品面（Stretch）：新 crate `hatch-goals`——目标工作区（GOAL.md + files/ + hidden_files/，状态机 + 关注度）、点子库（SQLite，dismiss 永不出现，worker 建议低置信度）、每日动态（单提示词 worker，每条标明来源事实，固定开场白，经 WorkerOutputs 交接）；`hatch goal/ideas/feed` CLI + `daily-feed` 默认任务（停用）
- [x] 模型路由深度（Stretch）：按 workload 的缓存 TTL 配置（Anthropic 300/3600 生效，其余忽略但仍统计上报值）、`hatch doctor` 各 workload 缓存统计（命中率 = cached / prompt）、`[[routing.fallback]]` 降级阶梯表（默认 refused_params → 下一模型、timeout/5xx → 重试一次再下一模型、empty_answer → 停止并报 BackendError），阶梯步骤记入事件日志

---

## 许可证

MIT，见 [LICENSE](LICENSE)。
