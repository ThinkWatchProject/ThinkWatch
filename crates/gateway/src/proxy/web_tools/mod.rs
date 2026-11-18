//! Enterprise authorization, guard checks and accounting around Core's web executor.

mod history;
mod protocol;
mod wire;

pub(super) use history::is_hosted_responses_item;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::{Bytes, to_bytes};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use think_watch_common::audit::AuditLogger;
use tokio::sync::Mutex;
use tw_api::{WebToolRequest, WebToolResult, WebToolsConfig, WebToolsMode};
use tw_dialect::ir::{Dialect, Usage};
use tw_gateway::web_tools::WebTools;
use tw_types::{Msg, msg};
use uuid::Uuid;

use super::generate::{ClientSurface, run};
use super::headers::{request_id_header, resolve_trace_id};
use super::pipeline::AiRound;
use super::{EarlyCancelSlot, GatewayErrorResponse, GatewayRequestIdentity, GatewayState};
use crate::error::GatewayError;
use crate::guards::{Caller, Guards};
use crate::redaction::Shape;
use think_watch_common::resources;

const MAX_BODY: usize = 8 * 1024 * 1024;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostedWebSettings {
    enabled_lineages: Vec<Uuid>,
    execution: WebToolsConfig,
}

impl HostedWebSettings {
    pub const KEY: &str = "gateway.web_tools";

    pub fn parse(value: Value) -> Result<Self, String> {
        let settings: Self = serde_json::from_value(value).map_err(|e| e.to_string())?;
        if settings.enabled_lineages.len() > 256
            || (settings.execution.mode == WebToolsMode::Core
                && settings.enabled_lineages.is_empty())
        {
            return Err("Hosted web tools require 1–256 authorized API key lineage IDs".into());
        }
        core_config(&settings.execution)?;
        Ok(settings)
    }

    fn permits(&self, identity: &GatewayRequestIdentity) -> bool {
        self.execution.mode == WebToolsMode::Core
            && identity
                .api_key_lineage_id
                .as_deref()
                .and_then(|id| Uuid::parse_str(id).ok())
                .is_some_and(|id| self.enabled_lineages.contains(&id))
    }

    pub async fn diagnose(
        &self,
        runtime: &HostedWebRuntime,
        guards: &Guards,
        audit: &AuditLogger,
        user: Uuid,
        mut request: WebToolRequest,
    ) -> Result<WebToolResult, GatewayError> {
        if self.execution.mode != WebToolsMode::Core {
            return Err(GatewayError::TransformError(
                "Save hosted web tools in Core mode before testing".into(),
            ));
        }
        let caller = Caller {
            user_id: Some(user.to_string()),
            trace_id: Uuid::new_v4().to_string(),
            model: "thinkwatch-web-test".into(),
            ..Default::default()
        };
        let identity = format!("console:{user}");
        let invocation = Invocation {
            runtime,
            audit,
            guards,
            caller: &caller,
            options: &self.execution,
            scoped: &self.execution,
            lineage: &identity,
            max_concurrent: self.execution.max_concurrent,
            diagnostics: true,
        };
        let mut result = invocation.execute(&mut request).await?;
        for warning in &mut result.warnings {
            warning.text = diagnostic_text(guards, &warning.text);
            warning.args.clear();
        }
        Ok(result)
    }
}

fn core_config(settings: &WebToolsConfig) -> Result<tw_config::Config, String> {
    let config = serde_json::from_value(
        json!({"version": tw_config::SCHEMA_VERSION, "web_tools": settings}),
    )
    .map_err(|e| e.to_string())?;
    match tw_config::validate(&config) {
        // This adapter only uses the executor; the enterprise gateway authenticates callers.
        Ok(()) | Err(tw_config::ValidationError::NoClients) => Ok(config),
        Err(error) => Err(error.to_string()),
    }
}

async fn enabled(
    state: &GatewayState,
    identity: &GatewayRequestIdentity,
) -> Option<HostedWebSettings> {
    let settings =
        HostedWebSettings::parse(state.dynamic_config.get(HostedWebSettings::KEY).await?).ok()?;
    settings.permits(identity).then_some(settings)
}

pub struct HostedWebRuntime {
    search: Mutex<Option<(WebToolsConfig, Arc<WebTools>)>>,
    executors: Mutex<Vec<(WebToolsConfig, Arc<WebTools>)>>,
    history: history::History,
    active: AtomicUsize,
}

