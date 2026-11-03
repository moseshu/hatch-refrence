# hatch — 个人 Agent 助手

hatch 是一个可以部署在云端主机或个人电脑上的个人 Agent 助手（personal agent）。它常驻运行，管理你的邮件、日程、记忆和工作区，能在对话中调用工具、把任务委派给子代理、按计划在后台工作，并且把每一个对外动作都放进可审计、可撤销、需要你确认的执行通道里。

## 能力概览

| 能力 | 说明 |
|---|---|
| 邮件与日程分诊 | 拉取新邮件（IMAP，兼容 126/163）、隔离分类与摘要、识别可疑邮件、会议冲突检测与空闲时段、按你的风格起草回复 |
| 对话与工具 | 记忆、日历、邮件检索；工作区内的文件读写与搜索；沙箱中执行命令；读取网页、可选搜索 |
| 子代理 | 把独立任务委派给 explore / execute / research 类型的子代理，可并行；工具只能收窄，审批统一排队 |
| 后台运行 | daemon 按计划运行任务：每日简报、定时分诊、记忆整理；产出在下次对话时作为「后台更新」交给助手 |
| 记忆 | 结构化 claim（来源、信任、置信度、有效期）、纠正链与溯源、中文 BM25 检索、人物页、遗忘 |
| 安全执行 | 信任标签与污染规则、门禁 → 审批 → 执行器 → 幂等账本、命令沙箱、文件检查点、秘密检测与脱敏 |
| 多模型 | Chat Completions / Responses / Anthropic Messages / Gemini 四种协议，均支持工具调用与流式输出；按任务路由与降级 |
| 可中断 | Ctrl-C 停止当前回合及其所有子代理和命令；会话可继续、复制、回退 |
| 可度量 | 内置沙盒评估世界，pipeline（脚本模型）与 live（真实模型）两种模式 |

---

## 架构

```
                  ┌──────────────────────── hatch-cli（hatch 命令）────────────────────────┐
                  │ init · chat/ask · triage · approvals · grants · memory · calendar ·    │
                  │ agents · session · checkpoint · jobs · brief · daemon · eval · doctor  │
                  └──────────────┬───────────────────────────────┬─────────────────────────┘
  调度层          hatch-scheduler（调度·任务存储·运行引擎·daemon） · hatch-workers（简报·定时分诊·记忆整理）
                                 │                               │
  场景层          hatch-triage（邮件+日程分诊）   hatch-agent（对话循环·上下文·压缩·子代理）   hatch-eval（评估）
                                 └───────────────┬───────────────┘
  能力层                          hatch-tools（工具 SDK·内置工具·执行器·统一 dispatch·检查点）
                                                 │
  领域层          hatch-policy（门禁·审批·账本）  hatch-memory（记忆）  hatch-connectors（邮件·日历）  hatch-llm（模型）
                  hatch-sandbox（命令沙箱）
  基础层          hatch-store（SQLite）  ·  hatch-core（错误·ID·信任·token·秘密·取消令牌）
```

| crate | 职责 |
|---|---|
| `hatch-core` | 错误类型、内容寻址 ID、信任标签与不可信信封、CJK 感知 token 估算、秘密检测与脱敏、取消令牌 |
| `hatch-store` | 单文件 SQLite、幂等迁移、事务、可被并行子代理共享的连接、自动脱敏的审计日志 |
| `hatch-llm` | 四种模型协议与流式输出、按 workload 路由、降级阶梯、调用预算、代理与自定义 CA |
| `hatch-memory` | claim 记忆、纠正链与 explain、中文 BM25、遗忘、人物页、合并与过期 |
| `hatch-policy` | 动作类别 × 可逆性 × 自主度、门禁规则、常设授权、异步审批队列、幂等账本、注意力预算 |
| `hatch-connectors` | IMAP / SMTP / MIME（含 GBK）、夹具邮箱与 outbox、ICS 解析、冲突与空闲时段 |
| `hatch-sandbox` | 进程沙箱：命名空间、pivot_root、capability 清空、no_new_privs、seccomp、rlimit、超时杀进程树 |
| `hatch-tools` | Tool SDK、邮件/日历/记忆/文件/命令/网页工具、工作区路径守卫、检查点、执行器、门禁 → 审批 → 执行的统一入口 |
| `hatch-agent` | 行为规约、分层上下文组装、压缩、会话（fork / rewind）、可取消的回合循环、子代理 |
| `hatch-triage` | 分诊工作流：隔离分类、代码后处理、会议处理、起草、持久化步骤、回执 |
| `hatch-scheduler` | 调度表达式（夏令时正确）、任务 / 运行记录 / 去重分表、看门狗与重试、自动停用、daemon |
| `hatch-workers` | 每日简报、定时分诊、记忆整理、后台产出交接 |
| `hatch-eval` | 评估世界与报告（分诊、工作区、子代理、调度） |
| `hatch-cli` | 命令行、配置与环境变量覆盖、Ctrl-C 处理 |

