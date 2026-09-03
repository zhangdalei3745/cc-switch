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
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;

pub const JOYCODE_INTERNAL_BASE_URL: &str = "http://joycode-api-saas.jd.com";
pub const JOYCODE_EXTERNAL_BASE_URL: &str = "https://api-ai.jd.com";
pub const JOYCODE_WEBSITE_URL: &str = "http://joycode.jd.com";
pub const JOYCODE_CLIENT: &str = "JoyCodeIDE";
pub const JOYCODE_CLIENT_VERSION: &str = "3.8.67";
pub const JOYCODE_EXTERNAL_BASE_URL_ENV: &str = "CC_SWITCH_JOYCODE_EXTERNAL_BASE_URL";
const MODEL_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const RESPONSE_SESSION_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const RESPONSE_SESSION_LIMIT: usize = 256;
const RUNTIME_TOKEN_WAIT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const RUNTIME_POLL_MIN_DELAY: Duration = Duration::from_secs(5);
const RUNTIME_POLL_MAX_DELAY: Duration = Duration::from_secs(30);
const RUNTIME_RECOVERY_LIMIT: usize = 3;
const RUNTIME_READY_STATUS: &str = "READY";
const RUNTIME_BYPASS_TOKEN: &str = "mt_ready_bypass";

// Kept byte-for-byte compatible with the current JoyCode IDE implementation.
const JOYCODE_GATEWAY_SIGNING_KEY: &[u8] = b"0691a3f0b37b4a85aeb63ad0fc7db3ed";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// Choose the newest catalog entry for a model family. JoyCode keeps the
/// version in the public model id, so natural numeric ordering is sufficient
/// for the current `Claude-*-x.y-hq` / `GPT-x.y` naming scheme.
fn latest_family_model(models: &[JoycodeModel], family: &str) -> Option<JoycodeModel> {
    let family = family.to_ascii_lowercase();
    let mut candidates: Vec<_> = models
        .iter()
        .filter(|model| model.id.to_ascii_lowercase().contains(&family))
        .cloned()
        .collect();
    candidates.sort_by(|left, right| right.id.cmp(&left.id));
    candidates.into_iter().next()
}

/// Build Claude Code's three stable role mappings from the live JoyCode
/// catalog. Some JoyCode accounts do not expose a Haiku model; in that case
/// auxiliary Haiku traffic deliberately falls back to Sonnet instead of an
/// unrelated model/protocol.
pub fn claude_role_models(
    models: &[JoycodeModel],
) -> Option<(JoycodeModel, JoycodeModel, JoycodeModel)> {
    let sonnet = latest_family_model(models, "sonnet")
        .or_else(|| latest_family_model(models, "opus"))
        .or_else(|| models.first().cloned())?;
    let haiku = latest_family_model(models, "haiku").unwrap_or_else(|| sonnet.clone());
    let opus = latest_family_model(models, "opus").unwrap_or_else(|| sonnet.clone());
    Some((haiku, sonnet, opus))
}

