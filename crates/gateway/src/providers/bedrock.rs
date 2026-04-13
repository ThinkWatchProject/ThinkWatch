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
    pub client: reqwest::Client,
    pub custom_headers: Vec<(String, String)>,
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
            client: reqwest::Client::new(),
            custom_headers: Vec::new(),
        }
    }

    pub fn with_custom_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.custom_headers = headers;
        self
    }

    fn resolve_headers(&self, request: &ChatCompletionRequest) -> Vec<(String, String)> {
        let uid = request.caller_user_id.as_deref().unwrap_or("");
        let email = request.caller_user_email.as_deref().unwrap_or("");
        self.custom_headers
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.replace("{{user_id}}", uid)
                        .replace("{{user_email}}", email),
                )
            })
            .collect()
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
        let client = &self.client;

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
        let custom_headers = self.resolve_headers(&request);
        let bedrock_req = convert_to_bedrock(&request);
        let body_bytes = serde_json::to_vec(&bedrock_req)
            .map_err(|e| GatewayError::TransformError(e.to_string()))?;

        let signed_headers = self.sign_request(&url, &body_bytes).await?;

        let mut req_builder = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .body(body_bytes);

        for (name, value) in &signed_headers {
            req_builder = req_builder.header(name.as_str(), value.as_str());
        }
        for (k, v) in &custom_headers {
            req_builder = req_builder.header(k.as_str(), v.as_str());
        }

        let resp = req_builder
            .send()
            .await
            .map_err(|e| GatewayError::NetworkError(e.to_string()))?;

        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(GatewayError::UpstreamRateLimited);
        }
        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(GatewayError::UpstreamAuthError);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderError(format!(
                "Bedrock returned {status}: {body}"
            )));
        }

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
        let client = self.client.clone();
        let url = self.stream_endpoint_url(&request.model);
        let model = request.model.clone();
        let region = self.region.clone();
        let access_key_id = self.access_key_id.clone();
        let secret_access_key = self.secret_access_key.clone();
        let provider_client = self.client.clone();
        let custom_headers = self.resolve_headers(&request);

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
                client: provider_client,
                custom_headers: Vec::new(),
            };
            let signed_headers = match provider.sign_request(&url, &body_bytes).await {
                Ok(h) => h,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };

            let mut req_builder = client
                .post(&url)
                .header("content-type", "application/json")
                .body(body_bytes);

            for (name, value) in &signed_headers {
                req_builder = req_builder.header(name.as_str(), value.as_str());
            }
            for (k, v) in &custom_headers {
                req_builder = req_builder.header(k.as_str(), v.as_str());
            }

            let resp = match req_builder.send().await {
                Ok(r) => r,
                Err(e) => {
                    yield Err(GatewayError::NetworkError(e.to_string()));
                    return;
                }
            };

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                yield Err(GatewayError::ProviderError(format!(
                    "Bedrock stream error: {body}"
                )));
                return;
            }

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
