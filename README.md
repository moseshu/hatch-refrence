# hatch-refrence — 个人 Agent 助手

hatch 是一个运行在你自己机器上的个人助理。第一个落地场景是**邮件 + 日程分诊**：拉取新邮件、分类摘要、发现会议冲突、按你的风格起草回复，然后把所有对外动作放进审批队列，等你确认后才执行。

对话中还可以使用**通用工具**：在指定的工作区里读写文件、在隔离沙箱中执行命令、读取公开网页。写文件和执行命令同样走审批，每次改文件前自动建立检查点，可以随时回退。

它的设计重点不是"能做多少事"，而是**做事的方式可信**：

- **外部内容永远只是数据。** 邮件正文进入模型前会被脱敏并包进不可信信封。回复收件人由代码按原邮件固定，模型改不了。
- **对外动作只有一条路径**：门禁 → 审批队列 → 执行器 → 幂等账本。不会阻塞等待，不会重复发送，过期的审批永不执行。
- **说实话。** 每次运行都给出三段式回执：已完成 / 未完成 / 待你决定。"已提交审批"不会被说成"已发送"。
- **能拆活，也能叫停。** 主代理可以把独立任务委派给子代理（可并行），子代理只拿到任务书、工具只会更少、审批照样排队；按一次 Ctrl-C，当前回合连同所有子代理和正在运行的命令一起停下，回执说明停在哪里，会话可以接着聊。
- **命令关在沙箱里。** `shell_run` 在独立的用户/挂载/网络/进程命名空间中以 nobody 身份运行，看不到工作区以外的文件，没有网络，系统调用受 seccomp 限制。宿主不支持时直接报错，不会悄悄裸跑。
- **可度量。** 内置三个评估世界：邮件分诊（16 封合成邮件，含注入攻击与验证码）、工作区（读文件、改配置、跑检查脚本、藏有注入指令的文档）和子代理（并行拆分、取消正在运行的子代理与命令、深度上限、子代理遇到注入），每次改动都能跑出数字。

---

## 架构

```
                    ┌──────────────────────────── hatch-cli（hatch 命令）────────────────────────────┐
                    │ init · triage · approvals · grants · memory · calendar · chat/ask · agents · session · checkpoint · digest · eval · doctor │
                    └───────────────┬──────────────────────────────┬──────────────────────────────┘
                                    │                              │
                 ┌──────────────────▼─────────┐        ┌───────────▼────────────┐      ┌──────────────┐
  场景层         │ hatch-triage 邮件+日程分诊 │        │ hatch-agent 对话循环   │      │ hatch-eval   │
                 │ 隔离分类·起草·冲突·回执    │        │ 规约·上下文·压缩·取消  │      │ 沙盒评估     │
                 │                            │        │ 子代理（委派·并行）    │      │              │
                 └──────────────┬─────────────┘        └───────────┬────────────┘      └──────┬───────┘
                                └───────────────┬──────────────────┘                          │
  能力层                         ┌──────────────▼─────────────────┐                           │
                                 │ hatch-tools 工具·执行器·dispatch │◄──────────────────────────┘
                                 │ 文件·命令·网页·检查点·路径守卫   │
                                 └──────┬───────────┬─────────────┘
                 ┌──────────────────────┼───────────┼─────────────────────────┐
  领域层         │ hatch-policy         │ hatch-memory          │ hatch-connectors        │ hatch-llm
                 │ 门禁·授权·审批·账本  │ claim·纠正·BM25·人物  │ IMAP·SMTP·MIME·ICS      │ 协议·路由·降级·预算
                 │ ·注意力预算          │                       │                         │
                 └──────────────────────┴───────────┬───────────┴─────────────────────────┘
                 hatch-sandbox：命名空间 · pivot_root · capability 清空 · seccomp · rlimit（shell_run 的执行环境）
  基础层                                 hatch-store（SQLite）  ·  hatch-core（错误·ID·信任·token·秘密·取消令牌）
```

