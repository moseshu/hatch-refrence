# Hatch 验证层

本次实现基于 `e7e06fb`。实现开发计划见 [专项计划](Docs/verification-development-plan.md)。本地未编译、未运行 Rust 测试；下列命令供服务器验证。

## 完成协议

计划执行统一经过：固定契约 → 保存执行意图 → 执行/审批 → 保存原始回执 → 独立回读 → 保存验证报告 → 提交步骤完成 → 任务结束前重新检查最终资源状态。

普通执行、审批恢复、回执已落库但步骤未完成的恢复共用结算函数。审批执行账本证明动作执行事实，验证报告证明观察到的资源状态，两者分开保存。已有收据的步骤恢复时只回读，不重新执行副作用。

校验结果为 `pass / fail / ambiguous / not_applicable`；模式为 `off / observe / enforce`。`not_applicable` 仅指读取步骤不产生外部写入或检查被显式关闭，不能被宣传为业务目标已验证。工作流 `StepVerifier` 是附加业务条件，无法覆盖独立回读失败。

`Answered` 表示回答生成，不表示业务目标已完成。聊天中有待审批或未证实的动作时，用明确状态替换候选完成回答。等待审批仍可作为已回答返回；已经尝试执行但未通过验证的动作返回 `verification_blocked`。进度事件仍实时发送，答案文字在完成检查后交付。

## 配置

- `HATCH_VERIFICATION_MODE=enforce`：默认值。受支持副作用必须有有效证据；未覆盖的副作用不隐式通过。
- `HATCH_VERIFICATION_MODE=observe`：保存影子报告，保留计划原有完成行为。报告保留 observe 标记，不能显示成“已验证”。
- `HATCH_VERIFICATION_MODE=off`：显式关闭检查，记录该事实，保留执行检查点。用于兼容性对照，不会删除历史报告。
- `HATCH_SEMANTIC_VERIFICATION=1`：在聊天本轮存在已独立确认的批准动作时，额外启用隔离语义 grader。默认关闭；先用实际服务器任务评估成本和误判。grader 不能覆盖确定性检查失败。

计划当前版本使用的模式在开始执行时固定。改变环境变量不重新解释旧版本结果。规则变更或已固定目标的修改需要新的计划版本。

语义 grader 只接收目标、rubric、经过独立回读确认的资源内容与候选产物；没有工作者历史或写入工具。它必须逐项输出结构化裁决并引用提供的证据 ID。缺条件、重复条件、伪造引用、无证据 Pass、非法 JSON、模型失败均不算通过。一次最多调用一个模型、输入 128KB、输出 2048 tokens；调用和成功返回的用量计入当前回合报告。模型失败未返回的 token 用量无法从供应商获取。

## 已有检查器

| 动作 | 证据 | 比较内容 |
| --- | --- | --- |
| `write_file` | 使用批准 payload 中的工作区重新读文件 | 完整批准内容的字节；仍遵循工作区边界和读取上限 |
| `create_event` | 按 UID 查本地日历数据库 | UID、标题、开始、结束、地点；不宣称同步到了远端 |
| `create_google_event` | 用原执行器的 Google writer 对 primary 发起独立 get | Google ID、标题、开始、结束、地点、参与人 |
| `send_email` | 原发送服务的 `read_sent` | 发件人、收件人、主题、正文；仅证明已发送记录，不证明收到/阅读 |

邮件正文只归一化 CRLF 与末尾空白；回读正文被解析器截断时视为证据不足。FileOutbox 读取实际 EML；Gmail 使用 `gmail users messages get --params '{"userId":"me","id":"...","format":"raw"}'`，校验 provider ID 和 SENT 标签，再解析原文。服务器 helper 必须支持该读取接口及相应 OAuth 权限；不支持时返回 ambiguous，不重复发送。其他 MailSender 默认明确报告不支持独立回读。

Google insert 的回执在独立 get 前保存。insert 结果不明确时保留 NeedsReview；get 失败不会把已创建事件变成可以重新 insert 的普通失败。时间比较使用解析后的绝对时间；参与人按规范化地址集合比较。取消或不可解析事件不能作为有效证据。

