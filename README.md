<!--
Copyright (c) 2026 Moses (https://github.com/moseshu)
Author: Moses
SPDX-License-Identifier: MIT
-->

# hatch-refrence — 个人 Agent 助手

> 作者：**Moses**（[github.com/moseshu](https://github.com/moseshu)） · 许可证：MIT · 语言：Rust

hatch 是一个运行在你自己机器上的个人助理。第一个落地场景是**邮件 + 日程分诊**：拉取新邮件、分类摘要、发现会议冲突、按你的风格起草回复，然后把所有对外动作放进审批队列，等你确认后才执行。

它的设计重点不是"能做多少事"，而是**做事的方式可信**：

- **外部内容永远只是数据。** 邮件正文进入模型前会被脱敏并包进不可信信封。回复收件人由代码按原邮件固定，模型改不了。
- **对外动作只有一条路径**：门禁 → 审批队列 → 执行器 → 幂等账本。不会阻塞等待，不会重复发送，过期的审批永不执行。
- **说实话。** 每次运行都给出三段式回执：已完成 / 未完成 / 待你决定。"已提交审批"不会被说成"已发送"。
- **可度量。** 内置沙盒评估世界（16 封合成邮件，含注入攻击与验证码），每次改动都能跑出数字。

---

## 架构

```
                    ┌──────────────────────────── hatch-cli（hatch 命令）────────────────────────────┐
                    │ init · triage · approvals · grants · memory · calendar · chat/ask · digest · eval · doctor │
                    └───────────────┬──────────────────────────────┬──────────────────────────────┘
                                    │                              │
                 ┌──────────────────▼─────────┐        ┌───────────▼────────────┐      ┌──────────────┐
  场景层         │ hatch-triage 邮件+日程分诊 │        │ hatch-agent 对话循环   │      │ hatch-eval   │
                 │ 隔离分类·起草·冲突·回执    │        │ 规约·分层上下文·压缩   │      │ 沙盒评估     │
                 └──────────────┬─────────────┘        └───────────┬────────────┘      └──────┬───────┘
                                └───────────────┬──────────────────┘                          │
  能力层                         ┌──────────────▼─────────────────┐                           │
                                 │ hatch-tools 工具·执行器·dispatch │◄──────────────────────────┘
                                 └──────┬───────────┬─────────────┘
                 ┌──────────────────────┼───────────┼─────────────────────────┐
  领域层         │ hatch-policy         │ hatch-memory          │ hatch-connectors        │ hatch-llm
                 │ 门禁·授权·审批·账本  │ claim·纠正·BM25·人物  │ IMAP·SMTP·MIME·ICS      │ 协议·路由·降级·预算
                 │ ·注意力预算          │                       │                         │
                 └──────────────────────┴───────────┬───────────┴─────────────────────────┘
  基础层                                 hatch-store（SQLite）  ·  hatch-core（错误·ID·信任·token·秘密）
```

| crate | 职责 |
|---|---|
| `hatch-core` | 错误类型、内容寻址 ID、信任标签与不可信信封、CJK 感知 token 估算、秘密检测与脱敏 |
| `hatch-store` | 单文件 SQLite、幂等迁移、事务、自动脱敏的审计日志 |
| `hatch-llm` | Chat Completions 兼容 / Messages 两种协议、按任务路由、降级阶梯、调用预算、JSON 提取 |
| `hatch-memory` | claim 记忆（来源/信任/置信度/有效期）、纠正链与 explain、中文 BM25 检索、forget、人物页 |
| `hatch-policy` | 动作类别 × 可逆性 × 自主度 L0–L3、门禁规则、常设授权、异步审批队列、幂等账本、注意力预算 |
| `hatch-connectors` | 最小 IMAP（含网易 ID 命令）、SMTP 465、MIME、GBK 解析、夹具邮箱与 outbox、ICS、冲突与空闲时段 |
| `hatch-tools` | Tool SDK、内置工具、执行器、统一 dispatch（门禁 → 审批 → 执行） |
| `hatch-agent` | 行为规约（`prompts/`）、分层上下文组装、压缩、会话、非阻塞回合循环 |
| `hatch-triage` | 分诊工作流：隔离分类、代码后处理、会议处理、起草、持久化步骤、回执 |
| `hatch-eval` | 评估世界、oracle/live 两种模式、安全与幂等指标、报告 |
| `hatch-cli` | `hatch` 命令行、配置与环境变量覆盖、费用护栏 |

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

**方式 B：配置文件。** 在 `~/.hatch/config.toml` 的 `[[models]]` 里填 `api_key_env`（只写环境变量名），并可在 `[routing]` 里为分类、起草、压缩分别指定模型和降级顺序。

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
hatch chat                       # 交互式；/approvals 查看待审批，/exit 退出
```

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
| `hatch eval [--live] [--max-calls N] [--max-tokens 4000] [--reasoning-effort low] [--out dir]` | 沙盒评估 |
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

完整说明见 [docs/SECURITY.md](docs/SECURITY.md)。

---

## 评估

```bash
hatch eval                         # pipeline 模式：不调用任何真实模型
hatch eval --live                  # live 模式：使用配置的模型（可加 --max-calls N 限制调用次数）
```

当前 pipeline 模式的结果（`eval/worlds/zh-office`，16 封邮件）：**11/11 检查通过**。非对抗样本分类 14/14，需回复草稿召回 5/5，发往攻击者的审批和发件都是 0，秘密泄露 0，冲突检出 1/1，重跑新增审批和模型调用都是 0，执行后重跑发件数保持 6 → 6。

> pipeline 模式由 oracle 模型按期望答案作答，并在对抗样本上**故意被骗**。它只验证管线和结构性防线，**不代表任何真实模型的能力**。

live 模式（真实推理模型 `gpt-6-luna`，2026-11-03，单次运行）：**11/11 检查通过**。非对抗样本分类 85.7%（12/14，两封通知类邮件被判为 action），需回复草稿召回 5/5、精确率 5/5，两封对抗邮件都被标记可疑，发往攻击者的审批和发件都是 0，秘密泄露 0，冲突检出 1/1，重跑 0 新增；共 21 次调用，tokens 17,798 / 5,543（其中推理 2,626）。样本小、单次运行，详见 [docs/EVALUATION.md](docs/EVALUATION.md)。

测试：`cargo test --workspace` 共 108 个测试，全部通过；`cargo clippy --workspace --all-targets` 零警告。

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

1. **每日简报**：定时汇总邮件、日程、待办，复用注意力预算与回执。
2. **出行与预订代办**：浏览器能力 + 支付只建议不执行。
3. **多通道**：企业微信 / 飞书 / Telegram 作为消息入口，每个通道有自己的分享规则。
4. **CalDAV 双向同步**：替代 ICS 单向导入。
5. **扩充评估世界**：已有真实模型基线；下一步增加更多语言风格、长线程、附件，并多次运行取均值。
6. **后台整理 worker**：记忆合并、过期提醒、联系人重要度推断（推断结果低置信、可否决）。

---

## 许可证

MIT © 2026 Moses Hu。见 [LICENSE](LICENSE)。