impl Default for HostedWebRuntime {
    fn default() -> Self {
        Self {
            search: Mutex::default(),
            executors: Mutex::default(),
            history: history::History::default(),
            active: AtomicUsize::new(0),
        }
    }
}

impl HostedWebRuntime {
    fn enter(&self, max: u32) -> Result<ToolSlot<'_>, GatewayError> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < max as usize).then_some(active + 1)
            })
            .map_err(|_| GatewayError::rate_limited("web_tools_concurrency", 1))?;
        Ok(ToolSlot(&self.active))
    }

    async fn executor(&self, options: WebToolsConfig) -> Result<Arc<WebTools>, GatewayError> {
        let mut entries = self.executors.lock().await;
        if let Some((_, executor)) = entries.iter().find(|(settings, _)| *settings == options) {
            return Ok(executor.clone());
        }
        let executor = make_executor(&options)?;
        if entries.len() >= 8 {
            entries.remove(0);
        }
        entries.push((options, executor.clone()));
        Ok(executor)
    }

    /// Engine cooldowns and minimum intervals are shared across client domain
    /// filters. Only page readers require a dedicated executor for redirects.
    async fn search_executor(
        &self,
        options: &WebToolsConfig,
    ) -> Result<Arc<WebTools>, GatewayError> {
        let mut entry = self.search.lock().await;
        if let Some((stored, executor)) = &*entry
            && stored == options
        {
            return Ok(executor.clone());
        }
        let executor = make_executor(options)?;
        *entry = Some((options.clone(), executor.clone()));
        Ok(executor)
    }
}

fn make_executor(options: &WebToolsConfig) -> Result<Arc<WebTools>, GatewayError> {
    let config = core_config(options).map_err(GatewayError::TransformError)?;
    WebTools::new(&config)
        .map(Arc::new)
        .map_err(|_| GatewayError::ProviderError("Could not build the hosted web client".into()))
}

struct ToolSlot<'a>(&'a AtomicUsize);

impl Drop for ToolSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

fn error(detail: impl Into<String>) -> Msg {
    msg!("gw.web_tools.failed", detail = detail.into() => "Web tools: {detail}")
}

fn validate_domains(domains: &[String]) -> Result<(), Msg> {
    let options = WebToolsConfig {
        allowed_domains: domains.to_vec(),
        ..WebToolsConfig::default()
    };
    core_config(&options).map(|_| ()).map_err(error)
}

fn request_policy(
    mut options: WebToolsConfig,
    plan: &protocol::Plan,
) -> Result<WebToolsConfig, GatewayError> {
    let contains = |parent: &str, child: &str| {
        parent == child || child.strip_suffix(parent).is_some_and(|s| s.ends_with('.'))
    };
    let normalize = |names: &[String]| {
        names
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect::<Vec<_>>()
    };
    let requested = normalize(&plan.allowed);
    options.allowed_domains = match (options.allowed_domains.is_empty(), requested.is_empty()) {
        (true, _) => requested,
        (false, true) => normalize(&options.allowed_domains),
        (false, false) => {
            let configured = normalize(&options.allowed_domains);
            let overlap: std::collections::BTreeSet<_> = configured
                .iter()
                .flat_map(|a| {
                    requested.iter().filter_map(move |b| {
                        if contains(a, b) {
                            Some(b.clone())
                        } else if contains(b, a) {
                            Some(a.clone())
                        } else {
                            None
                        }
                    })
                })
                .collect();
            if overlap.is_empty() {
                return Err(GatewayError::PolicyBlocked(
                    "Web tool domains do not match the gateway allowlist".into(),
                ));
            }
            overlap.into_iter().collect()
        }
    };
    options.blocked_domains.extend(normalize(&plan.blocked));
    options.blocked_domains.sort_unstable();
    options.blocked_domains.dedup();
    Ok(options)
}

pub(super) struct HostedRequest<'a> {
    pub state: &'a GatewayState,
    pub headers: &'a HeaderMap,
    pub identity: &'a GatewayRequestIdentity,
    pub cancel: Option<EarlyCancelSlot>,
    pub arrived: Option<std::time::Instant>,
    pub body: &'a Bytes,
    pub surface: ClientSurface,
    pub path: &'a str,
    pub query: Option<&'a str>,
}

