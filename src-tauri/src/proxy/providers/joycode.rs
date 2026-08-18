//! JD JoyCode protocol helpers shared by every application ingress.
//!
//! JoyCode is not a regular OpenAI-compatible provider: authentication uses
//! dedicated headers, the model catalog is a POST endpoint, and each model may
//! select a different wire protocol. Keep those rules in one module so callers
//! never infer capabilities from model names.

use crate::provider::Provider;
use crate::proxy::error::ProxyError;
use crate::proxy::hyper_client::{ProxyResponse, MAX_RESPONSE_BODY_BYTES};
use bytes::Bytes;
use futures::StreamExt;
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const JOYCODE_INTERNAL_BASE_URL: &str = "http://joycode-api-saas.jd.com";
pub const JOYCODE_WEBSITE_URL: &str = "http://joycode.jd.com";
pub const JOYCODE_CLIENT: &str = "JoyCodeIDE";
pub const JOYCODE_CLIENT_VERSION: &str = "3.8.67";
pub const JOYCODE_EXTERNAL_BASE_URL_ENV: &str = "CC_SWITCH_JOYCODE_EXTERNAL_BASE_URL";
const MODEL_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const RESPONSE_SESSION_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const RESPONSE_SESSION_LIMIT: usize = 256;

// Kept byte-for-byte compatible with the referenced JoyCode IDE implementation.
// This signature is only used for an explicitly configured HTTPS gateway base;
// CC Switch never guesses the external host.
const JOYCODE_GATEWAY_SIGNING_KEY: &[u8] = b"0691a3f0b37b4a85aeb63ad0fc7db3ed";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoycodeNetwork {
    Internal,
    External,
}

impl JoycodeNetwork {
    pub fn parse(value: Option<&str>) -> Result<Self, ProxyError> {
        match value
            .unwrap_or("internal")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "internal" | "intranet" => Ok(Self::Internal),
            "external" | "internet" => Ok(Self::External),
            other => Err(ProxyError::ConfigError(format!(
                "Unsupported JoyCode network: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoycodeWireApi {
    Responses,
    Anthropic,
    Chat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoycodeModel {
    pub id: String,
    pub owned_by: String,
    pub wire_api: JoycodeWireApi,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
struct CachedCatalog {
    loaded_at: std::time::Instant,
    models: HashMap<String, JoycodeModel>,
}

static MODEL_CATALOGS: OnceLock<RwLock<HashMap<String, CachedCatalog>>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ResponseSessionKey {
    provider_id: String,
    account_scope: String,
    app_type: String,
    session_id: String,
    model: String,
}

#[derive(Debug, Clone)]
struct ResponseSession {
    response_id: String,
    request_input: Vec<Value>,
    response_output: Vec<Value>,
    updated_at: std::time::Instant,
}

#[derive(Debug, Clone)]
pub struct JoycodeResponseSessionContext {
    key: ResponseSessionKey,
    request_input: Vec<Value>,
}

static RESPONSE_SESSIONS: OnceLock<Mutex<HashMap<ResponseSessionKey, ResponseSession>>> =
    OnceLock::new();
static CHAT_CACHE_KEY_REJECTED: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();

fn response_sessions() -> &'static Mutex<HashMap<ResponseSessionKey, ResponseSession>> {
    RESPONSE_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn catalogs() -> &'static RwLock<HashMap<String, CachedCatalog>> {
    MODEL_CATALOGS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn rejected_chat_cache_keys() -> &'static RwLock<HashSet<String>> {
    CHAT_CACHE_KEY_REJECTED.get_or_init(|| RwLock::new(HashSet::new()))
}

fn chat_cache_capability_key(provider: &Provider, model: &str) -> String {
    let network = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.joycode_network.as_deref())
        .unwrap_or("internal");
    format!("{}\0{network}\0{model}", provider.id)
}

pub fn chat_prompt_cache_key_supported(provider: &Provider, model: &str) -> bool {
    rejected_chat_cache_keys()
        .read()
        .map(|rejected| !rejected.contains(&chat_cache_capability_key(provider, model)))
        .unwrap_or(true)
}

pub fn mark_chat_prompt_cache_key_unsupported(provider: &Provider, model: &str) {
    if let Ok(mut rejected) = rejected_chat_cache_keys().write() {
        rejected.insert(chat_cache_capability_key(provider, model));
    }
}

pub fn clear_responses_session(context: &JoycodeResponseSessionContext) {
    if let Ok(mut sessions) = response_sessions().lock() {
        sessions.remove(&context.key);
    }
}

pub fn is_joycode_provider(provider: &Provider) -> bool {
    provider
        .meta
        .as_ref()
        .and_then(|meta| meta.provider_type.as_deref())
        == Some("joycode")
}

pub fn is_internal_base_url(base_url: &str) -> bool {
    let normalized = base_url.trim().trim_end_matches('/');
    normalized == JOYCODE_INTERNAL_BASE_URL
        || normalized.starts_with(&format!("{JOYCODE_INTERNAL_BASE_URL}/"))
}

pub fn model_from_gemini_endpoint(endpoint: &str) -> Option<String> {
    let path = endpoint.split('?').next().unwrap_or(endpoint);
    let marker = "/models/";
    let start = path.find(marker)? + marker.len();
    let model = path[start..].split(':').next().unwrap_or_default().trim();
    (!model.is_empty()).then(|| model.to_string())
}

/// Convert Gemini `generateContent` input into an Anthropic Messages-shaped
/// intermediate representation. The existing Claude bridges can then route it
/// to JoyCode Responses, Chat, or Anthropic without duplicating those mature
/// converters (including media/tool normalization).
pub fn gemini_request_to_anthropic(
    body: Value,
    model: &str,
    stream: bool,
) -> Result<Value, ProxyError> {
    let mut messages = Vec::new();
    let contents = body
        .get("contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for content in contents {
        let role = match content.get("role").and_then(Value::as_str) {
            Some("model") => "assistant",
            _ => "user",
        };
        let mut blocks = Vec::new();
        for part in content
            .get("parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                blocks.push(json!({"type": "text", "text": text}));
            } else if let Some(inline) = part.get("inlineData").or_else(|| part.get("inline_data"))
            {
                if let (Some(media_type), Some(data)) = (
                    inline
                        .get("mimeType")
                        .or_else(|| inline.get("mime_type"))
                        .and_then(Value::as_str),
                    inline.get("data").and_then(Value::as_str),
                ) {
                    let block_type = if media_type.starts_with("image/") {
                        "image"
                    } else {
                        "document"
                    };
                    blocks.push(json!({
                        "type": block_type,
                        "source": {"type": "base64", "media_type": media_type, "data": data}
                    }));
                }
            } else if let Some(file) = part.get("fileData").or_else(|| part.get("file_data")) {
                let media_type = file
                    .get("mimeType")
                    .or_else(|| file.get("mime_type"))
                    .and_then(Value::as_str);
                let url = file
                    .get("fileUri")
                    .or_else(|| file.get("file_uri"))
                    .and_then(Value::as_str);
                if let (Some(media_type), Some(url)) = (media_type, url) {
                    if !url.starts_with("http://") && !url.starts_with("https://") {
                        return Err(ProxyError::InvalidRequest(
                            "JoyCode only accepts Gemini fileData with an HTTP(S) URL; upload-only Gemini file URIs cannot be forwarded safely"
                                .to_string(),
                        ));
                    }
                    let block_type = if media_type.starts_with("image/") {
                        "image"
                    } else {
                        "document"
                    };
                    blocks.push(json!({
                        "type": block_type,
                        "source": {"type": "url", "url": url}
                    }));
                }
            } else if let Some(call) = part.get("functionCall") {
                let name = call.get("name").and_then(Value::as_str).unwrap_or("tool");
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("joycode_{}", uuid::Uuid::new_v4().simple()));
                blocks.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": call.get("args").cloned().unwrap_or_else(|| json!({}))
                }));
            } else if let Some(result) = part.get("functionResponse") {
                let id = result
                    .get("id")
                    .and_then(Value::as_str)
                    .or_else(|| result.get("name").and_then(Value::as_str))
                    .unwrap_or("tool");
                let response = result.get("response").cloned().unwrap_or(Value::Null);
                blocks.push(json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": response
                }));
            }
        }
        if !blocks.is_empty() {
            messages.push(json!({"role": role, "content": blocks}));
        }
    }

    let mut output = json!({
        "model": model,
        "messages": messages,
        "max_tokens": body.pointer("/generationConfig/maxOutputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(8192),
        "stream": stream,
    });
    if let Some(system) = body.get("systemInstruction") {
        let parts = system
            .get("parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let blocks: Vec<Value> = parts
            .into_iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .map(|text| json!({"type": "text", "text": text}))
            })
            .collect();
        if !blocks.is_empty() {
            output["system"] = Value::Array(blocks);
        }
    }
    if let Some(config) = body.get("generationConfig") {
        for (gemini, anthropic) in [
            ("temperature", "temperature"),
            ("topP", "top_p"),
            ("stopSequences", "stop_sequences"),
        ] {
            if let Some(value) = config.get(gemini) {
                output[anthropic] = value.clone();
            }
        }
    }
    if let Some(declarations) = body
        .get("tools")
        .and_then(Value::as_array)
        .and_then(|tools| {
            tools
                .iter()
                .find_map(|tool| tool.get("functionDeclarations"))
        })
        .and_then(Value::as_array)
    {
        output["tools"] = Value::Array(
            declarations
                .iter()
                .map(|declaration| json!({
                    "name": declaration.get("name").cloned().unwrap_or_else(|| json!("tool")),
                    "description": declaration.get("description").cloned().unwrap_or(Value::Null),
                    "input_schema": declaration.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"}))
                }))
                .collect(),
        );
    }
    Ok(output)
}

