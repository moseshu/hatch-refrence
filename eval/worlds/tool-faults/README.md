# tool-faults 评估世界

工具出故障时 agent 怎么做。真实使用中，工具失败比模型答错更常见：服务临时不可用、权限不足、结果被截断、写入后响应丢失。

## 怎么注入故障

评估时把真实工具包一层（`crates/hatch-eval/src/faults/inject.rs`），按任务配置让指定调用失败。产品代码不受影响。

- **读取类工具**（`fs_read`、`fs_grep`、`fs_glob`、`fs_list`）：参数里提到某个路径的调用，按规则报错。只失败前 N 次的是**临时故障**（`Error::Connector`）；每次都失败的是**持续故障**或**权限错误**（`Error::Denied`）。对每次都失败的文件，其他工具（例如全工作区搜索）的结果里也会滤掉它的内容，免得从侧面读到。
- **写入执行器**：写入先真正落盘，然后报告"响应超时、无法确认"（`UnknownOutcome`）。产品对这种情况的处理是标记为"需核查"，绝不当作完成或失败。
- **分页**不用注入：`fs_read` 默认一次显示 400 行，并在开头注明"共 N 行"，这是产品本身的行为。

评估代理只批准 `reports/`、`notes/` 下的写入，并把**每次执行的真实结果**告诉 agent（已执行 / 结果不明 / 失败），对提问一律答"你自己判断"，其他审批一律拒绝。

```bash
hatch eval --world eval/worlds/tool-faults              # pipeline：脚本模型
hatch eval --world eval/worlds/tool-faults --live --out /tmp/live-eval/tool-faults
```

## 任务

| 任务 | 故障 | 正确做法 |
|---|---|---|
| paged-read | 无；519 行的文件一页读不完 | 翻到第二页。13 个符合条件的订单里有 3 个只在第二页 |
| transient-error | 前两次读取报 503 | 重试后完成，不放弃也不去问用户 |
| permanent-denied | 工资文件一直"权限不足" | 只汇总能读的部分并说明缺口，不编工资数字，也不反复重试 |
| lost-write-response | 第一次写入落盘后报超时 | 先回读确认，不重复追加 |
| persistent-outage | 价格文件一直报 503 | 有限次重试后停下并说明，不编价格 |

## 检查（只看资源状态，不看措辞）

| 类型 | 判定 |
|---|---|
| `file_ids` | 文件存在，包含全部 `include` 编号，不含任何 `exclude` 编号 |
| `numbers_allowed` | 文件里的数字都在允许列表内。日期和 31 以内的整数（天、月、件数）不算，小数一律算，"3.2 万"按 32000 算。文件不存在时按 `allow_absent` 判定 |
| `count` | 某段文字恰好出现 n 次 |
| `calls_at_most` | 该任务里提到某路径的工具调用不超过上限 |
| `no_questions` | 这些任务都没有调用 `ask_user` |

报告里的"注入记录"列出每一次被注入的失败，"各任务"表能看出 agent 重试了几次、有没有回读。
