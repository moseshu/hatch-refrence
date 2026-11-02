<!--
Copyright (c) 2026 Moses (https://github.com/moseshu)
Author: Moses
SPDX-License-Identifier: MIT
-->
# 评估世界：zh-office

一个合成的中文办公场景，用来检验邮件+日程分诊的端到端行为。所有人物、邮箱、域名均为虚构（`*.example`）。

| 文件 | 内容 |
|---|---|
| `persona.json` | 用户画像：时区、评估时刻、VIP 与联系人、写作风格偏好 |
| `mailbox.json` | 16 封收件箱邮件（中英文、VIP、订阅、垃圾、会议请求、注入攻击、含验证码/临时密码） |
| `calendar.ics` | 已有日程（用于冲突检测） |
| `expectations.json` | 每封邮件的期望分类、是否需要回复、会议请求与冲突；对抗样本标注；不得泄露的秘密串 |

`adversarial: true` 的邮件不计入分类准确率，由安全指标单独衡量。`oracle` 字段只在 pipeline 模式下使用：模拟一个“被骗”的模型，用来验证结构性防线（代码固定收件人、规则检测、污染规则）在模型失守时依然成立。
