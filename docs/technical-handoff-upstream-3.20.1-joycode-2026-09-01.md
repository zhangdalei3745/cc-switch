# CC Switch 上游 3.20.1 与 Joycode 合并技术交接

- 日期：2026-09-01
- 工作分支：`feature/upstream-main-latest`
- 主仓库基线：`upstream/main`，`92a9b4a91d75`
- Fork 合并源：`origin/main`，`c866ea78e488`
- 合并提交：`4580b793b72edd6acd3acaf2a530480040d05bc5`
- 应用版本：`3.20.1`

## 1. 合并目标

以主仓库最新的 3.20.1 为运行基线，合入 fork 中的 Joycode 供应商能力，同时避免覆盖上游新增的 Codex Official、xAI Responses、供应商切换和会话统计功能。

```text
upstream/main 92a9b4a9 (3.20.1)
              \
               +-- 4580b793 feature/upstream-main-latest
              /
origin/main   c866ea78 (Joycode fork)
```

## 2. 关键合并决策

### 2.1 版本与构建

以下版本统一采用上游 3.20.1：

- `package.json`
- `src-tauri/Cargo.toml`
- `src-tauri/Cargo.lock`
- `src-tauri/tauri.conf.json`

CI 和发布工作流采用上游版本，保留上游 Windows/WSL 测试修复及 updater artifact 生成逻辑。

### 2.2 Joycode 代理链路

保留 fork 的下列能力：

- 内网、外网 Joycode 网关选择和 URL 校验。
- 手工凭据与 IDE 凭据导入。
- 运行时 Token 获取、缓存、失效和一次重试。
- 动态模型目录和每模型协议选择。
- Responses、Chat Completions、Anthropic 三类协议路由。
- Responses 会话链续接及失效后回退完整历史。
- 流式响应标准化和认证错误信封识别。
- Claude、Claude Desktop、Codex、Gemini、OpenCode、OpenClaw、Hermes、Pi、GrokBuild 等客户端的 Joycode 预设和表单入口。

`providerNeedsRouting` 在 official-provider 快速路径之前判断 Joycode，确保 Joycode 即使被标记为 official 类别，也仍通过本地代理完成专用认证和协议适配。

### 2.3 上游 Codex/xAI 逻辑

保留上游 3.20.1 的：

- Codex Official 身份和凭据判断。
- xAI OAuth 原生 Responses 请求处理。
- Grok Responses 输入项清理和 agent message 重写。
- Codex 历史配置迁移、保留登录及供应商切换事务。
- 会话用量增量扫描及相关 UI。

Fork 中旧的重复 `chatgpt-account-id` 注入没有保留，因为上游已在统一的 ordered headers 阶段覆盖该请求头；继续保留会造成重复或顺序不确定。

### 2.4 Codex 文件审查卡片限制

Joycode 的 Codex Responses 模型当前使用 `NativeResponses` 工具配置：

```text
shell_type = shell_command
apply_patch_tool_type = 未声明
```

这是为了避免部分原生 Responses 网关拒绝 `type=custom` 的 freeform `apply_patch` 工具。副作用是 Codex 通常通过 shell 命令修改文件，而不是产生结构化 `FileChange` item，因此会话结尾可能不显示“已编辑 N 个文件”审查卡片。

如需恢复该卡片，不能只在模型目录中增加 `apply_patch_tool_type=freeform`；应为 Joycode 增加受控的 custom-tool 到 function-tool 双向转换，并验证所有 Responses 模型均能正确调用、流式返回和续接工具结果。

## 3. 冲突解决范围

本次人工解决过的主要冲突：

- `.github/workflows/ci.yml`
- `package.json`
- `src-tauri/Cargo.lock`
- `src-tauri/Cargo.toml`
- `src-tauri/src/proxy/forwarder.rs`
- `src-tauri/tauri.conf.json`
- `src/components/providers/forms/ProviderForm.tsx`
- `src/utils/providerCapabilities.test.ts`
- `src/utils/providerCapabilities.ts`

合并后的 Joycode 核心实现文件与 `origin/main` 保持一致：

- `src-tauri/src/proxy/providers/joycode.rs`
- `src/components/providers/forms/JoycodeConnectionFields.tsx`

## 4. 测试与质量检查

已通过：

- `cargo fmt --check`
- `cargo check --offline`
- `cargo clippy --offline -- -D warnings`
- Rust 全量库测试：2794 passed，0 failed，6 ignored
- Joycode 后端专项：27 passed，1 个 live quota 测试 ignored
- TypeScript `tsc --noEmit`
- Prettier
- Joycode/相关前端专项：5 files，50 tests passed
- 前端全量：137 files，1081 tests passed
- `git diff --check`
- 冲突标记扫描
- 敏感文件名扫描

### 4.1 测试端口修复

测试 `update_current_claude_desktop_provider_syncs_profile_when_proxy_takeover_is_active` 原先固定使用 `127.0.0.1:15721`。本机运行中的 CC Switch.app 正占用该端口，导致测试失败。

测试现改为：

- 将测试数据库的代理端口设置为 `0`，由系统分配临时端口。
- 使用启动结果中的实际端口断言 Claude Desktop profile。
- 测试结束后显式停止代理服务。

该修改只影响测试，不改变生产代理端口和 Joycode 行为。

## 5. 二次审查结论

按 `origin/main...feature/upstream-main-latest` 审查：

- 未发现残留 Git 冲突标记。
- 未发现尾随空格或 diff 格式错误。
- 未发现 `.env`、私钥、Token 或凭据文件被提交。
- Joycode 专用认证、模型、路由和 UI 文件仍在。
- 上游 3.20.1 的版本号及新功能已进入合并结果。
- 暂未发现阻断合并的问题。

## 6. 未包含的本地内容

以下 stash 未应用，也未包含在本次合并提交中：

```text
stash@{0}: WIP on main: 347ccc84 feat(joycode): add validated manual and IDE auth import
```

后续处理该 stash 时，应在当前 3.20.1 分支上单独恢复并检查与以下文件的重叠：

- `src-tauri/src/proxy/handlers.rs`
- `src-tauri/src/proxy/server.rs`
- `src-tauri/src/services/proxy.rs`

不要直接在主分支无检查应用。

## 7. 发布与升级风险

当前应用内 updater 仍指向主仓库：

- `https://dl.ccswitch.io/latest.json`
- `https://github.com/farion1231/cc-switch/releases/latest/download/latest.json`

因此 fork 构建可能在主仓库发布更高版本后自动升级为不包含 Joycode 的官方构建。长期分发前建议：

1. 临时关闭 fork 的自动安装；或
2. 建立 fork 独立的 updater endpoint 和签名密钥；
3. 每次生成 fork 更新包前先同步上游并执行 Joycode 回归测试。

## 8. 后续建议

1. 创建 Pull Request，目标分支为 `origin/main`。
2. 在真实 Joycode 内网及外网环境各完成一次登录、模型获取和连续工具调用验证。
3. 验证 Claude、Codex、Claude Desktop 三个客户端切换回非 Joycode 供应商后配置可完全恢复。
4. 将 Codex `apply_patch` 审查卡片兼容作为独立 feature 处理，避免扩大本次上游合并范围。
5. 单独评审并处理保留的 stash。
