<!--
Copyright (c) 2026 Moses (https://github.com/moseshu)
Author: Moses
SPDX-License-Identifier: MIT
-->

# 架构

Author: Moses

## 1. 分层原则

| 层 | crate | 规则 |
|---|---|---|
| 基础层 | `hatch-core`、`hatch-store` | 不依赖任何领域概念 |
| 领域层 | `hatch-llm`、`hatch-memory`、`hatch-policy`、`hatch-connectors` | 互不依赖，只依赖基础层 |
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

## 5. 模型层

- `ChatModel` trait 有两个 HTTP 实现：Chat Completions 兼容协议和 Messages 协议。还有两个确定性实现（脚本化模型、闭包模型），用于测试和评估。
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

## 7. 扩展点

- **新场景**：写一个新的工作流 crate，复用 `route_action`、`Notifier`、`Receipt` 的模式。
- **新连接器**：实现 `MailSource` / `MailSender`，或者新增一个 `ActionExecutor`。
- **新工具**：实现 `Tool`；有外部效果的工具实现 `to_approval`，不要实现 `call`。
- **新评估世界**：在 `eval/worlds/<name>/` 下放 persona、mailbox、calendar、expectations 四个文件。