pub fn anthropic_message_to_gemini(message: &Value) -> Value {
    let parts: Vec<Value> = message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                Some(json!({"text": block.get("text").and_then(Value::as_str).unwrap_or("")}))
            }
            Some("tool_use") => Some(json!({"functionCall": {
                "id": block.get("id").cloned().unwrap_or(Value::Null),
                "name": block.get("name").cloned().unwrap_or_else(|| json!("tool")),
                "args": block.get("input").cloned().unwrap_or_else(|| json!({}))
            }})),
            _ => None,
        })
        .collect();
    let usage = message.get("usage").cloned().unwrap_or_else(|| json!({}));
    let prompt = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let finish_reason = match message.get("stop_reason").and_then(Value::as_str) {
        Some("max_tokens") => "MAX_TOKENS",
        Some("tool_use") => "STOP",
        Some("refusal") => "SAFETY",
        _ => "STOP",
    };
    json!({
        "candidates": [{
            "content": {"role": "model", "parts": parts},
            "finishReason": finish_reason,
            "index": 0
        }],
        "usageMetadata": {
            "promptTokenCount": prompt,
            "cachedContentTokenCount": cached,
            "candidatesTokenCount": output,
            "totalTokenCount": prompt.saturating_add(output)
        },
        "modelVersion": message.get("model").cloned().unwrap_or(Value::Null),
        "responseId": message.get("id").cloned().unwrap_or(Value::Null)
    })
}

#[derive(Default)]
pub struct AnthropicToGeminiSseNormalizer {
    buffer: String,
    utf8_remainder: Vec<u8>,
    response_id: String,
    model: String,
    input_tokens: u64,
    cache_read_tokens: u64,
    tool_calls: HashMap<u64, (String, String, String)>,
}

impl AnthropicToGeminiSseNormalizer {
    pub fn push_bytes(&mut self, bytes: &[u8]) -> Vec<u8> {
        crate::proxy::sse::append_utf8_safe(&mut self.buffer, &mut self.utf8_remainder, bytes);
        let mut output = Vec::new();
        while let Some(block) = crate::proxy::sse::take_sse_block(&mut self.buffer) {
            self.handle_block(&block, &mut output);
        }
        output
    }

    pub fn finish(&mut self) -> Vec<u8> {
        if !self.utf8_remainder.is_empty() {
            self.buffer
                .push_str(&String::from_utf8_lossy(&self.utf8_remainder));
            self.utf8_remainder.clear();
        }
        let block = std::mem::take(&mut self.buffer);
        let mut output = Vec::new();
        if !block.trim().is_empty() {
            self.handle_block(&block, &mut output);
        }
        output
    }

