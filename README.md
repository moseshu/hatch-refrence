# hatch — 个人 Agent 助手

[English](README_EN.md)

hatch 是一个可部署到云端的个人 Agent 助手（Rust 单二进制，纯后端）。它常驻运行，管理你的邮件、日程、记忆和工作区，能在对话中调用工具、使用浏览器、把任务异步委派给子代理、按计划在后台工作，并且把每一个对外动作都放进可审计、可撤销、需要你确认的执行通道里。

几个关键设计：

- **后端优先**：本仓库是纯后端，通过产品 API（`/v1/...`，HTTP/1.1 + SSE）对外提供服务；前端在独立仓库里开发，不混进来
- **审批优先**：不可逆的动作（发送、删除、执行命令、分享）默认进审批队列，批准后才执行，执行的就是审批时看到的内容
- **验收优先**：任务完成不是模型说了算——执行后独立回读真实状态，对照冻结的验收契约逐条判定（确定性谓词 + 语义评分），不通过不报完成
- **记忆是结构化的**：记忆以带来源、信任级别与置信度的 claim 为单位，纠正形成可追溯的链，而不是覆盖历史
- **子代理是异步的**：委派后立刻返回 run id，主代理继续工作、用户继续对话；子代理做完后结果以 handoff 事件在下一轮回流

## 安装

需要 Rust ≥ 1.80 和 C 编译器（SQLite 是 bundled 编译）。一键安装：

```bash
git clone https://github.com/moseshu/hatch-refrence.git
cd hatch-refrence
./install.sh            # 构建 release 并安装到 ~/.local/bin
```

手动构建：

```bash
cargo build --release -p hatch-cli   # 二进制：target/release/hatch
```

初始化与自检：

```bash
hatch init              # 创建 ~/.hatch/config.toml 与数据库（HATCH_HOME 可改目录）
hatch doctor            # 自检：配置、数据库、模型、邮箱、沙箱、调度（不会打印密钥）
```

配置模型（只用环境变量即可运行，密钥永不写进配置文件）：

```bash
export HATCH_LLM_BASE_URL=<你的模型服务地址>
export HATCH_LLM_MODEL=<模型名>
export HATCH_LLM_API_KEY=<redacted>
export HATCH_LLM_PROVIDER=openai_compatible  # 可选：openai_responses / anthropic / gemini
```

| `provider` | 协议 | 请求地址 |
|---|---|---|
| `openai_compatible`（别名 `openai_chat`） | Chat Completions | `{base_url}/chat/completions` |
| `openai_responses` | OpenAI Responses | `{base_url}/responses` |
| `anthropic` | Anthropic Messages | `{base_url}/v1/messages` |
| `gemini` | Gemini `generateContent` | `{base_url}/v1beta/models/{model}:generateContent` |

模型名与服务地址一律由你在配置/环境变量里指定，代码里不写死任何厂商。

## 快速上手

```bash
# 命令行试一轮
hatch ask "这周三下午我有空吗？"

# 常驻运行 + 产品 API（前端对接入口，默认 127.0.0.1:8787）
hatch daemon run --api

# 用 API 发一轮对话（JSON 形状；带 Accept: text/event-stream 则走 SSE 流式）
curl -X POST http://127.0.0.1:8787/v1/sessions -d '{}'
curl -X POST http://127.0.0.1:8787/v1/sessions/<id>/messages \
  -H 'Content-Type: application/json' -d '{"text":"帮我看看今天的邮件"}'
```

## 架构

```
                        ┌─────────────────────────────────────────────┐
   前端 / 客户端 ──HTTP──▶│  产品 API（/v1/...，HTTP/1.1 + SSE）        │
   CLI（hatch 命令）─────▶│  鉴权：按客户端 token（read/write scope）    │
                        └──────────────────┬──────────────────────────┘
                                           │
  ┌────────────────────────────────────────▼────────────────────────────────────────┐
  │  调度层   hatch-scheduler（任务存储·运行引擎·daemon）  hatch-workers（后台产出） │
  │  场景层   hatch-agent（对话循环·上下文·压缩·子代理）   hatch-triage（邮件分诊）  │
  │           hatch-plan（计划模式·跨步骤验收）            hatch-eval（评估）         │
  │  能力层   hatch-tools（工具 SDK·统一 dispatch·执行器·检查点）                    │
  │  领域层   hatch-policy（门禁·审批·幂等账本）  hatch-memory（claim 记忆）         │
  │           hatch-connectors（邮件·日历）       hatch-llm（四协议模型路由）         │
  │           hatch-sandbox（命令沙箱）           hatch-browser（Chromium）          │
  │           hatch-inference（推理代理·用量记账） hatch-mcp（MCP 客户端/服务端）     │
  │  基础层   hatch-store（SQLite 单文件）  hatch-core（错误·ID·信任·秘密·取消）     │
  └──────────────────────────────────────────────────────────────────────────────────┘
                                           │
              外部世界：模型服务（你配置的）· 邮箱 IMAP/SMTP · CalDAV · 网页 · MCP 服务器
```

