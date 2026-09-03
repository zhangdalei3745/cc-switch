# JoyCode Claude 协议兼容修复技术交接

- 日期：2026-09-03
- 工作分支：`fix/joycode-claude-error`
- 基线：`origin/main`，`e9d33d14e19bf4cb8996b9f2655fea77452f73d5`
- 核心修复提交：`4b365b4c fix(joycode)：兼容 Claude 自适应思考并识别流式业务错误`
- 影响范围：仅 JoyCode 的 Anthropic 请求/响应链路

## 1. 问题背景

JoyCode 目录中的新 Claude 模型使用 Anthropic wire API，但在 Claude Code、Claude Desktop 和 Codex 的调用过程中存在两类兼容风险：

1. Claude Opus 4.6/4.7/4.8、Claude Sonnet 4.6 的扩展思考参数仍可能使用旧的 `thinking.enabled + budget_tokens` 形式，未转换为 JoyCode 当前模型接受的自适应思考参数。
2. JoyCode 可能以 HTTP 200 返回包含业务错误的 JSON 或 SSE 包装；原链路可能继续按正常流处理，导致错误信息延迟、丢失或表现为流式解析异常。
3. Codex Responses 函数工具携带的顶层 `strict` 字段会被通用 Responses→Anthropic 转换保留，但 JoyCode 当前 Anthropic 适配器不接受该可选字段，并返回 `tools.N.custom.strict: Extra inputs are not permitted`。

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

仅在 Codex 等 Responses 客户端经过 `Responses → Anthropic` 转换、且上游模型为 JoyCode Anthropic 时，移除每个工具对象顶层的 `strict` 字段。JSON Schema 中的 `input_schema`、`required` 和 `additionalProperties` 保持不变，因此不会损失工具参数约束。Claude Code 和 Claude Desktop 的原生 Anthropic 请求不会进入该清理分支，工具定义保持原样；JoyCode Responses、Chat 以及其他供应商也继续保留原有行为。

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

- `forwarder.rs`：接入 JoyCode Anthropic 流式响应归一化与业务错误校验。
- `joycode.rs`：实现模型匹配、自适应思考转换、双层 SSE 解包及业务错误提取。

## 5. 验证结果

已通过以下检查：

- JoyCode 单元测试：34 passed，1 ignored。
- JoyCode Anthropic stream-start 定向测试：2 passed。
- Claude Desktop proxy 定向测试：11 passed。
- Codex adaptive thinking 与 signed thinking 工具循环测试：2 passed。
- `src/utils/providerCapabilities.test.ts`：24 passed。
- `cargo fmt --check`。
- `cargo check --locked`。
- `cargo clippy --lib --locked -- -D warnings`。

被忽略的 JoyCode live contract 测试需要有效本地登录并会消耗真实额度，因此本次没有自动执行。

## 6. 风险与边界

- 自适应思考和响应错误处理只在 `providerType=joycode` 且实际 wire API 为 Anthropic 时生效。工具 `strict` 清理进一步限定在 Responses→Anthropic 转换分支，Claude Code 和 Claude Desktop 原生 Anthropic 请求不受该清理影响。
- 当前验证覆盖源码链路、模型目录、当前配置和定向测试。
- 已安装的 `/Applications/CC Switch.app` 仍可能是旧二进制；严格运行时 E2E 需要构建、安装并重启当前分支版本后，分别从 Claude Code、Claude Desktop 和 Codex 发起真实请求。
- 如果现场返回 HTTP 504，通常表示请求已到达 JoyCode/nginx 后发生上游超时；应结合 CC Switch 请求日志和 JoyCode 返回体区分协议错误与上游服务超时。

## 7. 回滚方式

若上线后需要回滚，仅需回滚核心修复提交及本交接文档提交，不需要调整数据库或迁移配置：

```bash
git revert 4b365b4c
```

若交接文档为独立提交，可按实际提交号单独回滚。回滚后新 Claude 模型将恢复旧思考参数和旧的 HTTP 200 流式错误处理行为。