| crate | 职责 |
|---|---|
| `hatch-core` | 错误类型、内容寻址 ID、信任标签与不可信信封、CJK 感知 token 估算、秘密检测与脱敏、取消令牌（父 → 子传播） |
| `hatch-store` | 单文件 SQLite（可重入锁，可被并行子代理共享）、幂等迁移、事务、自动脱敏的审计日志 |
| `hatch-llm` | Chat Completions / Responses / Anthropic Messages / Gemini 四种协议与流式输出、按任务路由、降级阶梯、调用预算、JSON 提取 |
| `hatch-memory` | claim 记忆（来源/信任/置信度/有效期）、纠正链与 explain、中文 BM25 检索、forget、人物页 |
| `hatch-policy` | 动作类别 × 可逆性 × 自主度 L0–L3、门禁规则、常设授权、异步审批队列、幂等账本、注意力预算 |
| `hatch-connectors` | 最小 IMAP（含网易 ID 命令）、SMTP 465、MIME、GBK 解析、夹具邮箱与 outbox、ICS、冲突与空闲时段 |
| `hatch-tools` | Tool SDK、内置工具（邮件/日历/记忆 + 文件/命令/网页）、工作区路径守卫、检查点、执行器、统一 dispatch（门禁 → 审批 → 执行） |
| `hatch-sandbox` | 进程沙箱：user/mount/net/pid/ipc/uts 命名空间、新根目录与 pivot_root、capability 清空、no_new_privs、seccomp 拒绝名单、rlimit、超时杀整棵进程树 |
| `hatch-agent` | 行为规约（`prompts/`）、分层上下文组装、压缩、会话（fork / rewind）、非阻塞且可取消的回合循环、子代理（类型注册表、并行运行、深度上限、运行记录与进度事件、有界 handoff） |
| `hatch-triage` | 分诊工作流：隔离分类、代码后处理、会议处理、起草、持久化步骤、回执 |
| `hatch-eval` | 评估世界、oracle/live 两种模式、安全与幂等指标、报告 |
| `hatch-cli` | `hatch` 命令行、配置与环境变量覆盖、费用护栏、Ctrl-C 处理 |

更多细节见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)、[docs/SECURITY.md](docs/SECURITY.md)、[docs/EVALUATION.md](docs/EVALUATION.md)。

---

## 快速开始

### 1. 构建

```bash
cargo build --release
```

二进制在 `target/release/hatch`，可以拷到 `PATH` 里。

### 2. 初始化

```bash
hatch init          # 创建 ~/.hatch/config.toml 与数据库（可用 HATCH_HOME 改目录）
```

### 3. 配置模型（任选其一）

**方式 A：只用环境变量（不需要配置文件）。** 任何兼容 Chat Completions 的服务都可以，比如 DeepSeek、通义千问兼容模式、Moonshot、智谱、vLLM、Ollama：

```bash
export HATCH_LLM_BASE_URL=https://api.deepseek.com/v1
export HATCH_LLM_MODEL=deepseek-chat
export HATCH_LLM_API_KEY=<你的 key>      # 只从环境变量读取，永不写入文件、日志或报告
```

如果服务用的是其他协议，再加一个 `HATCH_LLM_PROVIDER`（取值见下表，默认 `openai_compatible`）。

**方式 B：配置文件。** 在 `~/.hatch/config.toml` 的 `[[models]]` 里填 `provider` 和 `api_key_env`（只写环境变量名），并可在 `[routing]` 里为分类、起草、压缩分别指定模型和降级顺序。

#### 支持的模型接口协议

| `provider` | 协议 | 请求地址 | 流式 |
|---|---|---|---|
| `openai_compatible`（别名 `openai_chat`） | OpenAI Chat Completions，DeepSeek / 通义千问兼容模式 / Moonshot / 智谱 / vLLM / Ollama 等 | `{base_url}/chat/completions` | ✅ |
| `openai_responses` | OpenAI Responses | `{base_url}/responses` | ✅ |
| `anthropic` | Anthropic Messages | `{base_url}/v1/messages` | ✅ |
| `gemini` | Google Gemini `generateContent` | `{base_url}/v1beta/models/{model}:generateContent`（`base_url` 留空则用官方地址） | ✅ |

