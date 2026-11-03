# 架构

## 1. 分层原则

| 层 | crate | 规则 |
|---|---|---|
| 基础层 | `hatch-core`、`hatch-store` | 不依赖任何领域概念 |
| 领域层 | `hatch-llm`、`hatch-memory`、`hatch-policy`、`hatch-connectors`、`hatch-sandbox` | 互不依赖，只依赖基础层 |
| 能力层 | `hatch-tools` | 把领域能力组合成"工具"，并提供**唯一的外发路径** |
| 场景层 | `hatch-agent`、`hatch-triage`、`hatch-eval` | 编排；不直接调用连接器发送任何东西 |
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
  classify  隔离分类（无工具、JSON、逐字段校验）→ 代码后处理（VIP / 低优先级 / 注入规则）
  calendar  会议请求 → 冲突检测 → 无冲突提议 create_event / 有冲突给出空闲时段
  draft     需回复且不可疑 → 起草正文 → reply_payload（收件人代码固定）→ route_action
  notify    AttentionPolicy：Immediate / Digest / Silent
  complete
receipt   已完成 / 未完成 / 待你决定 + 调用与 token 统计
```

每一步都会写入 `workflow_steps(workflow, item, step)`。标记为 `done` 的步骤不会重做，`failed` 的步骤下次运行重试。调用预算耗尽时整轮停止，剩下的邮件下次自动续跑。

## 4. 对话回合

```
用户输入 → 写入会话 → 超过阈值则压缩（在用户消息边界切，保留最近 N 轮）
loop ≤ max_steps:
  分层组装：行为规约 → 当前时间 → 用户画像（preference/style 类 claim）→ 本轮检索记忆 → 早先摘要 → 历史
  预算：CJK 感知估算；只丢最旧的完整回合
  模型（Main）→ 文本：结束
             → 工具调用：逐个 dispatch；需要审批的入队后继续，不等待
  不可信的工具输出会让本轮后续的外发处于"污染"状态
```

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

## 5. 模型层

- `ChatModel` trait 有四个 HTTP 实现：Chat Completions、Responses、Anthropic Messages、Gemini `generateContent`。还有两个确定性实现（脚本化模型、闭包模型），用于测试和评估。
- 流式：`ChatModel::chat_stream` 推送 `StreamEvent`（`TextDelta` / `ToolCallStarted` / `Done`）。四个 HTTP 实现都通过共享的 SSE 解析器（`provider/sse.rs`）做真流式，各自的累积器（`chat_stream.rs`、`responses_stream.rs`、`anthropic_stream.rs`、`gemini_stream.rs`）把事件拼回与非流式完全相同的 `ChatResponse`；没有原生流式的实现走 trait 默认方法（先整段请求再一次性发出事件）。
- `Router::chat_stream` 沿用同样的重试 / 降级 / 预算规则，另加两条：服务端在发出任何内容前拒绝流式（`StreamRejected`，如 400/404/415）时，用同一个模型非流式重试一次；一旦已经输出了文本或工具调用，出错就直接返回，不再换模型，避免输出两个不同的回答。
- Gemini 的 `reasoning_effort` → `thinkingBudget` 映射（low 1024 / medium 8192 / high 24576）以及把 Gemini 调用 id 和 `thoughtSignature` 打包进中立的 `ToolCall::id`（`<id>~sig~<signature>`）都是本项目自己的设计。
- 所有 HTTP 请求（含流式）遵守 `HTTPS_PROXY` / `NO_PROXY`，CA = 内置 Mozilla 根 + `HATCH_CA_BUNDLE` / `SSL_CERT_FILE`。
- `Router` 按 workload（main / triage / draft / compaction / memory_flush）选择模型链。瞬时错误（429、5xx、网络错误）重试一次，再换下一个模型；永久错误直接换下一个；预算错误立即停止。
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

## 7. 扩展点

- **新场景**：写一个新的工作流 crate，复用 `route_action`、`Notifier`、`Receipt` 的模式。
- **新连接器**：实现 `MailSource` / `MailSender`，或者新增一个 `ActionExecutor`。
- **新工具**：实现 `Tool`；有外部效果的工具实现 `to_approval`，不要实现 `call`。执行结果含外部内容的执行器，把类型名加进 `UNTRUSTED_RESULT_KINDS`。
- **搜索后端**：实现 `SearchBackend`（内置 SearXNG 兼容实现）。
- **新评估世界**：邮件类世界放 persona、mailbox、calendar、expectations 四个文件；工作区类世界放 `project/`、`tasks.json`、`expectations.json`（含审批 allow 规则）。