| crate | 职责 |
|---|---|
| `hatch-core` | 错误类型、内容寻址 ID、信任标签与不可信信封、CJK 感知 token 估算、秘密检测与脱敏、取消令牌 |
| `hatch-store` | 单文件 SQLite、幂等迁移、事务、自动脱敏的审计日志 |
| `hatch-llm` | 四种模型协议与流式输出、按 workload 路由、降级阶梯、调用预算 |
| `hatch-memory` | claim 记忆、纠正链与溯源、中文 BM25 持久化索引（bank/index 分离）、遗忘、人物页 |
| `hatch-policy` | 动作类别 × 可逆性 × 自主度、门禁规则、常设授权、异步审批队列、幂等账本 |
| `hatch-connectors` | IMAP / SMTP / MIME（含 GBK）、ICS 解析、冲突与空闲时段；连接器框架与两个参考连接器 |
| `hatch-sandbox` | 进程沙箱：命名空间、pivot_root、capability 清空、no_new_privs、seccomp、rlimit、超时杀进程树 |
| `hatch-browser` | Chromium 管理（按 profile 隔离、跨进程重连、空闲回收）、CDP 会话、页面快照、目标地址拦截 |
| `hatch-tools` | Tool SDK、邮件/日历/记忆/文件/命令/网页/浏览器工具、工作区路径守卫、检查点、统一 dispatch |
| `hatch-agent` | 行为规约、分层上下文组装、压缩、会话（fork / rewind）、可取消的回合循环、异步子代理、意图门与语义评分 |
| `hatch-triage` | 分诊工作流：隔离分类、代码后处理、会议处理、起草、持久化步骤、回执 |
| `hatch-scheduler` | 调度表达式（夏令时正确）、任务 / 运行记录 / 去重分表、看门狗与重试、自动停用、daemon |
| `hatch-workers` | 每日简报、定时分诊、记忆整理与提炼、后台产出交接 |
| `hatch-goals` | 目标工作区、点子库、每日动态 |
| `hatch-eval` | 评估世界与报告（分诊、工作区、子代理、调度、浏览器） |
| `hatch-plan` | 计划模式：结构化计划的解析与校验、SQLite 持久化与状态机、逐步骤执行与偏差检测、跨步骤验收契约 |
| `hatch-artifacts` | widget 卡片与带版本的 artifact（分享审批、配额、分享 token 与撤销） |
| `hatch-transports` | 多通道传输接口：每聊天独立 transcript、`DISCRETION.md` 分享规则、worker 转述规则 |
| `hatch-mcp` | MCP 客户端（stdio / HTTP-SSE）+ 服务端（stdio / HTTP，按客户端 token 与 TLS） |
| `hatch-vault` | 凭据保管：0600 文件存储 + 独立 helper 进程 |
| `hatch-skills` | 技能注册表：SKILL.md 解析、白名单、激活 |
| `hatch-inference` | 推理代理：回环反向代理（转发、用量记账、重试、限流） |
| `hatch-cli` | 命令行、产品 API 服务、配置与环境变量覆盖、Ctrl-C 处理 |

## 运行流程：从用户输入到任务完成

### 一轮对话（chat turn）