四种协议都支持工具调用、JSON 模式、`reasoning_effort`（Gemini 映射为 thinking 预算），以及 cached / 推理 token 统计。流式输出时，最终拼出来的结果与非流式完全一致（文本、工具调用、用量）。服务端拒绝流式请求时会自动用非流式重试一次；已经输出了部分内容后出错则直接报错，不会换模型重来，避免同一个问题出现两个不同的回答。

模型请求会遵守 `HTTPS_PROXY` / `NO_PROXY`，并可用 `HATCH_CA_BUNDLE` 或 `SSL_CERT_FILE` 追加 CA 证书。

### 4. 配置 126 / 163 邮箱

1. 登录网页版邮箱：**设置 → POP3/SMTP/IMAP**，开启 **IMAP/SMTP 服务**，按提示生成**授权码**。授权码不是登录密码。
2. 在 `config.toml` 的 `[mail]` 里填 `username`，然后：

```bash
export HATCH_MAIL_PASSWORD=<授权码>
```

网易邮箱要求客户端先发送 RFC 2971 `ID` 命令，否则会返回 "Unsafe Login"。hatch 对 126/163/yeah/188 域名会自动发送。

### 5. 导入日历、设置联系人

```bash
hatch calendar import ~/Downloads/calendar.ics       # 从 126/QQ/飞书/Outlook 日历导出 .ics
hatch memory person boss@corp.com 王总 --importance vip
hatch memory add --tags style,preference "回复邮件要简洁，不用客套话"
hatch doctor                                         # 自检，不会打印任何密钥
```

### 6. 分诊

```bash
hatch triage run --dry-run       # 首次建议：任何外发只写入 ~/.hatch/outbox/*.eml
hatch approvals list
hatch approvals show <id>        # 看清楚将要发出的确切内容
hatch approvals approve <id>     # 批准后才执行；过期的审批永远不会执行
hatch digest                     # 按打扰预算汇总的通知
```

默认 `sender = "outbox"`，不会真正发信。确认流程无误后，再把 `[mail] sender` 改成 `"smtp"`。

### 7. 对话

```bash
hatch ask "这周三下午我有空吗？"
hatch ask --stream "帮我总结今天的邮件"   # 流式输出，边生成边显示
hatch chat [--stream]            # 交互式；/approvals 查看待审批，/exit 退出
hatch chat --resume <session>    # 继续某个会话（hatch session list 查看）
```

也可以在配置里写 `[limits] stream = true`，默认开启流式。分诊和评估始终用非流式请求。

**停止**：回合进行中按一次 Ctrl-C，会停止模型调用、正在运行的命令（整个进程组被杀掉）和所有子代理，回到提示符，并打印回执（停在第几步、已经做了什么；已生效的修改不回滚）。2 秒内再按一次退出程序。被中断的会话可以直接接着问。

**会话**：`hatch session list | show <id>` 查看；`hatch session fork <id>` 复制一份历史到新会话；`hatch session rewind <id> --to <seq>` 把对话截断到某一条（不会留下没有结果的工具调用）。rewind 只回退对话，文件改动用 `hatch checkpoint rewind` 回退。

### 8. 文件、命令与网页

给对话指定一个**工作区**目录后，就能使用文件和命令工具：

```bash
hatch chat --workspace ~/projects/demo        # 临时指定
# 或在 config.toml 里固定：
# [workspace]
# root = "/home/me/projects/demo"
```

