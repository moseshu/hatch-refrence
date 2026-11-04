# 架构

## 1. 分层原则

| 层 | crate | 规则 |
|---|---|---|
| 基础层 | `hatch-core`、`hatch-store` | 不依赖任何领域概念 |
| 领域层 | `hatch-llm`、`hatch-memory`、`hatch-policy`、`hatch-connectors`、`hatch-sandbox`、`hatch-browser` | 互不依赖，只依赖基础层 |
| 能力层 | `hatch-tools` | 把领域能力组合成"工具"，并提供**唯一的外发路径** |
| 场景层 | `hatch-agent`、`hatch-triage`、`hatch-eval`、`hatch-plan` | 编排；不直接调用连接器发送任何东西 |
| 调度层 | `hatch-scheduler`、`hatch-workers` | `hatch-scheduler` 只依赖基础层（调度、任务存储、运行引擎、daemon），不认识任何具体任务；`hatch-workers` 把场景包装成任务种类 |
| 入口层 | `hatch-cli` | 读取配置、组装依赖、打印结果 |

依赖只能向下。连接器只负责收发，从不做决定；所有"该不该做"的判断都在 `hatch-policy` 里。

## 2. 一次对外动作的完整路径

```
提议（工具调用 / 分诊起草）
   │  PlannedAction：类别、收件人、数据标签
   ▼
Gatekeeper.evaluate(请求, 有效授权)
   ├─ Deny ──────────────► 原因回给模型 / 写入回执
   ├─ RequireApproval ───► ApprovalQueue.submit（幂等键唯一，payload 不可变，带 TTL）
   │                         … 用户稍后 approve / reject …
   └─ Allow ─────────────► 同样写入审批表（自动批准，原因写明是哪条授权）
                                   │
                                   ▼
                     execute_approved：状态必须为 approved
                                   → 校验 payload 哈希
                                   → ActionLedger.begin(幂等键)
                                         Fresh       → 执行 → complete
                                         AlreadyDone → 不重复，返回原结果
                                         InProgress  → 不重试，如实报告"状态不明"
```

自动执行的动作也会留下审批记录，所以审计视图只有一个。

## 3. 分诊工作流

```
sync     IMAP / 夹具 → mail_messages（Message-ID 去重）→ people.touch → UID 游标
for each 未完成邮件（按时间顺序）：
  classify  隔离分类（无工具、JSON、逐字段校验）→ 代码后处理（VIP / 低优先级 / 注入规则 / 分类边界 R1–R6，见下）
  calendar  会议请求 → 冲突检测 → 无冲突提议 create_event / 有冲突给出空闲时段
  draft     需回复且不可疑 → 起草正文 → reply_payload（收件人代码固定）→ route_action
  notify    AttentionPolicy：Immediate / Digest / Silent
  complete
receipt   已完成 / 未完成 / 待你决定 + 调用与 token 统计
```

**分类边界（notification / fyi 与 action / reply）**：提示词
（`prompts/triage_classify_zh.md` 的"类别边界"一节）给定义与例子，
代码（`crates/hatch-triage/src/boundary.rs`）用确定性规则复核，只在
信号明确时覆盖模型，且可疑邮件与分类失败（Unclassified）不参与：

| 规则 | 内容 |
|---|---|
| R1 系统通知 | 系统发件人（`noreply@…`）或系统事件标记（验证码、已重置、订阅续订），且无提问、无明确确认请求 → `notification` |
| R2 明确无需回复 | 含"无需回复 / no need to reply" → `fyi`（机构发件人 → `notification`） |
| R3 订阅推送 | 含退订/营销标记且无请求信号 → `newsletter` |
| R4 纯知会 | 无请求信号但有知会标记（同步、更新、推迟、提醒、余额……）→ `fyi`（机构发件人 → `notification`） |
| R5 明确请求保护 | 有明确请求信号（提问、约时间、请确认/请提交……）却被判为 `notification` / `newsletter` / `fyi` → `reply`（提问或约时间）或 `action`；系统发件人、系统事件、订阅推送不参与 |
| R6 fyi 一致性 | `fyi` 一律 `needs_reply=false` |

"请求信号"的定义是窄的：提问（`?` / `？` / 请问 / 能否 / could you
……）、约时间（想约 / schedule a call ……）、强祈使（请确认 / 请回复 /
请审批 / 请提交 / 请处理 / please confirm ……）、直接索取（发我 /
send me ……）。光有截止日（"请在 12 月 31 日前安排使用"）或泛泛指示
（"请登录后修改"）**不算**请求信号——那正是 R1/R4 要纠正的混淆。
`notification` 与 `fyi` 的分界看发件人：机构发件人（部门/系统/银行
等角色地址或名称，如 `hr@…`、"人力资源部"、"IT 部"）→
`notification`，个人发件人 → `fyi`。类别集合不变。

每一步都会写入 `workflow_steps(workflow, item, step)`。标记为 `done` 的步骤不会重做，`failed` 的步骤下次运行重试。调用预算耗尽时整轮停止，剩下的邮件下次自动续跑。

## 4. 对话回合

```
用户输入 → 写入会话 → 超过阈值则压缩（在用户消息边界切，保留最近 N 轮）
loop ≤ max_steps:
  已取消？ → 写入一条“本轮被中断”的说明，结束（Cancelled）
  分层组装：行为规约（子代理再追加角色说明）→ 当前时间 → 用户画像 → 本轮检索记忆 → 早先摘要 → 历史
  预算：CJK 感知估算；只丢最旧的完整回合
  模型（可取消）→ 文本：结束
                 → 工具调用：delegate / delegate_parallel → 子代理（见 4b）
                             其他 → 逐个 dispatch；需要审批的入队后继续，不等待
                             批次中途取消 → 剩余调用写入“已取消，未执行”，保证下一次请求格式完整
  不可信的工具输出会让本轮后续的外发处于"污染"状态
```

回合有五种结束方式：`Answered`、`StepLimit`、`Budget`、`ModelError`、`Cancelled`。取消是第三种终态——不是失败，也不是“没做完”；已经生效的效果不回滚，回执写明停在第几步、做过什么。

### 取消如何传播

`hatch_core::CancelToken` 是一棵树：主回合的令牌是根，每个子代理拿到 `child()`。父取消时所有子孙都取消；单个子取消不影响父和兄弟。令牌被检查的地方：

| 位置 | 行为 |
|---|---|
| 回合循环 | 每一步开始前、每个工具调用前 |
| 模型调用 | 在工作线程里进行，调用方每 20 ms 检查一次令牌，取消后立刻返回；流式读取在下一个数据块时断开连接。已经发出的非流式请求在服务端会继续完成，但结果被丢弃、用量不计 |
| 沙箱命令 | 每 10 ms 检查一次，取消即对整个进程组发 SIGKILL，结果标注“已取消” |
| 网页读取 | 每一跳重定向前、读正文的每个数据块之间 |
| 执行前 | 动作还没开始就取消时，审批保持“已批准”、不占用幂等账本，之后仍可执行 |

