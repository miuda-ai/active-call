# SIP 变量系统完整说明

## 概述

Active-Call 的变量系统现在明确区分 **SIP Headers** 和 **普通变量**，提供更清晰的变量管理和模板渲染机制。

## 核心特性

### 1. 变量分类

#### SIP Headers
- **识别规则**：通过 `sip.extract_headers` 配置提取的 headers
- **来源**：
  - 从 SIP INVITE 请求中提取（配置 `sip.extract_headers`）
  - 通过 `<set_var key="X-xxx" value="..."/>` 动态设置
- **访问方式**：`{{ sip["X-Header-Name"] }}`（必须使用字典语法）
- **用途**：SIP 协议相关的元数据
- **重要**：Header 名称中的连字符会被 Jinja2 解析为减号，所以 `{{ X-Header }}` 会失败

#### 普通变量
- **识别规则**：不以 `X-` 或 `x-` 开头的变量
- **来源**：
  - 通过 `<set_var key="var_name" value="..."/>` 设置
  - 外部注入（API 调用时传入）
- **访问方式**：`{{ var_name }}`
- **用途**：业务逻辑相关的数据

#### 内置会话变量（v0.3.38+）
- **识别规则**：系统自动注入，无需配置
- **来源**：通话建立时自动生成
- **访问方式**：`{{ session_id }}`、`{{ call_type }}` 等
- **变量列表**：
  - `session_id` — 通话会话唯一标识
  - `call_type` — 通话类型（`sip`/`websocket`/`webrtc`/`b2bua`）
  - `caller` — 主叫方 SIP URI（仅 SIP 通话）
  - `callee` — 被叫方 SIP URI（仅 SIP 通话）
  - `start_time` — 通话开始时间（RFC 3339）
- **特性**：不会覆盖外部传入的同名变量

### 2. 模板访问

在 Playbook 的 YAML 配置和 Prompt 中，可以使用以下语法：

```yaml
---
llm:
  greeting: "你好 {{ sip[\"X-Customer-Name\"] }}，你的会员等级是 {{ member_level }}"
---
# Scene: main
客户编号：{{ sip["X-Customer-ID"] }}
业务类型：{{ business_type }}
```

**关键点**：
- ✅ SIP Headers 必须通过 `sip` 字典访问：`{{ sip["X-Header"] }}`
- ✅ 普通变量直接访问：`{{ variable_name }}`
- ❌ 错误：`{{ X-Header }}` 会被解析为减法运算

### 3. set_var 指令

LLM 可以在对话过程中动态设置变量：

#### 设置 SIP Header
```xml
<set_var key="X-Call-Status" value="completed" />
<set_var key="X-Ticket-ID" value="TKT-12345" />
```

#### 设置普通变量
```xml
<set_var key="user_name" value="张三" />
<set_var key="order_id" value="ORD-98765" />
```

#### 批量设置（不推荐）
```xml
<set_var key="_hangup_headers" value='{"X-Key1":"val1","X-Key2":"val2"}' />
```

### 4. BYE Headers 渲染

挂断时，`hangup_headers` 模板可以访问所有变量：

```yaml
---
sip:
  extract_headers:
    - "X-Customer-ID"
    - "X-Session-Type"
  hangup_headers:
    # 访问 SIP Headers
    X-Customer: "{{ sip[\"X-Customer-ID\"] }}"
    X-Type: "{{ sip[\"X-Session-Type\"] }}"
    
    # 访问普通变量
    X-Call-Result: "{{ call_result }}"
    X-Agent-Name: "{{ agent_name }}"
    X-Duration: "{{ call_duration }}"
---
```

在对话中设置这些变量：
```markdown
<set_var key="call_result" value="successful" />
<set_var key="agent_name" value="Alice" />
<set_var key="call_duration" value="180" />
```

## 动态场景 Prompt 渲染（v0.3.38+）

从 v0.3.38 开始，通过 `<set_var>` 设置的变量可以在场景切换时动态渲染到目标场景的 Prompt 中。

### 工作原理

1. 解析 Playbook 时，原始模板保存在 `Scene.raw_prompt` 中
2. 每次 `<goto>` 切换场景时，使用当前 `extras` 重新渲染
3. 内置变量、SIP Headers、set_var 变量均可用
4. 渲染失败时自动回退到已有 Prompt

### 示例