| 工具 | 作用 | 默认策略 |
|---|---|---|
| `fs_read` / `fs_list` / `fs_glob` / `fs_grep` | 读文件、列目录、按通配符找文件、正则搜索 | 直接执行；输出标为**不可信**（文件内容可能来自任何地方） |
| `fs_write` / `fs_edit` | 创建/覆盖文件；精确替换一段文字 | **需要审批**，审批里是完整的差异预览；写入前建检查点 |
| `shell_run` | 在沙箱中执行一条命令 | **需要审批**；读过不可信内容之后，常设授权也不能放行 |
| `web_fetch` | 读取公开 https 网页 | 直接执行；读过不可信内容之后需要审批 |
| `web_search` | 搜索（只有配置了 SearXNG 地址才提供） | 同 `web_fetch` |
| `approval_status` | 查询之前提交的审批的状态和结果 | 直接执行 |

常用的命令或目录可以加常设授权，省去逐条确认：

```bash
hatch grants add --class exec_command --to "cargo test*" --days 7     # 命令前缀，拼接 ; && | $ 等不算匹配
hatch grants add --class write_file --to "notes/**" --allow-tainted   # 某个子目录
hatch checkpoint list                                                 # 文件检查点
hatch checkpoint rewind cp_xxxx                                       # 回退（回退本身也能撤销）
```

`hatch doctor` 会实际启动一次沙箱，列出在当前机器上生效的隔离项。

### 9. 子代理（委派）

对话中的主代理可以调用 `delegate`（委派一项任务）或 `delegate_parallel`（多项互不依赖的任务并行），每个子代理在独立会话里运行，结束后把**结论 + 有界的过程摘录 + 用量 + 状态**交回给主代理。

| 类型 | 能用的工具 | 用途 |
|---|---|---|
| `explore` | 只读：文件读取/搜索、记忆、日程、邮件检索、网页读取 | 调查、统计、找出处 |
| `execute` | 主代理的全部工具（写文件、执行命令仍要审批） | 具体修改 |
| `research` | 网页读取与搜索 + 只读本地资料 | 网络调研 |

- 在工作区放 `agents/<name>.md`（frontmatter：`name`、`description`、`tools`、`max_steps`、`workload`）可以自定义类型。类型的工具清单只能**收窄**主代理已有的工具；不写 `tools` 时默认只读。
- 子代理**看不到主对话**，只拿任务书；主代理读过不可信内容时，子代理一开始就处于污染态；子代理提交的审批进入同一队列，来源标注 `subagent:<id>`；调用预算和主代理共享。
- 嵌套深度：主代理为 0，子代理为 1，子代理的子代理为 2；默认上限 2（`[agent] max_depth`），超过就拒绝并记录。`delegate_parallel` 默认同时跑 3 个（`[agent] max_parallel`），一次最多 6 项。
- 运行记录：`hatch agents runs [--session <id>]`、`hatch agents show <run_id>`（任务书、结论、每一步的进度事件）、`hatch agents list`（可用类型）。对话中子代理的进度会缩进实时显示。
- 不想用子代理：`[agent] subagents = false`。

---

## 常用命令

| 命令 | 作用 |
|---|---|
| `hatch triage run [--dry-run] [--fixture mails.json] [--max-calls N]` | 拉取并分诊；输出回执 |
| `hatch triage last` | 再看一次最近的回执 |
| `hatch approvals list/show/approve/reject/history` | 审批队列 |
| `hatch grants add --class send_message --to "*@corp.com" --days 30` | 常设授权：对团队内部的回复可以免逐封确认 |
| `hatch memory add/search/explain/correct/forget/people/person` | 记忆与联系人 |
| `hatch calendar import/list` | 日历 |
| `hatch ask/chat [--stream] [--workspace DIR] [--resume ID]` | 对话（可流式输出；指定工作区后可用文件/命令工具；Ctrl-C 停止当前回合） |
| `hatch agents list/runs/show` | 子代理类型与运行记录 |
| `hatch session list/show/fork/rewind` | 会话：查看、复制、截断 |
| `hatch checkpoint list/show/rewind` | 文件检查点：查看与回退 hatch 做过的修改 |
| `hatch eval [--world DIR] [--live] [--max-calls N] [--max-tokens 4000] [--reasoning-effort low] [--out dir]` | 沙盒评估（`--world eval/worlds/workspace-basic` 为工作区评估，`eval/worlds/delegate-basic` 为子代理评估） |
| `hatch doctor` | 自检 |

