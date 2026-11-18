use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Semaphore};
use tw_config::{ModelSpec, ReasoningEffort, ReasoningLevel, ReportedSpec};
use uuid::Uuid;

use super::transport::Shape;
use super::{GatewayRequestIdentity, GatewayState};
use crate::call_ctx::{CallCtx, substitute_template};
use crate::model_mapping::ModelMapper;
use crate::router::{ModelRouter, RouteEntry};
use think_watch_common::resources;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelDiscovery {
    aliases: BTreeMap<String, Alias>,
    profiles: BTreeMap<Uuid, BTreeMap<String, ModelSpec>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Alias {
    target: String,
    #[serde(default)]
    hide_original: bool,
}

impl ModelDiscovery {
    pub const KEY: &str = "gateway.model_discovery";

    pub fn parse(value: Value) -> Result<Self, String> {
        let cfg: Self = serde_json::from_value(value).map_err(|e| e.to_string())?;
        let valid_name = |name: &str| {
            !name.is_empty()
                && name.len() <= 256
                && !name.contains(char::is_whitespace)
                && !name.contains('*')
                && !name.contains(char::is_control)
        };
        if cfg.aliases.len() > 256 || cfg.profiles.values().map(BTreeMap::len).sum::<usize>() > 1024
        {
            return Err("Model discovery supports at most 256 aliases and 1024 profiles".into());
        }
        for (name, alias) in &cfg.aliases {
            if !valid_name(name)
                || !valid_name(&alias.target)
                || cfg.aliases.contains_key(&alias.target)
            {
                return Err(
                    "Aliases require exact model IDs and must point directly at a routed model"
                        .into(),
                );
            }
        }
        for (provider, models) in &cfg.profiles {
            let provider = provider.to_string();
            for (name, spec) in models {
                if !valid_name(name) {
                    return Err("Model capability profiles require exact upstream model IDs".into());
                }
                tw_config::check_model_spec(&provider, name, Some(spec))
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(cfg)
    }

    pub fn validate_aliases(
        &self,
        router: &ModelRouter,
        existing: &std::collections::HashSet<String>,
    ) -> Result<(), String> {
        for (name, alias) in &self.aliases {
            if existing.contains(name) || router.has_model_name(name) {
                return Err(format!("Alias '{name}' conflicts with an existing model"));
            }
            if !router.can_route_alias_target(&alias.target) {
                return Err(format!(
                    "Alias '{name}' target '{}' has no active route",
                    alias.target
                ));
            }
        }
        Ok(())
    }

    pub(super) async fn load(state: &GatewayState) -> Self {
        let mut cfg = state
            .dynamic_config
            .get(Self::KEY)
            .await
            .and_then(|v| Self::parse(v).ok())
            .unwrap_or_default();
        let router = state.router.load();
        // A model created after an alias was saved always keeps its name.
        cfg.aliases.retain(|name, alias| {
            !router.has_model_name(name) && router.can_route_alias_target(&alias.target)
        });
        cfg
    }

    pub(super) fn resolve(&self, model: &str) -> String {
        self.aliases
            .get(model)
            .map_or_else(|| model.into(), |a| a.target.clone())
    }

    pub(super) fn mapper(&self) -> ModelMapper {
        ModelMapper::from_config(
            self.aliases
                .iter()
                .map(|(name, a)| (name.clone(), a.target.clone()))
                .collect(),
        )
    }

    pub(super) fn apply_visibility(
        &self,
        models: &mut Vec<String>,
        router: &ModelRouter,
        identity: &GatewayRequestIdentity,
    ) {
        for (name, alias) in &self.aliases {
            if identity.may_call(&alias.target) && router.route(&alias.target).is_some() {
                models.push(name.clone());
            }
        }
        models.retain(|name| {
            !self
                .aliases
                .values()
                .any(|a| a.hide_original && a.target == *name)
        });
        models.sort_unstable();
        models.dedup();
    }

    pub(super) fn profile(&self, provider: Uuid, model: &str) -> Option<&ModelSpec> {
        self.profiles.get(&provider)?.get(model)
    }
}

struct ModelMetadata {
    reported: ReportedSpec,
    native: Option<Value>,
}

type Metadata = BTreeMap<String, ModelMetadata>;
type MetadataKey = (String, String);
type MetadataEntry = (Instant, Arc<Metadata>);

#[derive(Default)]
pub(crate) struct MetadataCache(Mutex<HashMap<MetadataKey, MetadataEntry>>);

impl MetadataCache {
    async fn lookup(&self, key: &MetadataKey) -> Option<Arc<Metadata>> {
        let mut cache = self.0.lock().await;
        cache.retain(|_, (expires, _)| *expires > Instant::now());
        cache.get(key).map(|(_, specs)| specs.clone())
    }

    async fn store(&self, key: MetadataKey, specs: Arc<Metadata>, ttl: Duration) {
        if ttl.is_zero() {
            return;
        }
        let mut cache = self.0.lock().await;
        if cache.len() < 16 {
            cache.insert(key, (Instant::now() + ttl, specs));
        }
    }
}

static CATALOG_FETCHES: Semaphore = Semaphore::const_new(8);

fn metadata_url(
    base: &str,
    dialect: tw_dialect::ir::Dialect,
    version: Option<&str>,
) -> (String, bool) {
    use tw_dialect::ir::Dialect;
    let query = version.map(|v| format!("client_version={v}"));
    let path = if dialect == Dialect::Gemini {
        "/v1beta/models"
    } else {
        "/v1/models"
    };
    let Some(versioned) = query
        .as_deref()
        .filter(|_| matches!(dialect, Dialect::Chat | Dialect::Responses))
    else {
        return (tw_dialect::url::upstream_url(base, path, None), false);
    };
    let parsed = reqwest::Url::parse(base).ok();
    let known = |host, paths: &[&str]| {
        parsed.as_ref().is_some_and(|url| {
            url.scheme() == "https"
                && url.host_str() == Some(host)
                && url.port_or_known_default() == Some(443)
                && paths.contains(&url.path().trim_end_matches('/'))
                && url.username().is_empty()
                && url.password().is_none()
        })
    };
    if known("api.openai.com", &["", "/v1"]) {
        return (
            tw_dialect::url::upstream_url(
                tw_gateway::chatgpt::BASE_URL,
                "/models",
                Some(versioned),
            ),
            true,
        );
    }
    if known("chatgpt.com", &["/backend-api/codex"]) {
        return (
            tw_dialect::url::upstream_url(base, "/models", Some(versioned)),
            false,
        );
    }
    (
        tw_dialect::url::upstream_url(base, path, Some(versioned)),
        false,
    )
}

async fn metadata(route: &RouteEntry, ctx: &CallCtx, version: Option<&str>) -> Arc<Metadata> {
    let upstream = &route.upstream;
    if !matches!(&upstream.shape, Shape::Standard) {
        return Default::default();
    }
    let dialect = route.protocol.dialect();
    let (url, official_cross_origin) = metadata_url(&upstream.base_url, dialect, version);
    let key = (
        upstream.negotiation_provider(route.provider_id, ctx).name,
        url.clone(),
    );
    if let Some(specs) = upstream.model_metadata.lookup(&key).await {
        return specs;
    }
    let Ok(_slot) = CATALOG_FETCHES.try_acquire() else {
        return Default::default();
    };
    let fetch = |url: String, official_cross_origin: bool| async move {
        let mut req = upstream.client.get(url);
        for (name, template) in &upstream.headers {
            // The fixed OpenAI catalog host only needs OpenAI auth, never
            // headers intended solely for the configured origin.
            if official_cross_origin && !name.eq_ignore_ascii_case("authorization") {
                continue;
            }
            req = req.header(name, substitute_template(template, &ctx.attrs));
        }
        if route.protocol.dialect() == tw_dialect::ir::Dialect::Anthropic
            && !upstream
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("anthropic-version"))
        {
            req = req.header("anthropic-version", tw_dialect::official::ANTHROPIC_VERSION);
        }
        let resp = req.send().await.ok()?.error_for_status().ok()?;
        let ttl = metadata_ttl(resp.headers());
        let (body, _memory): (Value, _) = resources::read_json(resp, 1024 * 1024).await.ok()?;
        let rows = body
            .get("data")
            .or_else(|| body.get("models"))?
            .as_array()?;
        Some((
            rows.iter()
                .take(4096)
                .filter_map(|item| {
                    let native = is_native(item);
                    let name = if native {
                        item["slug"].as_str()?
                    } else {
                        item["id"].as_str().or_else(|| item["name"].as_str())?
                    };
                    let id = name.strip_prefix("models/").unwrap_or(name);
                    (id.len() <= 256).then(|| {
                        (
                            id.to_owned(),
                            ModelMetadata {
                                reported: reported(item),
                                native: native.then(|| item.clone()),
                            },
                        )
                    })
                })
                .collect::<Metadata>(),
            ttl,
        ))
    };
    let (specs, ttl) = tokio::time::timeout(Duration::from_secs(3), async {
        let result = fetch(url, official_cross_origin).await;
        if result.is_none()
            && (official_cross_origin
                || version.is_some()
                    && matches!(
                        dialect,
                        tw_dialect::ir::Dialect::Chat | tw_dialect::ir::Dialect::Responses
                    ))
        {
            let (ordinary_url, _) = metadata_url(&upstream.base_url, dialect, None);
            fetch(ordinary_url, false).await
        } else {
            result
        }
    })
    .await
    .ok()
    .flatten()
    .unwrap_or((Metadata::new(), Duration::from_secs(15)));
    let specs = Arc::new(specs);
    upstream.model_metadata.store(key, specs.clone(), ttl).await;
    specs
}

fn metadata_ttl(headers: &reqwest::header::HeaderMap) -> Duration {
    use reqwest::header::{AGE, CACHE_CONTROL};
    let mut ttl = 60;
    for directive in headers
        .get_all(CACHE_CONTROL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
    {
        if directive.eq_ignore_ascii_case("no-cache") || directive.eq_ignore_ascii_case("no-store")
        {
            return Duration::ZERO;
        }
        if let Some((key, value)) = directive.split_once('=')
            && key.trim().eq_ignore_ascii_case("max-age")
            && let Ok(seconds) = value.trim().trim_matches('"').parse::<u64>()
        {
            ttl = ttl.min(seconds);
        }
    }
    let age = headers
        .get(AGE)
        .and_then(|v| v.to_str().ok()?.parse().ok())
        .unwrap_or(0);
    Duration::from_secs(ttl.saturating_sub(age))
}

fn is_native(model: &Value) -> bool {
    model["slug"].as_str().is_some_and(|slug| !slug.is_empty())
        && model["display_name"].is_string()
        && model["supported_reasoning_levels"].is_array()
        && model["shell_type"].is_string()
        && model["truncation_policy"].is_object()
        && (model["base_instructions"].is_string() || model["model_messages"].is_object())
}

fn reported(model: &Value) -> ReportedSpec {
    let effort = model["supported_reasoning_levels"]
        .as_array()
        .map(|levels| (levels, &model["default_reasoning_level"]))
        .or_else(|| {
            model
                .pointer("/effort/supported_levels")?
                .as_array()
                .map(|levels| (levels, &model["effort"]["default_level"]))
        });
    let reasoning_effort = effort.and_then(|(levels, default)| {
        let supported_levels: Vec<_> = levels
            .iter()
            .filter_map(|v| v.as_str().or_else(|| v["effort"].as_str()))
            .filter_map(ReasoningLevel::from_slug)
            .collect();
        if !levels.is_empty() && supported_levels.is_empty() {
            return None;
        }
        let default_level = default
            .as_str()
            .and_then(ReasoningLevel::from_slug)
            .filter(|v| supported_levels.contains(v));
        Some(ReasoningEffort {
            supported_levels,
            default_level,
        })
    });
    ReportedSpec {
        context_window: model["context_window"]
            .as_u64()
            .or_else(|| model["inputTokenLimit"].as_u64())
            .filter(|n| *n > 0),
        max_output_tokens: model["max_output_tokens"]
            .as_u64()
            .or_else(|| model["outputTokenLimit"].as_u64())
            .filter(|n| *n > 0),
        reasoning: model["supports_reasoning"]
            .as_bool()
            .or_else(|| model["reasoning"].as_bool())
            .or_else(|| model["thinking"].as_bool()),
        reasoning_effort,
        image_input: model["input_modalities"]
            .as_array()
            .map(|a| a.iter().any(|v| v == "image")),
    }
}

pub(super) async fn codex_catalog(
    router: &ModelRouter,
    cfg: &ModelDiscovery,
    names: &[String],
    identity: &GatewayRequestIdentity,
    version: Option<&str>,
) -> Value {
    let ctx = CallCtx::new(None, identity.user_id.clone(), identity.user_email.clone());
    let mut routes = BTreeMap::new();
    for route in names
        .iter()
        .filter_map(|name| router.route(&cfg.resolve(name)))
        .flatten()
    {
        let key = (
            route.provider_id,
            route.protocol.dialect().slug().to_owned(),
        );
        routes.entry(key).or_insert_with(|| route.clone());
    }
    let mut requests = stream::iter(routes.into_iter().take(64).map(|(key, route)| {
        let ctx = ctx.clone();
        let version = version.map(str::to_owned);
        async move { (key, metadata(&route, &ctx, version.as_deref()).await) }
    }))
    .buffer_unordered(8);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    let mut specs = HashMap::new();
    while let Ok(Some((key, reported))) = tokio::time::timeout_at(deadline, requests.next()).await {
        specs.insert(key, reported);
    }

    let models: Vec<_> = names
        .iter()
        .map(|name| {
            let model = cfg.resolve(name);
            let mut capabilities = Vec::new();
            let mut native = Vec::new();
            for route in router.route(&model).into_iter().flatten() {
                let upstream_model = route.upstream_model.as_deref().unwrap_or(&model);
                let dialect = route.protocol.dialect();
                let metadata = specs
                    .get(&(route.provider_id, dialect.slug().to_owned()))
                    .and_then(|m| m.get(upstream_model));
                native.push(metadata.and_then(|metadata| metadata.native.as_ref()));
                let mut spec = metadata
                    .map(|metadata| metadata.reported.clone())
                    .unwrap_or_default();
                if let Some(manual) = cfg.profile(route.provider_id, upstream_model) {
                    spec.context_window =
                        manual.context_window.map(u64::from).or(spec.context_window);
                    spec.max_output_tokens = manual
                        .max_output_tokens
                        .map(u64::from)
                        .or(spec.max_output_tokens);
                    spec.reasoning = manual.reasoning.or(spec.reasoning);
                    spec.image_input = manual.image_input.or(spec.image_input);
                    spec.reasoning_effort =
                        manual.reasoning_effort.clone().or(spec.reasoning_effort);
                }
                if spec.reasoning == Some(false) {
                    spec.reasoning_effort = Some(ReasoningEffort {
                        supported_levels: Vec::new(),
                        default_level: None,
                    });
                }
                if let Some(effort) = &mut spec.reasoning_effort
                    && dialect != tw_dialect::ir::Dialect::Responses
                {
                    effort
                        .supported_levels
                        .retain(|level| tw_dialect::think::parse_openai(level.slug()).is_some());
                }
                capabilities.push(spec);
            }
            let same_native = native.first().copied().flatten().filter(|first| {
                native
                    .iter()
                    .all(|candidate| candidate.is_some_and(|definition| definition == *first))
            });
            project(name, &capabilities, same_native)
        })
        .collect();
    json!({"models": models})
}

fn project(name: &str, candidates: &[ReportedSpec], native: Option<&Value>) -> Value {
    let profiles: Option<Vec<_>> = candidates
        .iter()
        .map(|c| c.reasoning_effort.as_ref())
        .collect();
    let levels: Vec<_> = profiles.as_ref().map_or_else(Vec::new, |profiles| {
        ReasoningLevel::ALL
            .iter()
            .copied()
            .filter(|level| {
                !profiles.is_empty() && profiles.iter().all(|p| p.supported_levels.contains(level))
            })
            .collect()
    });
    let default = profiles.as_ref().and_then(|p| {
        let first = p.first()?.default_level?;
        (levels.contains(&first) && p.iter().all(|p| p.default_level == Some(first)))
            .then_some(first)
    });
    let image = !candidates.is_empty() && candidates.iter().all(|s| s.image_input == Some(true));
    let mut out = native.cloned().unwrap_or_else(|| json!({
        "slug": name, "display_name": name, "description": "Available through ThinkWatch",
        "base_instructions": "You are a coding assistant. Inspect relevant files before editing, use the provided tools when needed, and explain changes and verification clearly.",
        "shell_type": "shell_command", "visibility": "list", "supported_in_api": true,
        "priority": 0, "support_verbosity": false, "experimental_supported_tools": [],
        "truncation_policy": {"mode":"tokens", "limit":10000}, "upgrade": null,
        "supports_search_tool": false, "node_repl_disabled": true,
        "service_tiers": [], "additional_speed_tiers": [], "default_service_tier": null,
    }));
    if out["slug"] != name {
        out["display_name"] = json!(name);
        out["upgrade"] = Value::Null;
    }
    out["slug"] = json!(name);
    out["visibility"] = json!("list");
    out["supported_in_api"] = json!(true);
    let native_levels = out["supported_reasoning_levels"].as_array();
    out["supported_reasoning_levels"] = json!(
        levels
            .iter()
            .map(|level| {
                native_levels
                    .and_then(|values| values.iter().find(|v| v["effort"] == level.slug()).cloned())
                    .unwrap_or_else(|| json!({"effort":level.slug(), "description":level.slug()}))
            })
            .collect::<Vec<_>>()
    );
    out["default_reasoning_level"] = json!(default.map(ReasoningLevel::slug));
    out["input_modalities"] = if image {
        json!(["text", "image"])
    } else {
        json!(["text"])
    };
    if let Some(window) = candidates
        .iter()
        .map(|s| s.context_window)
        .collect::<Option<Vec<_>>>()
        .and_then(|v| v.into_iter().min())
    {
        if out["context_window"] != window || !out["max_context_window"].is_u64() {
            out["max_context_window"] = json!(window);
        }
        out["context_window"] = json!(window);
    }
    if let Some(maximum) = candidates
        .iter()
        .map(|s| s.max_output_tokens)
        .collect::<Option<Vec<_>>>()
        .and_then(|v| v.into_iter().min())
    {
        out["max_output_tokens"] = json!(maximum);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_only_capabilities_shared_by_every_route() {
        let a = reported(
            &json!({"context_window":10000,"supported_reasoning_levels":["low","high"],"default_reasoning_level":"high","input_modalities":["text","image"]}),
        );
        let b = reported(&json!({"context_window":12000,"effort":{"supported_levels":["high"]}}));
        let answer = project("alias", &[a.clone(), b], None);
        assert_eq!(answer["context_window"], 10000);
        assert_eq!(answer["supported_reasoning_levels"][0]["effort"], "high");
        assert_eq!(answer["input_modalities"], json!(["text"]));
        assert!(answer["default_reasoning_level"].is_null());
        assert_eq!(
            project("alias", &[a, ReportedSpec::default()], None)["supported_reasoning_levels"],
            json!([])
        );
    }
}
