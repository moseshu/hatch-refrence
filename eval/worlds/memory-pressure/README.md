# memory-pressure 评估世界

记忆召回加压：记忆够多、固定注入装不下时，任务需要的那条记忆能不能进到模型的上下文里；进不来时，模型会不会自己去查。

memory-crossday 的记忆只有几条，带标签的偏好会被整份注入，测不到召回。这个世界预置 32 条记忆：

- 12 条最早的带标签偏好，正好占满固定注入（按创建时间取最早 12 条）；
- 4 条**目标记忆**，比这 12 条新，只能靠自动召回或模型主动搜索拿到；
- 16 条普通记忆，其中不少和请求有共同的词，参与 BM25 排名竞争。

## 怎么跑

一共 3 种自动召回方式。每次只改召回方式，模型、配置、任务、时钟都不变：

```bash
hatch eval --world eval/worlds/memory-pressure                                   # pipeline：脚本模型
hatch eval --world eval/worlds/memory-pressure --live --recall auto   --out /tmp/live-eval/memory-pressure/auto
hatch eval --world eval/worlds/memory-pressure --live --recall legacy --out /tmp/live-eval/memory-pressure/legacy
hatch eval --world eval/worlds/memory-pressure --live --recall off    --out /tmp/live-eval/memory-pressure/off
```

| `--recall` | 含义 |
|---|---|
| `auto` | 产品现状（默认）：固定注入 12 条，其余记忆走 BM25 自动召回，只排除实际注入的那 12 条 |
| `legacy` | a891e66 修复之前：所有带标签的记忆都不参与召回（包括第 13 条以后的），且先取前 5 再排除 |
| `off` | 关闭自动召回，固定注入和 `memory_search` 不变：只能靠模型主动搜索 |

`legacy` 和 `off` 只是评估开关，不在用户配置里。

## 目标记忆

| 目标 | 会话 | 情形 | 修复后的自动召回 |
|---|---|---|---|
| signature | supplier-reply | 词面交集：请求和偏好都提到回复、供应商、邮件 | 能召回；`legacy` 召回不到（带标签） |
| contract-owner | contract-revision | 字面编号：请求给出 HT-2291，旁边有编号相近的 HT-2219 | 能召回（不带标签，三种方式里只有 `off` 拿不到） |
| room | design-sync | 换了说法：请求说"开会的地方"，偏好说"会议室" | 召回不到：只共享"会""订"这类单字 |
| peanut | client-lunch | 完全没提：请求不涉及饮食，和偏好没有共同的索引词 | 召回不到 |

"修复后的自动召回"一列是按现有分词和排序推算的，pipeline 测试会核对它，真模型下排序完全相同。

## 报告怎么看

**检查**只看任务结果（不看措辞），计入通过数：

- 供应商回复的落款带"分机 8021"，收件人是周敏；
- 合同修订稿只发给周宁（收件人恰好是他一人），不发给李娜；
- 设计会日程在 12 月 1 日 15:00 到 16:00，地点含 301；
- 客户午餐只选了不含花生的两家。

**目标记忆**表是观测记录，不计入通过数：

| 列 | 含义 |
|---|---|
| 首个请求 | 会话的第一次模型请求里，这条记忆以什么方式出现在系统上下文（固定注入 / 自动召回 / —） |
| 任一请求 | 会话内任意一次请求里出现过（审批后的后续轮次会用新的输入重新召回） |
| 工具找到 | `memory_search` / `memory_explain` 的结果里出现过这条记忆 |
| memory_search / 提问 / 步数 | 为拿到它付出的代价 |

把"首个请求"和检查结果对照看，就能分开两件事：

- 自动召回拿到了、任务做对了：召回起作用了；
- 自动召回没拿到、任务仍然做对了：模型自己查到了（看"工具找到"和 memory_search 次数）；
- 两个都没有：这条记忆实际上用不上。

`auto` 对 `legacy` 的差别集中在 signature；`auto` 对 `off` 的差别说明自动召回省了多少次主动搜索，或者救回了多少原本会失败的任务。

## 评估代理

和 memory-crossday 相同：只批准 `notes/` 下的文件写入；邮件和日程草稿保持待审批，供检查；其他审批（包括 `ask_user` 提问）一律拒绝，提问次数记在目标记忆表里。不跑夜间提取。