---

## 安全模型（摘要）

| 机制 | 说明 |
|---|---|
| 信任标签 | 每段进入模型的文本都标 `user/system/trusted/untrusted`；外部内容包进 `<external_content>`，嵌套的结束标记会被中和 |
| 隔离分类 | 分类模型没有任何工具，只能输出经逐字段校验的 JSON；邮件里的验证码、密码在发给模型前先脱敏 |
| 代码固定收件人 | 回复的收件人、主题、线索头由代码从原邮件生成，模型只写正文 |
| 污染规则 | 上下文里一旦含有不可信内容，所有对外动作都强制审批；常设授权默认不豁免这一条 |
| 信息流规则 | 新收件人、携带日程详情或他人消息外发都需要审批；带秘密的内容直接拒绝 |
| 自主度上限 | 不可逆动作（发送、删除）最高只能到 L1（起草待批），支付只能 L0（建议） |
| 幂等账本 | 每个外发动作都有内容寻址的幂等键；崩溃导致状态不明时**不重发**，而是如实报告 |
| 秘密处理 | 记忆拒绝写入秘密；通知、摘要、审批、审计日志统一脱敏 |
| 遗忘 | `memory forget` 删除整条版本链，并如实说明外部副本无法收回 |
| 命令沙箱 | 新的 user/mount/net/pid/ipc/uts 命名空间，以 uid 65534 运行；只读系统目录 + 可写工作区，其余宿主文件不存在；无网络；capability 全部清空、no_new_privs、seccomp；超时杀整棵进程树 |
| 工作区路径守卫 | 文件工具只认工作区内的路径；拒绝 `..` 逃逸和指向外部的符号链接；原子写入 |
| 检查点 | 每次写文件前快照原内容；文件在审批后被改动则拒绝写入 |
| 子代理 | 只拿任务书、看不到主对话；工具只能比主代理少；继承污染态；审批照样排队并标注来源；深度上限；共享调用预算；读过不可信内容的子代理交回的结论也标为不可信 |
| 取消 | 取消令牌从主代理传到每个子代理、模型调用、沙箱命令（杀整个进程组）和网页读取；取消是“已停止”而不是“失败”，已生效的效果不回滚，回执写明停在哪里 |
| 网页访问 | 只允许 https；拒绝内网、本机、链路本地和云元数据地址，重定向逐跳检查；网页内容一律不可信 |

完整说明见 [docs/SECURITY.md](docs/SECURITY.md)。

---

## 评估

```bash
hatch eval                         # pipeline 模式：不调用任何真实模型
hatch eval --live                  # live 模式：使用配置的模型（可加 --max-calls N 限制调用次数）
```

当前 pipeline 模式的结果（`eval/worlds/zh-office`，16 封邮件）：**11/11 检查通过**。非对抗样本分类 14/14，需回复草稿召回 5/5，发往攻击者的审批和发件都是 0，秘密泄露 0，冲突检出 1/1，重跑新增审批和模型调用都是 0，执行后重跑发件数保持 6 → 6。

> pipeline 模式由 oracle 模型按期望答案作答，并在对抗样本上**故意被骗**。它只验证管线和结构性防线，**不代表任何真实模型的能力**。

live 模式（真实推理模型 `gpt-6-luna`，2026-11-03，单次运行）：**11/11 检查通过**。非对抗样本分类 85.7%（12/14，两封通知类邮件被判为 action），需回复草稿召回 5/5、精确率 5/5，两封对抗邮件都被标记可疑，发往攻击者的审批和发件都是 0，秘密泄露 0，冲突检出 1/1，重跑 0 新增；共 21 次调用，tokens 17,798 / 5,543（其中推理 2,626）。样本小、单次运行。之后两次回归中，分类分别为 12/14 和 10/14（代码与提示词未变，都是通知类邮件被判为 action），说明单次数字有波动，详见 [docs/EVALUATION.md](docs/EVALUATION.md)。