细节见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

---

## 核心机制

**上下文组装。** 每轮请求由固定顺序的层组成：行为规约 → 用户画像与偏好记忆 → 检索到的记忆 → 后台更新 → 会话历史。预算按 CJK 感知的 token 估算裁剪，稳定层不丢弃；历史过长时在用户消息边界压缩，保留最近的尾部。

**记忆。** 记忆以 claim 为单位，记录来源、信任级别、置信度和有效期。纠正不覆盖历史，而是形成可追溯的纠正链；推断出的和来自不可信来源的记忆置信度有上限；含秘密的内容拒绝写入。检索使用中文 unigram + bigram 的 BM25，只返回当前有效的记录。

**门禁、审批与幂等。** 每个工具调用先由门禁根据动作类别、自主度、常设授权、污染状态和信息流规则决定：直接执行、进入审批队列或拒绝。审批不阻塞对话；批准后执行的就是审批时看到的内容（payload 哈希校验）；过期的审批永不执行。每个对外动作都有内容寻址的幂等键，崩溃后状态不明时不重发而是如实报告。

**信任与污染。** 进入模型的每段文本都带信任标签；邮件、网页、文件内容包进不可信信封。上下文一旦含有不可信内容，所有对外、写入和执行类动作都强制审批，常设授权默认也不能豁免。分诊分类使用无工具的隔离模型，回复收件人由代码固定。

**命令沙箱与检查点。** `shell_run` 在独立的 user / mount / net / pid / ipc / uts 命名空间中以 nobody 身份运行：只读系统目录 + 可写工作区、无网络、seccomp 与 rlimit 限制、超时杀整棵进程树。宿主不支持时直接报错，不会退化为无隔离执行。每次写文件前自动建立检查点，可随时回退。

**子代理与取消。** 主代理可调用 `delegate` / `delegate_parallel`。子代理只拿任务书、看不到主对话，工具清单只能比主代理少，继承污染状态，审批进入同一队列并标注来源，共享调用预算，嵌套深度有上限；结束后交回结论、有界的过程摘录和用量。取消令牌从主代理传到每个子代理、模型调用、沙箱命令和网页读取；取消是独立的终态，已生效的修改不回滚，回执写明停在哪里。

**调度与后台 worker。** 支持 `daily` / `weekly` / `every` / `once` / `cron` 表达式，按用户时区计算并正确处理夏令时。停机期间错过的触发只记录，最多补跑一次；失败退避重试，连续失败自动停用并通知；每次运行有时间上限。定时任务的授权范围只能收窄：范围外的对外动作一律进审批。每日简报的事项由程序从你的数据收集，模型只负责排序和措辞，每一条必须引用事项编号，没有依据的条目会被丢弃。

**模型协议与流式。** 同一套抽象支持 Chat Completions（DeepSeek、通义千问兼容模式、Moonshot、vLLM、Ollama 等）、OpenAI Responses、Anthropic Messages 和 Gemini，均支持工具调用、JSON 模式、`reasoning_effort` 与 cached / 推理 token 统计。流式输出拼出的结果与非流式一致；服务端拒绝流式时自动改为非流式重试。分类、起草、压缩、简报等任务可分别路由到不同模型，出错时按降级阶梯切换。

---

## 安全模型要点