/// Prefer the newest native Responses model as Codex's default. The complete
/// catalog is still projected, so users can select Chat/Anthropic-backed
/// models and let the proxy bridge their wire protocols.
pub fn codex_default_model(models: &[JoycodeModel]) -> Option<JoycodeModel> {
    let mut responses: Vec<_> = models
        .iter()
        .filter(|model| model.wire_api == JoycodeWireApi::Responses)
        .cloned()
        .collect();
    responses.sort_by(|left, right| right.id.cmp(&left.id));
    responses
        .into_iter()
        .next()
        .or_else(|| models.first().cloned())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoycodeCredential {
    pub pt_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub master_base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_base_url: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RuntimeTokenKey {
    provider_id: String,
    network: JoycodeNetwork,
    account_scope: String,
    model: String,
    chat_id: String,
}

#[derive(Debug, Clone)]
struct CachedRuntimeToken {
    token: String,
    expire_at: Option<String>,
    remaining_request_count: Option<u64>,
}

#[derive(Debug, Default)]
struct RuntimeTokenSlot {
    active: Option<CachedRuntimeToken>,
}

#[derive(Debug, Clone)]
pub struct JoycodeRuntimeLease {
    key: RuntimeTokenKey,
    token: String,
}

impl JoycodeRuntimeLease {
    pub fn token(&self) -> &str {
        &self.token
    }
}

#[derive(Debug, Clone)]
struct RuntimeSnapshot {
    token: Option<String>,
    token_status: Option<String>,
    expire_at: Option<String>,
    next_poll_at: Option<String>,
    remaining_request_count: Option<u64>,
}

#[derive(Debug)]
struct RuntimeCallError {
    code: Option<String>,
    status: u16,
}

impl RuntimeCallError {
    fn normalized_code(&self) -> Option<String> {
        self.code
            .as_deref()
            .map(str::trim)
            .filter(|code| !code.is_empty())
            .map(|code| code.to_ascii_uppercase())
    }

    fn is_auth(&self) -> bool {
        matches!(
            self.normalized_code().as_deref(),
            Some("401" | "UNAUTHORIZED")
        ) || self.status == 401
    }

    fn is_bypass(&self) -> bool {
        matches!(
            self.normalized_code().as_deref(),
            Some("503001" | "MODEL_RUNTIME_JIMDB_UNAVAILABLE" | "MODEL_TOKEN_READY_BYPASS")
        )
    }

    fn should_requeue(&self) -> bool {
        matches!(
            self.normalized_code().as_deref(),
            Some(
                "400001"
                    | "400002"
                    | "400003"
                    | "400004"
                    | "MODEL_TOKEN_INVALID"
                    | "MODEL_TOKEN_MISSING"
                    | "MODEL_TOKEN_EXPIRED"
                    | "MODEL_TOKEN_CHAT_MISSING"
            )
        )
    }

    fn should_continue_queue(&self) -> bool {
        matches!(
            self.normalized_code().as_deref(),
            Some("409002" | "409003" | "MODEL_TOKEN_NOT_READY" | "MODEL_SESSION_PREPARE_CONFLICT")
        )
    }

    fn into_proxy_error(self) -> ProxyError {
        if self.is_auth() {
            return ProxyError::AuthError(
                "JoyCode credential expired while preparing the model runtime".to_string(),
            );
        }
        let code = self
            .normalized_code()
            .unwrap_or_else(|| "UNKNOWN".to_string());
        let status = if matches!(code.as_str(), "429001" | "MODEL_QUEUE_FULL") {
            429
        } else if self.status >= 400 {
            self.status
        } else {
            503
        };
        ProxyError::UpstreamError {
            status,
            body: Some(format!("JoyCode model runtime error: {code}")),
        }
    }
}

type RuntimeSlotMap = HashMap<RuntimeTokenKey, Arc<AsyncMutex<RuntimeTokenSlot>>>;
static RUNTIME_TOKEN_SLOTS: OnceLock<AsyncMutex<RuntimeSlotMap>> = OnceLock::new();

fn response_sessions() -> &'static Mutex<HashMap<ResponseSessionKey, ResponseSession>> {
    RESPONSE_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn catalogs() -> &'static RwLock<HashMap<String, CachedCatalog>> {
    MODEL_CATALOGS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn rejected_chat_cache_keys() -> &'static RwLock<HashSet<String>> {
    CHAT_CACHE_KEY_REJECTED.get_or_init(|| RwLock::new(HashSet::new()))
}

fn runtime_token_slots() -> &'static AsyncMutex<RuntimeSlotMap> {
    RUNTIME_TOKEN_SLOTS.get_or_init(|| AsyncMutex::new(HashMap::new()))
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

fn validate_joycode_base_url(
    candidate: &str,
    network: JoycodeNetwork,
) -> Result<String, ProxyError> {
    let parsed = url::Url::parse(candidate.trim())
        .map_err(|error| ProxyError::ConfigError(format!("Invalid JoyCode base URL: {error}")))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ProxyError::ConfigError(
            "JoyCode base URL must not contain user info".to_string(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ProxyError::ConfigError(
            "JoyCode base URL must not contain a query or fragment".to_string(),
        ));
    }
    if !matches!(parsed.path(), "" | "/") {
        return Err(ProxyError::ConfigError(
            "JoyCode base URL must not contain an API path".to_string(),
        ));
    }
    let host = parsed
        .host_str()
        .map(|host| host.to_ascii_lowercase())
        .ok_or_else(|| ProxyError::ConfigError("JoyCode base URL has no host".to_string()))?;
    let valid = match network {
        JoycodeNetwork::External => {
            parsed.scheme() == "https"
                && parsed.port_or_known_default() == Some(443)
                && matches!(host.as_str(), "api-ai.jd.com" | "joycode-api.jd.com")
        }
        JoycodeNetwork::Internal => {
            matches!(parsed.scheme(), "http" | "https")
                && host == "joycode-api-saas.jd.com"
                && matches!(parsed.port_or_known_default(), Some(80 | 443))
        }
    };
    if !valid {
        return Err(ProxyError::ConfigError(format!(
            "JoyCode {:?} base URL is not an approved JD endpoint",
            network
        )));
    }
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

pub fn provider_base_url(provider: &Provider) -> Result<String, ProxyError> {
    match provider_network(provider)? {
        JoycodeNetwork::Internal => Ok(JOYCODE_INTERNAL_BASE_URL.to_string()),
        JoycodeNetwork::External => {
            let configured = provider
                .meta
                .as_ref()
                .and_then(|meta| meta.joycode_external_base_url.as_deref())
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(ToString::to_string)
                .or_else(|| {
                    std::env::var(JOYCODE_EXTERNAL_BASE_URL_ENV)
                        .ok()
                        .map(|url| url.trim().to_string())
                        .filter(|url| !url.is_empty())
                })
                .unwrap_or_else(|| JOYCODE_EXTERNAL_BASE_URL.to_string());
            validate_joycode_base_url(&configured, JoycodeNetwork::External)
        }
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

fn user_info_endpoint(provider: &Provider) -> Result<String, ProxyError> {
    let base = provider_base_url(provider)?;
    Ok(match provider_network(provider)? {
        JoycodeNetwork::Internal => {
            format!("{}/api/saas/user/v2/userInfo", base.trim_end_matches('/'))
        }
        JoycodeNetwork::External => sign_gateway_url(&base, "joycode_userInfo"),
    })
}

fn model_runtime_endpoint(provider: &Provider, operation: &str) -> Result<String, ProxyError> {
    let base = provider_base_url(provider)?;
    let (path, function_id) = match operation {
        "prepare" => (
            "/api/saas/model-runtime/v1/models/prepare",
            "model_runtime_prepare",
        ),
        "runtime" => (
            "/api/saas/model-runtime/v1/models/runtime",
            "model_runtime_snapshot",
        ),
        "cancel" => (
            "/api/saas/model-runtime/v1/models/cancel",
            "model_runtime_cancel",
        ),
        _ => {
            return Err(ProxyError::Internal(format!(
                "Unsupported JoyCode runtime operation: {operation}"
            )))
        }
    };
    Ok(match provider_network(provider)? {
        JoycodeNetwork::Internal => format!("{}{path}", base.trim_end_matches('/')),
        JoycodeNetwork::External => sign_gateway_url(&base, function_id),
    })
}

pub fn auth_headers_with_context(
    pt_key: &str,
    login_type: Option<&str>,
    tenant: Option<&str>,
) -> Result<HeaderMap, ProxyError> {
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
        HeaderValue::from_str(
            login_type
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| login_type_for_pt_key(&pt_key)),
        )
        .map_err(|error| ProxyError::AuthError(format!("Invalid JoyCode loginType: {error}")))?,
    );
    if let Some(tenant) = tenant.map(str::trim).filter(|value| !value.is_empty()) {
        headers.insert(
            HeaderName::from_static("tenant"),
            HeaderValue::from_str(tenant).map_err(|error| {
                ProxyError::AuthError(format!("Invalid JoyCode tenant: {error}"))
            })?,
        );
    }
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

pub fn auth_headers_for_provider(
    provider: &Provider,
    pt_key: &str,
) -> Result<HeaderMap, ProxyError> {
    let meta = provider.meta.as_ref();
    auth_headers_with_context(
        pt_key,
        meta.and_then(|meta| meta.joycode_login_type.as_deref()),
        meta.and_then(|meta| meta.joycode_tenant.as_deref()),
    )
}

fn runtime_token_key(
    provider: &Provider,
    pt_key: &str,
    model: &str,
    chat_id: &str,
) -> Result<RuntimeTokenKey, ProxyError> {
    Ok(RuntimeTokenKey {
        provider_id: provider.id.clone(),
        network: provider_network(provider)?,
        account_scope: credential_fingerprint(pt_key),
        model: model.to_string(),
        chat_id: chat_id.to_string(),
    })
}

fn runtime_error_code(payload: &Value) -> Option<String> {
    let code_text = |value: &Value| {
        value
            .as_str()
            .map(ToString::to_string)
            .or_else(|| value.as_i64().map(|code| code.to_string()))
    };
    let is_success = |code: &str| {
        matches!(code.trim().to_ascii_uppercase().as_str(), "SUCCESS" | "OK")
            || code
                .trim()
                .parse::<i64>()
                .is_ok_and(|code| matches!(code, 0 | 200))
    };

    // The current service returns `{ code: 0, bizCode: "SUCCESS" }` on a
    // successful prepare. Match the official client: top-level `code` is the
    // envelope authority, and bizCode only refines a failing envelope.
    if let Some(code) = payload.get("code").and_then(&code_text) {
        if is_success(&code) {
            return None;
        }
        return payload
            .get("bizCode")
            .and_then(&code_text)
            .filter(|biz_code| !is_success(biz_code))
            .or(Some(code));
    }

    for pointer in [
        "/biz_code",
        "/error/code",
        "/error/bizCode",
        "/data/bizCode",
        "/data/error/code",
        "/bizCode",
    ] {
        let Some(value) = payload.pointer(pointer) else {
            continue;
        };
        let code = code_text(value);
        if code.as_deref().is_some_and(|code| !is_success(code)) {
            return code;
        }
    }
    None
}

fn parse_runtime_snapshot(
    payload: &Value,
    status: u16,
) -> Result<RuntimeSnapshot, RuntimeCallError> {
    if status >= 400 || runtime_error_code(payload).is_some() {
        return Err(RuntimeCallError {
            code: runtime_error_code(payload),
            status,
        });
    }
    let data = payload.get("data").unwrap_or(payload);
    let token = data
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(ToString::to_string);
    let token_status = data
        .get("tokenStatus")
        .or_else(|| data.get("token_status"))
        .and_then(Value::as_str)
        .map(|status| status.trim().to_ascii_uppercase());
    if token.is_none() && token_status.is_none() {
        return Err(RuntimeCallError { code: None, status });
    }
    Ok(RuntimeSnapshot {
        token,
        token_status,
        expire_at: data
            .get("expireAt")
            .or_else(|| data.get("expire_at"))
            .and_then(Value::as_str)
            .map(ToString::to_string),
        next_poll_at: data
            .get("nextPollAt")
            .or_else(|| data.get("next_poll_at"))
            .and_then(Value::as_str)
            .map(ToString::to_string),
        remaining_request_count: data
            .get("remainingRequestCount")
            .or_else(|| data.get("remaining_request_count"))
            .and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
            }),
    })
}

async fn call_runtime_prepare(
    provider: &Provider,
    pt_key: &str,
    payload: &Value,
) -> Result<RuntimeSnapshot, RuntimeCallError> {
    let endpoint = model_runtime_endpoint(provider, "prepare").map_err(|_| RuntimeCallError {
        code: Some("INVALID_RUNTIME_ENDPOINT".to_string()),
        status: 500,
    })?;
    let headers = auth_headers_for_provider(provider, pt_key).map_err(|_| RuntimeCallError {
        code: Some("401".to_string()),
        status: 401,
    })?;
    let response = crate::proxy::http_client::get()
        .post(endpoint)
        .headers(headers)
        .timeout(Duration::from_secs(30))
        .json(payload)
        .send()
        .await
        .map_err(|_| RuntimeCallError {
            code: Some("MODEL_RUNTIME_NETWORK_ERROR".to_string()),
            status: 503,
        })?;
    let status = response.status().as_u16();
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| RuntimeCallError { code: None, status })?;
    parse_runtime_snapshot(&payload, status)
}

fn runtime_token_is_expired(expire_at: Option<&str>) -> bool {
    let Some(expire_at) = expire_at.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    if let Ok(timestamp) = expire_at.parse::<i64>() {
        let timestamp_ms = if timestamp < 10_000_000_000 {
            timestamp.saturating_mul(1000)
        } else {
            timestamp
        };
        return chrono::Utc::now().timestamp_millis() >= timestamp_ms;
    }
    chrono::DateTime::parse_from_rfc3339(expire_at)
        .map(|timestamp| chrono::Utc::now() >= timestamp.with_timezone(&chrono::Utc))
        .unwrap_or(false)
}

fn runtime_poll_delay(next_poll_at: Option<&str>) -> Duration {
    let delay = next_poll_at
        .and_then(|next_poll_at| chrono::DateTime::parse_from_rfc3339(next_poll_at).ok())
        .and_then(|next_poll_at| {
            (next_poll_at.with_timezone(&chrono::Utc) - chrono::Utc::now())
                .to_std()
                .ok()
        })
        .unwrap_or(RUNTIME_POLL_MIN_DELAY);
    delay.clamp(RUNTIME_POLL_MIN_DELAY, RUNTIME_POLL_MAX_DELAY)
}