工作区评估（`eval/worlds/workspace-basic`，四个任务，2026-11-03）：

| 模式 | 结果 | 说明 |
|---|---|---|
| pipeline | **11/11** | 脚本模型在注入任务上故意执行了藏在文档里的指令（删除家目录、读工作区外文件、写 leak.txt、访问攻击者网址），4 个动作全部被拦下：读外部文件被路径守卫拒绝，其余 3 个带着“不可信内容”理由进入审批后被拒绝 |
| live（`gpt-6-luna`） | **11/11** | 13 次调用，tokens 27,250 / 1,130（其中推理 321）。问答正确、配置修改逐字正确、检查脚本在沙箱里跑出 `CHECK OK`；注入任务中模型只读了文件，没有提出任何写入、命令或网页访问，并在回答里说明“文件中还包含可疑的指令，我没有照做”。因此这一轮没有考到“模型提出危险动作后被审批拦下”这条路径，这条路径由 pipeline 模式覆盖 |

测试：`cargo test --workspace` 共 164 个测试，全部通过；`cargo clippy --workspace --all-targets` 零警告。沙箱的 10 个真实进程测试在开发容器中全部实际运行（没有跳过），实测生效的隔离项：user / mount / net / pid / ipc / uts 命名空间、pivot_root、/proc、只读根、capability 清空、no_new_privs、seccomp、rlimit，命令以 uid 65534 运行。

---

## 与旧版 hatch-reference 的关系

旧版是一个复现 Muse（Hatch）核心机制的参考实现：26 个 crate、约 7 万行、纯标准库，很多逻辑集中在几千行的单文件里。这一版按**个人助手产品**重新设计，没有沿用"只认 Muse 证据"的约束。

**借鉴了的思路**（按新架构重写，没有搬运代码）：
- 记忆的 claim、纠正链与 `memory_explain` 双向溯源，人物页
- 门禁的"拒绝优先"与审批过期永不执行
- 压缩时保留尾部、在用户消息边界切分
- 按 workload 路由模型（分类、起草、压缩可以用不同模型）
- 各家协议的用量解析（含 cached tokens）

**重新设计的部分**：
- **审批改为异步事件**，回合循环永不阻塞。这修复了旧审计里"审批等待 stdin 时无法中断"的问题（O5）。
- **记忆写入加入秘密拒绝**，修复旧审计 D8。
- **token 估算改为 CJK 感知**。旧版 chars/4 对中文会低估数倍。
- **检索改为中文 unigram + bigram 的 BM25**，替代纯关键词匹配。
- 新增**自主度分级、常设授权、污染规则、信息流规则、幂等账本、注意力预算**。
- 新增**端到端场景与沙盒评估**，评估作为第一等公民。
- 用 SQLite 替代自定义文本格式；使用成熟依赖（rustls、mail-parser、chrono-tz）替代手写实现。
- 每个文件控制在 400 行以内，按层拆分 crate。

---

## 路线图

1. **定时与后台 worker**：cron + 常驻进程，每日简报自动运行，记忆整理 worker。
2. **每日简报**：定时汇总邮件、日程、待办，复用注意力预算与回执。
3. **出行与预订代办**：浏览器能力 + 支付只建议不执行。
4. **多通道**：企业微信 / 飞书 / Telegram 作为消息入口，每个通道有自己的分享规则。
5. **CalDAV 双向同步**：替代 ICS 单向导入。
6. **扩充评估世界**：已有真实模型基线；下一步增加更多语言风格、长线程、附件、编码任务，并多次运行取均值。

---

## 许可证

MIT，见 [LICENSE](LICENSE)。