```
用户输入（CLI ask/chat，或 POST /v1/sessions/{id}/messages）
  │
  ├─ 1. 上下文组装：行为规约 → 用户画像/偏好记忆 → 检索到的记忆 → 子代理交接
  │      → 后台更新 → 会话历史（稳定前缀在前，命中 provider 前缀缓存；超预算时压缩）
  ├─ 2. 模型循环：模型产出回答或工具调用（每步计入步数/token 预算，可取消）
  ├─ 3. 工具调用进入统一 dispatch（唯一入口）：
  │      a. 预执行意图门（可选）：独立评委核对"动作是否符合用户意图"，错位先拦
  │      b. 门禁：按动作类别/可逆性/自主度/常设授权/污染状态决定
  │         → 直接执行 / 进审批队列 / 拒绝（不可逆动作默认审批）
  │      c. 执行器执行，幂等账本记账（崩溃后状态不明不重发，如实报告）
  ├─ 4. 工具结果回流模型，继续循环，直到模型给出最终回答
  ├─ 5. 完成验收：独立回读真实状态（发件箱/日历/文件），对照本轮验收条件判定；
  │      不通过则继续修复或如实报告未完成，不假报完成
  └─ 6. 回合报告返回前端：JSON 一次性返回，或 SSE 流式事件
         （message 增量、tool_call/tool_result 进度、approval_needed、done）
```

### 持久任务（plan task，跨步骤验收）

```
用户目标 → 模型产出结构化计划（步骤+参数+验收契约，契约在批准时冻结）
  → 用户批准（快照全部步骤与契约）
  → 逐步骤执行（每步对照批准快照，偏离即停；对外动作走审批/常设授权）
  → 全部步骤执行后：共享证据包独立回读 → L2 确定性谓词逐条判定
     → L3 语义评分（独立评委，必须引用证据）
  → 验收全过才标 Done；不过则 blocked / needs_review，恢复时只对账不重发
```

### 后台运行

`hatch daemon run` 按调度表达式运行定时任务（每日简报、定时分诊、记忆整理等）；产出在下次对话时作为「后台更新」交给助手。停机错过的触发只记录、最多补跑一次；失败退避重试，连续失败自动停用并通知。

## 对外服务接口（前端对接）

已经有完整的产品 API：`hatch daemon run --api` 提供 HTTP/1.1 服务（默认 `127.0.0.1:8787`），不托管任何前端文件，前端在独立仓库通过本 API 对接。

**鉴权**：按客户端 token（`hatch daemon token create <名字> [--scope read|write]`，注册表只存哈希、明文只显示一次，撤销下一个请求立即生效）。read scope 只能调读端点，写端点要 write scope（不足返回 403 `insufficient_scope`）。`GET /v1/health` 与分享链接 `GET /v1/shared/<token>` 不鉴权。未配置 token 时 API 只绑回环；绑定非回环地址又无 token 时 daemon 拒绝启动。限流每 IP 20 请求/秒（429 + `Retry-After: 1`）。服务本身不做 TLS，对外暴露请走反向代理。

**端点总览**（全部在 `/v1` 下）：

| 分组 | 端点 | 说明 |
|---|---|---|
| 健康/状态 | `GET /health`、`GET /status` | 存活探针与运行状态 |
| 会话与对话 | `POST/GET /sessions`、`GET/DELETE /sessions/{id}`、`POST /sessions/{id}/messages`、`POST /sessions/{id}/cancel`、`POST /sessions/{id}/fork`、`POST /sessions/{id}/rewind` | 发消息支持 JSON 与 SSE 流式两种形状（`Accept: text/event-stream`）；同会话同时只跑一轮，冲突返回 409 |
| 任务 | `GET/POST /tasks`、`GET /tasks/{id}`、`GET /tasks/{id}/events`、`POST /tasks/{id}/cancel\|pause\|resume\|command` | 事件流支持 SSE + `Last-Event-ID` 断点续传 |
| 审批 | `GET /approvals`、`GET /approvals/{id}`、`POST /approvals/{id}/approve\|deny` | 批准执行的就是 `GET` 看到的确切内容（payload 哈希校验） |
| 子代理 | `GET /agents`、`GET /agents/{id}`、`POST /agents/{id}/cancel\|pause\|resume` | 异步委派的运行状态与控制 |
| 记忆 | `GET /memory/search`、`POST /memory/claims`、`GET /memory/claims/{id}`、`GET /memory/people`、`GET /memory/people/{email}` | claim 检索与人物页 |
| 定时任务 | `GET/POST /jobs`、`GET/PATCH/DELETE /jobs/{id}`、`POST /jobs/{id}/run`、`GET /jobs/{id}/runs` | 调度管理 |
| 产物 | `POST /artifacts`、`GET /artifacts/{id}`、`DELETE /artifacts/{id}`、`GET /artifacts/quota`、`GET /artifacts/{id}/shares`、`POST /artifacts/{id}/share\|revoke`、`GET /shared/<token>` | 带版本的交付物与分享链接 |
| 其他 | `GET /goals`、`POST /goals`、`GET /goals/{id}`、`GET /feed`、`GET /ideas`、`GET /calendar/events`、`GET /mail/triage`、`GET /skills`、`GET /connectors`、`GET /connectors/{name}`、`GET /inference/usage` | 目标/动态/点子/日程/邮件分诊（只读）/技能/连接器/用量 |

