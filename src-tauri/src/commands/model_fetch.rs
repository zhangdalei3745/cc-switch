//! 模型列表获取命令
//!
//! 提供 Tauri 命令，供前端在供应商表单中获取可用模型列表。

use crate::services::model_fetch::{self, FetchedModel};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenCodeModelRef {
    pub provider_id: String,
    pub model_id: String,
}

const OPENCODE_MODELS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// 获取 OpenCode 当前运行时可用的模型。
///
/// 复用工具更新页的 CLI 定位逻辑执行 `opencode models`，因此会包含 OpenCode
/// 已加载的 OAuth 模型与 Zen 免费模型，而不是只读取 opencode.json。
#[tauri::command]
pub async fn get_opencode_models() -> Result<Vec<OpenCodeModelRef>, String> {
    tokio::task::spawn_blocking(|| {
        // Align runtime discovery with the OpenCode config directory that
        // cc-switch already uses for live read/write (settings override included).
        let config_dir = crate::opencode_config::get_opencode_dir();
        let config_dir_env = config_dir.to_string_lossy().into_owned();
        let extra_env = [
            ("OPENCODE_CONFIG_DIR", config_dir_env),
            ("OPENCODE_DISABLE_PROJECT_CONFIG", "true".to_string()),
        ];
        let output = super::misc::run_detected_tool_command_with_timeout(
            "opencode",
            &["models"],
            Some(OPENCODE_MODELS_TIMEOUT),
            &extra_env,
            &config_dir,
        )?;
        if !output.status.success() {
            let stderr = super::misc::decode_command_output(&output.stderr);
            let stdout = super::misc::decode_command_output(&output.stdout);
            let detail = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            return Err(if detail.is_empty() {
                "Failed to load OpenCode models".to_string()
            } else {
                format!("Failed to load OpenCode models: {detail}")
            });
        }

        Ok(parse_opencode_models(&super::misc::decode_command_output(
            &output.stdout,
        )))
    })
    .await
    .map_err(|e| format!("OpenCode model discovery task failed: {e}"))?
}

fn parse_opencode_models(output: &str) -> Vec<OpenCodeModelRef> {
    output
        .lines()
        .filter_map(|line| {
            let (provider_id, model_id) = line.trim().split_once('/')?;
            if provider_id.is_empty()
                || model_id.is_empty()
                || !provider_id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                || model_id
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control())
            {
                return None;
            }
            Some((provider_id.to_string(), model_id.to_string()))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|(provider_id, model_id)| OpenCodeModelRef {
            provider_id,
            model_id,
        })
        .collect()
}