任务结束时重新回读最终资源；同一计划多次写同一文件，最后一次写入定义最终状态。独立业务步骤和终局资源检查不是通用自然语言目标正确性的完整证明，复杂跨应用语义还需要相应工作流 rubric 或语义 grader。

## 持久化与恢复

schema v22 增加 `verification_runs` 和 `verification_reports`。前者保存固定契约、模式、执行阶段、原始回执、审批关联、观察次数；后者保存规则摘要、条件裁决、证据来源和摘要、观察时间。任务详情 CLI/API 会展示报告，聊天 API 返回本轮验证报告。

- 保存动作结果后崩溃：补做读取和裁决。
- 保存报告后崩溃：复用报告并幂等提交步骤状态。
- dispatch 后、保存回执前崩溃：保守报告未知，人工对账，不盲目重放。
- 已保存待审批关联、步骤状态还未更新：用持久审批 ID 恢复。
- 未修改的完成步骤经历局部重规划：沿用原始批准 payload 和收据，重新读取，不重复动作。
- 旧数据库没有证据的 Done：不批量补成已验证；缺少可靠回读信息时明确阻塞。
- 计划版本、验收标准、目标、规则摘要失配，或任务已取消/暂停：拒绝迟到报告和完成提交。

Pass 的独立观察在 5 分钟内有效；超期只能用剩余预算重新读取，不能复用旧 Pass。每个步骤和任务终局检查最多 3 次观察，预算持久化，重启不清零。只有 ambiguous 才在同次运行中继续读取；明确 Fail 不靠重复执行碰运气。预算耗尽后保留失败原因；未知状态路由 NeedsHuman。明确不符合验收标准进入既有 Permanent/重规划处理，不自动重复原操作。

自动生成修复计划、跨版本累计的两轮修复预算，以及通用浏览器/任意命令的业务检查器仍属于后续扩展；当前不会把三次读取冒充三轮自动修复。执行期租约仍由现有任务运行时负责；验证提交另外检查计划版本和任务取消/暂停状态，未新增完整的租约 generation 绑定协议。

## 规则维护

机器规则在 [builtin-v1.json](verification/rules/builtin-v1.json)，编译进检查器。运行任务只能固定并引用已发布规则，不能通过修改工作区文件改变当前裁决。新增规则经 Git 评审发布，保留稳定 ID、触发条件、读取来源、比较条件与回归案例；不能让被验证的工作者在当次运行中修改规则。

新失败应先写入 `verification/candidates/`，附正确对照和可复现条件，再评审晋升。操作技巧与验收条件分开记录。规则调优样本和固定评估集应分开，避免把调优命中率当泛化效果。

## 服务器验证

先使用独立测试数据库和 fixture；不要让 workspace 测试连接真实发送服务。

```sh
HATCH_VERIFICATION_MODE=enforce cargo test -p hatch-plan --test run_plan
HATCH_VERIFICATION_MODE=enforce cargo test -p hatch-tools --test create_google_event
HATCH_VERIFICATION_MODE=enforce cargo test -p hatch-connectors
HATCH_VERIFICATION_MODE=enforce cargo test -p hatch-agent
cargo clippy --workspace --all-targets
cargo test --workspace --no-fail-fast
```

新增回归覆盖：伪成功写入无法被业务回调放行；审批恢复也检查真实文件；观察预算跨恢复保持；保存执行回执后不重复写入；修改 expected 导致审批偏离；Google 回读不一致与执行回执分离；grader 的伪造证据和缺失验收条件不能通过。

真实账户试点分别验证正确结果、错字段、读取超时、helper 不支持读取、进程中断和恢复。对照 off/observe/enforce 使用新的计划版本，避免复用旧版本固定模式。检查数据库里的执行次数、验证报告数，以及 CLI/API 的状态一致性。

首轮服务器验证后再扩展语义 grader 的默认使用范围。上线记录至少包含样本数、误放行、误拦截、ambiguous、重复副作用、耗时与模型成本；固定测试零失败不能解释为生产零风险。
