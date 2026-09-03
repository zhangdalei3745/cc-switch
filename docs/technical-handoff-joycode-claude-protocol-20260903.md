# JoyCode Claude 协议兼容修复技术交接

- 日期：2026-09-03
- 工作分支：`fix/joycode-claude-error`
- 基线：`origin/main`，`dd2224b2db8b0e890ffbaebe4ee8c078ef49b036`
- 相关修复提交：`4b365b4c`（思考与业务错误）、`3b534e97` / `b034d5c6`（Codex 工具字段）、`ec47ccba`（根 schema 组合关键字）；本次补充 Responses Lite 动态工具兼容
- 影响范围：仅 JoyCode 的 Anthropic 请求/响应链路

## 1. 问题背景

JoyCode 目录中的新 Claude 模型使用 Anthropic wire API，但在 Claude Code、Claude Desktop 和 Codex 的调用过程中存在两类兼容风险：

1. Claude Opus 4.6/4.7/4.8、Claude Sonnet 4.6 的扩展思考参数仍可能使用旧的 `thinking.enabled + budget_tokens` 形式，未转换为 JoyCode 当前模型接受的自适应思考参数。
2. JoyCode 可能以 HTTP 200 返回包含业务错误的 JSON 或 SSE 包装；原链路可能继续按正常流处理，导致错误信息延迟、丢失或表现为流式解析异常。
3. Codex Responses 函数工具携带的顶层 `strict` 字段会被通用 Responses→Anthropic 转换保留，但 JoyCode 当前 Anthropic 适配器不接受该可选字段，并返回 `tools.N.custom.strict: Extra inputs are not permitted`。
4. Codex 内置工具可能在 `input_schema` 根节点使用 `oneOf`、`anyOf` 或 `allOf`。JoyCode Claude 的 Anthropic 适配器会返回 HTTP 400：`input_schema does not support oneOf, allOf, or anyOf at the top level`。
5. Codex Responses Lite 会把动态工具放在 `input[].type=additional_tools` 中。通用 Responses→Anthropic 转换只读取顶层 `tools`，导致动态工具在转换时丢失。

本次修复严格限定在 JoyCode Anthropic 路径，没有修改其他供应商，也没有改变 JoyCode 非 Claude 模型的协议行为。

## 2. 修复内容

### 2.1 自适应思考转换

对已确认需要自适应思考的新 Claude 模型，将旧式思考配置转换为：

```json
{
  "thinking": { "type": "adaptive" },
  "output_config": { "effort": "high" }
}
```

转换会保留调用方显式给出的有效 effort，并根据原 `budget_tokens` 和模型默认值选择兼容等级。旧模型及非 JoyCode Anthropic 请求不进入该分支。

### 2.2 HTTP 200 业务错误识别

新增 JoyCode Anthropic 响应校验：

- 检查非流式 JSON 中的业务错误包。
- 检查 SSE 首个有效事件中的错误。
- 解包 JoyCode 可能返回的双层 SSE 数据。
- 当错误同时包含外层描述和 `error.cause` 时，优先暴露更具体的 cause。
- 保留正常 Anthropic SSE 事件及流式响应，不进行全量缓冲。

### 2.3 Codex 工具字段兼容

仅在 Codex 等 Responses 客户端经过 `Responses → Anthropic` 转换、且上游模型为 JoyCode Anthropic 时：

- 移除每个工具对象顶层的 `strict` 字段。
- 将 `input_schema` 根节点的 `oneOf` / `anyOf` 展开为普通对象：合并分支属性，只保留所有分支共同要求的必填字段。
- 将根节点的 `allOf` 展开为普通对象：合并分支属性和全部必填字段。
- 同一属性在不同分支定义不同时，保留为该属性内部的 `anyOf`；已实测 JoyCode Claude 接受嵌套组合结构，拒绝范围仅为 `input_schema` 根节点。

该转换不改变工具调用参数的外层结构。Claude Code 和 Claude Desktop 的原生 Anthropic 请求不会进入清理分支，工具定义保持原样；JoyCode Responses、Chat 以及其他供应商也继续保留原有行为。

### 2.4 Codex Responses Lite 动态工具

仅在 `Codex Responses → JoyCode Anthropic` 路由进入通用协议转换前：