CLI 的 Ctrl-C 处理器取消当前回合的根令牌；2 秒内第二次按下才退出进程。

## 4b. 子代理（`hatch-agent::subagent`）

```
主代理调用 delegate / delegate_parallel
  → 解析任务书（task、type、max_steps）→ 类型存在？深度未超限？父任务未取消？
       否 → 记录一条 rejected 运行，返回拒绝原因
  → 新建运行记录（spawned）与独立会话 → running
  → 子代理 = 同一个 Agent 循环，但：
       工具 = 父代理工具 ∩ 类型允许的工具（只能更少）
       系统提示追加“子代理角色”与类型说明；用户消息只有任务书
       start_tainted = 父代理当前是否污染；审批来源 = subagent:<id>
       令牌 = 父令牌.child()；调用预算与父代理共享（同一个 Router）
  → 进度事件（每步、每次工具调用一行，截断并脱敏）写入 subagent_events，并转发给父观察者（CLI 缩进打印）
  → 结束：completed / failed / cancelled，写入步数、用量、结论
  → handoff：状态行 + 结论（≤ 4000 字）+ 最近 20 条事件摘录（≤ 2500 字）+ 待审批动作
     子代理读过不可信内容 → 整个 handoff 标为不可信，父代理随之进入污染态
```

`delegate_parallel` 用 `std::thread::scope` 按 `max_parallel` 分批并行，结果按输入顺序返回；某个子代理线程 panic 只会让它自己的结果变成 failed。`Db` 内部是可重入互斥锁，多个子代理线程共享同一个数据库连接，同一线程在事务里嵌套调用不会自锁。

内置类型：`explore`（只读）、`execute`（全部工具，可再委派）、`research`（网页 + 只读）。工作区 `agents/*.md` 可以加类型；不能覆盖内置类型，没写 `tools` 的自定义类型默认只读。

深度：主代理 0、子代理 1、孙代理 2。可以委派的类型在到达上限时仍保留配置，只是不再看到委派工具；如果模型硬要调用，会被记录为一条 rejected 运行。

### 会话 fork / rewind

`fork` 复制整段对话（包括压缩标记和摘要），不复制审批、文件改动和子代理记录。`rewind --to <seq>` 删除该序号之后的记录；如果截断点会留下“发起了工具调用但没有结果”的消息，会拒绝并给出可用的序号；删掉的摘要所覆盖的旧记录会恢复为可见。对话回退不影响文件，文件用检查点回退。

## 4a. 通用工具：文件、命令、网页

```
fs_read / fs_list / fs_glob / fs_grep ──► Workspace.resolve（规范化 → 拒绝 .. 逃逸 → 最深已存在祖先必须 canonicalize 在根内）
                                          → O_NOFOLLOW 打开 → 输出标为不可信
fs_write / fs_edit ──► 读当前内容 → 生成完整新内容 + 差异预览 + base_hash ──► 审批
                       执行器 WriteFileExecutor：当前哈希 ≠ base_hash → 拒绝；否则 CheckpointStore.snapshot → 原子写
shell_run ──► ExecPayload{root, cwd, command, timeout} ──► 审批 ──► ExecCommandExecutor → hatch-sandbox
web_fetch / web_search ──► FetchPayload ──► （未污染时自动批准）──► 执行器：guard 检查每一跳 → HTML 转文本 → 不可信
```

- 写入、命令、网页访问都是"延迟动作"：工具只生成带不可变 payload 的审批，真正的副作用只在执行器里发生，和发邮件走同一条路径（门禁 → 审批 → 幂等账本）。
- payload 里带着提议时的**工作区绝对路径**，所以 `hatch approvals approve` 在任何目录、用任何参数执行，跑的都是被审阅的那个动作。
- 命令、网页、搜索的执行结果标为不可信，会污染后续上下文。

### 沙箱进程链（`hatch-sandbox`）

```
hatch ──spawn（新进程组，环境清空）──► P：helper（hatch __sandbox-helper 或 hatch-sandbox-helper）
                                         │ 父进程退出则 SIGKILL；从父命名空间写 C 的 uid/gid 映射（65534 ↔ 调用者）
                                         └─fork─► C：unshare(user|mount|ipc|uts|pid|net)
                                                    └─fork─► G：新 pid 命名空间的 1 号进程
                                                       setsid · 主机名 · tmpfs 新根（只读 /usr /bin /lib* /sbin、/etc 白名单、
                                                       /dev 少量设备、私有 /tmp、/proc、可写工作区）· pivot_root · 根只读
                                                       · rlimit · 清空 capability · no_new_privs · seccomp · execve
```

超时由调用方杀掉 P 的整个进程组：C 退出 → G 收到死亡信号 → 1 号进程退出时内核清理整个 pid 命名空间。任何一步失败都返回带标记的错误，命令不会在缺少隔离的情况下运行。

## 4d. 浏览器（`hatch-browser`）

```
browser_open ──► 目标地址检查 ──► （未污染时自动批准）──► BrowserOpenExecutor ──► Broker.open
                                                                                    │
Broker：每个 profile 一个 Chromium 进程（main / scheduler / research，独立 user-data-dir 与 HOME/NSS 证书库）
  │  启动：专用 launcher 线程 spawn（PR_SET_PDEATHSIG 跟随进程而非线程）、独立进程组、DevTools 只监听 127.0.0.1
  │  端点写入 <profile>/hatch-devtools.json，其他 hatch 进程（如 hatch approvals approve）据此重新连接
  ▼
会话 = 一个标签页（browser_sessions 表）：Created → Navigating → Ready ⇄ Busy → NeedsUser（登录 / 验证码）→ Closed
  │  每个标签页一条 CDP WebSocket；读线程处理 Fetch.requestPaused（每个请求、每一跳重定向做目标检查）和 JS 对话框
  ▼
快照：固定的内部脚本给可见的可操作元素打 data-hatch-ref 编号，返回正文 + 元素列表；模型只能用编号引用元素，不能执行脚本
  ▼
detect：页面状态（正常 / 需要登录 / 人机验证 / 站点拦截 / 结账页）与点击分类（链接 / 页面控件 / 提交 / 支付，不确定按提交）
```

- **工具与动作类别**：`browser_open` 是 `fetch_web`；读取、查找、滚动、截图、后退、关闭、输入、选择是 `browse_local`（不向网站提交任何东西，不受污染规则限制）；`browser_click` 的类别由代码按元素决定：链接 → `fetch_web`，页面控件 → `browse_local`，可能提交 → `browser_submit`（不可逆，默认审批），支付 → `payment`（只能建议）。
- **提交的审批**：payload 记录会话、元素编号、页面地址、元素描述和完整表单内容，并附截图路径。`BrowserClickExecutor` 在点击前重新读取页面、元素与表单，任何一项与审批时不同都拒绝点击。
- **跨进程**：会话记录里有 target id；另一个进程通过 DevTools 端点连到同一个标签页。浏览器已退出时，会话在下一次使用时于原地址重新打开（同一 profile 的登录状态保留，之前填写的内容不保留，结果里会说明）。
- **接管**：`hatch browser takeover <id>` 用有界面的 Chromium 打开同一 profile 的页面，用户完成登录或验证后关闭窗口；`hatch browser resume <id>` 让会话回到 Ready，下次使用时重新打开。
- **回收**：标签页空闲超过 `idle_timeout_secs` 自动关闭；本进程启动的浏览器在没有打开的会话或进程退出时停止。
- **测试工具**：`hatch_browser::testkit::MockSite` 是一个本地网站（文章、筛选表单、登录墙、验证码、联系表单、结账、注入页面、内网重定向），记录每一次表单提交。

