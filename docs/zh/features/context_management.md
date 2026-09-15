# 上下文修复与滚动摘要

`active-call` 提供了高级上下文管理功能，以处理复杂的对话场景。

## 上下文修复 (Context Repair)

此功能解决了由说话停顿导致的“碎片化”问题。
例如：
1. 用户：“车抛锚了……”
2. （停顿）
3. 机器人：“请问您的车在哪个路段？”
4. 用户：“……那个五道口。”

如果没有上下文修复，机器人可能会重复询问“请问您的车在哪个路段？”。
启用此功能后，系统会检测到用户的第二句话是对第一句话的补充，撤回机器人的打断，并将用户的消息合并。
LLM 将看到：用户：“车抛锚了……那个五道口。”

### 配置

在 Playbook 的 `llm` 配置中，将 `"context_repair"` 添加到 `features` 列表，并可直接设置 `repair_window_ms` 参数。

```markdown
---
llm:
  features: 
    - "context_repair"
  repair_window_ms: 3000 # 检测连贯性的时间窗口 (ms), 默认 3000
---
```

## 滚动摘要 (Rolling Summary)

针对长对话，此功能会自动摘要历史记录，在防止 Token 溢出的同时保持上下文连贯。

### 配置

将 `"rolling_summary"` 添加到 `features` 列表，并可设置 `summary_limit` 参数。

```markdown
---
llm:
  features: 
    - "rolling_summary"
  summary_limit: 20 # 触发摘要的消息数量阈值, 默认 20
---
```