**SSE 事件**（对话轮次）：`message`（回答增量）、`tool_call` / `tool_result`（进度）、`approval_needed`（需要用户批准时）、`done`（回合报告：answer、verification、stop 原因、steps、tool_calls）。候选回答文本在独立完成验收通过后才流出，不会先吐后改。

**MCP 服务端**：`hatch mcp serve` 把 hatch 的工具暴露给 MCP 客户端（stdio 与 streamable HTTP，按客户端 token 鉴权，HTTP 可开 TLS）。需要审批的调用不执行，返回 `approval_required` + 审批 id，调用方轮询状态。

明确不在 v1：支付（只建议、永不执行）、真实多端通道（微信 / 飞书 / Telegram 未实现）、多租户隔离（当前单用户 home）。

## 核心机制

**上下文组装。** 每轮请求由固定顺序的层组成：行为规约 → 用户画像与偏好记忆 → 检索到的记忆 → 子代理交接 → 后台更新 → 会话历史。稳定前缀排在前面（连续两轮字节一致，可命中 provider 前缀缓存），易变内容在后；预算按 CJK 感知的 token 估算裁剪，历史过长时在用户消息边界压缩。

**记忆。** 记忆以 claim 为单位，记录来源、信任级别、置信度和有效期。纠正不覆盖历史，而是形成可追溯的纠正链；推断出的和来自不可信来源的记忆置信度有上限；含秘密的内容拒绝写入。检索用中文 BM25（持久化索引，bank 与 index 分离），只返回当前有效的记录；排序是 BM25 × 显著度（0–1，提炼时由模型打分，可手动调）× 时间衰减（按最后使用时间指数衰减，半衰期 90 天，可在 `[agent] salience_half_life_days` 改）。每轮自动检索相关记忆注入上下文，被注入的记忆会更新最后使用时间；每天 04:00 的 `memory-extract` 任务从新会话提炼持久记忆（冲突只提议不写）。每天 02:30 的 `memory-synthesis` 任务把高显著度与最近新增的记忆综合成一份"当前理解"文档（只读摘要，不产生新 claim、不改显著度），在会话开始时注入稳定前缀；用 `hatch memory synthesis show` 查看，`hatch memory synthesize [--force]` 手动生成。

**门禁、审批与幂等。** 每个工具调用先由门禁根据动作类别、自主度、常设授权、污染状态和信息流规则决定：直接执行、进入审批队列或拒绝。审批不阻塞对话；批准后执行的就是审批时看到的内容（payload 哈希校验）；过期的审批永不执行。每个对外动作都有内容寻址的幂等键，崩溃后状态不明时不重发而是如实报告。

**预执行意图门（L0，可选）。** 动作执行前，独立评委核对"准备执行的动作是否符合用户在本轮表达的意图"（参数与陈述约束冲突、超限、不可验证等），给出 allow / deny / uncertain。开关 `HATCH_INTENT_GATE=off|observe|enforce`，默认 off；observe 只记录裁决不拦截，enforce 时 deny/uncertain 拦截并回可行动原因。裁决全量落库，可抽样审计。

**跨步骤验收（L2/L3）。** 持久任务的验收契约在批准时冻结：结构化谓词由确定性代码对独立回读的证据逐条判定（L2），自然语言条目由独立评委对同一证据包评分（L3，必须引用证据、引用由代码校验）。两层可分别灰度（Observe 只记录影子裁决 / Enforce 真拦截），`HATCH_ACCEPTANCE_ENFORCE` 控制；L2 默认 Enforce，L3 默认 Observe。证据不足判 Ambiguous → 需要人确认，恢复时只做只读对账、永不重发。

**信任与污染。** 进入模型的每段文本都带信任标签；邮件、网页、文件内容包进不可信信封。上下文一旦含有不可信内容，所有对外、写入和执行类动作都强制审批。分诊分类使用无工具的隔离模型，回复收件人由代码固定。