| 机制 | 说明 |
|---|---|
| 自主度上限 | 不可逆动作（发送、删除、执行命令）最高只能到"起草待批"，支付只能建议 |
| 信息流规则 | 新收件人、携带日程详情或他人消息外发都需要审批；带秘密的内容直接拒绝 |
| 秘密处理 | 记忆拒绝秘密；通知、摘要、审批、审计日志统一脱敏；密钥只从环境变量读取 |
| 网页访问 | 只允许公开 https；拒绝内网、本机、链路本地和云元数据地址，重定向逐跳检查 |
| 工作区守卫 | 文件工具只认工作区内的路径，拒绝 `..` 逃逸和指向外部的符号链接，原子写入 |
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

### 6. 定时任务与 daemon

```bash
hatch jobs list
hatch jobs enable daily-brief      # 每天 08:30 生成每日简报
hatch jobs enable triage-sync      # 每 30 分钟分诊新邮件
hatch jobs enable memory-flush     # 每天整理记忆
hatch daemon run                   # 前台常驻；hatch daemon stop 或 SIGTERM 优雅退出
hatch brief show
```

`hatch init` 创建的默认任务均为停用状态。systemd 示例见 [contrib/systemd/hatch-daemon.service](contrib/systemd/hatch-daemon.service)。

### 常用命令

| 命令 | 作用 |
|---|---|
| `hatch chat / ask [--stream] [--workspace DIR] [--resume ID]` | 对话 |
| `hatch triage run [--dry-run]` / `triage last` | 邮件分诊与回执 |
| `hatch approvals list/show/approve/reject/history` | 审批队列 |
| `hatch grants add/list/revoke` | 常设授权 |
| `hatch memory add/search/explain/correct/forget/people/person` | 记忆与联系人 |
| `hatch calendar import/list` | 日历 |
| `hatch agents list/runs/show` | 子代理类型与运行记录 |
| `hatch session list/show/fork/rewind` | 会话 |
| `hatch checkpoint list/show/rewind` | 文件检查点 |
| `hatch jobs add/list/show/enable/disable/remove/run-now/history` | 定时任务 |
| `hatch brief show/list` | 每日简报 |
| `hatch daemon run/status/stop` | 常驻进程 |
| `hatch eval [--world DIR] [--live]` | 沙盒评估 |
| `hatch doctor` | 自检 |

---

## 评估

```bash
hatch eval                                          # pipeline：脚本模型，不调用真实模型
hatch eval --live                                   # live：使用配置的模型（可加 --max-calls N）
hatch eval --world eval/worlds/workspace-basic      # 其他世界：delegate-basic、scheduler-basic
```

| 世界 | 考察内容 | pipeline | live（`gpt-6-luna`，单次运行） |
|---|---|---|---|
| `zh-office` | 邮件分类、起草、注入邮件、验证码脱敏、冲突检测、幂等 | 11/11 | 10/11–11/11（分类 10–12/14） |
| `workspace-basic` | 文件问答与修改、沙箱命令、文档中的注入指令 | 11/11 | 11/11 |
| `delegate-basic` | 并行委派、取消、深度上限、子代理遇到注入 | 13/13 | 10/10 |
| `scheduler-basic` | 夏令时、停机补跑、失败与超时、简报事实一致性、定时外发授权 | 17/17 | 17/17 |

pipeline 模式的脚本模型会在对抗样本上故意"上当"，用来验证结构性防线；它不代表真实模型的能力。详细数字与解读见 [docs/EVALUATION.md](docs/EVALUATION.md)。

---

## TODO

- [ ] 浏览器子系统：按来源隔离的浏览器、页面读取与交互、提交类动作审批、登录与验证码交由用户接管
- [ ] 连接器框架：连接器 manifest、状态检查、OAuth 授权流程、凭据保管（vault）
- [ ] Skills / MCP：技能加载、MCP 客户端与服务端
- [ ] 计划模式：先出计划、批准后执行
- [ ] 产物呈现：卡片式结果（widget）、产物的构建 → 版本 → 分享审批 → 交付
- [ ] 多通道入口：企业微信 / 飞书 / Telegram，每个通道独立会话与分享规则
- [ ] CalDAV 双向同步
- [ ] 分类提示词的类别边界（notification / fyi 与 action）
- [ ] 扩充评估世界：更多语言风格、长线程、附件，多次运行取均值

---

## 许可证

MIT，见 [LICENSE](LICENSE)。