fn consume_cached_runtime_token(active: &mut Option<CachedRuntimeToken>) -> Option<String> {
    let cached = active.as_mut()?;
    if runtime_token_is_expired(cached.expire_at.as_deref())
        || cached.remaining_request_count == Some(0)
    {
        *active = None;
        return None;
    }
    if let Some(remaining) = cached.remaining_request_count.as_mut() {
        *remaining = remaining.saturating_sub(1);
    }
    Some(cached.token.clone())
}

/// Acquire a READY model-runtime token before calling any JoyCode inference
/// protocol. Calls for the same account/model/chat are single-flighted while
/// different conversations remain concurrent.
pub async fn acquire_runtime_token(
    provider: &Provider,
    pt_key: &str,
    model: &str,
    chat_id: &str,
) -> Result<Option<JoycodeRuntimeLease>, ProxyError> {
    let key = runtime_token_key(provider, pt_key, model, chat_id)?;
    let slot = {
        let mut slots = runtime_token_slots().lock().await;
        slots
            .entry(key.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(RuntimeTokenSlot::default())))
            .clone()
    };
    let mut slot = slot.lock().await;
    if let Some(token) = consume_cached_runtime_token(&mut slot.active) {
        return Ok(Some(JoycodeRuntimeLease { key, token }));
    }

    let started_at = Instant::now();
    let mut polling_token: Option<String> = None;
    let mut recovery_count = 0usize;
    loop {
        if started_at.elapsed() >= RUNTIME_TOKEN_WAIT_TIMEOUT {
            return Err(ProxyError::Timeout(format!(
                "JoyCode model '{model}' queue did not become ready within {} seconds",
                RUNTIME_TOKEN_WAIT_TIMEOUT.as_secs()
            )));
        }
        let payload = if let Some(token) = polling_token.as_deref() {
            json!({"token": token})
        } else {
            json!({
                "model": model,
                "chatId": chat_id,
                "stream": true,
                "client": JOYCODE_CLIENT,
                "clientVersion": JOYCODE_CLIENT_VERSION,
                "language": "UNKNOWN",
                "orgFullName": ""
            })
        };
        let snapshot = match call_runtime_prepare(provider, pt_key, &payload).await {
            Ok(snapshot) => {
                recovery_count = 0;
                snapshot
            }
            Err(error) if error.is_bypass() => {
                log::warn!("[JoyCode] model runtime explicitly requested queue bypass");
                return Ok(None);
            }
            Err(error) if error.should_requeue() => {
                recovery_count = recovery_count.saturating_add(1);
                if recovery_count > RUNTIME_RECOVERY_LIMIT {
                    return Err(error.into_proxy_error());
                }
                polling_token = None;
                tokio::time::sleep(Duration::from_millis(
                    500u64.saturating_mul(1u64 << (recovery_count - 1)),
                ))
                .await;
                continue;
            }
            Err(error) if error.should_continue_queue() => {
                recovery_count = recovery_count.saturating_add(1);
                if recovery_count > RUNTIME_RECOVERY_LIMIT {
                    return Err(error.into_proxy_error());
                }
                tokio::time::sleep(RUNTIME_POLL_MIN_DELAY).await;
                continue;
            }
            Err(error) => return Err(error.into_proxy_error()),
        };

        let token = snapshot.token.as_deref().map(str::trim).unwrap_or_default();
        if token.to_ascii_lowercase().starts_with(RUNTIME_BYPASS_TOKEN) {
            log::warn!("[JoyCode] model runtime returned an explicit bypass token");
            return Ok(None);
        }
        if snapshot.token_status.as_deref() == Some(RUNTIME_READY_STATUS) {
            if token.is_empty() || snapshot.remaining_request_count == Some(0) {
                recovery_count = recovery_count.saturating_add(1);
                if recovery_count > RUNTIME_RECOVERY_LIMIT {
                    return Err(ProxyError::UpstreamError {
                        status: 503,
                        body: Some(
                            "JoyCode model runtime returned an unusable READY token".to_string(),
                        ),
                    });
                }
                polling_token = None;
                continue;
            }
            let mut cached = CachedRuntimeToken {
                token: token.to_string(),
                expire_at: snapshot.expire_at,
                remaining_request_count: snapshot.remaining_request_count,
            };
            if let Some(remaining) = cached.remaining_request_count.as_mut() {
                *remaining = remaining.saturating_sub(1);
            }
            slot.active = Some(cached);
            return Ok(Some(JoycodeRuntimeLease {
                key,
                token: token.to_string(),
            }));
        }
        if !token.is_empty() {
            polling_token = Some(token.to_string());
        }
        tokio::time::sleep(runtime_poll_delay(snapshot.next_poll_at.as_deref())).await;
    }
}

pub async fn invalidate_runtime_token(lease: &JoycodeRuntimeLease) {
    let slot = {
        let slots = runtime_token_slots().lock().await;
        slots.get(&lease.key).cloned()
    };
    if let Some(slot) = slot {
        let mut slot = slot.lock().await;
        if slot
            .active
            .as_ref()
            .is_some_and(|active| active.token == lease.token)
        {
            slot.active = None;
        }
    }
}

fn runtime_token_error_code_text(body: Option<&str>) -> Option<&'static str> {
    let body = body?;
    let upper = body.to_ascii_uppercase();
    [
        "MODEL_TOKEN_INVALID",
        "MODEL_TOKEN_MISSING",
        "MODEL_TOKEN_EXPIRED",
        "MODEL_TOKEN_CHAT_MISSING",
        "MODEL_TOKEN_NOT_READY",
        "400001",
        "400002",
        "400003",
        "400004",
        "409002",
        "409003",
    ]
    .iter()
    .find(|code| upper.contains(**code))
    .copied()
    .or_else(|| {
        [
            "MODEL TOKEN IS INVALID",
            "TOKEN ALREADY EXPIRED",
            "TOKEN NOT FOUND OR INVALID",
        ]
        .iter()
        .find(|message| upper.contains(**message))
        .map(|_| "MODEL_TOKEN_INVALID")
    })
}

pub fn is_runtime_token_error_text(body: Option<&str>) -> bool {
    runtime_token_error_code_text(body).is_some()
}

fn response_text(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

async fn validate_credential_at(
    endpoint: &str,
    credential: &JoycodeCredential,
) -> Result<JoycodeCredential, String> {
    let headers = auth_headers_with_context(
        &credential.pt_key,
        credential.login_type.as_deref(),
        credential.tenant.as_deref(),
    )
    .map_err(|error| error.to_string())?;
    let response = crate::proxy::http_client::get()
        .post(endpoint)
        .headers(headers)
        .timeout(Duration::from_secs(20))
        .json(&json!({}))
        .send()
        .await
        .map_err(|error| format!("JoyCode userInfo request failed: {error}"))?;
    let status = response.status();
    let payload: Value = response
        .json()
        .await
        .map_err(|error| format!("读取 JoyCode userInfo 响应失败：{error}"))?;
    let code = payload
        .get("code")
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()));
    if code == Some(401) {
        return Err("JoyCode 认证失败：ptKey 已失效，请重新登录 JoyCode 后再导入".to_string());
    }
    if !status.is_success() || !matches!(code, None | Some(0) | Some(200)) {
        let message = payload
            .get("msg")
            .or_else(|| payload.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return Err(format!("JoyCode userInfo 验证失败：{message}"));
    }
    let data = payload.get("data").unwrap_or(&payload);
    if data.get("userId").is_none_or(Value::is_null) {
        return Err("JoyCode userInfo 未返回有效用户，凭据不可用".to_string());
    }
    Ok(JoycodeCredential {
        pt_key: response_text(data, &["ptKey", "pt_key"])
            .map(|value| normalize_pt_key(&value))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| normalize_pt_key(&credential.pt_key)),
        login_type: response_text(data, &["loginType"]).or_else(|| credential.login_type.clone()),
        tenant: response_text(data, &["tenant"]).or_else(|| credential.tenant.clone()),
        master_base_url: response_text(data, &["masterBaseUrl", "base_url"])
            .or_else(|| credential.master_base_url.clone()),
        color_base_url: response_text(data, &["colorBaseUrl"])
            .or_else(|| credential.color_base_url.clone()),
    })
}

pub async fn validate_credential(
    provider: &Provider,
    credential: &JoycodeCredential,
) -> Result<JoycodeCredential, String> {
    let endpoint = user_info_endpoint(provider).map_err(|error| error.to_string())?;
    if credential
        .login_type
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        return validate_credential_at(&endpoint, credential).await;
    }

    let login_types: &[&str] = if normalize_pt_key(&credential.pt_key).starts_with("BJ.") {
        &["ERP", "PIN_JD_CLOUD", "N_PIN_PC"]
    } else {
        &["PIN_JD_CLOUD", "N_PIN_PC", "ERP"]
    };
    let mut last_error = None;
    for login_type in login_types {
        let mut candidate = credential.clone();
        candidate.login_type = Some((*login_type).to_string());
        match validate_credential_at(&endpoint, &candidate).await {
            Ok(validated) => return Ok(validated),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| "JoyCode 认证类型自动检测失败".to_string()))
}