## 4e. 技能、MCP 与凭据保管

```text
技能：$HATCH_HOME/skills/<名>/SKILL.md ──► parse（手写 frontmatter，无 YAML 库）
        │  hatch skills list（只看元数据）/ load（激活进 active.json）
        └─► 对话每轮：正文进上下文稳定层（记忆之后、后台更新之前）
            + 工具按技能的 tools 列表过滤（list_skills / load_skill 永远保留）
            + load_skill 工具可当轮加载，allowlist 从当轮起生效
MCP 客户端：[[mcp_servers]] ──► AnyClient::connect（transport = stdio | http）
        │  stdio：Client::connect（子进程，JSON-RPC 2.0，协议 2024-11-05），一次重连
        │  http：HttpClient::connect（POST JSON-RPC 到 url，普通 JSON 或 SSE 回复）
        │  超时按 timeout_secs（默认 30）；token_ref 经 vault reveal，stdio 进
        │  子进程环境 MCP_TOKEN，http 作 Authorization: Bearer（值永不记日志）
        └─► 工具注册为 mcp_<服务器>_<工具> ──► to_approval（kind mcp_call）
            ──► 审批队列 ──► McpCallExecutor ──► 服务器进程（输出标不可信）
MCP 服务端：hatch mcp serve ──► Server::run_stdio（stdin/stdout NDJSON）
        │  或 --transport http ──► HttpServer::run（streamable HTTP，见下）
        │  tools/list = 传入的 Registry（内置 + 文件/命令/网页 + 连接器 +
        │  浏览器；不含 mcp_* 客户端工具与技能管理工具）
        │  tools/call ──► dispatch_call（同对话路径：plan → 门禁 → 审批队列）
        │  需要审批 → structuredContent {status: approval_required,
        │  approval_id, reasons}，不执行；轮询用 approval_status 工具
        │  只读直接执行；结果 structuredContent.trust 标注 system/untrusted
        │  resources/list|read = hatch://people、hatch://memory（真实数据，各限 100）
        │  prompts/list = []（诚实空列表）；每调用超时（默认 60s），stdin EOF 或
        │  Ctrl-C/SIGTERM 时退出；HTTP 传输见下（`--transport http`）
连接器：$HATCH_HOME/connectors/<名>.toml ──► Runner::call（子进程 CLI，一行 JSON 请求 / 一行 JSON 响应）
        │  超时（默认 30s）杀整个进程组；错误码稳定，不含秘密与上游原文
        └─► 工具注册为 connector_<连接器>_<操作> ──► to_approval（kind connector_call）
            ──► 审批队列 ──► ConnectorCallExecutor ──► 连接器进程（输出标不可信）
            token_ref 存在时由 vault helper 注入子进程环境（inject），agent 进程不经手值
vault helper：$HATCH_HOME/vault/tokens.json（目录 0700，文件 0600，记录格式）
        │  hatch-vault-helper 进程持有 token（stdin/stdout NDJSON 协议）
        │  hatch vault set（stdin / --from-env 读值）/ list（只显示名字 + 过期标记）/ delete
        │  status / stop；随父进程死亡（PDEATHSIG + 父 pid 轮询）与空闲超时（默认 300s）退出
        └─► MCP 的 token_ref 是这里的 token 名字；长连接用 reveal（生成瞬间），一次调用用 inject
OAuth：begin_flow（127.0.0.1 临时端口监听）→ wait_for_code（验 state）→ exchange_code（存入 vault）
        只对 mock 端点做过端到端测试；真实供应商需要注册 client id 与 redirect URI（未做）
```

