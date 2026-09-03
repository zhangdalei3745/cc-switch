# CC Switch 上游增量同步与 Joycode 保留技术交接

- 日期：2026-09-02
- 工作分支：`feature/upstream-main-20260902`
- Fork 基线：`origin/main`，`5a974b7b22b2e7892c6b9cc9914c971199c2b55d`
- 上游目标：`upstream/main`，`c58a25b2ae583710f24ce8867e48340bda619f7d`
- 合并提交：`39b2a6b647781a287f005dd08b50d41a2e115be9`
- 应用版本：`3.20.1`

## 1. 同步目标

在昨日已完成的上游 3.20.1 大合并基础上，继续同步上游新增的 7 个提交，同时确认 Joycode 定制不会被新预设、可访问性和用量统计改动破坏。

## 2. 上游新增内容

本次合入的上游提交：

- `e4b03a38`：修复 resumed Codex rollout 在文件名或 metadata ID 不匹配时被错误延迟的问题。
- `cbbf7279`：新增 AICodeWith 预设。
- `68d71cc6`：新增 9527CODE 预设。
- `4f62f676`：修复 9527CODE 图标主题适配。
- `b1250dc7`：Hermes 最新版本改为从 GitHub Releases 读取。
- `460aa8c7`：补充 Claude Fable 5.1 / Mythos 5.1 定价，并恢复 Sonnet 5 标准价。
- `c58a25b2`：为 GUI 控件补充可访问名称。

## 3. 合并与冲突处理

`origin/main...upstream/main` 的 7 个新提交与 Joycode 定制的直接重叠集中在 9 个 provider preset 文件：

- `src/config/claudeDesktopProviderPresets.ts`
- `src/config/claudeProviderPresets.ts`
- `src/config/codexProviderPresets.ts`
- `src/config/geminiProviderPresets.ts`
- `src/config/grokBuildProviderPresets.ts`
- `src/config/hermesProviderPresets.ts`
- `src/config/openclawProviderPresets.ts`
- `src/config/opencodeProviderPresets.ts`
- `src/config/piProviderPresets.ts`

`git merge-tree --write-tree` 预检没有文本冲突；实际 merge 由 `ort` 策略自动完成。合并后确认：

- Joycode 后端实现 `src-tauri/src/proxy/providers/joycode.rs` 与 fork 基线一致。
- Joycode 前端表单 `src/components/providers/forms/JoycodeConnectionFields.tsx` 与 fork 基线一致。
- `providerNeedsRouting` 仍保持在 official-provider 快速路径之前处理 Joycode。
- 版本号与锁文件保持 `3.20.1`。
- 无冲突标记、尾随空白或敏感文件名。
- 未发现 `.env`、私钥、Token 或凭据文件被提交。

## 4. 本地验证结果

在合并分支上通过：

- `cargo fmt --check`
- `cargo check --locked`
- `cargo clippy --locked --all-targets -- -D warnings`
- `cargo test --lib`
  - 2803 passed
  - 0 failed
  - 6 ignored
- TypeScript `tsc --noEmit`
- Prettier check
- Vitest full suite
  - 137 files passed
  - 1081 tests passed

测试期间曾因本机磁盘剩余空间不足导致 Rust 编译失败。已清理 `src-tauri/target` 后重新从干净状态编译并完整执行上述检查，最终全部通过。

## 5. 变更文件清单

本次增量合并相对 `origin/main` 修改 31 个文件：

```text
README.md
README_DE.md
README_JA.md
README_ZH.md
assets/partners/logos/9527-banner.png
src-tauri/src/commands/misc.rs
src-tauri/src/database/schema.rs
src-tauri/src/database/tests.rs
src-tauri/src/services/session_usage_codex.rs
src/App.tsx
src/components/common/FullScreenPanel.tsx
src/components/profiles/ProfileSwitcher.tsx
src/components/providers/forms/BasicFormFields.tsx
src/components/providers/forms/CommonConfigEditor.tsx
src/components/proxy/ProxyToggle.tsx
src/components/skills/UnifiedSkillsPanel.tsx
src/config/claudeDesktopProviderPresets.ts
src/config/claudeProviderPresets.ts
src/config/codexProviderPresets.ts
src/config/geminiProviderPresets.ts
src/config/grokBuildProviderPresets.ts
src/config/hermesProviderPresets.ts
src/config/openclawProviderPresets.ts
src/config/opencodeProviderPresets.ts
src/config/piProviderPresets.ts
src/i18n/locales/en.json
src/i18n/locales/ja.json
src/i18n/locales/zh-TW.json
src/i18n/locales/zh.json
src/icons/extracted/index.ts
src/icons/extracted/metadata.ts
```

另有本技术交接文档：

```text
docs/technical-handoff-upstream-20260902-joycode.md
```

## 6. 风险与后续建议

- 上游预设文件与 Joycode 的自动合并结果已由全量测试覆盖，但仍建议在真实环境中做一次 Joycode 登录、模型获取和 Claude/Codex 连续工具调用回归。
- `9527-banner.png` 为上游新增二进制资源，大小约 67KB，未发现敏感内容特征。
- 本次没有修改 Joycode 核心实现，未扩大上游合并的影响半径。
- 若后续继续长期维护 fork，建议建立固定同步节奏，并在每次同步后执行同一组本地检查和 CI。
