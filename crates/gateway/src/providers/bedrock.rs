use super::traits::*;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

/// AWS Bedrock provider using the Converse API.
///
/// Authentication uses AWS SigV4 signing via the `aws-sigv4` crate.
/// Requires: access_key_id, secret_access_key, region.
///
/// The `api_key` field stores credentials as `{access_key_id}:{secret_access_key}` format.
/// The `base_url` field stores the region (e.g. "us-east-1").
pub struct BedrockProvider {
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub client: reqwest::Client,
}

impl BedrockProvider {
    /// Create a new Bedrock provider.
    ///
    /// - `region`: AWS region (e.g. "us-east-1")
    /// - `credentials`: Formatted as `{access_key_id}:{secret_access_key}`
    pub fn new(region: String, credentials: String) -> Self {
        let (access_key_id, secret_access_key) = credentials
            .split_once(':')
            .map(|(a, s)| (a.to_string(), s.to_string()))
            .unwrap_or_else(|| (credentials.clone(), String::new()));

        Self {
            region,
            access_key_id,
            secret_access_key,
            client: reqwest::Client::new(),
        }
    }

    fn endpoint_url(&self, model_id: &str) -> String {
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/converse",
            self.region, model_id
        )
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

        let credentials = Credentials::new(
            &self.access_key_id,
            &self.secret_access_key,
            None,
            None,
            "agent-bastion",
        );

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
        // Bedrock streaming uses a different endpoint (/converse-stream) with
        // binary event-stream encoding. For now, fall back to non-streaming
        // by making a sync call and emitting a single chunk.
        let provider = Self {
            region: self.region.clone(),
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
            client: self.client.clone(),
        };

        Box::pin(async_stream::stream! {
            match provider.chat_completion(request).await {
                Ok(resp) => {
                    let text = resp
                        .choices
                        .first()
                        .and_then(|c| c.message.content.as_str())
                        .unwrap_or("")
                        .to_string();

                    yield Ok(ChatCompletionChunk {
                        id: resp.id,
                        object: "chat.completion.chunk".to_string(),
                        created: resp.created,
                        model: resp.model,
                        choices: vec![ChunkChoice {
                            index: 0,
                            delta: serde_json::json!({"role": "assistant", "content": text}),
                            finish_reason: Some("stop".to_string()),
                        }],
                        usage: resp.usage,
                    });
                }
                Err(e) => {
                    yield Err(e);
                }
            }
        })
    }
}