    fn handle_block(&mut self, block: &str, output: &mut Vec<u8>) {
        let data = block
            .lines()
            .filter_map(|line| line.trim_end_matches('\r').strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(event) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let message = event.get("message").unwrap_or(&event);
                self.response_id = message
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.model = message
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if let Some(usage) = message.get("usage") {
                    self.input_tokens = usage
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    self.cache_read_tokens = usage
                        .get("cache_read_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                }
            }
            Some("content_block_start") => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let Some(block) = event.get("content_block") else {
                    return;
                };
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block
                            .get("text")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                        {
                            self.emit_part(json!({"text": text}), output);
                        }
                    }
                    Some("tool_use") => {
                        self.tool_calls.insert(
                            index,
                            (
                                block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("tool")
                                    .to_string(),
                                String::new(),
                            ),
                        );
                    }
                    _ => {}
                }
            }
            Some("content_block_delta") => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let Some(delta) = event.get("delta") else {
                    return;
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            self.emit_part(json!({"text": text}), output);
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                            if let Some((_, _, arguments)) = self.tool_calls.get_mut(&index) {
                                arguments.push_str(partial);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some((id, name, arguments)) = self.tool_calls.remove(&index) {
                    let args =
                        serde_json::from_str::<Value>(&arguments).unwrap_or_else(|_| json!({}));
                    self.emit_part(
                        json!({"functionCall": {"id": id, "name": name, "args": args}}),
                        output,
                    );
                }
            }
            Some("message_delta") => {
                let Some(delta) = event.get("delta") else {
                    return;
                };
                let finish_reason = match delta.get("stop_reason").and_then(Value::as_str) {
                    Some("max_tokens") => "MAX_TOKENS",
                    Some("refusal") => "SAFETY",
                    _ => "STOP",
                };
                let output_tokens = event
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                self.emit(
                    json!({
                        "candidates": [{"finishReason": finish_reason, "index": 0}],
                        "usageMetadata": {
                            "promptTokenCount": self.input_tokens,
                            "cachedContentTokenCount": self.cache_read_tokens,
                            "candidatesTokenCount": output_tokens,
                            "totalTokenCount": self.input_tokens.saturating_add(output_tokens)
                        },
                        "modelVersion": self.model,
                        "responseId": self.response_id
                    }),
                    output,
                );
            }
            _ => {}
        }
    }

    fn emit_part(&self, part: Value, output: &mut Vec<u8>) {
        self.emit(
            json!({
                "candidates": [{
                    "content": {"role": "model", "parts": [part]},
                    "index": 0
                }],
                "modelVersion": self.model,
                "responseId": self.response_id
            }),
            output,
        );
    }

    fn emit(&self, value: Value, output: &mut Vec<u8>) {
        output.extend_from_slice(b"data: ");
        if let Ok(encoded) = serde_json::to_vec(&value) {
            output.extend_from_slice(&encoded);
        }
        output.extend_from_slice(b"\n\n");
    }
}

pub fn provider_network(provider: &Provider) -> Result<JoycodeNetwork, ProxyError> {
    JoycodeNetwork::parse(
        provider
            .meta
            .as_ref()
            .and_then(|meta| meta.joycode_network.as_deref()),
    )
}

pub fn provider_base_url(provider: &Provider) -> Result<String, ProxyError> {
    match provider_network(provider)? {
        JoycodeNetwork::Internal => Ok(JOYCODE_INTERNAL_BASE_URL.to_string()),
        JoycodeNetwork::External => provider
            .meta
            .as_ref()
            .and_then(|meta| meta.joycode_external_base_url.as_deref())
            .map(str::trim)
            .filter(|url| url.starts_with("https://"))
            .map(|url| url.trim_end_matches('/').to_string())
            .or_else(|| {
                std::env::var(JOYCODE_EXTERNAL_BASE_URL_ENV)
                    .ok()
                    .map(|url| url.trim().trim_end_matches('/').to_string())
                    .filter(|url| url.starts_with("https://"))
            })
            .ok_or_else(|| {
                ProxyError::ConfigError(
                    "JoyCode external gateway address is not available; deploy an official HTTPS address through CC_SWITCH_JOYCODE_EXTERNAL_BASE_URL"
                        .to_string(),
                )
            }),
    }
}

pub fn login_type_for_pt_key(pt_key: &str) -> &'static str {
    if pt_key.starts_with("BJ.") {
        "ERP"
    } else {
        "N_PIN_PC"
    }
}

/// Accept the raw token as well as the common forms copied from cookie/config
/// views. Only the token value is forwarded to JoyCode.
pub fn normalize_pt_key(value: &str) -> String {
    let trimmed = value.trim().trim_matches(['\'', '"']);
    let candidate = trimmed
        .split(';')
        .find_map(|part| {
            let part = part.trim();
            let (name, value) = part.split_once('=')?;
            matches!(
                name.trim().to_ascii_lowercase().as_str(),
                "ptkey" | "pt_key"
            )
            .then(|| value.trim().trim_matches(['\'', '"']))
        })
        .unwrap_or(trimmed);
    candidate.to_string()
}

pub fn sign_gateway_url_at(base_url: &str, function_id: &str, timestamp_ms: u128) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let message = format!("joycode_ide&{function_id}&{timestamp_ms}");
    let mut mac = <HmacSha256 as Mac>::new_from_slice(JOYCODE_GATEWAY_SIGNING_KEY)
        .expect("JoyCode signing key has a valid HMAC length");
    mac.update(message.as_bytes());
    let signature = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}/api?appid=joycode_ide&functionId={function_id}&t={timestamp_ms}&sign={signature}",
        base_url.trim_end_matches('/')
    )
}

pub fn sign_gateway_url(base_url: &str, function_id: &str) -> String {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    sign_gateway_url_at(base_url, function_id, timestamp_ms)
}

pub fn endpoint_for(provider: &Provider, wire_api: JoycodeWireApi) -> Result<String, ProxyError> {
    let base = provider_base_url(provider)?;
    let (internal_path, function_id) = match wire_api {
        JoycodeWireApi::Responses => ("/api/saas/openai/v1/responses", "responses_completions"),
        JoycodeWireApi::Anthropic => ("/api/saas/anthropic/v1/messages", "anthropic_completions"),
        JoycodeWireApi::Chat => ("/api/saas/openai/v2/chat/completions", "chat_completions"),
    };
    Ok(match provider_network(provider)? {
        JoycodeNetwork::Internal => format!("{}{internal_path}", base.trim_end_matches('/')),
        JoycodeNetwork::External => sign_gateway_url(&base, function_id),
    })
}