- 识别合法的 `input[].type=additional_tools`、`role=developer` 载体。
- 把载体中的工具合并到顶层 `tools`，按工具类型和名称去重。
- 删除已经消费的载体项，避免它被误当成普通消息。
- 后续继续复用现有 namespace 展平、`strict` 删除和根 schema 组合关键字转换。

当前 Codex 的 `agent_message` 使用 `content[].type=input_text`，现有转换会将其保留为 Anthropic `user` 文本块；定向测试确认没有丢消息，因此本次没有增加多余改写。

## 3. 三客户端协议链路

```text
                         CC Switch 本地协议网关
                                   │
        ┌──────────────────────────┼──────────────────────────┐
        │                          │                          │
 Claude Code                Claude Desktop                 Codex
 /v1/messages        /claude-desktop/v1/messages       /v1/responses
        │                          │                          │
        │ Anthropic                │ Anthropic                │ Responses
        ▼                          ▼                          ▼
 JoyCode 模型目录          安全别名映射真实模型        Responses → Anthropic
        │                          │                          │
        └──────────── JoyCode /api/saas/anthropic/v1/messages ┘
                                   │
                    thinking / SSE / 业务错误兼容
                                   │
                 ┌─────────────────┴──────────────────┐
                 │                                    │
          Anthropic SSE 返回                 Anthropic → Responses
       Claude Code / Desktop                         Codex
```

| 客户端 | CC Switch 入站协议 | JoyCode 上游协议 | CC Switch 返回协议 | 状态 |
| --- | --- | --- | --- | --- |
| Claude Code | Anthropic Messages | Anthropic | Anthropic SSE | 已适配 |
| Claude Desktop | Anthropic Messages | Anthropic | Anthropic SSE | 已适配 |
| Codex | OpenAI Responses | Anthropic | OpenAI Responses | 已适配 |

Codex 默认模型选择偏好 Responses，但显式 Claude 模型会按 JoyCode 实时目录中的 `wire_api=Anthropic` 进行精确解析，然后执行双向协议转换。

## 4. 变更文件

```text
src-tauri/src/proxy/forwarder.rs
src-tauri/src/proxy/providers/joycode.rs
```

其中：

- `forwarder.rs`：在 JoyCode Codex→Anthropic 转换前提升 Responses Lite 动态工具，并接入 Anthropic 响应归一化。
- `joycode.rs`：实现模型匹配、自适应思考、业务错误提取、工具 schema 清理和 `additional_tools` 提升。

## 5. 验证结果

### 5.1 2026-09-03 JoyCode 外网 live 字段矩阵

使用实时模型目录和当前登录态逐字段探测，响应体按 `Content-Encoding` 解压后检查业务错误；未记录或输出凭据。

**Anthropic / Claude-Opus-4.8-hq**

| 字段/能力 | 结果 | 处理策略 |
| --- | --- | --- |
| `metadata`、`stop_sequences` | 通过 | 保留 |
| 顶层及工具级 `cache_control` | 通过 | 保留 |
| `eager_input_streaming` | 通过 | 保留 |
| `defer_loading` | 字段被识别；仅有 deferred 工具时按规则拒绝 | 不按“不兼容字段”删除 |
| `thinking.type=enabled`（新 Claude 模型） | 不支持旧式 budget 配置 | 仅 JoyCode 新模型转换为 `adaptive + output_config.effort` |
| `output_config.effort` | 通过 | 保留，供 adaptive thinking 使用 |
| `context_management` | 当前 JoyCode 适配器不接受 | 仅 JoyCode Anthropic 删除可选服务端压缩提示，完整消息历史仍保留 |
| `service_tier` | 模型不支持 | 当前 Codex→Anthropic 本就不透传；原生请求不静默降级 |
| `top_k` | 当前模型已废弃 | 当前 Codex→Anthropic 本就不透传；原生请求不静默改采样语义 |
| `output_config.format` | `Extra inputs are not permitted` | 不静默删除结构化输出约束，记录为上游能力缺口 |
| `container`、`inference_geo` | `Extra inputs are not permitted` | 不静默删除状态/地域语义 |
| 工具 `input_examples`、`allowed_callers` | `Extra inputs are not permitted` | 不影响当前 Codex 工具；原生高级工具能力暂不宣称兼容 |
| 工具 `strict` | `Extra inputs are not permitted` | 仅 Codex→Anthropic 转换后删除；原生 Claude 请求不改 |
| 工具根 schema `oneOf` / `anyOf` / `allOf` | 不支持 | 仅 Codex→Anthropic 转换后展开 |