- **技能**：`hatch-skills` 只做解析与发现。`SKILL.md` 超过 64 KiB、frontmatter 缺失/损坏、名字非法都会被拒绝；`scan` 跳过损坏的技能而不是让整个列表失败。技能正文是用户自装的工作方式说明，不是用户指令：它不能让模型发信、改文件或做任何对外动作而不经审批；它的 `tools` 列表只能收窄、不能扩大可用工具。
- **MCP 客户端**：`hatch-mcp` 支持 stdio 与 HTTP 两种传输（JSON-RPC 2.0，协议 `2024-11-05`）。stdio：`initialize` 之后才 `tools/list`，一次重连；读超时、EOF、JSON-RPC 错误都会如实报错，不静默降级。HTTP：向 `url` POST JSON-RPC，接受普通 JSON 回复或 SSE 流（`data:` 事件里找匹配 id 的回复，注释与无关事件跳过）；不支持 WebSocket（workspace 的 `tungstenite` 只有 handshake 特性）。所有远程工具在对话里都走 `to_approval`（`mcp_call`），策略允许时自动执行并记入审计；执行结果进 `UNTRUSTED_RESULT_KINDS`，返回给模型时包进 `<external_content>`。CLI 的 `hatch mcp call` 只读工具直接执行，写入类工具需要 `--approve` 并在输出里明确写出确认。token 经 vault helper `reveal` 在生成子进程的瞬间传入（helper 起不来时回退文件读取并警告）；HTTP 时同一 token 作 `Authorization: Bearer`，header 值永不记入日志，HTTP 错误只报状态码、不带回包体。
- **MCP 服务端**：`hatch mcp serve`（`hatch mcp server --stdio` 同义）把 hatch 工具通过 stdio 暴露出去；`hatch mcp serve --transport http`（`--bind` 默认 `127.0.0.1:8080`）则通过 streamable HTTP（MCP 协议 `2025-03-26`，stdio 仍是 `2024-11-05`）暴露同一套工具。`tools/list` 来自调用方传入的真实 Registry（CLI 组装：内置 + 文件/命令/网页 + 连接器 + 已启用的浏览器工具；不含 `mcp_*` 客户端工具与技能管理工具）。HTTP 端点都在 `/mcp`：`POST` 一个 JSON-RPC 请求（`initialize` 建会话、回 `Mcp-Session-Id` 响应头；之后每次请求必须带该头，缺失 400、未知 404；通知回 202；`Accept: text/event-stream` 时回 SSE `data:` 事件，否则回普通 JSON；不支持 batch），`GET` 为已建会话开 SSE 流（目前不发服务端通知，只有 `: connected` 与每 15 秒一次的 `: ping`，会话删除或关机时关闭），`DELETE` 终结会话（204）。两种传输共用同一个 `Server`：`tools/call` 都走 `dispatch_call`——与对话完全相同的 plan → 门禁 → 审批队列 → 执行器路径，上下文标记为已污染；需要审批时不执行，返回 `structuredContent {status: "approval_required", approval_id, reasons}`，调用方用 `approval_status` 工具轮询，用户在 hatch 里批准后轮询看到 `executed`。只读调用直接执行。结果的 `structuredContent.trust` 标注 `system` / `untrusted`。`resources/list|read` 是真实的 `hatch://people` 与 `hatch://memory`（各限 100 条）；`prompts/list` 诚实返回空列表。每个调用在 worker 线程跑、有超时（默认 60s，`--timeout-secs` 可改）；超时后返回 `-32000`，worker 线程脱离继续跑（可能事后完成——见 SECURITY.md）；stdin EOF 或 Ctrl-C/SIGTERM 时干净退出。两种传输的审批用不同的 origin 隔离：stdio 是 `mcp:serve`，HTTP 是 `mcp:http`——`approval_status` 只能查到同一 origin 的审批，HTTP 客户端看不到 stdio 客户端的审批，反之亦然。HTTP 的安全措施（绑定、token、限流）见 SECURITY.md §9d。
- **连接器框架**：`hatch-connectors::framework`（manifest / protocol / runner / registry）。清单是 `$HATCH_HOME/connectors/<名>.toml`（或 `$HATCH_CONNECTORS_DIR`）：`name` / `command`（无 shell，直接 spawn）/ `args` / `env`（变量名）/ `token_ref`（vault token 名）/ `token_env`（默认 `CONNECTOR_TOKEN`）/ `timeout_secs`（默认 30）/ `ops`（名字、说明、`read_only`、参数 schema）/ `connect_guidance`。Runner 协议：一次调用一个进程，stdin 一行 JSON `{"op","args","request_id"}`，stdout 一行 JSON `{"ok","result"|"error":{"code","message"}}`；错误码是稳定的中性码（`unknown_op` / `invalid_args` / `not_configured` / `auth_missing` / `not_found` / `upstream_error` / `timeout` / `bad_response` / `internal`），消息里绝不出现秘密与上游原文。超时杀整个进程组（unix 上 setsid + kill(-pgid)）。`status` 操作做状态检查：没有 `status` 操作、缺环境变量、超时或任何失败都返回 `ok: false` + 指引，绝不挂起、绝不承诺结果。工具层（`hatch-tools::connector`）把每个操作注册为 `connector_<连接器>_<操作>`，`call` 永远拒绝，`to_approval`（`connector_call`）走审批队列；只读操作用 `FetchWeb` 类，写入操作用 `ExecCommand` 类；执行器有 `token_ref` 时走 vault helper `inject`（值只进子进程环境），否则直接 spawn。结果进 `UNTRUSTED_RESULT_KINDS`。计划器（`hatch plan`）也提供连接器工具；MCP / 技能工具仍未提供给计划器。参考连接器只有两个：`hatch-notes-connector`（`HATCH_NOTES_DIR` 下的 `<名>.md`，`status` / `list_notes` / `read_note` / `write_note` / `delete_note`）与 `hatch-mockhttp-connector`（`HATCH_MOCKHTTP_BASE_URL`，只连测试时启动的本地 mock 服务器，`status` / `get_item` / `create_item`）。**不做 65 个真实连接器**：新供应商连接器 = 新清单 + 一个实现 runner 协议的子进程 CLI，框架不用改。
- **凭据保管**：`hatch-vault` 是文件存储 + 独立 helper 进程。文件是每 token 一条记录的 `tokens.json`（`value` / `connector` / `expires_at` / `refresh_token` / `refresh_supported` / `refresh_url`），目录 0700、文件 0600，原子重写；旧版“名字 → 字符串”格式仍能读，首次写入时迁移为记录格式。`hatch-vault-helper`（`hatch-vault` 的 bin）持有 token：父进程（agent/CLI）只持有名字，通过 stdin/stdout NDJSON 发请求。`inject`：helper 生成子进程并把 token 设进**子进程独有**的环境，跑完返回 exit code/stdout/stderr，父进程看不到值；`request`：helper 自己做 HTTP 调用并加 `Authorization: Bearer`，只返回状态与 body；`reveal`：把值交回父进程——只用于长连接 stdio MCP 服务器生成子进程的瞬间，调用方必须立即丢弃，这是唯一有文档记录的父进程内存暴露。`refresh`：有 `refresh_token` + `refresh_url` 且 `refresh_supported` 时对该端点换 token（只在 mock 上测过），否则报 `refresh_unsupported`。helper 随父进程死亡（启动时 `PR_SET_PDEATHSIG` + 每 500ms 轮询 `--parent-pid` 是否存活）、空闲 300 秒自动退出，也可 `shutdown` 操作或 `hatch vault stop`（读 `helper.pid` 发 SIGTERM）停止；`hatch doctor` 检查 helper 状态与文件权限。helper 不能启动时，CLI 回退到文件直接读写并在 stderr 明确警告。

## 4f. 计划模式（`hatch-plan`）

```text
hatch plan new <任务> / hatch chat --plan
  → request_plan：系统提示 + 可用工具列表 → 模型（Main workload，json_mode）
      → parse_plan_json：extract_json（整段 / 代码块 / 最外层 {...}）
        → 校验：steps 非空（≤ 32）、id 唯一、title/expected 非空、arguments 是对象
                工具必须在 registry 里、files 与 path 参数经 Workspace.resolve
                （无工作区时任何碰文件的步骤都被拒绝）
      → PlanStore.create：plans + plan_steps，状态 pending_approval
        （同一任务尚未批准的旧计划自动 superseded）
  → hatch plan approve <id>：只接受 pending_approval / needs_reapproval
      → 快照全部步骤的规范 JSON + 哈希（approved_steps_json / approved_hash）
      → 记录 decided_by + decided_at + note，并写审计日志 plan_approved
  → hatch plan run <id>：只接受 approved / executing（后者是断点续跑）
      → 全量 detect_deviation（多步 / 少步 / 顺序）→ 偏差 → needs_reapproval + deviation 记录，停
      → 每步执行前 check_step（工具、参数、文件与快照逐项比对）→ 偏差 → 同上，停
      → dispatch_call（与对话同一条门禁 → 审批 → 执行路径）
          Done → 记录输出 → 下一步
          排队审批 → 步骤记为 awaiting_approval + approval id，整轮暂停；
                    审批决定后再次 run：Executed → 从幂等账本取结果补记完成，
                    Rejected/Expired/Failed → 步骤失败，停
          失败（工具错误 / 门禁拒绝）→ 步骤记为 failed，停；计划留在 executing，可续跑
      → 全部完成 → completed
  → hatch plan edit：改步骤（工具/参数/文件/标题/预期/风险），改前按同一规则重校验；
      已批准的计划改后回到 pending_approval，快照清空，必须重新批准
```