pub fn model_list_endpoint(provider: &Provider) -> Result<String, ProxyError> {
    let base = provider_base_url(provider)?;
    Ok(match provider_network(provider)? {
        JoycodeNetwork::Internal => format!(
            "{}/api/saas/models/v2/modelList",
            base.trim_end_matches('/')
        ),
        JoycodeNetwork::External => sign_gateway_url(&base, "joycode_modelList"),
    })
}

pub fn auth_headers(pt_key: &str) -> Result<HeaderMap, ProxyError> {
    let pt_key = normalize_pt_key(pt_key);
    if pt_key.is_empty() {
        return Err(ProxyError::AuthError(
            "JoyCode ptKey is empty; log in with the official JoyCode client first".to_string(),
        ));
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("ptkey"),
        HeaderValue::from_str(&pt_key)
            .map_err(|error| ProxyError::AuthError(format!("Invalid JoyCode ptKey: {error}")))?,
    );
    headers.insert(
        HeaderName::from_static("logintype"),
        HeaderValue::from_static(login_type_for_pt_key(&pt_key)),
    );
    headers.insert(
        HeaderName::from_static("x-ms-client-request-id"),
        HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()).expect("UUID is a valid header"),
    );
    headers.insert(
        HeaderName::from_static("client"),
        HeaderValue::from_static(JOYCODE_CLIENT),
    );
    headers.insert(
        HeaderName::from_static("clientversion"),
        HeaderValue::from_static(JOYCODE_CLIENT_VERSION),
    );
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=UTF-8"),
    );
    Ok(headers)
}

pub fn decorate_body(body: &mut Value, wire_api: JoycodeWireApi) {
    body["client"] = Value::String(JOYCODE_CLIENT.to_string());
    body["clientVersion"] = Value::String(JOYCODE_CLIENT_VERSION.to_string());
    if wire_api == JoycodeWireApi::Responses {
        body["store"] = Value::Bool(true);
    }
}

/// Chat Completions has no portable document input block. Reject instead of
/// silently dropping a PDF/file or serializing base64 as ordinary text tokens.
pub fn validate_media_for_wire(body: &Value, wire_api: JoycodeWireApi) -> Result<(), ProxyError> {
    if wire_api != JoycodeWireApi::Chat {
        return Ok(());
    }
    fn contains_document(value: &Value) -> bool {
        match value {
            Value::Object(object) => {
                matches!(
                    object.get("type").and_then(Value::as_str),
                    Some("document" | "input_file")
                ) || object.values().any(contains_document)
            }
            Value::Array(values) => values.iter().any(contains_document),
            _ => false,
        }
    }
    if contains_document(body) {
        return Err(ProxyError::InvalidRequest(
            "The selected JoyCode Chat model cannot carry document blocks without converting file bytes into costly text tokens; choose a Responses or Anthropic model"
                .to_string(),
        ));
    }
    Ok(())
}

/// Convert a full Responses replay into a safe incremental request when the
/// client history still contains the exact request/output prefix previously
/// completed by JoyCode. Any edit or branch mismatch clears the chain.
pub fn prepare_responses_session(
    provider: &Provider,
    pt_key: &str,
    app_type: &str,
    session_id: Option<&str>,
    body: &mut Value,
) -> Option<JoycodeResponseSessionContext> {
    body["store"] = Value::Bool(true);
    let session_id = session_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
        .or_else(|| stable_conversation_id(body))?;
    let model = body.get("model")?.as_str()?.trim();
    let input = body.get("input")?.as_array()?.clone();
    let key = ResponseSessionKey {
        provider_id: provider.id.clone(),
        account_scope: credential_fingerprint(pt_key),
        app_type: app_type.to_string(),
        session_id,
        model: model.to_string(),
    };
    let context = JoycodeResponseSessionContext {
        key: key.clone(),
        request_input: input.clone(),
    };

    if body
        .get("previous_response_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty())
    {
        return Some(context);
    }

    let previous = response_sessions().lock().ok().and_then(|mut sessions| {
        prune_response_sessions(&mut sessions);
        sessions.get(&key).cloned()
    });
    let Some(previous) = previous else {
        return Some(context);
    };

    if let Some(incremental) =
        incremental_input(&input, &previous.request_input, &previous.response_output)
    {
        body["input"] = Value::Array(incremental);
        body["previous_response_id"] = Value::String(previous.response_id);
    } else if let Ok(mut sessions) = response_sessions().lock() {
        sessions.remove(&key);
    }
    Some(context)
}

pub fn stable_conversation_id(body: &Value) -> Option<String> {
    for key in ["conversation", "conversation_id", "prompt_cache_key"] {
        if let Some(value) = body
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(value.to_string());
        }
    }
    let metadata = body.get("client_metadata").or_else(|| body.get("metadata"));
    for key in ["thread_id", "session_id", "conversation_id"] {
        if let Some(value) = metadata
            .and_then(|metadata| metadata.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(value.to_string());
        }
    }
    None
}

fn incremental_input(
    current: &[Value],
    previous_input: &[Value],
    previous_output: &[Value],
) -> Option<Vec<Value>> {
    if previous_output.is_empty()
        || current.len() <= previous_input.len() + previous_output.len()
        || current.get(..previous_input.len())? != previous_input
    {
        return None;
    }
    let replayed_output = current
        .get(previous_input.len()..previous_input.len().saturating_add(previous_output.len()))?;
    if !replayed_output
        .iter()
        .zip(previous_output)
        .all(|(input_item, output_item)| response_items_match(input_item, output_item))
    {
        return None;
    }
    let delta = current[previous_input.len() + previous_output.len()..].to_vec();
    (!delta.is_empty()).then_some(delta)
}