impl HostedRequest<'_> {
    pub(super) async fn execute(self) -> Option<Result<Response, GatewayErrorResponse>> {
        let lineage = self.identity.api_key_lineage_id.as_deref()?;
        let settings = enabled(self.state, self.identity).await;
        if settings.is_none() && !history::might_contain_hosted_items(self.body) {
            return None;
        }
        let mut body: Value = serde_json::from_slice(self.body).ok()?;
        let plan = if settings.is_some() {
            match protocol::recognize(self.surface.dialect, &body) {
                Ok(plan) => plan,
                Err(err) => return Some(Err(GatewayError::TransformError(err.text).into())),
            }
        } else {
            None
        };
        let runtime = &self.state.hosted_web;
        let standalone = plan.as_ref().and_then(|p| p.standalone(&body));
        let restored =
            runtime
                .history
                .restore(lineage, self.surface.dialect, &mut body, plan.is_some());
        let plan = match (restored, plan) {
            (Err(e), _) => return Some(Err(GatewayError::TransformError(e.text).into())),
            (Ok(false), None) => return None,
            (Ok(true), None) => {
                return Some(
                    self.forward(
                        &body,
                        self.headers.clone(),
                        self.cancel.clone(),
                        AiRound::Client,
                    )
                    .await,
                );
            }
            (Ok(_), Some(plan)) => plan,
        };
        Some(
            match tokio::time::timeout(
                Duration::from_secs(110),
                self.hosted(settings?.execution, plan, body, standalone, lineage),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(GatewayError::ProviderTimeout(
                    "Hosted web tools exceeded the turn deadline".into(),
                )
                .into()),
            },
        )
    }

    async fn forward(
        &self,
        body: &Value,
        headers: HeaderMap,
        cancel: Option<EarlyCancelSlot>,
        round: AiRound,
    ) -> Result<Response, GatewayErrorResponse> {
        let bytes = serde_json::to_vec(body).expect("JSON request");
        if bytes.len() > MAX_BODY.min(resources::policy().max_body_bytes) {
            return Err(GatewayError::TransformError(
                "Hosted web-tool conversation exceeds the request limit".into(),
            )
            .into());
        }
        let mut budget = resources::body_budget().reserve();
        if !budget.grow(bytes.len().saturating_mul(resources::BODY_HEADROOM)) {
            return Err(GatewayError::rate_limited("gateway_memory", 1).into());
        }
        run(
            self.state.clone(),
            headers,
            self.identity.clone(),
            cancel,
            self.arrived,
            budget.wrap(bytes.into()),
            self.surface,
            self.path,
            self.query,
            round,
        )
        .await
    }

    async fn forward_to_upstream_tools(&self) -> Result<Response, GatewayErrorResponse> {
        let mut body: Value = serde_json::from_slice(self.body)
            .map_err(|error| GatewayError::TransformError(error.to_string()))?;
        if let Some(lineage) = self.identity.api_key_lineage_id.as_deref() {
            self.state
                .hosted_web
                .history
                .restore(lineage, self.surface.dialect, &mut body, false)
                .map_err(|error| GatewayError::TransformError(error.text))?;
        }
        let mut headers = self.headers.clone();
        headers.insert("x-trace-id", request_id_header(&Uuid::new_v4().to_string()));
        self.forward(&body, headers, None, AiRound::HostedContinuation)
            .await
    }

    async fn hosted(
        &self,
        options: WebToolsConfig,
        plan: protocol::Plan,
        mut body: Value,
        standalone: Option<WebToolRequest>,
        lineage: &str,
    ) -> Result<Response, GatewayErrorResponse> {
        let model = body["model"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| GatewayError::TransformError("Missing 'model' field".into()))?
            .to_owned();
        let cfg = super::ModelDiscovery::load(self.state).await;
        let mapped = cfg.resolve(&model);
        let id = resolve_trace_id(self.headers);
        let caller = Caller::of(self.identity, &id, &mapped);
        let guards = self.state.guards.load_full();
        let stream = body["stream"] == true;
        let fallback = options.fallback_to_upstream;
        let max_concurrent = options.max_concurrent;
        let max_uses = plan
            .max_uses
            .unwrap_or(options.max_tool_calls)
            .min(options.max_tool_calls) as usize;
        let runtime = &self.state.hosted_web;
        let scoped = request_policy(options.clone(), &plan)?;
        let invocation = Invocation {
            runtime,
            audit: &self.state.audit,
            guards: &guards,
            caller: &caller,
            options: &options,
            scoped: &scoped,
            lineage,
            max_concurrent,
            diagnostics: false,
        };

        if let Some(request) = standalone {
            super::pipeline::run_preflight_stages(
                self.state,
                self.identity,
                &id,
                &mapped,
                AiRound::Client,
            )
            .await?;
            if self.state.router.load().route(&mapped).is_none() {
                return Err(GatewayError::TransformError("No route for this model".into()).into());
            }
            if let Some(cancel) = &self.cancel {
                cancel.request(&id, None);
                cancel.model(&mapped);
            }
            let mut call = protocol::Call {
                native_id: Uuid::new_v4().simple().to_string(),
                id: format!("toolu_tw_{}", Uuid::new_v4().simple()),
                request,
            };
            let result = match invocation.tool(&mut call).await {
                Ok(result) => result,
                Err(GatewayError::ProviderError(_)) if fallback => {
                    return self.forward_to_upstream_tools().await;
                }
                Err(error) => return Err(error.into()),
            };
            runtime
                .history
                .remember(lineage, plan.dialect, &call, &result);
            return render(
                plan.dialect,
                stream,
                &id,
                &protocol::standalone_response(&model, &call, &result),
            );
        }

        plan.rewrite(&mut body)
            .map_err(|e| GatewayError::TransformError(e.text))?;
        let mut usage = Usage::default();
        let mut exchanges = Vec::new();
        let mut retained_bytes = 0usize;
        let mut sources = Vec::new();
        let mut used = 0;
        let key = if plan.dialect == Dialect::Anthropic {
            "content"
        } else {
            "output"
        };
        let mut headers = self.headers.clone();
        headers.insert("x-trace-id", request_id_header(&id));
        loop {
            let response = self
                .forward(
                    &body,
                    headers.clone(),
                    (used == 0).then(|| self.cancel.clone()).flatten(),
                    if used == 0 {
                        AiRound::Client
                    } else {
                        AiRound::HostedContinuation
                    },
                )
                .await?;
            if !response.status().is_success() {
                return Ok(response);
            }
            let bytes = to_bytes(response.into_body(), MAX_BODY)
                .await
                .map_err(|_| {
                    GatewayError::ProviderInvalidResponse(
                        "Web-tool model response exceeded the buffer or ended prematurely".into(),
                    )
                })?;
            let mut answer: Value = serde_json::from_slice(&bytes).map_err(|_| {
                GatewayError::ProviderInvalidResponse("Web-tool model response must be JSON".into())
            })?;
            sum_usage(plan.dialect, &answer, &mut usage);
            let mut calls = plan
                .calls(&answer)
                .map_err(|e| GatewayError::ProviderInvalidResponse(e.text))?;
            if used + calls.len() > max_uses {
                return Err(GatewayError::PolicyBlocked(
                    "Hosted web-tool call limit exceeded".into(),
                )
                .into());
            }
            let mut results = Vec::with_capacity(calls.len());
            for call in &mut calls {
                let result = match invocation.tool(call).await {
                    Ok(result) => result,
                    Err(GatewayError::ProviderError(_))
                        if used == 0 && results.is_empty() && fallback =>
                    {
                        return self.forward_to_upstream_tools().await;
                    }
                    Err(error) => return Err(error.into()),
                };
                runtime
                    .history
                    .remember(lineage, plan.dialect, call, &result);
                sources.extend(
                    result
                        .sources
                        .iter()
                        .map(|source| (source.url.clone(), source.title.clone())),
                );
                results.push((call.clone(), result));
            }
            used += results.len();
            let items = answer[key].as_array().ok_or_else(|| {
                GatewayError::ProviderInvalidResponse(
                    "Web-tool model response has no content".into(),
                )
            })?;
            let mut external = Vec::new();
            for item in items {
                let id = if plan.dialect == Dialect::Anthropic {
                    &item["id"]
                } else {
                    &item["call_id"]
                };
                if let Some((call, result)) = results.iter().find(|(call, _)| *id == call.id) {
                    exchanges.extend(protocol::native_blocks(plan.dialect, call, result));
                } else if matches!(
                    item["type"].as_str(),
                    Some("tool_use" | "function_call" | "custom_tool_call")
                ) {
                    external.push(item.clone());
                } else {
                    exchanges.push(item.clone());
                }
            }
            retained_bytes = retained_bytes.saturating_add(bytes.len());
            if retained_bytes > MAX_BODY {
                return Err(GatewayError::ProviderInvalidResponse(
                    "Hosted web-tool exchanges exceed the response limit".into(),
                )
                .into());
            }
            if results.is_empty() || !external.is_empty() {
                exchanges.extend(external);
                answer[key] = json!(exchanges);
                answer["usage"] = if plan.dialect == Dialect::Anthropic {
                    tw_dialect::anthropic::response::usage_json(&usage)
                } else {
                    tw_dialect::responses::response::usage_json(&usage)
                };
                if plan.dialect == Dialect::Responses {
                    citations(&mut answer, &sources);
                }
                return render(plan.dialect, stream, &id, &answer);
            }
            plan.append(&mut body, &answer, &results)
                .map_err(|e| GatewayError::TransformError(e.text))?;
            headers.insert("x-trace-id", request_id_header(&Uuid::new_v4().to_string()));
        }
    }
}