- **状态机**：`draft → pending_approval → approved → executing → completed`，另有 `rejected / superseded / cancelled / needs_reapproval`。`approve` 只接受 `pending_approval` / `needs_reapproval`；`run` 只接受 `approved` / `executing`；终态（`completed / rejected / superseded / cancelled`）不可编辑、不可再批准。
- **计划即审批边界**：批准的不是"任务意图"，而是快照里那一份精确的步骤清单。执行时核对的是快照，不是当前步骤表——编辑过的步骤在重新批准前不可能被执行。
- **不断点重跑**：已完成的步骤跳过；失败的步骤在续跑时重试；等审批的步骤在审批决定后从账本取结果，不重复执行已批准的动作。
- **对话集成是浅的**：`hatch chat --plan` / `[agent] plan_mode = true` 只在回合开始前走"制定计划 → 存库待批 → 打印 → 返回"这一支，`hatch-agent` 的回合循环本身没有改动。计划的工具清单来自对话当时组装好的 registry（含技能 allowlist 过滤）；但 `hatch plan new` 的 CLI 注册表目前只含内置 + 文件/命令/网页/浏览器工具，MCP 与技能工具暂不进计划（见 SECURITY.md 9e）。

## 4g. 产物呈现（`hatch-artifacts`）

一个新 crate，放 widget 与 artifact 两件东西（它们共用"把模型产出变成可
交付、可分享的东西"这一职责，分开两个 crate 只会多一层依赖）。

**Widget**（`widget.rs`）：JSON 卡片，三种类型——`options`（选项：id /
label / reply）、`list`（行：title / subtitle / link）、`html`（HTML 卡片，
解析时即过 allowlist 净化）。每个 widget 带一个 `embed_token`
（`hatch_core::ids::random_id("wgt")`）。渲染规则由 `render(widget,
token_present)` 编码：无 token 只返回纯文本 fallback（`text_fallback`），
有 token 才附上卡片 JSON；fallback 不丢信息（全部选项 label 与 reply、全
部行与链接、HTML 的可见文本都在）。CLI 没有界面，永远走 fallback。MCP
服务端把卡片 JSON 放进 `structuredContent.widget`（与 `trust` 并列），客
户端合规的做法是只在 token 出现在消息里时渲染卡片——这个规则在渲染点执
行，不在传输层强制，见 SECURITY.md §9f。

**Widget 如何流到各处**：工具结果多了一个结构化侧通道——`ToolOutput` /
`DispatchResult` 的 `widget: Option<Value>` 字段（纯 JSON，避免
`hatch-tools` 与 `hatch-artifacts` 循环依赖）。`show_options` 工具
（`ReadPersonal`，直接调用）返回 fallback 文本 + widget；`dispatch_call`
原样透传；MCP 服务端抄进 `structuredContent.widget`；chat 回合忽略它
（模型看到的是 fallback 文本，可以直接引用）。`create_artifact` /
`share_artifact` 是审批型工具，执行结果是纯文本（批准后执行的输出没有
widget 侧通道——执行器只返回字符串，见 SECURITY.md §9f 的缺口）。

**Artifact**（`store.rs`）：版本化目录 `$HATCH_HOME/artifacts/<id>/v<n>/`，
每版一个内容文件（`content.md` / `content.html`）+ `manifest.json`（id、
标题、类型、版本、创建时间、来源、SHA-256 校验和），`meta.json` 记标题、
版本列表、已分享、已交付。新版本永不覆盖旧版；读版本时校验 checksum，
不符即拒绝。类型只有 `document` 与 `html`，其他类型直接拒绝。构建前用
`hatch_core::secrets` 扫描标题+内容，命中则 `SecretRejected`（错误里只写
秘密种类，不回显值），且什么都不写；HTML 类型构建时过 `sanitize_html`。
`--from` 的源文件走 Workspace 守卫（无工作区配置时拒绝）。

**分享与交付是两步，不是文件写完就算**（`store.rs` / `executor.rs` /
`commands/artifact.rs`）：
`hatch artifact share <id>` 只提交一个 `share_artifact` 审批（类别
`SendMessage`，幂等键 `share-artifact:<id>`）；`hatch approvals approve`
执行 `ShareArtifactExecutor`，它是唯一把 `shared` 置真的代码；`deliver`
要求已分享，组装含最新版绝对路径的交付消息后才把 `delivered` 置真。只写
文件不交付时 `delivered == false`——`delivered` 是"交付消息已发出"的记录，
不是文件存在与否。助手侧的 `create_artifact`（`WriteFile` 类，默认需审批，
执行器 `create_artifact`）与 `share_artifact`（`SendMessage` 类）同样走
门禁 → 审批 → 执行器 → 幂等账本；执行器里对秘密再扫一次（纵深防御）。

## 4h. 多通道传输（`hatch-transports`）

设计依据是 Muse agent 框架（深挖文档 §9.4、§12；路线图 R13 /
RW-16）：主 chat 与 side chat 共享记忆/目标、各有 transcript；外部
chat 的 worker 只备内容、主 agent 转述与决定；每 chat 有
`DISCRETION.md`；设备能力声明式、缺失时如实告知开通路径。以下哪些是
Muse 的证据、哪些是本仓库的原创设计，各模块文档里写明了——文件布局、
pipe 传输的 JSON 形状、redact 的行模式、默认能力清单都是 ORIGINAL。

**接口**（`transport.rs`）：`Transport` trait（`name` / `status` /
`list_chats` / `send` / `poll_once`）+ `TurnRunner`（transport 回调进
来的 agent 回合：`run_turn(msg, session, input)`）+ `TurnOutcome`
（回答文本、可选 widget、可选待交付 artifact id、内容标签、待审批
id）。`TransportRegistry` 按名字注册；未配置的传输只报告状态与指引，
不承诺投递。`not_implemented()` 列出 wechat / feishu / telegram 三个
已知名字——它们只是名字，不是传输。

**消息**（`message.rs`）：inbound 是 channel / chat / sender / 文本
和/或一张图片；图片是引用（文件名 + mime + 大小），字节绝不进 prompt，
图片也绝不是指令。outbound 是 relay 组装好的最终文本（已过 discretion、
widget 已渲染、artifact 已交付）。

**每聊天隔离**（`chats.rs`）：`$HATCH_HOME/chats/<chat-id>/` 下
`session.json`（`{"session_id": "ses_…"}`，首次使用时建会话，标题
`transport:<transport>:<chat-id>`）与 `DISCRETION.md`。记忆与目标在同
一个数据库里——主 chat 与 side chat 共享记忆，但 transcript 各走各的
session，chat A 的消息永不进 chat B 的上下文（单测覆盖）。

**分享规则**（`discretion.rs`）：`DISCRETION.md` 每行一条
`forbid: <类别>` / `allow: <类别>`，类别是 mail / other_people /
secrets / files；未写明的默认允许，缺文件等于全开放——但 secrets 永远
拦截，与文件无关。检查顺序：秘密扫描命中或 `secrets` 标签 → 整条拦截；
带标签的违禁类别 → 整条拦截；否则对可定位的行做 redact（引用的邮件头
`发件人/主题/…`、`张三说：` 式转述），删完没剩可发内容时拦截。标签来自
turn runner——按本轮用过的工具推断（`mail_*` → mail，`fs_*` →
files），是启发式（见 SECURITY.md §9g 的缺口）。用户在对话中随口说的
规则（`extract_mentioned_rules`，只认"不要/别 + 把/发/分享 + 类别"这
种显式否定句式）由 pipe 传输在跑回合前记进 `DISCRETION.md`，并在回合
输入里告诉模型已记下。