fn response_items_match(input: &Value, output: &Value) -> bool {
    if output.get("type").and_then(Value::as_str) == Some("message")
        && input.get("role").and_then(Value::as_str) == Some("assistant")
        && output.get("role").and_then(Value::as_str) == Some("assistant")
    {
        return message_content_matches(input.get("content"), output.get("content"));
    }
    if output.get("type").and_then(Value::as_str) == Some("function_call")
        && input.get("type").and_then(Value::as_str) == Some("function_call")
    {
        return ["call_id", "name", "arguments"]
            .iter()
            .all(|field| input.get(*field) == output.get(*field));
    }
    match (
        input.get("type").and_then(Value::as_str),
        input.get("id").and_then(Value::as_str),
        output.get("type").and_then(Value::as_str),
        output.get("id").and_then(Value::as_str),
    ) {
        (Some(input_type), Some(input_id), Some(output_type), Some(output_id)) => {
            input_type == output_type && input_id == output_id
        }
        _ => input == output,
    }
}

fn message_content_matches(input: Option<&Value>, output: Option<&Value>) -> bool {
    let Some(input) = input.and_then(Value::as_array) else {
        return false;
    };
    let Some(output) = output.and_then(Value::as_array) else {
        return false;
    };
    input.len() == output.len()
        && input.iter().zip(output).all(|(left, right)| {
            left.get("type") == right.get("type")
                && left.get("text") == right.get("text")
                && left.get("refusal") == right.get("refusal")
        })
}

pub fn record_completed_response(context: &JoycodeResponseSessionContext, payload: &Value) {
    let response = if payload.get("type").and_then(Value::as_str) == Some("response.completed") {
        payload.get("response").unwrap_or(payload)
    } else {
        payload
    };
    let Some(response_id) = response
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return;
    };
    let Some(output) = response
        .get("output")
        .and_then(Value::as_array)
        .filter(|output| !output.is_empty())
        .cloned()
    else {
        return;
    };
    if let Ok(mut sessions) = response_sessions().lock() {
        prune_response_sessions(&mut sessions);
        if sessions.len() >= RESPONSE_SESSION_LIMIT && !sessions.contains_key(&context.key) {
            if let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, session)| session.updated_at)
                .map(|(key, _)| key.clone())
            {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(
            context.key.clone(),
            ResponseSession {
                response_id: response_id.to_string(),
                request_input: context.request_input.clone(),
                response_output: output,
                updated_at: std::time::Instant::now(),
            },
        );
    }
}

fn prune_response_sessions(sessions: &mut HashMap<ResponseSessionKey, ResponseSession>) {
    sessions.retain(|_, session| session.updated_at.elapsed() <= RESPONSE_SESSION_TTL);
}

#[derive(Default)]
pub struct JoycodeResponsesSseNormalizer {
    buffer: String,
    utf8_remainder: Vec<u8>,
    pending_event: Option<String>,
    session_context: Option<JoycodeResponseSessionContext>,
}

impl JoycodeResponsesSseNormalizer {
    pub fn with_session_context(context: Option<JoycodeResponseSessionContext>) -> Self {
        Self {
            session_context: context,
            ..Self::default()
        }
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) -> Vec<u8> {
        crate::proxy::sse::append_utf8_safe(&mut self.buffer, &mut self.utf8_remainder, bytes);
        let mut output = String::new();
        while let Some(block) = crate::proxy::sse::take_sse_block(&mut self.buffer) {
            self.handle_block(&block, &mut output);
        }
        output.into_bytes()
    }

    pub fn finish(&mut self) -> Vec<u8> {
        if !self.utf8_remainder.is_empty() {
            self.buffer
                .push_str(&String::from_utf8_lossy(&self.utf8_remainder));
            self.utf8_remainder.clear();
        }
        let mut output = String::new();
        if !self.buffer.trim().is_empty() {
            let block = std::mem::take(&mut self.buffer);
            self.handle_block(&block, &mut output);
        }
        if let Some(event) = self.pending_event.take() {
            output.push_str(&event);
            output.push_str("\n\n");
        }
        output.into_bytes()
    }

    fn handle_block(&mut self, block: &str, output: &mut String) {
        let trimmed = block.trim();
        if trimmed.is_empty() {
            return;
        }
        let mut lines = trimmed.lines();
        let first = lines.next().unwrap_or_default().trim_end_matches('\r');
        if lines.next().is_none() {
            if let Some(outer) = first.strip_prefix("data:") {
                let inner = outer.trim_start();
                if inner.starts_with("event:") {
                    if let Some(previous) = self.pending_event.replace(inner.to_string()) {
                        output.push_str(&previous);
                        output.push_str("\n\n");
                    }
                    return;
                }
                if inner.starts_with("data:") {
                    if let Some(event) = self.pending_event.take() {
                        output.push_str(&event);
                        output.push('\n');
                    }
                    self.observe_data(inner.trim_start_matches("data:").trim_start());
                    output.push_str(inner);
                    output.push_str("\n\n");
                    return;
                }
            }
        }
        if let Some(event) = self.pending_event.take() {
            output.push_str(&event);
            output.push('\n');
        }
        for line in trimmed.lines() {
            if let Some(data) = line.trim_end_matches('\r').strip_prefix("data:") {
                self.observe_data(data.trim_start());
            }
        }
        output.push_str(trimmed);
        output.push_str("\n\n");
    }

    fn observe_data(&self, data: &str) {
        let Some(context) = self.session_context.as_ref() else {
            return;
        };
        let Ok(payload) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if payload.get("type").and_then(Value::as_str) == Some("response.completed") {
            record_completed_response(context, &payload);
        }
    }
}

pub async fn normalize_responses_response(
    response: ProxyResponse,
    is_streaming: bool,
    context: Option<JoycodeResponseSessionContext>,
) -> Result<ProxyResponse, ProxyError> {
    let status = response.status();
    let mut headers = response.headers().clone();
    headers.remove(http::header::CONTENT_LENGTH);
    if !is_streaming || response.is_json() {
        let bytes = response.bytes_with_limit(MAX_RESPONSE_BODY_BYTES).await?;
        if let Ok(payload) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(context) = context.as_ref() {
                record_completed_response(context, &payload);
            }
        }
        return Ok(ProxyResponse::buffered(status, headers, bytes));
    }

    let stream = Box::pin(response.bytes_stream());
    let normalizer = JoycodeResponsesSseNormalizer::with_session_context(context);
    let normalized = futures::stream::unfold(
        (stream, normalizer, false),
        |(mut stream, mut normalizer, finished)| async move {
            if finished {
                return None;
            }
            loop {
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        let output = normalizer.push_bytes(&chunk);
                        if output.is_empty() {
                            continue;
                        }
                        return Some((Ok(Bytes::from(output)), (stream, normalizer, false)));
                    }
                    Some(Err(error)) => {
                        return Some((Err(error), (stream, normalizer, true)));
                    }
                    None => {
                        let output = normalizer.finish();
                        if output.is_empty() {
                            return None;
                        }
                        return Some((Ok(Bytes::from(output)), (stream, normalizer, true)));
                    }
                }
            }
        },
    );
    Ok(ProxyResponse::streamed(status, headers, normalized))
}