struct Invocation<'a> {
    runtime: &'a HostedWebRuntime,
    audit: &'a AuditLogger,
    guards: &'a Guards,
    caller: &'a Caller,
    options: &'a WebToolsConfig,
    scoped: &'a WebToolsConfig,
    lineage: &'a str,
    max_concurrent: u32,
    diagnostics: bool,
}

impl Invocation<'_> {
    async fn tool(&self, call: &mut protocol::Call) -> Result<WebToolResult, GatewayError> {
        self.execute(&mut call.request).await
    }

    async fn execute(&self, request: &mut WebToolRequest) -> Result<WebToolResult, GatewayError> {
        let audit = self.audit;
        let text = serde_json::to_string(request).expect("web-tool request JSON");
        let wrapped = json!({"messages":[{"role":"user", "content":text}]}).to_string();
        let screening = self
            .guards
            .content
            .screen(Dialect::Anthropic, wrapped.as_bytes());
        crate::content_filter::record(audit, self.caller, self.guards.content.mode, &screening);
        if let Some(hit) = screening.refusal() {
            return Err(crate::content_filter::refusal(hit, &self.guards.redaction));
        }
        let bytes = screening
            .body
            .map_or_else(|| wrapped.into_bytes(), |body| body.to_vec());
        let shape = Shape::Generate(Dialect::Anthropic);
        let (findings, ledger) = self.guards.redaction.look(&bytes, shape);
        crate::redaction::record(audit, self.caller, &self.guards.redaction, &findings);
        let (protected, _) = self.guards.redaction.replace(bytes, &ledger, shape);
        let wrapped: Value = serde_json::from_slice(&protected).map_err(|_| {
            GatewayError::PolicyBlocked("Web-tool input changed by request protection".into())
        })?;
        *request = serde_json::from_str(
            wrapped["messages"][0]["content"]
                .as_str()
                .unwrap_or_default(),
        )
        .map_err(|_| {
            GatewayError::PolicyBlocked("Web-tool input changed by request protection".into())
        })?;
        let runtime = self.runtime;
        let _slot = runtime.enter(self.max_concurrent)?;
        let (action, executor) = match request {
            WebToolRequest::Search {
                allowed_domains,
                blocked_domains,
                ..
            } => {
                *allowed_domains = self.scoped.allowed_domains.clone();
                *blocked_domains = self.scoped.blocked_domains.clone();
                ("search", runtime.search_executor(self.options).await?)
            }
            WebToolRequest::OpenPage { .. } => {
                ("open_page", runtime.executor(self.scoped.clone()).await?)
            }
            WebToolRequest::FindInPage { .. } => {
                ("find_in_page", runtime.executor(self.scoped.clone()).await?)
            }
        };
        let result = executor.execute(self.lineage, request).await;
        let event = if self.diagnostics {
            "gateway.web_tool_test"
        } else {
            "gateway.web_tool"
        };
        audit.log(self.caller.audit(event).resource("thinkwatch-web").detail(json!({
            "trace_id": self.caller.trace_id, "action": action, "success": result.is_ok(),
            "cached": result.as_ref().is_ok_and(|r| r.cached), "sources": result.as_ref().map_or(0, |r| r.sources.len()),
        })));
        result.map_err(|error| {
            GatewayError::ProviderError(if self.diagnostics {
                diagnostic_text(self.guards, &error.text)
            } else {
                "Hosted web tool could not retrieve public results".into()
            })
        })
    }
}