**命令沙箱与浏览器。** `shell_run` 在独立的 user / mount / net / pid / ipc / uts 命名空间中以 nobody 身份运行：只读系统目录 + 可写工作区、无网络、seccomp 与 rlimit 限制、超时杀整棵进程树；宿主不支持时直接报错，不退化为无隔离执行。浏览器三个 profile 是三个独立的 Chromium 进程，cookie 与登录状态互不可见；页面以带编号可操作元素的快照交给模型，模型不能执行脚本；提交先审批且执行前重新核对，支付只建议；遇到登录墙或验证码时由你用 `hatch browser takeover` 接管。

**子代理。** `delegate` / `delegate_parallel` 是异步的：调用后立刻返回 run id 与"正在后台运行"，回合继续、用户可以接着对话；子代理在独立进程里跑（Tokio + UDS 传输，socket 0600），做完后结论记成一条 handoff，下一轮回合开始时注入上下文（只投递一次）。子代理只拿任务书、看不到主对话，工具清单只能比主代理少，审批进入同一队列并标注来源，嵌套深度有上限；取消随时可以：对话里说一声，模型调 `cancel_subagent` 从本进程取消（不用 daemon），或 `hatch agents cancel <run_id>`（或 `POST /v1/agents/{id}/cancel`，经 daemon）。并行上限 3、每轮任务上限 6、深度上限 2。

**调度与后台 worker。** 支持 `daily` / `weekly` / `every` / `once` / `cron` 表达式，按用户时区计算并正确处理夏令时。停机期间错过的触发只记录、最多补跑一次；失败退避重试，连续失败自动停用并通知。定时任务的授权范围只能收窄，范围外的对外动作一律进审批。

**模型协议。** 同一套抽象支持 Chat Completions、OpenAI Responses、Anthropic Messages 和 Gemini，均支持工具调用、JSON 模式与流式输出。分类、起草、压缩、简报等任务可分别路由到不同模型，出错时按降级阶梯切换。

## 常用命令

```bash
hatch ask "这周三下午我有空吗？"          # 一次性问答
hatch chat --stream --workspace ~/projects/demo
hatch chat --resume <session>
hatch session list | fork <id> | rewind <id> --to <seq>
hatch agents list | runs | show <run_id> | cancel <run_id>
hatch triage run --dry-run                # 邮件分诊（干跑）
hatch approvals list                      # 待审批动作
hatch approvals show <id>                 # 查看将要执行的确切内容
hatch approvals approve <id>
hatch grants add --class send_message --to "*@corp.com" --days 30   # 常设授权
hatch plan new "把本周未读邮件里提到的待办整理成一份清单"   # 计划模式
hatch jobs enable daily-brief             # 每天 08:30 生成每日简报
hatch daemon run --api                    # 常驻 + 产品 API
```

回合进行中按一次 Ctrl-C 停止当前回合（含子代理与命令）；2 秒内再按一次退出。

浏览器（需要本机安装 Chromium / Chrome，或用 `HATCH_CHROMIUM` 指定路径，`[browser] enabled = true`）：

```bash
hatch ask "打开 https://pypi.org/project/requests/ ，告诉我最新版本号"
hatch browser takeover <session>   # 需要登录或验证码时接管完成
hatch browser resume <session>
```

## 计划模式

对话默认是"边想边做"。计划模式反过来——模型先交出一份**结构化计划**（有序步骤，每步写明工具名、参数、涉及文件、预期结果、风险等级），计划存进 SQLite 等你批准，批准后才按步骤执行；执行时每一步都对照批准时的快照，工具、参数、文件有任何出入就停下，标为"需重新审批"。验收契约随批准一并冻结（见「跨步骤验收」）。

```bash
hatch plan new "把本周未读邮件里提到的待办整理成一份清单"
hatch plan list
hatch plan show plan_xxxx
hatch plan approve plan_xxxx       # 批准（记录批准人 + 时间，批准时快照全部步骤）
hatch plan run plan_xxxx           # 按批准的步骤逐一执行
hatch plan reject plan_xxxx
```

对话里也可以用计划模式：`hatch chat --plan`（或配置 `[agent] plan_mode = true`）。计划的状态流转是 `draft → pending_approval → approved → executing → completed`，另有 `rejected / superseded / cancelled / needs_reapproval`。