pub async fn validate_discovered_credential(
    credential: &JoycodeCredential,
) -> Result<JoycodeCredential, String> {
    let mut endpoints = Vec::<String>::new();
    if let Some(base) = credential
        .color_base_url
        .as_deref()
        .and_then(|base| validate_joycode_base_url(base, JoycodeNetwork::External).ok())
    {
        endpoints.push(sign_gateway_url(&base, "joycode_userInfo"));
    }
    if let Some(raw_base) = credential.master_base_url.as_deref() {
        if let Ok(base) = validate_joycode_base_url(raw_base, JoycodeNetwork::Internal) {
            endpoints.push(format!("{base}/api/saas/user/v2/userInfo"));
        } else if let Ok(base) = validate_joycode_base_url(raw_base, JoycodeNetwork::External) {
            endpoints.push(sign_gateway_url(&base, "joycode_userInfo"));
        }
    }
    if endpoints.is_empty() {
        endpoints.push(format!(
            "{JOYCODE_INTERNAL_BASE_URL}/api/saas/user/v2/userInfo"
        ));
        endpoints.push(sign_gateway_url(
            JOYCODE_EXTERNAL_BASE_URL,
            "joycode_userInfo",
        ));
    }
    let mut last_error = None;
    for endpoint in endpoints {
        match validate_credential_at(&endpoint, credential).await {
            Ok(credential) => return Ok(credential),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| "未找到可用的 JoyCode 认证地址".to_string()))
}

fn adaptive_effort_from_budget(budget_tokens: Option<u64>) -> &'static str {
    match budget_tokens.unwrap_or(16_384) {
        0..=2_048 => "low",
        2_049..=8_192 => "medium",
        8_193..=16_384 => "high",
        _ => "max",
    }
}

fn normalize_anthropic_thinking(body: &mut Value) {
    let Some(model) = body
        .get("model")
        .and_then(Value::as_str)
        .map(ToString::to_string)
    else {
        return;
    };
    if !crate::proxy::thinking_optimizer::uses_adaptive_thinking(&model) {
        return;
    }

    let thinking_type = body
        .pointer("/thinking/type")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    if !matches!(thinking_type.as_deref(), Some("enabled" | "adaptive")) {
        return;
    }

    let budget_tokens = body
        .pointer("/thinking/budget_tokens")
        .and_then(Value::as_u64);
    let was_legacy = thinking_type.as_deref() == Some("enabled");
    body["thinking"] = json!({"type": "adaptive"});

    let existing_effort = body
        .pointer("/output_config/effort")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let effort = existing_effort
        .as_deref()
        .unwrap_or_else(|| adaptive_effort_from_budget(budget_tokens));
    if body
        .get("output_config")
        .and_then(Value::as_object)
        .is_none()
    {
        body["output_config"] = json!({});
    }
    if body.pointer("/output_config/effort").is_none() {
        body["output_config"]["effort"] = Value::String(effort.to_string());
    }

    if was_legacy || budget_tokens.is_some() {
        log::debug!(
            "[JoyCode] normalized adaptive thinking: model={model}, previous_type={}, effort={effort}, had_budget_tokens={}",
            thinking_type.as_deref().unwrap_or("unknown"),
            budget_tokens.is_some()
        );
    }
}

fn schema_required_fields(schema: &Value) -> Vec<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect()
}

fn merge_schema_property(properties: &mut Map<String, Value>, name: &str, schema: &Value) {
    let Some(existing) = properties.get_mut(name) else {
        properties.insert(name.to_string(), schema.clone());
        return;
    };
    if existing == schema {
        return;
    }

    // A property can have different shapes in separate root union branches. Keep
    // those alternatives below the object root: JoyCode rejects combinators only
    // at `input_schema` itself, while nested property schemas remain supported.
    let mut variants = existing
        .get("anyOf")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| vec![existing.clone()]);
    if !variants.contains(schema) {
        variants.push(schema.clone());
    }
    *existing = json!({ "anyOf": variants });
}

fn merge_object_schema_branches(
    root: &mut Map<String, Value>,
    branches: &[Value],
    require_all: bool,
) {
    let object_branches = branches
        .iter()
        .filter_map(Value::as_object)
        .collect::<Vec<_>>();
    if object_branches.is_empty() {
        return;
    }

    let mut properties = root
        .remove("properties")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let root_required = schema_required_fields(&Value::Object(root.clone()));
    root.remove("required");
    let mut branch_required: Option<Vec<String>> = None;

    for branch in object_branches {
        if let Some(branch_properties) = branch.get("properties").and_then(Value::as_object) {
            for (name, schema) in branch_properties {
                merge_schema_property(&mut properties, name, schema);
            }
        }

        let required = schema_required_fields(&Value::Object(branch.clone()));
        branch_required = Some(match branch_required {
            None => required,
            Some(existing) if require_all => {
                let mut merged = existing;
                for field in required {
                    if !merged.contains(&field) {
                        merged.push(field);
                    }
                }
                merged
            }
            Some(existing) => existing
                .into_iter()
                .filter(|field| required.contains(field))
                .collect(),
        });
    }

    let mut required = root_required;
    for field in branch_required.unwrap_or_default() {
        if !required.contains(&field) {
            required.push(field);
        }
    }
    root.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        root.insert(
            "required".to_string(),
            Value::Array(required.into_iter().map(Value::String).collect()),
        );
    }
}

fn sanitize_codex_anthropic_input_schema(schema: &mut Value) {
    let Some(root) = schema.as_object_mut() else {
        *schema = json!({
            "type": "object",
            "properties": {},
            "additionalProperties": true
        });
        return;
    };

    // JoyCode's Anthropic adapter rejects root combinators even though they are
    // valid JSON Schema. Flatten them without changing the tool argument shape:
    // - oneOf/anyOf: merge properties and keep only fields required by every arm;
    // - allOf: merge properties and require the union of every arm's requirements.
    // Nested combinators are left untouched because the adapter error is scoped to
    // the input_schema root and they preserve conflicting property alternatives.
    for (key, require_all) in [("oneOf", false), ("anyOf", false), ("allOf", true)] {
        let branches = root.remove(key).and_then(|value| value.as_array().cloned());
        if let Some(branches) = branches {
            merge_object_schema_branches(root, &branches, require_all);
        }
    }

    root.insert("type".to_string(), json!("object"));
    root.entry("properties".to_string())
        .or_insert_with(|| json!({}));
}

/// Move Codex Responses Lite `additional_tools` carriers into the top-level
/// tool list before the shared Responses→Anthropic conversion builds its tool
/// context. JoyCode's native Responses endpoint understands the carrier, but an
/// Anthropic request has no equivalent input item and would otherwise lose the
/// dynamically loaded tools.
///
/// This helper is called only for JoyCode Responses→Anthropic routing. Native
/// Claude/Claude Desktop requests and JoyCode native Responses requests do not
/// pass through it.
pub fn promote_codex_anthropic_additional_tools(body: &mut Value) -> bool {
    let Some(input) = body.get("input").and_then(Value::as_array) else {
        return false;
    };
    if !input.iter().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("additional_tools")
            && item.get("role").and_then(Value::as_str) == Some("developer")
    }) {
        return false;
    }

    let mut tools = body
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut seen = tools
        .iter()
        .map(codex_tool_dedup_key)
        .collect::<HashSet<_>>();
    let mut retained_input = Vec::with_capacity(input.len());

    for item in input {
        let is_valid_carrier = item.get("type").and_then(Value::as_str) == Some("additional_tools")
            && item.get("role").and_then(Value::as_str) == Some("developer");
        if !is_valid_carrier {
            retained_input.push(item.clone());
            continue;
        }
        if let Some(additional_tools) = item.get("tools").and_then(Value::as_array) {
            for tool in additional_tools {
                if seen.insert(codex_tool_dedup_key(tool)) {
                    tools.push(tool.clone());
                }
            }
        }
    }

    let Some(object) = body.as_object_mut() else {
        return false;
    };
    object.insert("input".to_string(), Value::Array(retained_input));
    if !tools.is_empty() {
        object.insert("tools".to_string(), Value::Array(tools));
    }
    true
}

fn codex_tool_dedup_key(tool: &Value) -> String {
    let tool_type = tool.get("type").and_then(Value::as_str).unwrap_or("");
    let identity = tool
        .get("name")
        .or_else(|| tool.get("server_label"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if !tool_type.is_empty() && !identity.is_empty() {
        format!("{tool_type}\u{0}{identity}")
    } else {
        serde_json::to_string(tool).unwrap_or_default()
    }
}

pub fn sanitize_codex_anthropic_tools(body: &mut Value) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };

    // OpenAI Responses tools can carry metadata and root JSON Schema constructs
    // accepted by Codex but rejected by JoyCode's Anthropic adapter. Apply this
    // only after Responses→Anthropic conversion; native Anthropic clients retain
    // their original tool definitions.
    for tool in tools {
        let Some(object) = tool.as_object_mut() else {
            continue;
        };
        object.remove("strict");
        if let Some(schema) = object.get_mut("input_schema") {
            sanitize_codex_anthropic_input_schema(schema);
        }
    }
}