fn diagnostic_text(guards: &Guards, text: &str) -> String {
    guards.redaction.mask(text).chars().take(700).collect()
}

fn citations(answer: &mut Value, sources: &[(String, String)]) {
    for item in answer["output"].as_array_mut().into_iter().flatten() {
        for part in item["content"].as_array_mut().into_iter().flatten() {
            if part["type"] != "output_text" {
                continue;
            }
            let text = part["text"].as_str().unwrap_or_default();
            let mut annotations = part["annotations"].as_array().cloned().unwrap_or_default();
            for (url, title) in sources {
                if let Some(start) = text.find(url)
                    && !annotations.iter().any(|a| a["url"] == *url)
                {
                    let start = text[..start].chars().count();
                    let end = start + url.chars().count();
                    annotations.push(json!({"type":"url_citation", "url":url, "title":title, "start_index":start, "end_index":end}));
                }
            }
            part["annotations"] = json!(annotations);
        }
    }
}

fn sum_usage(dialect: Dialect, answer: &Value, usage: &mut Usage) {
    let next = if dialect == Dialect::Anthropic {
        tw_dialect::anthropic::response::usage(&answer["usage"])
    } else {
        tw_dialect::responses::response::usage(&answer["usage"])
    };
    usage.input = usage.input.saturating_add(next.input);
    usage.output = usage.output.saturating_add(next.output);
    usage.cache_read = usage.cache_read.saturating_add(next.cache_read);
    usage.cache_write = usage.cache_write.saturating_add(next.cache_write);
    usage.reasoning = usage.reasoning.saturating_add(next.reasoning);
    usage.cache_1h |= next.cache_1h;
}