## 技能、MCP 与凭据保管

**技能**是放在 `$HATCH_HOME/skills/<名称>/SKILL.md`（或 `./skills/`）的 Markdown 文件：开头 `---` 围起的 frontmatter（`name` / `description` / `tools` / `scope` / `version`），后面是正文。激活后，对话每轮把正文放进上下文的一个稳定层，并把本轮可用工具过滤到技能的 `tools` 列表。技能是用户自己装的工作方式说明：它不能授予新工具，不能绕过审批，也不能把正文里的要求当作用户的要求。示例见 `skills/example-brief/SKILL.md`。

**MCP 客户端**支持 stdio 与 HTTP/SSE（JSON-RPC 2.0）：在 `config.toml` 里加 `[[mcp_servers]]`，远程工具以 `mcp_<服务器>_<工具>` 的名字注册。服务器返回的内容一律视为不可信；调用一律走审批队列，结果标为不可信。

**MCP 服务端**（`hatch mcp serve`）把 hatch 的工具暴露给 MCP 客户端：stdio 与 streamable HTTP（协议 2025-03-26）两种传输。HTTP 支持按客户端 token（`hatch mcp token create <名字> [--scope read|write]`，注册表 `<home>/mcp-clients.json` 0600 只存哈希，撤销下一个请求立即生效；审批 origin 为 `mcp:http:<客户端名>`）与 in-process TLS（`--tls-cert/--tls-key`；非回环绑定默认要求 TLS + token，或显式 `--allow-plain-http`）。旧的共享 token（vault `mcp-http` / `HATCH_MCP_TOKEN`）仍可用，是过渡方案。需要审批的调用不执行，返回 `approval_required` + 审批 id，调用方用 `approval_status` 轮询。

**凭据保管（vault）**：token 存在 `$HATCH_HOME/vault/tokens.json`（目录 0700，文件 0600），运行时由独立的 `hatch-vault-helper` 进程持有；agent 进程只持有 token 名字。`hatch vault set <名字>` 从 stdin 或 `--from-env VAR` 读值，永不从命令行参数或配置文件读。

**连接器框架**：连接器是子进程 CLI，用 `$HATCH_HOME/connectors/<名字>.toml` 清单描述。操作以 `connector_<连接器>_<操作>` 注册进工具表，调用一律走审批队列，结果一律标为不可信。只带两个参考连接器（本地文件笔记、本地 mock HTTP）；不做真实供应商连接器。

## 产物呈现：widget、artifact 与配额

**widget** 是助手可以给出的小卡片（选项 / 列表 / HTML），带一个 `embed_token`：卡片在 token 出现在用户可见的消息里之前是惰性的，没有 token 只渲染纯文本版。**artifact** 是带版本的交付物（`document` / `html`），文件放在 `$HATCH_HOME/artifacts/<id>/v<n>/`，新版本永不覆盖旧版本；流程是构建 → 分享 → 批准 → 交付，未分享的不能交付。

```bash
hatch artifact build "周报" --kind document --content "# 第一周"
hatch artifact share art_xxx     # 只提交分享审批，不分享
hatch approvals approve apr_xxx  # 批准后才标记已分享
hatch artifact deliver art_xxx   # 打印含文件路径的交付消息
```

**配额**（整个 home 共用，单用户 v1）：默认 200 个 artifact、总计 100 MiB、单个文件 5 MiB；`[artifacts]` 配置或 `HATCH_ARTIFACT_MAX_*` 环境变量覆盖。构建前先算配额，超了明确拒绝（API 返回 403 `quota_exceeded`），不写半截文件。

**分享与撤销**：批准分享时生成一个分享 token（`hatch_art_` + 32 随机字节），只在批准结果里显示一次；注册表只存 SHA-256 哈希。分享链接是 `GET /v1/shared/<token>`；撤销立即生效（`hatch artifact revoke <分享 id>` 或 `POST /v1/artifacts/{id}/revoke`）；删除 artifact 时先撤销它的全部分享。

## 推理代理