/// JoyCode may return an authentication envelope with HTTP 200. Detect it
/// before protocol conversion so every client receives a real auth error
/// instead of a misleading JSON/SSE parse failure.
pub async fn validate_auth_envelope(response: ProxyResponse) -> Result<ProxyResponse, ProxyError> {
    if !response.is_json() {
        return Ok(response);
    }
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.bytes_with_limit(MAX_RESPONSE_BODY_BYTES).await?;
    if let Ok(payload) = serde_json::from_slice::<Value>(&bytes) {
        let unauthorized = payload.get("code").and_then(Value::as_i64) == Some(401)
            || payload
                .pointer("/data/loginUrl")
                .and_then(Value::as_str)
                .is_some();
        if unauthorized {
            return Err(ProxyError::AuthError(
                "JoyCode credential expired; open the official login page and import it again"
                    .to_string(),
            ));
        }
    }
    Ok(ProxyResponse::buffered(status, headers, bytes))
}

pub fn prompt_cache_key(
    provider: &Provider,
    pt_key: &str,
    app_type: &str,
    session_id: &str,
    model: &str,
) -> String {
    let account_scope = credential_fingerprint(pt_key);
    let network = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.joycode_network.as_deref())
        .unwrap_or("internal");
    let digest = Sha256::digest(
        format!(
            "{}\0{account_scope}\0{network}\0{app_type}\0{session_id}\0{model}",
            provider.id
        )
        .as_bytes(),
    );
    format!(
        "ccs-jc-{}",
        digest
            .iter()
            .take(16)
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn credential_fingerprint(pt_key: &str) -> String {
    Sha256::digest(normalize_pt_key(pt_key).as_bytes())
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn parse_model_catalog(payload: &Value) -> Result<Vec<JoycodeModel>, String> {
    let response_code = payload.get("code").and_then(|code| {
        code.as_i64()
            .or_else(|| code.as_str().and_then(|code| code.parse().ok()))
    });
    if response_code == Some(401) {
        return Err(
            "JoyCode 认证失败：账号未登录或 ptKey 已失效，请重新登录 JoyCode 并填写最新 ptKey"
                .to_string(),
        );
    }
    if let Some(code) = response_code.filter(|code| *code != 0 && *code != 200) {
        let message = payload
            .get("msg")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .unwrap_or("未知错误");
        return Err(format!("JoyCode 返回错误 {code}：{message}"));
    }
    let entries = payload
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "JoyCode model response has no data array".to_string())?;
    let mut models = HashMap::<String, JoycodeModel>::new();
    for entry in entries {
        let Some(id) = entry
            .get("chatApiModel")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let ext = entry.get("extJson").cloned().or_else(|| {
            entry
                .get("ext")
                .and_then(Value::as_str)
                .and_then(|raw| serde_json::from_str(raw).ok())
        });
        let adapter_type = ext
            .as_ref()
            .and_then(|ext| ext.get("adapterType"))
            .and_then(Value::as_str);
        let wire_api = match adapter_type {
            Some("openai-response") => JoycodeWireApi::Responses,
            Some("anthropic") => JoycodeWireApi::Anthropic,
            _ => JoycodeWireApi::Chat,
        };
        let context_window = u64_field(entry, &["maxTotalTokens", "respMaxTokens"]).or_else(|| {
            ext.as_ref()
                .and_then(|ext| u64_field(ext, &["maxTotalTokens", "respMaxTokens"]))
        });
        let max_output_tokens =
            u64_field(entry, &["maxOutputTokens", "respMaxTokens"]).or_else(|| {
                ext.as_ref()
                    .and_then(|ext| u64_field(ext, &["maxOutputTokens", "respMaxTokens"]))
            });
        models.insert(
            id.to_string(),
            JoycodeModel {
                id: id.to_string(),
                owned_by: "jd".to_string(),
                wire_api,
                context_window,
                max_output_tokens,
            },
        );
    }
    let mut models = models.into_values().collect::<Vec<_>>();
    models.sort_by(|left, right| left.id.cmp(&right.id));
    if models.is_empty() {
        return Err("JoyCode returned no usable models".to_string());
    }
    Ok(models)
}

fn u64_field(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        value.get(*key).and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|raw| raw.parse().ok()))
        })
    })
}

pub async fn fetch_models(provider: &Provider, pt_key: &str) -> Result<Vec<JoycodeModel>, String> {
    let endpoint = model_list_endpoint(provider).map_err(|error| error.to_string())?;
    let headers = auth_headers(pt_key).map_err(|error| error.to_string())?;
    let response = crate::proxy::http_client::get()
        .post(&endpoint)
        .headers(headers)
        .timeout(Duration::from_secs(20))
        .json(&json!({
            "client": JOYCODE_CLIENT,
            "clientVersion": JOYCODE_CLIENT_VERSION,
        }))
        .send()
        .await
        .map_err(|error| format!("JoyCode model request failed: {error}"))?;
    let status = response.status();
    let response_text = response
        .text()
        .await
        .map_err(|error| format!("读取 JoyCode 模型响应失败：{error}"))?;
    let payload: Value = serde_json::from_str(&response_text).map_err(|_| {
        format!(
            "JoyCode 模型接口返回了非 JSON 响应（HTTP {}）",
            status.as_u16()
        )
    })?;
    if !status.is_success() {
        if let Err(error) = parse_model_catalog(&payload) {
            return Err(error);
        }
        return Err(format!("JoyCode 模型请求失败（HTTP {}）", status.as_u16()));
    }
    let models = parse_model_catalog(&payload)?;
    cache_models(&catalog_scope(provider, pt_key), &models);
    Ok(models)
}

