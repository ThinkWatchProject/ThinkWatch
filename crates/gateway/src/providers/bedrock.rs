use super::traits::*;
use futures::Stream;
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

/// AWS Bedrock provider using the Converse API.
///
/// Authentication uses AWS SigV4 signing. Two modes:
/// - Static credentials: `api_key` = `{access_key_id}:{secret_access_key}`
/// - IMDSv2 (EC2): `api_key` is empty — credentials are fetched from
///   the instance metadata service at request time.
///
/// The `base_url` field stores the region (e.g. "us-east-1").
pub struct BedrockProvider {
    pub region: String,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub base: ProviderBase,
}

impl BedrockProvider {
    /// Create a new Bedrock provider.
    ///
    /// - `region`: AWS region (e.g. "us-east-1")
    /// - `credentials`: `{access_key_id}:{secret_access_key}`, or empty for IMDSv2
    pub fn new(region: String, credentials: String) -> Self {
        let (access_key_id, secret_access_key) = if credentials.is_empty() {
            (None, None)
        } else {
            let (a, s) = credentials
                .split_once(':')
                .map(|(a, s)| (a.to_string(), s.to_string()))
                .unwrap_or_else(|| (credentials.clone(), String::new()));
            (Some(a), Some(s))
        };

        Self {
            region,
            access_key_id,
            secret_access_key,
            base: ProviderBase::new(String::new()),
        }
    }