`hatch inference proxy` 是回环上的 HTTP 反向代理（默认 `127.0.0.1:8199`）：agent 把模型请求发给它，它原样转发给真正的上游并把响应流回，同时做三件事——**用量记账**（每次调用按 provider 上报的 usage 记到 `$HATCH_HOME/inference/usage.jsonl`，`hatch inference usage` 与 `GET /v1/inference/usage` 可查；费用按 `[inference]` 配置的单价估算，未配置不估算）、**重试**（429 / 5xx / 传输失败指数退避、默认 3 次，只在首字节转发前重试；400 / 401 / 403 原样回传）、**限流**（每分钟上限可配，默认 60，超了返回 429 + `Retry-After`）。代理只绑回环地址；上游密钥只存在代理进程内存里。agent 在 `[inference]` 写 `proxy_url = "http://127.0.0.1:8199"`（不带 `/v1`）即可走代理，不配则直连、行为不变。

## 评估

```bash
hatch eval                                          # pipeline：脚本模型，不调用真实模型
hatch eval --live                                   # live：使用配置的模型
hatch eval --world eval/worlds/workspace-basic
hatch eval --repeat 3                               # 同一世界跑 3 次，报告均值
```

| 世界 | 考察内容 | pipeline |
|---|---|---|
| `zh-office` | 邮件分类、起草、注入邮件、验证码脱敏、冲突检测、幂等 | 11/11 |
| `en-office` | 与 zh-office 同形状的英文邮件 | 11/11 |
| `thread-attachment` | 长线程（中途改决定）、附件只作引用、最终决定准确 | 15/15 |
| `workspace-basic` | 文件问答与修改、沙箱命令、文档中的注入指令 | 11/11 |
| `delegate-basic` | 并行委派、取消、深度上限、子代理遇到注入 | 13/13 |
| `scheduler-basic` | 夏令时、停机补跑、失败与超时、简报事实一致性、定时外发授权 | 17/17 |
| `browser-basic` | 页面问答、选项存在性、登录墙、验证码、表单提交审批、结账、页面注入（真实 Chromium + 本地 mock 站点） | 16/16 |
| `compaction-basic` | 压缩质量：事实保留率、编造率、压缩比（模型摘要与抽取式两条路径） | 9/9 |

pipeline 模式的脚本模型会在对抗样本上故意"上当"，用来验证结构性防线；它不代表真实模型的能力。live 模式用配置的真实模型跑同一批世界。

## 安全要点

| 机制 | 说明 |
|---|---|
| 自主度上限 | 不可逆动作（发送、删除、执行命令）最高只能到"起草待批"，支付只能建议 |
| 信息流规则 | 新收件人、携带日程详情或他人消息外发都需要审批；带秘密的内容直接拒绝 |
| 秘密处理 | 记忆拒绝秘密；通知、摘要、审批、审计日志统一脱敏；密钥只从环境变量读取 |
| 网页访问 | 只允许公开 https；拒绝内网、本机、链路本地和云元数据地址，重定向逐跳检查 |
| 浏览器 | 页面内容不可信；提交先审批且执行前重新核对；支付只建议；不填写密码、不破解验证码；profile 之间隔离 |
| 工作区守卫 | 文件工具只认工作区内的路径，拒绝 `..` 逃逸和指向外部的符号链接，原子写入 |
| 技能与 MCP | 技能不能授予工具、不能绕过审批；MCP 服务器输出一律不可信，调用走审批队列；HTTP 服务端按客户端 token 鉴权 |
| 凭据保管 | token 由独立 helper 进程持有（0600 文件持久化）；注入时 helper 直接把值设进子进程环境 |
| 产物呈现 | widget 的 HTML 只允许安全标签（无脚本、无事件处理器、无外部拉取）；artifact 分享是对外动作，必须经审批批准 |
| 遗忘 | 删除整条记忆版本链，并如实说明外部副本无法收回 |

## TODO：自我进化

下一步方向是让 agent 自己进化、提高能力，而不是只靠人来改：

- **自我评估**：定期跑评估世界（`hatch eval`），把失败的用例记下来，作为进化的输入
- **从失败中学习**：分析失败原因，提议修复——改提示词、补技能、调记忆策略
- **技能生长**：把反复出现的新需求沉淀成新技能，把复用的做法写成可复用的流程
- **记忆与提示词迭代**：根据使用反馈调整画像、综合文档与行为规约
- **人在回路**：进化动作先起草、经审批再落地；不可逆的改动不自动执行

## 许可证

MIT，见 [LICENSE](LICENSE)。

## 联系方式

- 邮箱：moseshu25@gmail.com