```markdown
# Scene: collect
请收集用户意向。收集后输出 <set_var key="intent" value="..." /> 然后 <goto scene="confirm" />

# Scene: confirm
会话ID：{{ session_id }}
用户意向：{{ intent }}
请确认以上信息。
```

切换到 `confirm` 场景时，`{{ intent }}` 和 `{{ session_id }}` 会被动态替换为实际值。

详细说明参见 [Playbook Advanced Features](./playbook_advanced_features.md#动态场景-prompt-渲染)

## 实现细节

### 变量存储
所有变量（SIP Headers 和普通变量）都存储在 `ActiveCall.call_state.extras: HashMap<String, serde_json::Value>` 中。

### SIP Headers 识别

系统在源头（提取 headers 时）就标记哪些是 SIP Headers：

1. **配置驱动**：在 Playbook 中通过 `sip.extract_headers` 明确指定要提取的 SIP Headers
2. **自动标记**：提取后，系统自动在 `extras` 中存储一个 `_sip_header_keys` 列表
3. **为什么需要 `_sip_header_keys`**：
   - 问题根源：SIP Header 名称包含连字符（如 `X-Customer-ID`），Jinja2 会将 `{{ X-Customer-ID }}` 解析为 `X - Customer - ID`（减法运算）
   - 解决方案：将 SIP Headers 放入 `sip` 字典，使用 `{{ sip["X-Customer-ID"] }}` 访问
   - 实现关键：需要在渲染时知道哪些变量是 SIP Headers，因此在提取时存储 `_sip_header_keys` 列表
   - 运行时缓存：`_sip_header_keys` 是配置的运行时缓存，连接"配置"和"渲染"两个阶段

```rust
// 在 handler.rs 中过滤并标记 SIP headers
if let Some(allowed_headers) = &sip_config.extract_headers {
    let mut state = active_call.call_state.write().await;
    if let Some(extras) = &mut state.extras {
        filter_headers(extras, allowed_headers);
        // 存储 SIP header keys 列表
        let header_keys: Vec<String> = extras.keys()
            .filter(|k| !k.starts_with('_'))
            .cloned()
            .collect();
        extras.insert("_sip_header_keys", serde_json::to_value(&header_keys)?);
    }
}
```

### 模板渲染

#### Playbook 解析时
```rust
// 从存储的 keys 列表构建 sip 字典
let sip_header_keys: Vec<String> = vars
    .get("_sip_header_keys")
    .and_then(|v| serde_json::from_value(v.clone()).ok())
    .unwrap_or_default();  // 没有配置 extract_headers 时为空

let mut sip_headers = HashMap::new();
for key in &sip_header_keys {
    if let Some(value) = vars.get(key) {
        sip_headers.insert(key.clone(), value.clone());
    }
}
context.insert("sip", serde_json::to_value(&sip_headers)?);
```

#### BYE Headers 渲染时
```rust
// 获取存储的 SIP header keys
let sip_header_keys: Vec<String> = state.extras
    .as_ref()
    .and_then(|e| e.get("_sip_header_keys"))
    .and_then(|v| serde_json::from_value(v.clone()).ok())
    .unwrap_or_default();  // 没有配置时为空

// sip 字典和所有变量都可访问
for (k, v) in extras {
    if k.starts_with('_') { continue; }  // 跳过内部 keys
    context.insert(k.clone(), v.clone());
    if sip_header_keys.contains(k) {
        sip_headers.insert(k.clone(), v.clone());
    }
}
context.insert("sip", serde_json::to_value(&sip_headers)?);
```

## 测试覆盖

### Playbook 模块测试
- `test_sip_dict_access_with_hyphens` - 测试 SIP Headers 通过 sip 字典访问
- `test_sip_dict_only_contains_sip_headers` - 验证 sip 字典只包含 SIP Headers
- `test_sip_dict_mixed_access` - 测试混合访问 SIP Headers 和普通变量
- `test_sip_dict_case_insensitive` - 测试大小写敏感性
- `test_sip_dict_empty_context` - 边界条件测试

### Handler 模块测试
- `test_set_var_with_sip_headers` - 测试批量设置 SIP Headers
- `test_set_var_individual_sip_header` - 测试单个 SIP Header 设置
- `test_bye_headers_with_all_variables` - 测试 BYE Headers 访问所有变量

## 使用场景

### 场景 1：呼入客户服务
```yaml
---
sip:
  extract_headers:
    - "X-Customer-ID"
    - "X-Customer-Level"
  hangup_headers:
    X-Call-Result: "{{ result }}"
    X-Satisfaction: "{{ satisfaction }}"
llm:
  greeting: "{{ sip[\"X-Customer-Level\"] }} 客户您好！"
---
# Scene: main
您的客户编号是 {{ sip["X-Customer-ID"] }}
```

### 场景 2：外呼营销
```yaml
---
sip:
  hangup_headers:
    X-Contact-Result: "{{ contact_result }}"
    X-Interest-Level: "{{ interest }}"
    X-Follow-Up: "{{ need_followup }}"
---
# Scene: main
<set_var key="contact_result" value="answered" />
客户对产品表示感兴趣 <set_var key="interest" value="high" />
需要后续跟进 <set_var key="need_followup" value="yes" />
```

### 场景 3：工单系统集成
```yaml
---
sip:
  extract_headers:
    - "X-Ticket-ID"
    - "X-Priority"
  hangup_headers:
    X-Resolution: "{{ resolution }}"
    X-Handled-By: "{{ agent }}"
---
# Scene: main
工单 {{ sip["X-Ticket-ID"] }} 优先级：{{ sip["X-Priority"] }}
问题已解决 <set_var key="resolution" value="solved" />
<set_var key="agent" value="AI-Agent-01" />
```

## 最佳实践

1. **命名约定**
   - SIP Headers：推荐使用 `X-` 前缀（RFC 规范），如 `X-Customer-ID`
   - 普通变量：使用下划线分隔，如 `customer_name`、`order_status`

2. **配置原则**
   - 在 `sip.extract_headers` 中明确列出需要提取的 headers
   - 不要依赖命名约定（X- 前缀），让配置驱动一切

3. **访问模式**
   - 始终通过 `sip` 字典访问 SIP Headers：`{{ sip["X-Header"] }}`
   - 直接访问普通变量：`{{ variable_name }}`
   - **重要**：包含连字符的变量必须使用字典访问，否则会被解析为减法

3. **BYE Headers**
   - 在 `hangup_headers` 中可以访问所有类型的变量
   - 优先使用普通变量存储业务逻辑数据
   - 使用 SIP Headers 传递协议层面的信息

4. **性能考虑**
   - `set_var` 是轻量级操作，可以频繁使用
   - 避免使用 `_hangup_headers` 批量设置，优先使用单个 `set_var`

5. **为什么之前没有发现问题**
   - 在引入 `sip` 字典之前，代码中从未直接在模板中使用过 `{{ X-Header-Name }}` 这样的语法
   - 实际使用时要么通过 API 参数传入（不经过模板），要么使用不含连字符的变量名
   - 当用户尝试在模板中使用 `{{ X-Header }}` 时才发现 Jinja2 会将其解析为减法运算

## 迁移指南

### 问题的由来

在 v0.3.38 之前，系统中虽然存在 SIP Headers（如 `X-Customer-ID`），但：
1. **从未在模板中直接引用**过这些包含连字符的变量
2. Headers 主要用于内部传递，不参与模板渲染
3. 当用户首次尝试在 Playbook 中使用 `{{ X-Header }}` 时，发现 Jinja2 将其解析为 `X - Header`（减法）

### 从旧版本迁移

**问题场景**：
```yaml
llm:
  greeting: "Hello {{ X-Customer-Name }}"  # ❌ 错误：被解析为 X - Customer - Name
```

**错误信息**：
```
Jinja2 template error: undefined variable 'X'
或
Jinja2 template error: unsupported operand type(s) for -: 'str' and 'str'
```

**解决方案**：
```yaml
sip:
  extract_headers:
    - "X-Customer-Name"  # 必须先配置提取
llm:
  greeting: "Hello {{ sip[\"X-Customer-Name\"] }}"  # ✅ 正确：字典访问
```

### 向后兼容性

- 普通变量（不含连字符）的访问方式保持不变
- 只有包含连字符的 SIP Headers 需要更新访问方式
- 所有测试已更新以覆盖新的访问模式

## 参考文档

- [Playbook Advanced Features (中文)](playbook_advanced_features.md)
- [Playbook Advanced Features (English)](playbook_advanced_features.en.md)
- [Template Syntax Comparison](template_syntax_comparison.md)