    pub fn with_custom_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.base = self.base.with_custom_headers(headers);
        self
    }

    fn endpoint_url(&self, model_id: &str) -> String {
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/converse",
            self.region, model_id
        )
    }

    fn stream_endpoint_url(&self, model_id: &str) -> String {
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/converse-stream",
            self.region, model_id
        )
    }

    /// Fetch temporary credentials from EC2 IMDSv2.
    async fn fetch_imdsv2_credentials(
        &self,
    ) -> Result<aws_credential_types::Credentials, GatewayError> {
        use aws_credential_types::Credentials;

        let imds_base = "http://169.254.169.254";
        let client = &self.base.client;

        // Step 1: Get IMDSv2 token
        let token = client
            .put(format!("{imds_base}/latest/api/token"))
            .header("X-aws-ec2-metadata-token-ttl-seconds", "300")
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("IMDSv2 token request failed: {e}")))?
            .text()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("IMDSv2 token read failed: {e}")))?;

        // Step 2: Get IAM role name
        let role = client
            .get(format!(
                "{imds_base}/latest/meta-data/iam/security-credentials/"
            ))
            .header("X-aws-ec2-metadata-token", &token)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("IMDSv2 role lookup failed: {e}")))?
            .text()
            .await
            .map_err(|e| GatewayError::ProviderError(format!("IMDSv2 role read failed: {e}")))?;
        let role = role.trim();

        // Step 3: Get credentials for that role
        let creds_json: serde_json::Value = client
            .get(format!(
                "{imds_base}/latest/meta-data/iam/security-credentials/{role}"
            ))
            .header("X-aws-ec2-metadata-token", &token)
            .send()
            .await
            .map_err(|e| {
                GatewayError::ProviderError(format!("IMDSv2 credentials fetch failed: {e}"))
            })?
            .json()
            .await
            .map_err(|e| {
                GatewayError::ProviderError(format!("IMDSv2 credentials parse failed: {e}"))
            })?;

        let ak = creds_json["AccessKeyId"]
            .as_str()
            .ok_or_else(|| GatewayError::ProviderError("IMDSv2: missing AccessKeyId".into()))?;
        let sk = creds_json["SecretAccessKey"]
            .as_str()
            .ok_or_else(|| GatewayError::ProviderError("IMDSv2: missing SecretAccessKey".into()))?;
        let session_token = creds_json["Token"].as_str().map(|s| s.to_string());

        Ok(Credentials::new(ak, sk, session_token, None, "imdsv2"))
    }

    async fn sign_request(
        &self,
        url: &str,
        body: &[u8],
    ) -> Result<Vec<(String, String)>, GatewayError> {
        use aws_credential_types::Credentials;
        use aws_sigv4::http_request::{
            PayloadChecksumKind, SignableBody, SignableRequest, SignatureLocation, SigningSettings,
            sign,
        };
        use aws_sigv4::sign::v4;
        use std::time::SystemTime;

        let credentials = match (&self.access_key_id, &self.secret_access_key) {
            (Some(ak), Some(sk)) => Credentials::new(ak, sk, None, None, "think-watch"),
            _ => self.fetch_imdsv2_credentials().await?,
        };

        let identity = credentials.into();
        let mut signing_settings = SigningSettings::default();
        signing_settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        signing_settings.signature_location = SignatureLocation::Headers;

        let signing_params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name("bedrock")
            .time(SystemTime::now())
            .settings(signing_settings)
            .build()
            .map_err(|e| GatewayError::ProviderError(format!("SigV4 params error: {e}")))?;

        let signable_request = SignableRequest::new(
            "POST",
            url,
            std::iter::once(("content-type", "application/json")),
            SignableBody::Bytes(body),
        )
        .map_err(|e| GatewayError::ProviderError(format!("Signable request error: {e}")))?;

        let (signing_instructions, _signature) = sign(signable_request, &signing_params.into())
            .map_err(|e| GatewayError::ProviderError(format!("SigV4 signing error: {e}")))?
            .into_parts();

        // Build a dummy http 1.x request to extract signed headers
        let mut http_req = http_1x::Request::builder()
            .method("POST")
            .uri(url)
            .header("content-type", "application/json")
            .body(())
            .map_err(|e| GatewayError::ProviderError(format!("HTTP request build error: {e}")))?;

        signing_instructions.apply_to_request_http1x(&mut http_req);

        let headers: Vec<(String, String)> = http_req
            .headers()
            .iter()
            .filter(|(name, _)| {
                // Only include AWS-specific headers (authorization, x-amz-*)
                let n = name.as_str();
                n == "authorization" || n.starts_with("x-amz-")
            })
            .map(|(name, value)| {
                (
                    name.to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();

        Ok(headers)
    }
}

// ---------- Bedrock Converse API types ----------

#[derive(Debug, Serialize)]
struct BedrockConverseRequest {
    messages: Vec<BedrockMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<BedrockSystemContent>>,
    #[serde(rename = "inferenceConfig")]
    #[serde(skip_serializing_if = "Option::is_none")]
    inference_config: Option<BedrockInferenceConfig>,
}

#[derive(Debug, Serialize)]
struct BedrockMessage {
    role: String,
    content: Vec<BedrockContent>,
}

#[derive(Debug, Serialize)]
struct BedrockContent {
    text: String,
}

#[derive(Debug, Serialize)]
struct BedrockSystemContent {
    text: String,
}

#[derive(Debug, Serialize)]
struct BedrockInferenceConfig {
    #[serde(rename = "maxTokens")]
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct BedrockConverseResponse {
    output: BedrockOutput,
    usage: BedrockUsage,
    #[serde(rename = "stopReason")]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BedrockOutput {
    message: BedrockOutputMessage,
}

#[derive(Debug, Deserialize)]
struct BedrockOutputMessage {
    // `role` is present in the Bedrock API response; serde requires the
    // field for deserialization even though we don't read it.
    #[allow(dead_code)]
    role: String,
    content: Vec<BedrockOutputContent>,
}

#[derive(Debug, Deserialize)]
struct BedrockOutputContent {
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BedrockUsage {
    #[serde(rename = "inputTokens")]
    input_tokens: u32,
    #[serde(rename = "outputTokens")]
    output_tokens: u32,
    #[serde(rename = "totalTokens")]
    total_tokens: u32,
}

// ---------- Conversion ----------

fn convert_to_bedrock(req: &ChatCompletionRequest) -> BedrockConverseRequest {
    let mut system = Vec::new();
    let mut messages = Vec::new();

    for msg in &req.messages {
        let text = match &msg.content {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };

        if msg.role == "system" {
            system.push(BedrockSystemContent { text });
        } else {
            messages.push(BedrockMessage {
                role: msg.role.clone(),
                content: vec![BedrockContent { text }],
            });
        }
    }

    BedrockConverseRequest {
        messages,
        system: if system.is_empty() {
            None
        } else {
            Some(system)
        },
        inference_config: Some(BedrockInferenceConfig {
            max_tokens: req.max_tokens,
            temperature: req.temperature,
        }),
    }
}

fn convert_from_bedrock(resp: BedrockConverseResponse, model: &str) -> ChatCompletionResponse {
    let text = resp
        .output
        .message
        .content
        .iter()
        .filter_map(|c| c.text.clone())
        .collect::<Vec<_>>()
        .join("");

    let finish_reason = resp.stop_reason.map(|r| match r.as_str() {
        "end_turn" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        "stop_sequence" => "stop".to_string(),
        other => other.to_string(),
    });

    ChatCompletionResponse {
        id: format!("bedrock-{}", uuid::Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp(),
        model: model.to_string(),
        choices: vec![Choice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_string(),
                content: serde_json::Value::String(text),
            },
            finish_reason,
        }],
        usage: Some(Usage {
            prompt_tokens: resp.usage.input_tokens,
            completion_tokens: resp.usage.output_tokens,
            total_tokens: resp.usage.total_tokens,
        }),
    }
}

// ---------- AiProvider ----------

impl AiProvider for BedrockProvider {
    fn name(&self) -> &str {
        "bedrock"
    }

    async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let url = self.endpoint_url(&request.model);
        let bedrock_req = convert_to_bedrock(&request);
        let body_bytes = serde_json::to_vec(&bedrock_req)
            .map_err(|e| GatewayError::TransformError(e.to_string()))?;

        let signed_headers = self.sign_request(&url, &body_bytes).await?;

        let builder = self
            .base
            .client
            .post(&url)
            .header("content-type", "application/json")
            .body(body_bytes);
        let builder = ProviderBase::apply_headers(builder, &signed_headers);
        let builder = self.base.apply_custom_headers(builder, &request);

        let resp = ProviderBase::send(builder).await?;
        let resp = ProviderBase::check_status(resp, "Bedrock").await?;

        let bedrock_resp: BedrockConverseResponse = resp
            .json()
            .await
            .map_err(|e| GatewayError::ProviderError(e.to_string()))?;

        Ok(convert_from_bedrock(bedrock_resp, &request.model))
    }

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
        let client = self.base.client.clone();
        let url = self.stream_endpoint_url(&request.model);
        let model = request.model.clone();
        let region = self.region.clone();
        let access_key_id = self.access_key_id.clone();
        let secret_access_key = self.secret_access_key.clone();
        let provider_client = self.base.client.clone();
        let custom_headers = self.base.resolve_headers(&request);

        let bedrock_req = convert_to_bedrock(&request);

        Box::pin(async_stream::stream! {
            let body_bytes = match serde_json::to_vec(&bedrock_req) {
                Ok(b) => b,
                Err(e) => {
                    yield Err(GatewayError::TransformError(e.to_string()));
                    return;
                }
            };

            // Sign the request
            let provider = BedrockProvider {
                region, access_key_id, secret_access_key,
                base: ProviderBase { base_url: String::new(), client: provider_client, custom_headers: Vec::new() },
            };
            let signed_headers = match provider.sign_request(&url, &body_bytes).await {
                Ok(h) => h,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };

            let builder = client
                .post(&url)
                .header("content-type", "application/json")
                .body(body_bytes);
            let builder = ProviderBase::apply_headers(builder, &signed_headers);
            let builder = ProviderBase::apply_headers(builder, &custom_headers);

            let resp = match ProviderBase::send(builder).await {
                Ok(r) => r,
                Err(e) => { yield Err(e); return; }
            };
            let resp = match ProviderBase::check_status(resp, "Bedrock").await {
                Ok(r) => r,
                Err(e) => { yield Err(e); return; }
            };

            let message_id = format!("bedrock-{}", uuid::Uuid::new_v4());

            // Emit initial role chunk
            yield Ok(ChatCompletionChunk {
                id: message_id.clone(),
                object: "chat.completion.chunk".to_string(),
                created: chrono::Utc::now().timestamp(),
                model: model.clone(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: serde_json::json!({"role": "assistant"}),
                    finish_reason: None,
                }],
                usage: None,
            });

            // Read the binary event-stream response
            let mut byte_stream = resp.bytes_stream();
            let mut buffer = bytes::BytesMut::new();

            while let Some(chunk_result) = byte_stream.next().await {
                let chunk = match chunk_result {
                    Ok(c) => c,
                    Err(e) => {
                        yield Err(GatewayError::NetworkError(e.to_string()));
                        break;
                    }
                };

                buffer.extend_from_slice(&chunk);

                // Try to decode event-stream frames from the buffer
                // AWS event-stream binary format:
                //   [4 bytes total_len] [4 bytes headers_len] [4 bytes prelude_crc]
                //   [headers] [payload] [4 bytes message_crc]
                while buffer.len() >= 12 {
                    let total_len = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;

                    if buffer.len() < total_len {
                        break; // Need more data
                    }

                    let headers_len = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;

                    // Payload starts after prelude (12 bytes) + headers
                    let payload_start = 12 + headers_len;
                    let payload_end = total_len - 4; // Exclude message CRC

                    if payload_start <= payload_end && payload_end <= buffer.len() {
                        let payload = &buffer[payload_start..payload_end];

                        // Try to parse as JSON — Bedrock wraps events in typed envelopes
                        if let Ok(event) = serde_json::from_slice::<serde_json::Value>(payload) {
                            // contentBlockDelta contains text chunks
                            if let Some(text) = event.get("contentBlockDelta")
                                .and_then(|d| d.get("delta"))
                                .and_then(|d| d.get("text"))
                                .and_then(|t| t.as_str()) {
                                    yield Ok(ChatCompletionChunk {
                                        id: message_id.clone(),
                                        object: "chat.completion.chunk".to_string(),
                                        created: chrono::Utc::now().timestamp(),
                                        model: model.clone(),
                                        choices: vec![ChunkChoice {
                                            index: 0,
                                            delta: serde_json::json!({"content": text}),
                                            finish_reason: None,
                                        }],
                                        usage: None,
                                    });
                            }

                            // messageStop signals end of stream
                            if event.get("messageStop").is_some() {
                                let stop_reason = event.get("messageStop")
                                    .and_then(|s| s.get("stopReason"))
                                    .and_then(|r| r.as_str())
                                    .unwrap_or("end_turn");

                                let finish_reason = match stop_reason {
                                    "end_turn" => "stop",
                                    "max_tokens" => "length",
                                    other => other,
                                };

                                yield Ok(ChatCompletionChunk {
                                    id: message_id.clone(),
                                    object: "chat.completion.chunk".to_string(),
                                    created: chrono::Utc::now().timestamp(),
                                    model: model.clone(),
                                    choices: vec![ChunkChoice {
                                        index: 0,
                                        delta: serde_json::json!({}),
                                        finish_reason: Some(finish_reason.to_string()),
                                    }],
                                    usage: None,
                                });
                            }

                            // metadata contains usage info
                            if let Some(usage) = event.get("metadata").and_then(|m| m.get("usage")) {
                                let input = usage.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                let output = usage.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                yield Ok(ChatCompletionChunk {
                                    id: message_id.clone(),
                                    object: "chat.completion.chunk".to_string(),
                                    created: chrono::Utc::now().timestamp(),
                                    model: model.clone(),
                                    choices: vec![],
                                    usage: Some(Usage {
                                        prompt_tokens: input,
                                        completion_tokens: output,
                                        total_tokens: input + output,
                                    }),
                                });
                            }
                        }
                    }

                    // Advance buffer past this frame
                    let _ = buffer.split_to(total_len);
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Conversion-layer unit tests
//
// `BedrockProvider` itself can't be exercised in integration tests:
// the endpoint URL is hard-coded to the public AWS host
// (`bedrock-runtime.{region}.amazonaws.com`) with no `base_url`
// override hook, so wiremock can't intercept it without DNS
// hijacking. The conversion functions are pure, so we test them
// directly against the wire shapes the AWS SDK documents — this
// catches the high-frequency regression: someone tweaks an OpenAI
// field name and the Bedrock translation silently drops it.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(messages: Vec<(&str, &str)>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "anthropic.claude-3-haiku".into(),
            messages: messages
                .into_iter()
                .map(|(role, text)| ChatMessage {
                    role: role.into(),
                    content: serde_json::Value::String(text.into()),
                })
                .collect(),
            temperature: Some(0.7),
            max_tokens: Some(256),
            stream: None,
            extra: serde_json::Value::Null,
            caller_user_id: None,
            caller_user_email: None,
            trace_id: None,
        }
    }

    // ---- convert_to_bedrock ----

    #[test]
    fn system_messages_are_hoisted_to_top_level_system_block() {
        // OpenAI puts the system prompt inside `messages[]`; Bedrock's
        // Converse API expects it at the top level. Failing to hoist
        // it leaves the system instruction inside `messages` where
        // Bedrock ignores it — silent quality regression.
        let r = req(vec![("system", "you are concise"), ("user", "hi")]);
        let out = convert_to_bedrock(&r);
        let system = out
            .system
            .as_ref()
            .expect("system block must be present when a system message exists");
        assert_eq!(system.len(), 1);
        assert_eq!(system[0].text, "you are concise");
        // The user message survives, the system message must NOT.
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0].role, "user");
        assert_eq!(out.messages[0].content[0].text, "hi");
    }

    #[test]
    fn no_system_messages_yields_none_system_block() {
        // Serializing `Some(vec![])` would emit an empty `system: []`
        // — Bedrock rejects it. Verify we project to None and the
        // skip_serializing_if drops the field entirely.
        let r = req(vec![("user", "hello")]);
        let out = convert_to_bedrock(&r);
        assert!(out.system.is_none());
        let body = serde_json::to_value(&out).unwrap();
        assert!(
            body.get("system").is_none(),
            "system field must be omitted when empty: {body}"
        );
    }

    #[test]
    fn inference_config_carries_max_tokens_and_temperature() {
        let r = req(vec![("user", "x")]);
        let out = convert_to_bedrock(&r);
        let cfg = out.inference_config.as_ref().expect("inferenceConfig set");
        assert_eq!(cfg.max_tokens, Some(256));
        assert_eq!(cfg.temperature, Some(0.7));
        // Wire shape: camelCase keys (Bedrock spec).
        let body = serde_json::to_value(&out).unwrap();
        assert_eq!(body["inferenceConfig"]["maxTokens"], 256);
        assert!(
            body["inferenceConfig"]["temperature"].as_f64().unwrap() > 0.69,
            "temperature must round-trip: {body}"
        );
    }

    #[test]
    fn multiple_user_assistant_turns_preserve_order() {
        // Bedrock requires strictly alternating user/assistant turns.
        // The converter must keep every non-system message and
        // preserve the order, otherwise multi-turn context breaks.
        let r = req(vec![
            ("user", "first"),
            ("assistant", "reply 1"),
            ("user", "second"),
            ("assistant", "reply 2"),
            ("user", "third"),
        ]);
        let out = convert_to_bedrock(&r);
        let roles: Vec<&str> = out.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "user", "assistant", "user"]
        );
        let texts: Vec<&str> = out
            .messages
            .iter()
            .map(|m| m.content[0].text.as_str())
            .collect();
        assert_eq!(
            texts,
            vec!["first", "reply 1", "second", "reply 2", "third"]
        );
    }

    #[test]
    fn non_string_content_serializes_to_json_text() {
        // OpenAI clients sometimes send `content` as an array (image
        // parts). Bedrock's Converse API takes plain text; we collapse
        // the structured content into its JSON representation so the
        // upstream still gets *something* meaningful instead of
        // silently dropping the message.
        let mut r = req(vec![]);
        r.messages.push(ChatMessage {
            role: "user".into(),
            content: json!([{"type": "text", "text": "hi"}]),
        });
        let out = convert_to_bedrock(&r);
        assert_eq!(out.messages.len(), 1);
        assert!(
            out.messages[0].content[0].text.contains("\"hi\""),
            "non-string content must collapse to JSON text, got {:?}",
            out.messages[0].content[0].text
        );
    }

    // ---- convert_from_bedrock ----

    fn bedrock_response(text: &str, stop_reason: Option<&str>) -> BedrockConverseResponse {
        BedrockConverseResponse {
            output: BedrockOutput {
                message: BedrockOutputMessage {
                    role: "assistant".into(),
                    content: vec![BedrockOutputContent {
                        text: Some(text.into()),
                    }],
                },
            },
            usage: BedrockUsage {
                input_tokens: 12,
                output_tokens: 5,
                total_tokens: 17,
            },
            stop_reason: stop_reason.map(|s| s.into()),
        }
    }

    #[test]
    fn bedrock_response_round_trips_into_openai_shape() {
        let resp = bedrock_response("hello world", Some("end_turn"));
        let out = convert_from_bedrock(resp, "anthropic.claude-3-haiku");
        assert_eq!(out.object, "chat.completion");
        assert_eq!(out.model, "anthropic.claude-3-haiku");
        assert_eq!(out.choices.len(), 1);
        assert_eq!(out.choices[0].message.role, "assistant");
        assert_eq!(out.choices[0].message.content, json!("hello world"));
        // OpenAI's `finish_reason` mapping: end_turn → stop, max_tokens →
        // length. Anything else passes through verbatim.
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = out.usage.expect("usage must be Some");
        assert_eq!(usage.prompt_tokens, 12);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 17);
    }

    #[test]
    fn stop_reason_max_tokens_maps_to_length() {
        // Pin the OpenAI-side mapping — clients use this field to
        // decide whether to retry-with-bigger-budget vs accept the
        // truncated answer. Mis-mapping it == "user thinks they got
        // a complete answer when they didn't".
        let resp = bedrock_response("partial", Some("max_tokens"));
        let out = convert_from_bedrock(resp, "x");
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn unknown_stop_reason_passes_through_verbatim() {
        // A future Bedrock release that introduces a new stop reason
        // shouldn't be silently mapped to a wrong OpenAI value. Pass
        // it through so dashboards can see "unknown" buckets.
        let resp = bedrock_response("blocked", Some("content_filtered"));
        let out = convert_from_bedrock(resp, "x");
        assert_eq!(
            out.choices[0].finish_reason.as_deref(),
            Some("content_filtered")
        );
    }

    #[test]
    fn multiple_text_content_blocks_concatenate() {
        // Bedrock can return multiple content blocks (e.g. tool-use
        // splits the response). The OpenAI envelope is a single
        // string, so we join them — losing the boundary is acceptable
        // since the OpenAI shape doesn't model it anyway.
        let resp = BedrockConverseResponse {
            output: BedrockOutput {
                message: BedrockOutputMessage {
                    role: "assistant".into(),
                    content: vec![
                        BedrockOutputContent {
                            text: Some("part one ".into()),
                        },
                        BedrockOutputContent {
                            text: Some("part two".into()),
                        },
                    ],
                },
            },
            usage: BedrockUsage {
                input_tokens: 1,
                output_tokens: 1,
                total_tokens: 2,
            },
            stop_reason: Some("end_turn".into()),
        };
        let out = convert_from_bedrock(resp, "x");
        assert_eq!(out.choices[0].message.content, json!("part one part two"));
    }
}