/// 获取供应商的可用模型列表
///
/// 使用 OpenAI 兼容的 GET /v1/models 端点。优先使用 `models_url` 精确覆写；
/// 否则对 baseURL 生成候选列表（含「剥离 Anthropic 兼容子路径」兜底），按序尝试。
#[tauri::command(rename_all = "camelCase")]
pub async fn fetch_models_for_config(
    base_url: String,
    api_key: String,
    is_full_url: Option<bool>,
    models_url: Option<String>,
    custom_user_agent: Option<String>,
    api_format: Option<String>,
    request_headers: Option<BTreeMap<String, String>>,
) -> Result<Vec<FetchedModel>, String> {
    // JoyCode 的模型目录是带签名的 POST 协议，不是 OpenAI GET /v1/models。
    // 对预设内网地址做兼容分流，使所有现有应用表单的“获取模型”按钮无需
    // 各自复制 JoyCode 协议实现。外网地址仍走显式 fetch_joycode_models，
    // 因为参考客户端没有给出可安全识别的官方外网 host。
    if crate::proxy::providers::joycode::is_internal_base_url(&base_url) {
        let provider = crate::provider::Provider {
            id: "joycode-model-fetch".to_string(),
            name: "JD Joycode".to_string(),
            settings_config: serde_json::json!({}),
            website_url: None,
            category: Some("cn_official".to_string()),
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(crate::provider::ProviderMeta {
                provider_type: Some("joycode".to_string()),
                joycode_network: Some("internal".to_string()),
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        };
        return crate::proxy::providers::joycode::fetch_models(&provider, &api_key)
            .await
            .map(|models| {
                models
                    .into_iter()
                    .map(|model| FetchedModel {
                        id: model.id,
                        owned_by: Some(model.owned_by),
                    })
                    .collect()
            });
    }

    // 与转发 / 检测路径共用 parse_custom_user_agent：非法 UA 静默忽略（不阻断取模型）。
    let user_agent = crate::provider::parse_custom_user_agent(custom_user_agent.as_deref())
        .ok()
        .flatten();
    model_fetch::fetch_models(
        &base_url,
        &api_key,
        is_full_url.unwrap_or(false),
        models_url.as_deref(),
        user_agent,
        api_format.as_deref(),
        request_headers.as_ref(),
    )
    .await
}

/// Fetch the JoyCode model catalog using its dedicated POST protocol.
///
/// The external gateway host is intentionally optional: the reference client
/// does not define a public host, therefore selecting `external` without an
/// official address returns a configuration error instead of guessing.
#[tauri::command(rename_all = "camelCase")]
pub async fn fetch_joycode_models(
    provider_id: String,
    network: String,
    external_base_url: Option<String>,
    pt_key: String,
    login_type: Option<String>,
    tenant: Option<String>,
) -> Result<Vec<crate::proxy::providers::joycode::JoycodeModel>, String> {
    let provider = crate::provider::Provider {
        id: if provider_id.trim().is_empty() {
            "joycode-preview".to_string()
        } else {
            provider_id
        },
        name: "JD Joycode".to_string(),
        settings_config: serde_json::json!({}),
        website_url: Some(crate::proxy::providers::joycode::JOYCODE_WEBSITE_URL.to_string()),
        category: Some("cn_official".to_string()),
        created_at: None,
        sort_index: None,
        notes: None,
        meta: Some(crate::provider::ProviderMeta {
            provider_type: Some("joycode".to_string()),
            joycode_network: Some(network),
            joycode_external_base_url: external_base_url,
            joycode_login_type: login_type,
            joycode_tenant: tenant,
            ..Default::default()
        }),
        icon: Some("joycode".to_string()),
        icon_color: None,
        in_failover_queue: false,
    };
    crate::proxy::providers::joycode::fetch_models(&provider, &pt_key).await
}

fn joycode_preview_provider(
    network: String,
    external_base_url: Option<String>,
    login_type: Option<String>,
    tenant: Option<String>,
) -> crate::provider::Provider {
    crate::provider::Provider {
        id: "joycode-auth-preview".to_string(),
        name: "JD Joycode".to_string(),
        settings_config: serde_json::json!({}),
        website_url: Some(crate::proxy::providers::joycode::JOYCODE_WEBSITE_URL.to_string()),
        category: Some("cn_official".to_string()),
        created_at: None,
        sort_index: None,
        notes: None,
        meta: Some(crate::provider::ProviderMeta {
            provider_type: Some("joycode".to_string()),
            joycode_network: Some(network),
            joycode_external_base_url: external_base_url,
            joycode_login_type: login_type,
            joycode_tenant: tenant,
            ..Default::default()
        }),
        icon: Some("joycode".to_string()),
        icon_color: None,
        in_failover_queue: false,
    }
}

#[tauri::command(rename_all = "camelCase")]
pub async fn validate_joycode_credential(
    network: String,
    external_base_url: Option<String>,
    pt_key: String,
    login_type: Option<String>,
    tenant: Option<String>,
) -> Result<crate::proxy::providers::joycode::JoycodeCredential, String> {
    let provider = joycode_preview_provider(
        network,
        external_base_url,
        login_type.clone(),
        tenant.clone(),
    );
    let credential = crate::proxy::providers::joycode::JoycodeCredential {
        pt_key,
        login_type,
        tenant,
        master_base_url: None,
        color_base_url: None,
    };
    crate::proxy::providers::joycode::validate_credential(&provider, &credential).await
}

#[tauri::command]
pub async fn import_joycode_credential(
) -> Result<Option<crate::proxy::providers::joycode::JoycodeCredential>, String> {
    let candidates =
        tokio::task::spawn_blocking(crate::proxy::providers::joycode::discover_joycode_credentials)
            .await
            .map_err(|error| format!("JoyCode credential discovery failed: {error}"))?;
    if candidates.is_empty() {
        return Ok(None);
    }
    let mut last_error = None;
    for candidate in candidates {
        match crate::proxy::providers::joycode::validate_discovered_credential(&candidate).await {
            Ok(credential) => return Ok(Some(credential)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| "未发现有效的 JoyCode 登录态".to_string()))
}

#[tauri::command]
pub async fn discover_joycode_pt_key() -> Result<Option<String>, String> {
    tokio::task::spawn_blocking(crate::proxy::providers::joycode::discover_latest_pt_key)
        .await
        .map_err(|error| format!("JoyCode credential discovery failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{parse_opencode_models, OpenCodeModelRef};

    #[test]
    fn parses_sorts_and_deduplicates_models() {
        assert_eq!(
            parse_opencode_models(
                "openrouter/vendor/model\nopencode/free-model\ninvalid\nopencode/free-model\n"
            ),
            vec![
                OpenCodeModelRef {
                    provider_id: "opencode".to_string(),
                    model_id: "free-model".to_string(),
                },
                OpenCodeModelRef {
                    provider_id: "openrouter".to_string(),
                    model_id: "vendor/model".to_string(),
                },
            ]
        );
    }

    #[test]
    fn skips_malformed_output_lines() {
        assert!(parse_opencode_models(
            "notice: loading models\n/model\nprovider/\nbad provider/model\nprovider/bad model\nprovider/bad\u{1b}[0m\n"
        )
        .is_empty());
    }
}