fn catalog_scope(provider: &Provider, pt_key: &str) -> String {
    let network = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.joycode_network.as_deref())
        .unwrap_or("internal");
    let digest = Sha256::digest(normalize_pt_key(pt_key).as_bytes());
    let account = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{}\0{network}\0{account}", provider.id)
}

fn cache_models(scope: &str, models: &[JoycodeModel]) {
    let model_map = models
        .iter()
        .cloned()
        .map(|model| (model.id.clone(), model))
        .collect();
    if let Ok(mut catalogs) = catalogs().write() {
        catalogs.insert(
            scope.to_string(),
            CachedCatalog {
                loaded_at: std::time::Instant::now(),
                models: model_map,
            },
        );
    }
}

fn cached_model(scope: &str, model_id: &str) -> Option<JoycodeModel> {
    let catalogs = catalogs().read().ok()?;
    let catalog = catalogs.get(scope)?;
    if catalog.loaded_at.elapsed() > MODEL_CACHE_TTL {
        return None;
    }
    catalog.models.get(model_id).cloned().or_else(|| {
        model_id
            .strip_suffix("-hq")
            .and_then(|plain| catalog.models.get(plain).cloned())
    })
}

pub async fn resolve_model(
    provider: &Provider,
    model_id: &str,
    pt_key: &str,
) -> Result<JoycodeModel, ProxyError> {
    let use_catalog_default = matches!(model_id.trim(), "" | "joycode" | "custom");
    if !use_catalog_default {
        if let Some(model) = cached_model(&catalog_scope(provider, pt_key), model_id) {
            return Ok(model);
        }
    }
    let models = fetch_models(provider, pt_key)
        .await
        .map_err(ProxyError::ConfigError)?;
    if use_catalog_default {
        return models.into_iter().next().ok_or_else(|| {
            ProxyError::InvalidRequest("JoyCode account has no available model".to_string())
        });
    }
    models
        .into_iter()
        .find(|model| model.id == model_id || model.id == format!("{model_id}-hq"))
        .ok_or_else(|| {
            ProxyError::InvalidRequest(format!(
                "JoyCode model '{model_id}' is not present in the current account catalog"
            ))
        })
}

#[derive(Debug)]
struct PtKeyCandidate {
    value: String,
    timestamp: String,
}

fn pt_key_timestamp(value: &str) -> String {
    value
        .split('.')
        .nth(2)
        .and_then(|part| part.find("202").map(|index| &part[index..]))
        .filter(|timestamp| timestamp.len() >= 14)
        .map(|timestamp| timestamp[..14].to_string())
        .unwrap_or_else(|| "00000000000000".to_string())
}

/// Prefer the newest credential between the provider snapshot and official
/// local JoyCode/JoyCoder storage. This keeps long-lived provider entries
/// usable after the official client silently refreshes ptKey.
pub fn resolve_latest_pt_key(configured: &str) -> String {
    let configured = normalize_pt_key(configured);
    match discover_latest_pt_key() {
        // Prefer official local storage on equal/unknown timestamps as well;
        // this mirrors the reference client's candidate order and lets an
        // empty saved key be filled after the user logs in.
        Some(local) if pt_key_timestamp(&local) >= pt_key_timestamp(&configured) => {
            normalize_pt_key(&local)
        }
        _ => configured,
    }
}

fn jetbrains_pt_key(path: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let marker = "name=\"userToken\" value=\"";
    let start = content.find(marker)? + marker.len();
    let value = content.get(start..)?.split('"').next()?;
    value
        .split_once('=')
        .map(|(_, key)| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

fn collect_jetbrains_paths() -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Library/Application Support/JetBrains"));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        roots.push(std::path::PathBuf::from(appdata).join("JetBrains"));
    }
    let mut paths = Vec::new();
    for root in roots {
        let Ok(products) = std::fs::read_dir(root) else {
            continue;
        };
        for product in products.flatten() {
            let Ok(options) = std::fs::read_dir(product.path().join("options")) else {
                continue;
            };
            for entry in options.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains("JoyCoderSettings") && name.ends_with(".xml") {
                    paths.push(entry.path());
                }
            }
        }
    }
    paths
}