**转述规则**（`relay.rs`）：worker 只产 `Draft`（文本 + 标签 + 事实
+ 需要的能力），`submit_draft` 只把草稿存进
`chats/<chat-id>/drafts/<draft-id>.json`、什么都不发；`deliver_draft`
必须带 `MainDecision`（主 agent 批准/拒绝，可改写文本），没有决定的
草稿没有任何发送路径。转述前同样跑 discretion 检查（对最终文本），草
稿需要的能力在设备清单里缺失时，转述被拒、回复是能力缺失的如实答复。
主 agent 自己的回合答案走 `deliver_main`（回合本身就是决定），同样过
检查、widget 渲染与 artifact 交付。审批事件进通道：turn 排队的审批 id
（`TurnOutcome.pending_approvals`，RW-8 的要求）附在发出的文本末尾。

**设备能力**（`capability.rs`）：`$HATCH_HOME/devices/<device-id>/
capabilities.json`，每项能力有 `supported` 与 `how_to_enable`；缺失
时的固定答复是"设备 X 不支持 Y，所以我现在做不了这件事。开通路径：…。
开通后告诉我，我再试。"未知设备一律按不支持处理（fail closed）。默认
只有 `local` 本机清单（`file_access` 支持；`computer.control` /
`camera` / `microphone` 不支持——Mac 无 `computer.control` 是本机实
测）。真实配对、`peerd` 同步、设备控制都不在范围内。

**pipe 参考传输**（`pipe.rs`）：`$HATCH_HOME/transports/pipe/` 下
`inbox/`（每条消息一个 JSON，`image` 是收件箱内的普通文件名；mime 与
大小取自真实文件）、`outbox/`（每条回复一个 JSON：`status` sent /
blocked）、`processed/`、`failed/`、`send.log`（每行一条发送记录）。
`poll_once` 按文件名顺序处理：解析 → 校验 → 记录随口提到的规则 →
解析会话 → 跑回合 → 经 relay 交付（`send` 写发件箱文件；拦截时写
blocked 通知文件，绝不含被拦截的草稿）→ 移进 `processed/`。图片回合
的输入里附"无法直接查看图片内容、被问到要如实说看不到"的说明；模型不
支持 vision，传输不假装支持。CLI 的 `transport run/poll` 用
`TransportRunner`（与 `hatch chat` 同一套门禁、审批队列与工具注册表，
但不提供浏览器与 MCP 工具）跑回合；`executed_deliveries`
（`outcome.rs`）从会话 transcript 里读回本轮已执行的
`deliver_artifact` 调用——只有批准已执行的交付才会把 artifact id 交
给传输，排队未批的交付不会被说成已交付。

## 4i. CalDAV 双向同步（`hatch-connectors::caldav`）

Phase 6，没有对应的 Muse 证据——这是本仓库自己的设计，按 CalDAV
标准（RFC 4791 / RFC 6578）实现。模块放在 `hatch-connectors` 里而不
是新 crate：同步只是现有日历存储（`calendar::CalendarRepo`、ICS 解析、
`calendar_events`）上的一层薄封装，加新 crate 只会重复导出同一批依赖。

**客户端**（`client.rs`）：PROPFIND 发现日历集合（先查 base URL，找
不到日历且有 `calendar-home-set` 时再查 home）、REPORT calendar-query
全量拉取、REPORT sync-collection 增量同步、PUT（新建用 If-None-Match
`*`，更新用 If-Match etag）、DELETE（If-Match）。HTTP 经 vault helper
（`VaultHttp` → `VaultClient::request_raw`）：token 只在 helper 进程里
变成 Bearer 头，本进程从不持有、记录、存储 token 值。helper 的
`request` op 为此扩展了任意方法、raw body 与自定义头（原先只支持
GET/POST + JSON body）。DAV XML 用手写解析器（`xml.rs`，工作区没有 XML
crate；只取 href / getetag / sync-token / calendar-data / status），
畸形 XML 是错误、不是"没有变化"。重试：瞬时 5xx 最多一次，4xx 与传输
错误不重试。

**同步状态**（`state.rs`，三张新表，见 schema.rs）：`caldav_calendars`
（每集合的 sync-token、ctag、最近同步时间）、`caldav_event_state`
（每事件的 etag、本地 hash、是否冲突中）、`caldav_conflicts`（冲突
记录：UID、种类、两边更新时间、保留了哪边、本地版本快照 JSON 或
`__deleted__`、状态）。本地变化靠稳定 hash（sha256 over
uid/title/起止时间/地点）与上次同步时的 hash 对比发现——本地事件没有
编辑时间戳，冲突记录里的"本地更新时间"是发现冲突的时间，文档里写明了，
不假装是编辑时间。

**一次同步**（`sync/mod.rs` + `sync/apply.rs`）：拉取（有 token 用
sync-collection，没有用宽范围 calendar-query；服务器拒绝
sync-collection 时回退全量拉取，etag 对比仍保证幂等）→ 应用到本地
→ 推送本地新增/修改/删除 → 存新 token。没有状态的本地事件（如 .ics
导入的）推送到主日历（配置的 `calendar_url`，否则第一个发现的集合）。
**冲突规则**（写死在 `sync` 模块文档与本节）：服务器获胜；本地版本
记进冲突记录；只有 `resolve --keep local` 明确批准后，本地版本才在下
次同步推送；`resolve` 本身不推送。删除冲突同理：服务器删、本地改 →
本地副本保留在库里直到处理；本地删、服务器改 → 服务器版本恢复进本地，
删除记进冲突记录。`resolve` 与 `status` 不走网络（`CaldavState` 的
方法），`sync` 与 `doctor` 的探活才联网。

**同步结果的去向**：CLI `calendar sync` 默认只打印摘要；`--chat <id>`
时摘要经 `Relay::deliver_main` 发到指定聊天——走与主 agent 回合答案
完全相同的路径（discretion 检查、widget 文本回退、artifact 交付），
绝不直接写 outbox 文件。

**已知局限**：不展开循环事件——拉取只取 DTSTART/DTEND，本地编辑后推送
会丢 RRULE；不同步参会人回复与附件（除非 .ics 自带）；PUT 后读回 etag
要多一次 PROPFIND（vault 路径只返回 status + body，不返回响应头）；
多日历时，未同步过的本地事件一律进主日历。

## 4c. 定时调度与后台 worker