fn render(
    dialect: Dialect,
    stream: bool,
    id: &str,
    answer: &Value,
) -> Result<Response, GatewayErrorResponse> {
    let (kind, body) = if stream {
        ("text/event-stream", wire::stream(dialect, answer))
    } else {
        ("application/json", answer.to_string().into_bytes())
    };
    let mut budget = resources::body_budget().reserve();
    if body.len() > MAX_BODY || !budget.grow(body.len().saturating_mul(resources::BODY_HEADROOM)) {
        return Err(GatewayError::rate_limited("gateway_memory", 1).into());
    }
    let mut response = ([(header::CONTENT_TYPE, kind)], budget.wrap(body.into())).into_response();
    response
        .headers_mut()
        .insert("x-metadata-request-id", request_id_header(id));
    if stream {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_execution_requires_an_explicit_key_lineage() {
        assert!(HostedWebSettings::parse(json!({})).is_ok());
        assert!(HostedWebSettings::parse(json!({"execution": {"mode":"core"}})).is_err());
        let id = Uuid::new_v4();
        let cfg =
            HostedWebSettings::parse(json!({"execution":{"mode":"core"},"enabled_lineages":[id]}))
                .unwrap();
        assert!(!cfg.permits(&GatewayRequestIdentity::default()));
        assert!(cfg.permits(&GatewayRequestIdentity {
            api_key_lineage_id: Some(id.to_string()),
            ..Default::default()
        }));
    }

    #[test]
    fn hosted_requests_can_only_narrow_admin_domains() {
        let cfg = WebToolsConfig {
            allowed_domains: vec!["Example.org".into()],
            ..Default::default()
        };
        let mut plan = protocol::Plan {
            dialect: Dialect::Responses,
            search: true,
            fetch: false,
            allowed: vec!["docs.example.org".into(), "test.org".into()],
            blocked: vec!["staff.example.org".into()],
            max_uses: None,
        };
        let effective = request_policy(cfg.clone(), &plan).unwrap();
        assert_eq!(effective.allowed_domains, ["docs.example.org"]);
        assert_eq!(effective.blocked_domains, ["staff.example.org"]);
        plan.allowed = vec!["notexample.org".into()];
        assert!(request_policy(cfg, &plan).is_err());
    }
}