/// Discover the newest JoyCode/JoyCoder credential from official local client
/// storage. Databases are opened read-only and the credential is returned only
/// to the explicit login/import command; it is never logged.
pub fn discover_latest_pt_key() -> Option<String> {
    let mut candidates = Vec::<PtKeyCandidate>::new();
    let mut add = |value: String| {
        let value = value.trim().to_string();
        if !value.is_empty() {
            candidates.push(PtKeyCandidate {
                timestamp: pt_key_timestamp(&value),
                value,
            });
        }
    };
    for path in collect_jetbrains_paths() {
        if let Some(value) = jetbrains_pt_key(&path) {
            add(value);
        }
    }
    let mut databases = Vec::new();
    if let Some(home) = dirs::home_dir() {
        databases
            .push(home.join("Library/Application Support/Code/User/globalStorage/state.vscdb"));
        databases
            .push(home.join("Library/Application Support/JoyCode/User/globalStorage/state.vscdb"));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let appdata = std::path::PathBuf::from(appdata);
        databases.push(appdata.join("Code/User/globalStorage/state.vscdb"));
        databases.push(appdata.join("JoyCode/User/globalStorage/state.vscdb"));
    }
    for path in databases.into_iter().filter(|path| path.exists()) {
        let Ok(connection) = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            continue;
        };
        let Ok(value) = connection.query_row(
            "SELECT value FROM ItemTable WHERE key = 'JoyCoder.joycoder-fe'",
            [],
            |row| row.get::<_, String>(0),
        ) else {
            continue;
        };
        if let Some(pt_key) = serde_json::from_str::<Value>(&value)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/jdhLoginInfo/ptKey")?
                    .as_str()
                    .map(str::to_string)
            })
        {
            add(pt_key);
        }
    }
    candidates.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));
    candidates
        .into_iter()
        .next()
        .map(|candidate| candidate.value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_type_follows_pt_key_prefix() {
        assert_eq!(login_type_for_pt_key("BJ.example"), "ERP");
        assert_eq!(login_type_for_pt_key("other"), "N_PIN_PC");
    }

    #[test]
    fn normalizes_pt_key_copied_from_cookie_or_config_views() {
        assert_eq!(normalize_pt_key("  BJ.raw  "), "BJ.raw");
        assert_eq!(normalize_pt_key("ptKey=BJ.config"), "BJ.config");
        assert_eq!(
            normalize_pt_key("foo=bar; pt_key=BJ.cookie; path=/"),
            "BJ.cookie"
        );
        assert_eq!(normalize_pt_key("\"BJ.quoted\""), "BJ.quoted");
    }

    #[test]
    fn model_catalog_reports_expired_credentials_actionably() {
        let error = parse_model_catalog(&json!({
            "code": "401",
            "data": null,
            "msg": "账号未登录"
        }))
        .unwrap_err();
        assert!(error.contains("ptKey 已失效"));
        assert!(error.contains("最新 ptKey"));
    }

    #[test]
    fn signed_url_is_deterministic_with_fixed_timestamp() {
        let url = sign_gateway_url_at("https://gateway.example/", "joycode_modelList", 42);
        assert_eq!(
            url,
            "https://gateway.example/api?appid=joycode_ide&functionId=joycode_modelList&t=42&sign=7190b40634d7fb32a193bc46c5b9504034b1c8d39c5b7d2856a8048469cb8374"
        );
    }

    #[test]
    fn parses_all_wire_protocols_and_limits() {
        let payload = json!({"data": [
            {"chatApiModel":"r", "maxTotalTokens": 1000, "respMaxTokens":"120", "extJson":{"adapterType":"openai-response"}},
            {"chatApiModel":"a", "ext":"{\"adapterType\":\"anthropic\",\"maxTotalTokens\":2000}"},
            {"chatApiModel":"c"},
            {"chatApiModel":""}
        ]});
        let models = parse_model_catalog(&payload).unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].wire_api, JoycodeWireApi::Anthropic);
        assert_eq!(models[1].wire_api, JoycodeWireApi::Chat);
        assert_eq!(models[2].wire_api, JoycodeWireApi::Responses);
        assert_eq!(models[2].context_window, Some(1000));
        assert_eq!(models[2].max_output_tokens, Some(120));
    }

    #[test]
    fn response_chain_removes_exact_replayed_prefix() {
        let previous_input =
            vec![json!({"role":"user","content":[{"type":"input_text","text":"hello"}]})];
        let previous_output = vec![json!({
            "id":"msg_1",
            "type":"message",
            "status":"completed",
            "role":"assistant",
            "content":[{"type":"output_text","text":"hi","annotations":[]}]
        })];
        let current = vec![
            previous_input[0].clone(),
            json!({"role":"assistant","content":[{"type":"output_text","text":"hi"}]}),
            json!({"role":"user","content":[{"type":"input_text","text":"next"}]}),
        ];
        assert_eq!(
            incremental_input(&current, &previous_input, &previous_output),
            Some(vec![current[2].clone()])
        );
    }

    #[test]
    fn response_chain_rejects_edited_assistant_history() {
        let previous_input = vec![json!({"role":"user","content":"hello"})];
        let previous_output = vec![json!({
            "type":"message",
            "role":"assistant",
            "content":[{"type":"output_text","text":"original"}]
        })];
        let current = vec![
            previous_input[0].clone(),
            json!({"role":"assistant","content":[{"type":"output_text","text":"edited"}]}),
        ];
        assert_eq!(
            incremental_input(&current, &previous_input, &previous_output),
            None
        );
    }

    #[test]
    fn converts_gemini_request_through_anthropic_intermediate() {
        let converted = gemini_request_to_anthropic(
            json!({
                "systemInstruction":{"parts":[{"text":"system"}]},
                "contents":[
                    {"role":"user","parts":[{"text":"hello"}]},
                    {"role":"model","parts":[{"functionCall":{"id":"call_1","name":"read","args":{"path":"a"}}}]}
                ],
                "generationConfig":{"maxOutputTokens":4096,"temperature":0.2},
                "tools":[{"functionDeclarations":[{"name":"read","parameters":{"type":"object"}}]}]
            }),
            "model-a",
            true,
        )
        .unwrap();
        assert_eq!(converted["model"], "model-a");
        assert_eq!(converted["stream"], true);
        assert_eq!(converted["max_tokens"], 4096);
        assert_eq!(converted["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(converted["tools"][0]["name"], "read");
    }

    #[test]
    fn preserves_gemini_media_without_turning_it_into_text_tokens() {
        let converted = gemini_request_to_anthropic(
            json!({
                "contents":[{"role":"user","parts":[
                    {"inlineData":{"mimeType":"application/pdf","data":"cGRm"}},
                    {"fileData":{"mimeType":"image/png","fileUri":"https://example.test/image.png"}}
                ]}]
            }),
            "model-a",
            false,
        )
        .unwrap();
        assert_eq!(converted["messages"][0]["content"][0]["type"], "document");
        assert_eq!(converted["messages"][0]["content"][1]["type"], "image");
        assert_eq!(
            converted["messages"][0]["content"][1]["source"]["type"],
            "url"
        );
    }

    #[test]
    fn chat_wire_rejects_documents_instead_of_tokenizing_base64() {
        let body = json!({
            "messages": [{"role":"user","content":[{
                "type":"document",
                "source":{"type":"base64","media_type":"application/pdf","data":"cGRm"}
            }]}]
        });
        assert!(validate_media_for_wire(&body, JoycodeWireApi::Chat).is_err());
        assert!(validate_media_for_wire(&body, JoycodeWireApi::Responses).is_ok());
    }

    #[test]
    fn extracts_gemini_model_from_uri() {
        assert_eq!(
            model_from_gemini_endpoint(
                "/gemini/v1beta/models/claude-sonnet:streamGenerateContent?alt=sse"
            ),
            Some("claude-sonnet".to_string())
        );
    }

    #[test]
    fn converts_anthropic_sse_to_incremental_gemini_sse() {
        let mut normalizer = AnthropicToGeminiSseNormalizer::default();
        let input = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"model-a\",\"usage\":{\"input_tokens\":5}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n"
        );
        let split = input.len() / 2;
        let mut output = normalizer.push_bytes(&input.as_bytes()[..split]);
        output.extend(normalizer.push_bytes(&input.as_bytes()[split..]));
        output.extend(normalizer.finish());
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\"text\":\"hello\""));
        assert!(output.contains("\"totalTokenCount\":7"));
    }
}