```
hatch daemon run ──► Daemon（pid 锁 · stop 文件 · SIGTERM/SIGINT → 根取消令牌）
                      │  每 tick_secs：
                      ▼
                   Engine::tick(now)
                      │  对每个到期任务：枚举 [next_run_at, now] 内的全部触发
                      ├─ 有待重试 / 上次被取消 → Retry（其余触发记为错过）
                      ├─ 最后一次触发迟到 ≤ grace → Scheduled（更早的记为错过）
                      └─ 否则全部记为错过；catch_up = once 时补跑一次
                      ▼
                   execute：job_runs 写 running → 子令牌 + 看门狗（timeout_secs）
                      → JobHandler::run(ctx) → ok / skipped / failed / cancelled
                      ▼
                   成功：next_run_at = 下一次触发，失败计数清零
                   失败：保留 next_run_at，retry_at = now + 退避，计数 +1；达到上限 → 停用 + 通知
                   取消：保留 next_run_at，下次启动重跑
```

- **调度表达式**（`schedule.rs`、`cron.rs`）：wall-clock 形式按用户时区计算，`localtime::resolve` 统一处理夏令时：重复的时刻取较早的那个（只触发一次），不存在的时刻顺延到间隙之后第一个存在的时刻；`every` 从锚点（创建或重新启用的时刻）按真实时间计算。
- **三表分离**：`jobs`（定义 + 调度状态）、`job_runs`（每次运行与"错过 k 次"的记录）、`job_dedup`（任务自己的去重键，如 `brief:2026-11-03`）。
- **daemon 中的模型调用**：worker 拿到的是 `RouterFactory`，每次运行新建路由器，所以 `[scheduler] max_calls_per_job` 是每次运行的预算，不会在常驻进程里一次用尽；用量写入 `usage_log`。
- **崩溃恢复**：daemon 启动时把残留的 `running` 记录标为失败（之后正常重试）；调度状态都在数据库里，重启后从 `next_run_at` 继续。

### 内置任务种类（`hatch-workers` + `hatch-goals`）

| 种类 | 行为 | 模型 |
|---|---|---|
| `daily_brief` | `facts::gather` 由代码收集事实（近 36 小时分诊出的紧急/待办/待回复邮件、今明两天日程、日程冲突、待审批、未读提醒）并编号；`compose` 让 Brief 模型只做排序和措辞、每个条目引用编号，代码丢弃无引用或引用不存在编号的条目，把未被引用的事实原样列在「其他事项」，覆盖缺口由代码写明；无事实时不调用模型；`brief:<日期>` 去重 | `brief` workload |
| `triage_sync` | 调用分诊工作流，门禁换成 `Gatekeeper::scoped(policy, job.scope)` | `triage` / `draft` |
| `memory_flush` | 同主题、同信任级别、相似度 ≥ 0.92（字二元组 Dice，去标点大小写）的记录自动合并为纠正链；0.75–0.92 只提议，模型仅标注"是否同一事实"；列出已过期记录与常联系人 | `memory_flush`（只标注） |
| `daily_feed` | 与简报共用 `facts::gather` 的事实；Feed 模型按 `[feed] prompt` 整理成条目，代码校验每条的事实引用（无引用或引用不存在即丢弃），无模型/输出不是 JSON/无有依据条目时退回"每条事实一条"的程序清单；无事实时不生成条目；`feed:<日期>` 去重；开场白是固定文本、只插一次 | `feed` workload |

后台产出写入 `worker_outputs`。`hatch chat` / `hatch ask` 开始时读取未读产出，按 1500 token 预算放进上下文的「后台更新」层（包在不可信信封里），然后标记已读；子代理不继承这一层。

### 4j. 目标、点子与动态（`hatch-goals`）

本节全部是本仓库自己的设计（ORIGINAL）——没有可对照的 Muse 框架证据，不主张这是 Muse 的行为。

- **目标工作区**：`$HATCH_HOME/goals/<id>/GOAL.md`（标题、`status:`、`created:`/`updated:` + 自由笔记）、`files/`（支撑文档，用户可读）、`hidden_files/`（worker 内部状态，不向用户展示）。状态机：`active → completed / paused / abandoned`，`paused → active / completed / abandoned`；`completed` / `abandoned` 是终态。`hatch goal create/list/show/complete/pause/resume/abandon`。
- **关注度**：纯函数 `goals::attention_level`——终态目标永远 `does_not_need_attention`；有未读且关联到该目标的 worker 产出（`worker_outputs.goal_id`）时为 `user_action_required`；48 小时内更新过为 `new_information`；其余为 `does_not_need_attention`。`hatch goal list/show` 实时计算，不存库。
- **点子库**：SQLite `ideas` 表（与 claims / briefs 同一惯例），字段 id / text / source / status（`new` / `acted` / `dismissed`）/ confidence / created_at。`hatch ideas list` 的 SQL 本身排除 `dismissed`，没有"包含已忽略"的开关，所以已忽略的点子无法通过列表再次出现。worker 产生的点子由 `record_suggestion` 记入：置信度固定 0.3、来源 `worker:<kind>`、状态 `new`——是建议，永远不是用户决定。
- **动态（feed）**：见上表 `daily_feed`。`feed_items` 表字段 id / created_at / title / content / source_facts（JSON 事实编号列表）/ model / is_intro。开场白 `INTRO_TEXT` 是固定文本，只说明 feed 是什么、每条都有来源，不声称了解用户；`ensure_intro` 只插一次。动态经 `WorkerOutputs`（kind `feed`）交接，不直接写进聊天。
- **为什么是独立 crate**：worker 是"定时跑的任务"，目标/点子/动态是"用户数据 + 产品面"，存储（磁盘工作区 vs SQLite 表）与 CLI 各不相同；`hatch-workers` 不依赖它，`hatch-goals` 依赖 `hatch-workers` 复用事实收集与产出交接，方向单一。

## 5. 模型层