**Responses / GPT-5.6 Sol**

已通过：`service_tier`、`safety_identifier`、`prompt_cache_retention`、`prompt_cache_options`、`prompt_cache_key`、`truncation`、`max_tool_calls`、`metadata`、`user`、`background=false`、`include=reasoning.encrypted_content`、`text.format`、`text.verbosity`、`reasoning.summary`、`reasoning.context=all_turns`、`client_metadata`、custom/namespace/tool-search/deferred tools、合法 `additional_tools`、`agent_message`，以及 `strict=false` 的复杂 JSON Schema。

另外使用当前 Codex `0.152.0` 的实际 Responses Lite 结构做了组合探测：带 `id` 的 `additional_tools`、developer message、`tool_choice=auto`、`parallel_tool_calls=false`、reasoning、include、prompt cache、text verbosity 和 `client_metadata` 同时存在时可正常完成。

仍发现两个有边界的字段：

- `top_p` 在 `GPT-5.6 Sol` 上返回模型级 `Unsupported parameter`。当前 Codex 请求结构不发送 `top_p`，因此不做全局删除，避免影响其他 Responses 模型的采样语义。
- `access_programs` 返回 `Unknown parameter`。当前 Codex 仅对内建 OpenAI provider 的 ChatGPT 鉴权请求发送该字段，API key 和 JoyCode 这类 custom provider 会省略，因此当前链路不需要改写；若未来 Codex 改为向 custom provider 发送，必须先决定是否允许丢弃其安全策略语义，不能静默删除。

`stream_options.reasoning_summary_delivery` 目前也只在 Codex 内建 OpenAI provider 上启用，JoyCode custom provider 不会收到该字段，因此本轮不把它当作 JoyCode 兼容缺陷。

### 5.2 本地检查

已通过以下检查：

- JoyCode 定向单元测试：38 passed，1 ignored。
- 新增顶层 `oneOf` / `anyOf` / `allOf` 展开测试，并验证原生 Anthropic 请求仍保留原 schema。
- 使用当前 JoyCode Claude Opus 路由实测嵌套属性 `anyOf`，HTTP 200，确认不会触发同类校验错误。
- JoyCode Anthropic stream-start 定向测试：2 passed。
- Claude Desktop proxy 定向测试：11 passed。
- Codex adaptive thinking 与 signed thinking 工具循环测试：2 passed。
- `src/utils/providerCapabilities.test.ts`：24 passed。
- `cargo fmt --check`。
- `cargo check --locked`。
- `cargo clippy --lib --locked -- -D warnings`。

JoyCode live contract 测试默认标记为 ignored，正常测试套件不会自动执行；本轮已使用有效本地登录态显式执行基础 contract 与字段矩阵验证。

## 6. 风险与边界

- 自适应思考和响应错误处理只在 `providerType=joycode` 且实际 wire API 为 Anthropic 时生效。`additional_tools` 提升、工具 `strict` 删除和根 schema 组合关键字清理进一步限定在 Responses→Anthropic 转换分支，Claude Code 和 Claude Desktop 原生 Anthropic 请求不受这些清理影响。
- 当前验证覆盖源码链路、模型目录、当前配置和定向测试。
- 已安装的 `/Applications/CC Switch.app` 仍可能是旧二进制；严格运行时 E2E 需要构建、安装并重启当前分支版本后，分别从 Claude Code、Claude Desktop 和 Codex 发起真实请求。
- 如果现场返回 HTTP 504，通常表示请求已到达 JoyCode/nginx 后发生上游超时；应结合 CC Switch 请求日志和 JoyCode 返回体区分协议错误与上游服务超时。

## 7. 回滚方式

若上线后需要回滚，可按反向顺序回滚本分支上的 JoyCode 兼容提交，不需要调整数据库或迁移配置。回滚后对应的思考参数、HTTP 200 业务错误、Codex 工具 schema 或动态工具兼容能力会分别恢复到提交前行为。