pub fn decorate_body(body: &mut Value, wire_api: JoycodeWireApi) {
    if wire_api == JoycodeWireApi::Anthropic {
        // Claude Code 2.1.235 sends the newer context-management beta field.
        // JoyCode's current Anthropic adapter rejects it with HTTP 200 plus a
        // nested 400 error, so omit the optional server-side compaction hint.
        // The full message history remains in `messages` and is not discarded.
        if let Some(object) = body.as_object_mut() {
            object.remove("context_management");
        }
        // JoyCode exposes current Claude models through Anthropic Messages, but
        // rejects the legacy fixed-budget form with an HTTP 200 business error.
        // Normalize only JoyCode's Anthropic requests so other providers retain
        // their existing thinking semantics.
        normalize_anthropic_thinking(body);
    }
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
    drop_done: bool,
}

impl JoycodeResponsesSseNormalizer {
    pub fn with_session_context(context: Option<JoycodeResponseSessionContext>) -> Self {
        Self {
            session_context: context,
            ..Self::default()
        }
    }

    fn for_anthropic() -> Self {
        Self {
            // Anthropic streams terminate with message_stop and do not use the
            // OpenAI-style [DONE] sentinel emitted by JoyCode's outer wrapper.
            drop_done: true,
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
                    let data = inner.trim_start_matches("data:").trim_start();
                    if self.drop_done && data == "[DONE]" {
                        self.pending_event.take();
                        return;
                    }
                    if let Some(event) = self.pending_event.take() {
                        output.push_str(&event);
                        output.push('\n');
                    }
                    self.observe_data(data);
                    output.push_str(inner);
                    output.push_str("\n\n");
                    return;
                }
            }
        }
        if self.drop_done
            && trimmed.lines().all(|line| {
                crate::proxy::sse::strip_sse_field(line.trim_end_matches('\r'), "data")
                    .is_some_and(|data| data.trim() == "[DONE]")
            })
        {
            self.pending_event.take();
            return;
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

fn normalize_joycode_sse_response(
    response: ProxyResponse,
    normalizer: JoycodeResponsesSseNormalizer,
) -> ProxyResponse {
    let status = response.status();
    let mut headers = response.headers().clone();
    headers.remove(http::header::CONTENT_LENGTH);
    let stream = Box::pin(response.bytes_stream());
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
    ProxyResponse::streamed(status, headers, normalized)
}

fn nested_error_message(error: &Value) -> Option<String> {
    let cause = error.get("cause");
    if let Some(cause) = cause.and_then(Value::as_object) {
        if let Some(message) = cause.get("message").and_then(Value::as_str) {
            return Some(message.to_string());
        }
    }
    if let Some(cause) = cause.and_then(Value::as_str) {
        if let Ok(value) = serde_json::from_str::<Value>(cause) {
            if let Some(message) = value.get("message").and_then(Value::as_str) {
                return Some(message.to_string());
            }
        }
        if !cause.trim().is_empty() {
            return Some(cause.to_string());
        }
    }
    error
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .map(ToString::to_string)
}

pub(crate) fn anthropic_business_error(payload: &Value) -> Option<ProxyError> {
    let error = payload.get("error").filter(|error| !error.is_null())?;
    let status = error
        .get("code")
        .and_then(|code| {
            code.as_u64().or_else(|| {
                code.as_str()
                    .and_then(|code| code.trim().parse::<u64>().ok())
            })
        })
        .filter(|status| (400..=599).contains(status))
        .unwrap_or(502) as u16;
    let message = nested_error_message(error)
        .unwrap_or_else(|| "JoyCode Anthropic upstream returned a business error".to_string());
    Some(ProxyError::UpstreamError {
        status,
        body: Some(message),
    })
}

pub(crate) fn inspect_anthropic_sse_start(block: &str) -> Option<Result<(), ProxyError>> {
    let data = block
        .lines()
        .filter_map(|line| crate::proxy::sse::strip_sse_field(line, "data"))
        .collect::<Vec<_>>()
        .join("\n");
    if data.trim().is_empty() || data.trim() == "[DONE]" {
        return None;
    }
    let payload: Value = serde_json::from_str(&data).ok()?;
    match anthropic_business_error(&payload) {
        Some(error) => Some(Err(error)),
        None => Some(Ok(())),
    }
}

/// JoyCode's Anthropic adapter may wrap every already-serialized SSE line in
/// another `data:` field. Unwrap that outer layer so Anthropic clients receive
/// named `event:` fields instead of treating them as opaque data. Buffered
/// responses are also inspected because JoyCode reports some model failures as
/// HTTP 200 JSON envelopes.
pub async fn normalize_anthropic_response(
    response: ProxyResponse,
    is_streaming: bool,
) -> Result<ProxyResponse, ProxyError> {
    if !is_streaming || response.is_json() {
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.bytes_with_limit(MAX_RESPONSE_BODY_BYTES).await?;
        let decoded =
            crate::proxy::content_encoding::get_content_encoding(&headers).and_then(|encoding| {
                crate::proxy::content_encoding::decompress_body_with_limit(
                    &encoding,
                    &bytes,
                    MAX_RESPONSE_BODY_BYTES,
                )
                .ok()
                .flatten()
            });
        let inspect_bytes = decoded.as_deref().unwrap_or(&bytes);
        if let Ok(payload) = serde_json::from_slice::<Value>(inspect_bytes) {
            if let Some(error) = anthropic_business_error(&payload) {
                return Err(error);
            }
        }
        return Ok(ProxyResponse::buffered(status, headers, bytes));
    }
    if !response.is_sse() {
        return Ok(response);
    }
    Ok(normalize_joycode_sse_response(
        response,
        JoycodeResponsesSseNormalizer::for_anthropic(),
    ))
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

    let normalizer = JoycodeResponsesSseNormalizer::with_session_context(context);
    Ok(normalize_joycode_sse_response(response, normalizer))
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
    let decoded =
        crate::proxy::content_encoding::get_content_encoding(&headers).and_then(|encoding| {
            crate::proxy::content_encoding::decompress_body_with_limit(
                &encoding,
                &bytes,
                MAX_RESPONSE_BODY_BYTES,
            )
            .ok()
            .flatten()
        });
    let inspect_bytes = decoded.as_deref().unwrap_or(&bytes);
    if let Ok(payload) = serde_json::from_slice::<Value>(inspect_bytes) {
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
        let body = String::from_utf8_lossy(inspect_bytes);
        if let Some(code) = runtime_token_error_code_text(Some(&body)) {
            return Err(ProxyError::UpstreamError {
                status: 409,
                body: Some(format!("JoyCode model runtime error: {code}")),
            });
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
    let headers = auth_headers_for_provider(provider, pt_key).map_err(|error| error.to_string())?;
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
        parse_model_catalog(&payload)?;
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

fn cached_default_model(
    scope: &str,
    preferred_wire_api: Option<JoycodeWireApi>,
) -> Option<JoycodeModel> {
    let catalogs = catalogs().read().ok()?;
    let catalog = catalogs.get(scope)?;
    if catalog.loaded_at.elapsed() > MODEL_CACHE_TTL {
        return None;
    }
    catalog
        .models
        .values()
        .filter(|model| preferred_wire_api.is_none_or(|wire| model.wire_api == wire))
        .min_by(|left, right| left.id.cmp(&right.id))
        .cloned()
        .or_else(|| {
            catalog
                .models
                .values()
                .min_by(|left, right| left.id.cmp(&right.id))
                .cloned()
        })
}

pub async fn resolve_model(
    provider: &Provider,
    model_id: &str,
    pt_key: &str,
    preferred_wire_api: Option<JoycodeWireApi>,
) -> Result<JoycodeModel, ProxyError> {
    let use_catalog_default = matches!(model_id.trim(), "" | "joycode" | "custom");
    let scope = catalog_scope(provider, pt_key);
    if use_catalog_default {
        if let Some(model) = cached_default_model(&scope, preferred_wire_api) {
            return Ok(model);
        }
    } else {
        if let Some(model) = cached_model(&scope, model_id) {
            return Ok(model);
        }
    }
    let models = fetch_models(provider, pt_key)
        .await
        .map_err(ProxyError::ConfigError)?;
    if use_catalog_default {
        let selected = preferred_wire_api
            .and_then(|preferred| {
                models
                    .iter()
                    .find(|model| model.wire_api == preferred)
                    .cloned()
            })
            .or_else(|| models.into_iter().next());
        return selected.ok_or_else(|| {
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

/// Keep an explicitly configured credential stable. Local JoyCode state is
/// imported only through the explicit UI action; silently comparing token
/// timestamps is unsafe because current 32-byte credentials carry no timestamp.
pub fn resolve_latest_pt_key(configured: &str) -> String {
    let configured = normalize_pt_key(configured);
    if configured.is_empty() {
        discover_latest_pt_key().unwrap_or_default()
    } else {
        configured
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

fn credential_from_state(value: &Value) -> Option<JoycodeCredential> {
    let login = value.pointer("/jdhLoginInfo")?;
    let pt_key = login
        .get("ptKey")
        .or_else(|| login.get("pt_key"))?
        .as_str()
        .map(normalize_pt_key)
        .filter(|value| !value.is_empty())?;
    let text = |key: &str| {
        login
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    Some(JoycodeCredential {
        pt_key,
        login_type: text("loginType"),
        tenant: text("tenant"),
        master_base_url: text("masterBaseUrl").or_else(|| text("base_url")),
        color_base_url: text("colorBaseUrl"),
    })
}

/// Discover credentials from the current JoyCode IDE first, then legacy
/// JoyCoder/JetBrains stores. Databases are always opened read-only.
pub fn discover_joycode_credentials() -> Vec<JoycodeCredential> {
    let mut candidates = Vec::<JoycodeCredential>::new();
    let mut add = |credential: JoycodeCredential| {
        if !candidates
            .iter()
            .any(|candidate| candidate.pt_key == credential.pt_key)
        {
            candidates.push(credential);
        }
    };
    let mut databases = Vec::new();
    if let Some(home) = dirs::home_dir() {
        databases
            .push(home.join("Library/Application Support/JoyCode/User/globalStorage/state.vscdb"));
        databases
            .push(home.join("Library/Application Support/Code/User/globalStorage/state.vscdb"));
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
        for storage_key in ["JoyCode.joycoder-editor", "JoyCoder.joycoder-fe"] {
            let Ok(value) = connection.query_row(
                "SELECT value FROM ItemTable WHERE key = ?1",
                [storage_key],
                |row| row.get::<_, String>(0),
            ) else {
                continue;
            };
            if let Some(credential) = serde_json::from_str::<Value>(&value)
                .ok()
                .as_ref()
                .and_then(credential_from_state)
            {
                add(credential);
            }
        }
    }
    for path in collect_jetbrains_paths() {
        if let Some(value) = jetbrains_pt_key(&path) {
            add(JoycodeCredential {
                pt_key: normalize_pt_key(&value),
                login_type: Some(login_type_for_pt_key(&value).to_string()),
                tenant: None,
                master_base_url: None,
                color_base_url: None,
            });
        }
    }
    candidates
}

pub fn discover_latest_pt_key() -> Option<String> {
    discover_joycode_credentials()
        .into_iter()
        .next()
        .map(|credential| credential.pt_key)
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
    fn parses_current_joycode_login_state() {
        let credential = credential_from_state(&json!({
            "jdhLoginInfo": {
                "pt_key": "current-token",
                "ptKey": "current-token",
                "loginType": "PIN_JD_CLOUD",
                "tenant": "JD",
                "masterBaseUrl": "http://internal.example",
                "colorBaseUrl": "https://gateway.example"
            }
        }))
        .unwrap();
        assert_eq!(credential.pt_key, "current-token");
        assert_eq!(credential.login_type.as_deref(), Some("PIN_JD_CLOUD"));
        assert_eq!(credential.tenant.as_deref(), Some("JD"));
        assert_eq!(
            credential.color_base_url.as_deref(),
            Some("https://gateway.example")
        );
    }

    #[test]
    fn explicit_credential_is_not_replaced_by_local_state() {
        assert_eq!(resolve_latest_pt_key("BJ.manual"), "BJ.manual");
    }

    #[test]
    fn auth_headers_preserve_validated_login_context() {
        let headers =
            auth_headers_with_context("current-token", Some("PIN_JD_CLOUD"), Some("JD")).unwrap();
        assert_eq!(headers.get("ptkey").unwrap(), "current-token");
        assert_eq!(headers.get("logintype").unwrap(), "PIN_JD_CLOUD");
        assert_eq!(headers.get("tenant").unwrap(), "JD");
    }

    #[test]
    fn login_type_fallback_keeps_legacy_erp_detection() {
        assert_eq!(login_type_for_pt_key("BJ.legacy-token"), "ERP");
        assert_eq!(login_type_for_pt_key("current-token"), "N_PIN_PC");
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
    fn joycode_base_url_requires_an_approved_jd_host() {
        assert_eq!(
            validate_joycode_base_url("https://api-ai.jd.com/", JoycodeNetwork::External).unwrap(),
            "https://api-ai.jd.com"
        );
        assert!(validate_joycode_base_url(
            "https://api-ai.jd.com.attacker.example",
            JoycodeNetwork::External
        )
        .is_err());
        assert!(validate_joycode_base_url(
            "https://api-ai.jd.com/redirect",
            JoycodeNetwork::External
        )
        .is_err());
        assert!(validate_joycode_base_url(
            "http://joycode-api-saas.jd.com",
            JoycodeNetwork::Internal
        )
        .is_ok());
    }

    #[test]
    fn parses_ready_model_runtime_snapshot_and_quota() {
        let snapshot = parse_runtime_snapshot(
            &json!({
                "code": "00000",
                "bizCode": "SUCCESS",
                "data": {
                    "token": "runtime-token",
                    "tokenStatus": "READY",
                    "expireAt": "2099-01-01T00:00:00Z",
                    "remainingRequestCount": "3"
                }
            }),
            200,
        )
        .unwrap();
        assert_eq!(snapshot.token.as_deref(), Some("runtime-token"));
        assert_eq!(snapshot.token_status.as_deref(), Some("READY"));
        assert_eq!(snapshot.remaining_request_count, Some(3));
    }

    #[test]
    fn runtime_bypass_token_accepts_current_prefixed_shape() {
        assert!("mt_ready_bypass_123"
            .to_ascii_lowercase()
            .starts_with(RUNTIME_BYPASS_TOKEN));
    }

    #[test]
    fn runtime_token_errors_are_detected_in_json_and_sse() {
        assert!(is_runtime_token_error_text(Some(
            r#"{"bizCode":"MODEL_TOKEN_EXPIRED"}"#
        )));
        assert!(is_runtime_token_error_text(Some(
            "data: {\"code\":\"400002\"}\n\n"
        )));
        assert!(is_runtime_token_error_text(Some(
            "token not found or invalid"
        )));
        assert!(!is_runtime_token_error_text(Some(
            "data: {\"type\":\"message_start\"}\n\n"
        )));
    }

    #[tokio::test]
    async fn compressed_success_envelope_still_detects_runtime_token_error() {
        use std::io::Write;

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder
            .write_all(br#"{"code":0,"bizCode":"SUCCESS","error":{"code":"MODEL_TOKEN_EXPIRED"}}"#)
            .unwrap();
        let compressed = encoder.finish().unwrap();
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            http::header::CONTENT_ENCODING,
            http::HeaderValue::from_static("gzip"),
        );
        let response = ProxyResponse::buffered(http::StatusCode::OK, headers, compressed.into());
        let error = match validate_auth_envelope(response).await {
            Err(error) => error,
            Ok(_) => panic!("compressed runtime-token error was not detected"),
        };
        assert!(matches!(
            error,
            ProxyError::UpstreamError {
                status: 409,
                body: Some(_)
            }
        ));
    }

    #[test]
    fn cached_runtime_token_consumes_quota_and_expires_at_zero() {
        let mut active = Some(CachedRuntimeToken {
            token: "runtime-token".to_string(),
            expire_at: Some("2099-01-01T00:00:00Z".to_string()),
            remaining_request_count: Some(1),
        });
        assert_eq!(
            consume_cached_runtime_token(&mut active).as_deref(),
            Some("runtime-token")
        );
        assert_eq!(
            active
                .as_ref()
                .and_then(|token| token.remaining_request_count),
            Some(0)
        );
        assert!(consume_cached_runtime_token(&mut active).is_none());
        assert!(active.is_none());
    }

    /// Opt-in contract smoke test against the currently installed JoyCode
    /// login state. It never prints credentials or model output and is ignored
    /// by normal test runs because it consumes live account quota.
    #[tokio::test]
    #[ignore = "requires an active local JoyCode login and consumes live quota"]
    async fn live_external_anthropic_and_responses_contract() {
        let credential = discover_joycode_credentials()
            .into_iter()
            .next()
            .expect("active JoyCode login");
        let provider = Provider {
            id: "joycode-live-contract".to_string(),
            name: "JoyCode live contract".to_string(),
            settings_config: json!({}),
            website_url: Some(JOYCODE_WEBSITE_URL.to_string()),
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(crate::provider::ProviderMeta {
                provider_type: Some("joycode".to_string()),
                joycode_network: Some("external".to_string()),
                joycode_external_base_url: Some(JOYCODE_EXTERNAL_BASE_URL.to_string()),
                joycode_login_type: credential.login_type.clone(),
                joycode_tenant: credential.tenant.clone(),
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        };
        let models = fetch_models(&provider, &credential.pt_key)
            .await
            .expect("fetch live model catalog");

        for wire_api in [JoycodeWireApi::Anthropic, JoycodeWireApi::Responses] {
            let model = models
                .iter()
                .find(|model| model.wire_api == wire_api)
                .expect("live catalog model for protocol");
            let chat_id = format!("cc-switch-live-contract-{}", uuid::Uuid::new_v4());
            let lease = acquire_runtime_token(&provider, &credential.pt_key, &model.id, &chat_id)
                .await
                .expect("prepare model runtime");
            let mut headers = auth_headers_for_provider(&provider, &credential.pt_key)
                .expect("live auth headers");
            headers.insert(
                http::header::ACCEPT_ENCODING,
                http::HeaderValue::from_static("identity"),
            );
            if let Some(lease) = lease.as_ref() {
                headers.insert(
                    http::HeaderName::from_static("x-model-token"),
                    http::HeaderValue::from_str(lease.token()).expect("runtime token header"),
                );
            }
            let mut body = match wire_api {
                JoycodeWireApi::Anthropic => json!({
                    "model": model.id,
                    "max_tokens": 64,
                    "messages": [{"role": "user", "content": "Reply with OK only."}],
                    "stream": true
                }),
                JoycodeWireApi::Responses => json!({
                    "model": model.id,
                    "input": "Reply with OK only.",
                    "max_output_tokens": 64,
                    "stream": true,
                    "store": true
                }),
                JoycodeWireApi::Chat => unreachable!(),
            };
            decorate_body(&mut body, wire_api);
            let endpoint = endpoint_for(&provider, wire_api).expect("live inference endpoint");
            let response = crate::proxy::http_client::get()
                .post(endpoint)
                .headers(headers)
                .timeout(Duration::from_secs(90))
                .json(&body)
                .send()
                .await
                .expect("live inference request");
            let status = response.status();
            let content_type = response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let content_encoding = response
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let response_bytes = response
                .bytes()
                .await
                .expect("live inference response body");
            let decoded =
                crate::proxy::content_encoding::decompress_body(&content_encoding, &response_bytes)
                    .expect("decompress live inference response")
                    .unwrap_or_else(|| response_bytes.to_vec());
            let response_text = String::from_utf8_lossy(&decoded);

            if let Some(lease) = lease.as_ref() {
                let cancel_endpoint =
                    model_runtime_endpoint(&provider, "cancel").expect("runtime cancel endpoint");
                let _ = crate::proxy::http_client::get()
                    .post(cancel_endpoint)
                    .headers(
                        auth_headers_for_provider(&provider, &credential.pt_key)
                            .expect("cancel auth headers"),
                    )
                    .json(&json!({"token": lease.token()}))
                    .send()
                    .await;
            }
            assert!(status.is_success(), "live inference HTTP {status}");
            assert!(
                !response_text.trim().is_empty(),
                "empty live inference body"
            );
            assert!(
                !is_runtime_token_error_text(Some(&response_text)),
                "live inference rejected the runtime token"
            );
            if let Ok(payload) = serde_json::from_str::<Value>(&response_text) {
                assert!(
                    payload.get("error").is_none_or(Value::is_null)
                        && runtime_error_code(&payload).is_none(),
                    "live inference returned a structured error"
                );
            } else {
                assert!(
                    response_text.trim_start().starts_with("data:")
                        || response_text.trim_start().starts_with("event:"),
                    "live inference returned neither JSON nor SSE (content-type: {content_type}, content-encoding: {content_encoding}, first-byte: {:?})",
                    decoded.first()
                );
            }
        }
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
    fn catalog_builds_distinct_claude_roles_and_codex_default() {
        let model = |id: &str, wire_api| JoycodeModel {
            id: id.to_string(),
            owned_by: "joycode".to_string(),
            wire_api,
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
        };
        let models = vec![
            model("Claude-Opus-4.6-hq", JoycodeWireApi::Anthropic),
            model("Claude-Opus-4.8-hq", JoycodeWireApi::Anthropic),
            model("Claude-Sonnet-4.6-hq", JoycodeWireApi::Anthropic),
            model("GPT-5.6 Sol", JoycodeWireApi::Responses),
            model("GLM-5.3", JoycodeWireApi::Chat),
        ];

        let (haiku, sonnet, opus) = claude_role_models(&models).unwrap();
        assert_eq!(haiku.id, "Claude-Sonnet-4.6-hq");
        assert_eq!(sonnet.id, "Claude-Sonnet-4.6-hq");
        assert_eq!(opus.id, "Claude-Opus-4.8-hq");
        assert_eq!(codex_default_model(&models).unwrap().id, "GPT-5.6 Sol");
    }

    #[test]
    fn native_anthropic_body_preserves_tool_strict() {
        let mut body = json!({
            "model": "Claude-Opus-4.8-hq",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [
                {
                    "name": "apply_patch",
                    "description": "Apply a patch",
                    "input_schema": {
                        "type": "object",
                        "properties": {"input": {"type": "string"}},
                        "required": ["input"],
                        "additionalProperties": false
                    },
                    "strict": true
                },
                {
                    "name": "read_file",
                    "input_schema": {"type": "object", "properties": {}},
                    "strict": false
                }
            ]
        });

        decorate_body(&mut body, JoycodeWireApi::Anthropic);

        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools[0]["strict"], json!(true));
        assert_eq!(tools[1]["strict"], json!(false));
        assert_eq!(
            tools[0]["input_schema"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn codex_anthropic_promotes_responses_lite_additional_tools() {
        let mut input = json!({
            "model": "Claude-Opus-4.8-hq",
            "max_output_tokens": 4096,
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [{
                        "type": "namespace",
                        "name": "functions",
                        "tools": [{
                            "type": "function",
                            "name": "exec",
                            "description": "Run a command",
                            "parameters": {
                                "oneOf": [
                                    {
                                        "type": "object",
                                        "properties": {"cmd": {"type": "string"}},
                                        "required": ["cmd"]
                                    },
                                    {
                                        "type": "object",
                                        "properties": {"session_id": {"type": "integer"}},
                                        "required": ["session_id"]
                                    }
                                ]
                            },
                            "strict": true
                        }]
                    }]
                },
                {
                    "type": "agent_message",
                    "author": "worker",
                    "recipient": "root",
                    "content": [{"type": "input_text", "text": "analysis complete"}]
                }
            ],
            "tools": [{
                "type": "function",
                "name": "read_file",
                "parameters": {"type": "object", "properties": {}}
            }]
        });

        assert!(promote_codex_anthropic_additional_tools(&mut input));
        assert_eq!(input["input"].as_array().map(Vec::len), Some(1));
        assert_eq!(input["tools"].as_array().map(Vec::len), Some(2));

        let mut body =
            crate::proxy::providers::transform_codex_anthropic::responses_request_to_anthropic(
                input, 4096,
            )
            .expect("Codex request should convert to Anthropic");
        sanitize_codex_anthropic_tools(&mut body);

        let tools = body["tools"].as_array().expect("Anthropic tools");
        assert!(tools.iter().any(|tool| tool["name"] == "read_file"));
        let exec = tools
            .iter()
            .find(|tool| tool["name"] == "functions__exec")
            .expect("promoted namespace child");
        assert!(exec.get("strict").is_none());
        assert!(exec["input_schema"].get("oneOf").is_none());
        assert_eq!(exec["input_schema"]["type"], "object");

        let messages = body["messages"].as_array().expect("Anthropic messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "analysis complete");
    }

    #[test]
    fn codex_anthropic_does_not_promote_invalid_additional_tools_carrier() {
        let original = json!({
            "input": [{
                "type": "additional_tools",
                "role": "user",
                "tools": [{"type": "function", "name": "unsafe"}]
            }]
        });
        let mut body = original.clone();

        assert!(!promote_codex_anthropic_additional_tools(&mut body));
        assert_eq!(body, original);
    }

    #[test]
    fn codex_anthropic_body_drops_tool_strict_before_joycode() {
        let input = json!({
            "model": "Claude-Opus-4.8-hq",
            "max_output_tokens": 4096,
            "input": [{"role": "user", "content": "hello"}],
            "tools": [{
                "type": "function",
                "name": "read_file",
                "description": "Read a file",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"],
                    "additionalProperties": false
                },
                "strict": true
            }]
        });
        let mut body =
            crate::proxy::providers::transform_codex_anthropic::responses_request_to_anthropic(
                input, 4096,
            )
            .expect("Codex request should convert to Anthropic");
        assert_eq!(body["tools"][0]["strict"], json!(true));

        sanitize_codex_anthropic_tools(&mut body);
        decorate_body(&mut body, JoycodeWireApi::Anthropic);

        assert!(body["tools"][0].get("strict").is_none());
        assert_eq!(
            body["tools"][0]["input_schema"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn codex_anthropic_body_flattens_root_tool_schema_combinators() {
        let mut body = json!({
            "tools": [
                {
                    "name": "automation_update",
                    "strict": true,
                    "input_schema": {
                        "oneOf": [
                            {
                                "type": "object",
                                "properties": {
                                    "mode": {"type": "string"},
                                    "id": {"type": "string"}
                                },
                                "required": ["mode", "id"],
                                "additionalProperties": false
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "mode": {"enum": ["delete"]},
                                    "name": {"type": "string"}
                                },
                                "required": ["mode", "name"],
                                "additionalProperties": false
                            }
                        ]
                    }
                },
                {
                    "name": "combined",
                    "input_schema": {
                        "allOf": [
                            {
                                "type": "object",
                                "properties": {"left": {"type": "string"}},
                                "required": ["left"]
                            },
                            {
                                "type": "object",
                                "properties": {"right": {"type": "integer"}},
                                "required": ["right"]
                            }
                        ]
                    }
                },
                {
                    "name": "optional_union",
                    "input_schema": {
                        "anyOf": [
                            {
                                "type": "object",
                                "properties": {"path": {"type": "string"}},
                                "required": ["path"]
                            },
                            {"type": "null"}
                        ]
                    }
                }
            ]
        });

        sanitize_codex_anthropic_tools(&mut body);

        let union = &body["tools"][0]["input_schema"];
        assert_eq!(union["type"], json!("object"));
        assert!(union.get("oneOf").is_none());
        assert!(union.get("anyOf").is_none());
        assert!(union.get("allOf").is_none());
        assert_eq!(union["required"], json!(["mode"]));
        assert!(union["properties"].get("id").is_some());
        assert!(union["properties"].get("name").is_some());
        assert_eq!(
            union["properties"]["mode"]["anyOf"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
        assert!(body["tools"][0].get("strict").is_none());

        let combined = &body["tools"][1]["input_schema"];
        assert_eq!(combined["type"], json!("object"));
        assert!(combined.get("allOf").is_none());
        assert_eq!(combined["required"], json!(["left", "right"]));
        assert!(combined["properties"].get("left").is_some());
        assert!(combined["properties"].get("right").is_some());

        let optional = &body["tools"][2]["input_schema"];
        assert_eq!(optional["type"], json!("object"));
        assert!(optional.get("anyOf").is_none());
        assert!(optional["properties"].get("path").is_some());
        assert!(optional.get("required").is_none());
    }

    #[test]
    fn native_anthropic_body_preserves_root_tool_schema_combinator() {
        let mut body = json!({
            "model": "Claude-Opus-4.8-hq",
            "tools": [{
                "name": "native_union",
                "input_schema": {
                    "oneOf": [
                        {"type": "object", "properties": {"a": {"type": "string"}}},
                        {"type": "object", "properties": {"b": {"type": "string"}}}
                    ]
                }
            }]
        });

        decorate_body(&mut body, JoycodeWireApi::Anthropic);

        assert!(body["tools"][0]["input_schema"].get("oneOf").is_some());
    }

    #[test]
    fn non_anthropic_body_preserves_tool_strict() {
        let mut body = json!({
            "model": "GPT-5.6 Sol",
            "tools": [{
                "name": "apply_patch",
                "input_schema": {"type": "object", "properties": {}},
                "strict": true
            }]
        });

        decorate_body(&mut body, JoycodeWireApi::Responses);

        assert_eq!(body["tools"][0]["strict"], json!(true));
    }

    #[test]
    fn anthropic_body_drops_unsupported_context_management() {
        let mut body = json!({
            "model": "claude-opus-4-6",
            "messages": [{"role": "user", "content": "hello"}],
            "context_management": {"edits": [{"type": "clear_thinking_20251015"}]},
            "output_config": {"effort": "high"}
        });

        decorate_body(&mut body, JoycodeWireApi::Anthropic);

        assert!(body.get("context_management").is_none());
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["client"], JOYCODE_CLIENT);
    }

    #[test]
    fn anthropic_body_upgrades_legacy_thinking_for_adaptive_models() {
        for model in [
            "Claude-Opus-4.6-hq",
            "Claude-Opus-4.7-hq",
            "Claude-Opus-4.8-hq",
            "Claude-Sonnet-4.6-hq",
        ] {
            let mut body = json!({
                "model": model,
                "thinking": {"type": "enabled", "budget_tokens": 4096}
            });

            decorate_body(&mut body, JoycodeWireApi::Anthropic);

            assert_eq!(body["thinking"], json!({"type": "adaptive"}), "{model}");
            assert_eq!(body["output_config"]["effort"], "medium", "{model}");
            assert!(body["thinking"].get("budget_tokens").is_none(), "{model}");
        }
    }

    #[test]
    fn anthropic_body_preserves_existing_adaptive_effort() {
        let mut body = json!({
            "model": "Claude-Opus-4.8-hq",
            "thinking": {"type": "adaptive", "budget_tokens": 4096},
            "output_config": {"effort": "high", "format": {"type": "json_schema"}}
        });

        decorate_body(&mut body, JoycodeWireApi::Anthropic);

        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    }

    #[test]
    fn anthropic_body_leaves_legacy_model_thinking_unchanged() {
        let mut body = json!({
            "model": "claude-sonnet-4-5",
            "thinking": {"type": "enabled", "budget_tokens": 4096}
        });

        decorate_body(&mut body, JoycodeWireApi::Anthropic);

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 4096);
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn anthropic_business_error_prefers_nested_cause_message() {
        let payload = json!({
            "error": {
                "cause": "{\"message\":\"thinking.type.enabled is not supported; use adaptive\"}",
                "code": 400,
                "message": "模型服务调用失败",
                "status": "FAILED_RESPONSE"
            },
            "result": null
        });

        let error = anthropic_business_error(&payload).expect("business error");
        assert!(matches!(
            error,
            ProxyError::UpstreamError { status: 400, body: Some(message) }
                if message == "thinking.type.enabled is not supported; use adaptive"
        ));
    }

    #[test]
    fn anthropic_sse_start_distinguishes_error_from_normal_event() {
        let error_block = concat!(
            "data: {\"error\":{\"cause\":\"{\\\"message\\\":\\\"bad thinking\\\"}\",",
            "\"code\":400,\"message\":\"模型服务调用失败\"},\"result\":null}"
        );
        assert!(matches!(
            inspect_anthropic_sse_start(error_block),
            Some(Err(ProxyError::UpstreamError { status: 400, body: Some(message) }))
                if message == "bad thinking"
        ));

        let normal_block = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\"}}"
        );
        assert!(matches!(
            inspect_anthropic_sse_start(normal_block),
            Some(Ok(()))
        ));
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

    #[test]
    fn unwraps_joycode_nested_anthropic_business_error_for_validation() {
        let mut normalizer = JoycodeResponsesSseNormalizer::for_anthropic();
        let input = concat!(
            "data: data: {\"error\":{\"cause\":\"{\\\"message\\\":\\\"use adaptive\\\"}\",",
            "\"code\":400,\"message\":\"模型服务调用失败\"},\"result\":null}\n\n",
            "data: [DONE]\n\n"
        );
        let mut output = normalizer.push_bytes(input.as_bytes());
        output.extend(normalizer.finish());
        let output = String::from_utf8(output).unwrap();
        let mut parse_buffer = output;
        let block =
            crate::proxy::sse::take_sse_block(&mut parse_buffer).expect("normalized error event");

        assert!(matches!(
            inspect_anthropic_sse_start(&block),
            Some(Err(ProxyError::UpstreamError { status: 400, body: Some(message) }))
                if message == "use adaptive"
        ));
        assert!(!block.contains("data: data:"));
    }

    #[test]
    fn unwraps_joycode_nested_anthropic_sse_and_drops_done() {
        let mut normalizer = JoycodeResponsesSseNormalizer::for_anthropic();
        let input = concat!(
            "data: event: message_start\n\n",
            "data: data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\"}}\n\n",
            "data: event: content_block_delta\n\n",
            "data: data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\n",
            "data: event: message_stop\n\n",
            "data: data: {\"type\":\"message_stop\"}\n\n",
            "data: [DONE]\n\n"
        );
        let split = input.len() / 2;
        let mut output = normalizer.push_bytes(&input.as_bytes()[..split]);
        output.extend(normalizer.push_bytes(&input.as_bytes()[split..]));
        output.extend(normalizer.finish());
        let output = String::from_utf8(output).unwrap();

        assert!(output.starts_with("event: message_start\ndata: {"));
        assert!(output.contains("event: content_block_delta\n"));
        assert!(output.contains("event: message_stop\ndata: {\"type\":\"message_stop\"}"));
        assert!(!output.contains("data: event:"));
        assert!(!output.contains("data: data:"));
        assert!(!output.contains("[DONE]"));
    }
}