- `ChatModel` trait 有四个 HTTP 实现：Chat Completions、Responses、Anthropic Messages、Gemini `generateContent`。还有两个确定性实现（脚本化模型、闭包模型），用于测试和评估。
- 流式：`ChatModel::chat_stream` 推送 `StreamEvent`（`TextDelta` / `ToolCallStarted` / `Done`）。四个 HTTP 实现都通过共享的 SSE 解析器（`provider/sse.rs`）做真流式，各自的累积器（`chat_stream.rs`、`responses_stream.rs`、`anthropic_stream.rs`、`gemini_stream.rs`）把事件拼回与非流式完全相同的 `ChatResponse`；没有原生流式的实现走 trait 默认方法（先整段请求再一次性发出事件）。
- `Router::chat_stream` 沿用同样的重试 / 降级 / 预算规则，另加两条：服务端在发出任何内容前拒绝流式（`StreamRejected`，如 400/404/415）时，用同一个模型非流式重试一次；一旦已经输出了文本或工具调用，出错就直接返回，不再换模型，避免输出两个不同的回答。
- Gemini 的 `reasoning_effort` → `thinkingBudget` 映射（low 1024 / medium 8192 / high 24576）以及把 Gemini 调用 id 和 `thoughtSignature` 打包进中立的 `ToolCall::id`（`<id>~sig~<signature>`）都是本项目自己的设计。
- 所有 HTTP 请求（含流式）遵守 `HTTPS_PROXY` / `NO_PROXY`，CA = 内置 Mozilla 根 + `HATCH_CA_BUNDLE` / `SSL_CERT_FILE`。
- **降级阶梯（`hatch-llm::router::FallbackTable`）**：按 workload 的有序表，每级由 trigger + action 组成。默认：`refused_params → next_model`，`timeout / server_error → retry_same`（同模型重试一次，再下一个），`empty_answer → stop`。配置 `[[routing.fallback]]` 只改写提到的 workload/trigger，其余保持默认；默认不跨 provider 切换。trigger 是对已有错误种类 + 报错文本的启发式分类（timeout 含 "timeout / timed out / deadline / connection"，5xx 含 "500/502/503/504/5xx/server error/internal error/overloaded"，其余 transient 归为 timeout，permanent 归为 refused_params），不是 provider 语义。空回答（无正文也无工具调用）是 `LlmErrorKind::Backend`：router 直接返回错、不记用量、不烧下一个模型的调用。每步都记入事件日志，格式 `ladder <workload> level=<n> <action>: …`。
- **提示缓存**：`ChatRequest.cache_ttl_secs` 由 router 按 workload 从 `[routing.cache_ttl_secs]` 填入（请求自带的值优先）。只有 Anthropic 会发送 `cache_control`（ephemeral，300 → `5m`，3600 → `1h`；配置校验只接受这两个值），挂在系统提示最后一个块上，无系统提示时挂在最后一条 user 文本块上。OpenAI 系与 Gemini 忽略该字段——它们的 `cached_tokens` 仍从 usage 上报里读取。`Router::cache_stats()` 按 workload 聚合成功调用的 prompt / cached / total token 与命中率（cached / prompt）；失败的调用不记用量所以不进统计。`hatch doctor` 对 `usage_log` 做同样的分组展示。
- `Router` 按 workload（main / triage / draft / compaction / memory_flush / brief / feed）选择模型链。瞬时错误（429、5xx、网络错误）重试一次，再换下一个模型；永久错误直接换下一个；预算错误立即停止。
- 调用预算在**每次尝试之前**检查，失败的尝试也计数。
- 用量只记录 token（含 cached），不估算金额。

## 6. 数据

一个 SQLite 文件（`~/.hatch/hatch.db`，WAL 模式）。主要表：

| 表 | 内容 |
|---|---|
| `mail_messages` / `mail_cursor` | 原始邮件与 UID 游标 |
| `triage_results` | 分类结果（摘要已脱敏） |
| `calendar_events` | 日程 |
| `claims` / `people` | 记忆与联系人 |
| `approvals` / `action_ledger` / `grants` | 审批、幂等账本、常设授权 |
| `notifications` | 通知与投递方式 |
| `workflow_runs` / `workflow_steps` | 运行记录、回执、持久化步骤 |
| `sessions` / `session_messages` | 对话与压缩标记 |
| `usage_log` / `audit` | token 用量与审计日志 |
| `checkpoints` | 文件修改前的快照（工作区、路径、哈希、内容；单文件 ≤ 2 MiB，总量 ≤ 200 MiB，最多 2000 个） |
| `subagent_runs` / `subagent_events` | 子代理运行（上级、所属会话、深度、类型、任务书、状态、步数、用量、结论）与逐步进度事件 |
| `jobs` / `job_runs` / `job_dedup` | 定时任务定义与调度状态、每次运行（含"错过 k 次"）、任务去重键 |
| `briefs` / `worker_outputs` | 每日简报（正文 + 事实清单 + 模型）、等待交给下次对话的后台产出（`goal_id` 关联目标，空 = 无目标） |
| `ideas` / `feed_items` | 点子库（文本、来源、状态、置信度）、动态条目（标题、正文、来源事实编号 JSON、模型、是否开场白） |
| `browser_sessions` | 浏览器会话：profile、来源对话、target id、当前地址与标题、状态与原因 |
| `plans` / `plan_steps` | 计划：任务原文、状态、批准人/时间/备注、批准步骤快照与哈希、偏差记录；每步的工具、参数、文件、预期、风险、执行状态与输出 |

`meta` 表里还有 `daemon_state`、`daemon_started_at`、`daemon_last_tick`、`daemon_stopped_at`（`hatch daemon status` 读取）。

## 7. 扩展点

- **新场景**：写一个新的工作流 crate，复用 `route_action`、`Notifier`、`Receipt` 的模式。
- **新连接器**：写一个实现 runner 协议的子进程 CLI + 一份 `$HATCH_HOME/connectors/<名>.toml` 清单即可，不用改框架；需要 token 时在清单里写 `token_ref`，执行器会走 vault helper 注入。旧的硬编码连接器（`MailSource` / `MailSender`）仍在，`ActionExecutor` 仍是批准后执行的扩展点。
- **支付（明确延期）**：没有钱包、结账或提交动作。`ActionClass::Payment` 的上限是 `Suggest`，gatekeeper 一律 Deny（只建议、不执行）；浏览器结账点击已映射到 `ActionClass::Payment`。扩展点是 `hatch-connectors::payment`（stub：`request_payment` 永远拒绝，并写明接线步骤）。接真实支付商时：写一个走 runner 协议的连接器子进程 + `$HATCH_HOME/connectors/payment.toml` 清单（`token_ref` 走 vault 注入），再由产品决策决定是否放宽 Payment 上限——agent loop 与 gatekeeper 本身不用改。
- **其它应用（明确延期）**：除两个参考连接器（`hatch-notes-connector`、`hatch-mockhttp-connector`）、CalDAV（§4i）与 pipe 参考传输外，没有真实接入。接新应用时按“新连接器”或“新传输”的扩展点走；微信 / 飞书 / Telegram 已在 `TransportRegistry::not_implemented` 里列为未实现，`hatch doctor` 如实报告。
- **新工具**：实现 `Tool`；有外部效果的工具实现 `to_approval`，不要实现 `call`。执行结果含外部内容的执行器，把类型名加进 `UNTRUSTED_RESULT_KINDS`。
- **搜索后端**：实现 `SearchBackend`（内置 SearXNG 兼容实现）。
- **子代理类型**：在工作区放 `agents/<name>.md`，或在代码里往 `TypeRegistry` 加 `AgentType`。
- **新任务种类**：实现 `hatch_scheduler::JobHandler`，在 `hatch-cli/src/sched.rs` 注册；有外部效果的动作用 `Gatekeeper::scoped` 并走 `route_action`；产出写 `WorkerOutputs`，用 `deliver` 按任务的投递方式通知。
- **浏览器页面识别**：登录墙、验证码、站点拦截、结账页和点击分类的规则在 `hatch_browser::detect`，修改时同步更新 `detect` 的单元测试。
- **新评估世界**：邮件类世界放 persona、mailbox、calendar、expectations 四个文件；工作区类世界放 `project/`、`tasks.json`、`expectations.json`（含审批 allow 规则）；子代理类世界放 `project/` 和 `delegate.json`；定时任务类世界放 `scheduler.json`；浏览器类世界放 `browser.json`（任务、期望与审批 allow 规则，站点由 `MockSite` 提供）。
