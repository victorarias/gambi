use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use rmcp::{
    ErrorData as McpError, Json, RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestMethod, CallToolRequestParams, CallToolResult, GetPromptRequestParams,
        GetPromptResult, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
        ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResult,
        ResourceContents, ServerCapabilities, ServerInfo, Tool,
    },
    schemars::JsonSchema,
    service::RequestContext,
    tool, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::auth::TokenState;
use crate::config::{ConfigStore, ExposureMode};
use crate::execution::{self, ExecuteOutput};
use crate::namespacing::{
    namespaced_resource_uri, parse_namespaced_resource_uri, split_namespaced,
};
use crate::upstream;

const AGGREGATED_PAGE_SIZE: usize = 100;
const COMPACT_DESCRIPTION_LIMIT: usize = 180;

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ListedServer {
    name: String,
    url: String,
    exposure_mode: String,
    policy_mode: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ListServersOutput {
    servers: Vec<ListedServer>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct UpstreamToolOutput {
    namespaced_name: String,
    server: String,
    upstream_name: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct UpstreamFailureOutput {
    server: String,
    error: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct UpstreamDiscoveryOutput {
    tools: Vec<UpstreamToolOutput>,
    failures: Vec<UpstreamFailureOutput>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct ExecuteRequest {
    code: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ExecuteResponse {
    result: serde_json::Value,
    tool_calls: usize,
    elapsed_ms: u128,
    stdout: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct HelpRequest {
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    tool: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct HelpToolSummaryOutput {
    namespaced_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    policy_level: String,
    policy_source: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct HelpServerOutput {
    name: String,
    url: String,
    exposure_mode: String,
    tool_count: usize,
    tools: Vec<HelpToolSummaryOutput>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct HelpToolDetailOutput {
    namespaced_name: String,
    server: String,
    upstream_name: String,
    description: String,
    policy_level: String,
    policy_source: String,
    input_schema: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_schema: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct HelpResponse {
    usage: String,
    servers: Vec<HelpServerOutput>,
    failures: Vec<UpstreamFailureOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<HelpToolDetailOutput>,
}

#[derive(Debug, Clone)]
struct McpServer {
    store: ConfigStore,
    upstream: upstream::UpstreamManager,
    exec_enabled: bool,
    tool_router: ToolRouter<Self>,
}

impl McpServer {
    fn new(store: ConfigStore, upstream: upstream::UpstreamManager, exec_enabled: bool) -> Self {
        Self {
            store,
            upstream,
            exec_enabled,
            tool_router: Self::tool_router(),
        }
    }

    fn local_tools(&self) -> Vec<Tool> {
        let mut tools = self.tool_router.list_all();
        if !self.exec_enabled {
            tools.retain(|tool| {
                let name = tool.name.as_ref();
                name != "gambi_execute" && name != "gambi_execute_escalated"
            });
        }
        tools
    }
}

#[tool_router(router = tool_router)]
impl McpServer {
    #[tool(
        name = "gambi_list_servers",
        description = "List configured upstream servers"
    )]
    async fn gambi_list_servers(&self) -> Result<Json<ListServersOutput>, String> {
        list_servers_output(&self.store).await
    }

    #[tool(
        name = "gambi_list_upstream_tools",
        description = "Discover upstream tool names and discovery failures for troubleshooting"
    )]
    async fn gambi_list_upstream_tools(&self) -> Result<Json<UpstreamDiscoveryOutput>, String> {
        list_upstream_tools_output(&self.store, &self.upstream).await
    }

    #[tool(
        name = "gambi_help",
        description = "Explain upstream MCP capabilities and return full tool metadata on demand (execute-only workflow)"
    )]
    async fn gambi_help(
        &self,
        Parameters(params): Parameters<HelpRequest>,
    ) -> Result<Json<HelpResponse>, String> {
        gambi_help_output(&self.store, &self.upstream, params).await
    }

    #[tool(
        name = "gambi_execute",
        description = "Safe execution path: run Python workflow in gambi (Monty runtime) with policy-aware upstream tool-call bridge. Escalated tools are blocked with ESCALATION_REQUIRED."
    )]
    async fn gambi_execute(
        &self,
        Parameters(params): Parameters<ExecuteRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<ExecuteResponse>, String> {
        if !self.exec_enabled {
            return Err("gambi_execute is disabled for this server instance".to_string());
        }

        let output: ExecuteOutput = execution::run_code_execution(
            &params.code,
            &self.store,
            &self.upstream,
            context,
            execution::ExecutionPolicy::Safe,
        )
        .await
        .map_err(|err| err.message)?;

        Ok(Json(ExecuteResponse {
            result: output.result,
            tool_calls: output.tool_calls,
            elapsed_ms: output.elapsed_ms,
            stdout: output.stdout,
        }))
    }

    #[tool(
        name = "gambi_execute_escalated",
        description = "Escalated execution path: run Python workflow with full upstream tool access when safe mode returns ESCALATION_REQUIRED."
    )]
    async fn gambi_execute_escalated(
        &self,
        Parameters(params): Parameters<ExecuteRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<ExecuteResponse>, String> {
        if !self.exec_enabled {
            return Err("gambi_execute_escalated is disabled for this server instance".to_string());
        }

        let output: ExecuteOutput = execution::run_code_execution(
            &params.code,
            &self.store,
            &self.upstream,
            context,
            execution::ExecutionPolicy::Escalated,
        )
        .await
        .map_err(|err| err.message)?;

        Ok(Json(ExecuteResponse {
            result: output.result,
            tool_calls: output.tool_calls,
            elapsed_ms: output.elapsed_ms,
            stdout: output.stdout,
        }))
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        let instructions = if self.exec_enabled {
            "gambi execute-only mode:\n\
             1. Call gambi_help to inspect servers/tools and get full tool metadata.\n\
             2. Call gambi_execute first (safe policy enforcement).\n\
             3. If safe mode returns ESCALATION_REQUIRED, call gambi_execute_escalated.\n\
             4. Safe heuristic marks tools as safe when name starts with get/list/search/lookup/fetch, or description starts with get.\n\
             5. Direct namespaced upstream tool calls are disabled.\n\
             6. If discovery is sparse, call gambi_list_upstream_tools for diagnostics."
        } else {
            "gambi execute-only mode with execution disabled (--no-exec):\n\
             Use gambi_help to inspect available upstream capabilities. Direct namespaced tool calls are disabled."
        };

        ServerInfo {
            instructions: Some(instructions.into()),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build(),
            ..Default::default()
        }
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        local_list_tools(self.local_tools(), request).await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if !self.exec_enabled
            && matches!(
                request.name.as_ref(),
                "gambi_execute" | "gambi_execute_escalated"
            )
        {
            return Err(McpError::method_not_found::<CallToolRequestMethod>());
        }

        route_tool_call(self, &self.tool_router, request, context).await
    }

    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        merged_list_prompts(&self.store, &self.upstream, request).await
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, McpError> {
        route_get_prompt(&self.store, &self.upstream, request, context).await
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        merged_list_resources(&self.store, &self.upstream, request).await
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        merged_list_resource_templates(&self.store, &self.upstream, request).await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        route_read_resource(&self.store, &self.upstream, request, context).await
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        if !self.exec_enabled && matches!(name, "gambi_execute" | "gambi_execute_escalated") {
            return None;
        }
        self.tool_router.get(name).cloned()
    }
}

fn upstream_discovery_enabled_for_mcp_listing() -> bool {
    std::env::var_os("GAMBI_DISABLE_UPSTREAM_DISCOVERY").is_none()
}

fn schema_compat_normalization_enabled() -> bool {
    std::env::var("GAMBI_TOOL_SCHEMA_COMPAT_NORMALIZATION")
        .ok()
        .map(|value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no"
            )
        })
        .unwrap_or(true)
}

async fn local_list_tools(
    mut tools: Vec<Tool>,
    request: Option<PaginatedRequestParams>,
) -> Result<ListToolsResult, McpError> {
    let cursor_offset = parse_cursor_offset(request.as_ref())?;
    let local_tool_count = tools.len();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools.dedup_by(|a, b| a.name == b.name);
    if schema_compat_normalization_enabled() {
        tools.iter_mut().for_each(normalize_tool_schema_for_clients);
    }

    let total_tool_count = tools.len();
    let (tools, next_cursor) = paginate_items(tools, request)?;

    if cursor_offset == 0 {
        info!(
            local_tool_count,
            total_tool_count,
            page_tool_count = tools.len(),
            next_cursor = next_cursor.as_deref().unwrap_or(""),
            "mcp tools/list response prepared"
        );
    }

    Ok(ListToolsResult {
        meta: None,
        next_cursor,
        tools,
    })
}

fn normalize_tool_schema_for_clients(tool: &mut Tool) {
    tool.input_schema = Arc::new(normalize_to_object_root(tool.input_schema.as_ref()));

    if let Some(output_schema) = tool.output_schema.as_ref() {
        let normalized = normalize_schema_fragments(output_schema.as_ref());
        if is_object_root_schema(&normalized) {
            tool.output_schema = Some(Arc::new(normalized));
        } else {
            // Some clients reject tool discovery when outputSchema does not conform.
            tool.output_schema = None;
        }
    }
}

fn normalize_to_object_root(schema: &Map<String, Value>) -> Map<String, Value> {
    let mut normalized = normalize_schema_fragments(schema);
    let has_object_root = normalized
        .get("type")
        .and_then(Value::as_str)
        .map(|value| value == "object")
        .unwrap_or(false);
    if !has_object_root {
        normalized.insert("type".to_string(), Value::String("object".to_string()));
    }
    normalized
}

fn normalize_schema_fragments(schema: &Map<String, Value>) -> Map<String, Value> {
    let mut normalized = Map::new();
    for (key, value) in schema {
        let next = if key == "properties" {
            match value {
                Value::Object(properties) => {
                    let mut normalized_properties = Map::new();
                    for (property_name, property_schema) in properties {
                        let normalized_property_schema = match property_schema {
                            Value::Object(map) => Value::Object(normalize_schema_fragments(map)),
                            // Some clients reject tools/list if any property schema is bare
                            // true/false. Replace with {} (accept-any object schema).
                            Value::Bool(_) => Value::Object(Map::new()),
                            Value::Array(items) => Value::Array(
                                items
                                    .iter()
                                    .map(|item| match item {
                                        Value::Object(map) => {
                                            Value::Object(normalize_schema_fragments(map))
                                        }
                                        other => other.clone(),
                                    })
                                    .collect(),
                            ),
                            other => other.clone(),
                        };
                        normalized_properties
                            .insert(property_name.clone(), normalized_property_schema);
                    }
                    Value::Object(normalized_properties)
                }
                other => other.clone(),
            }
        } else {
            match value {
                Value::Object(map) => Value::Object(normalize_schema_fragments(map)),
                Value::Array(items) => Value::Array(
                    items
                        .iter()
                        .map(|item| match item {
                            Value::Object(map) => Value::Object(normalize_schema_fragments(map)),
                            other => other.clone(),
                        })
                        .collect(),
                ),
                // Preserve JSON Schema booleans outside properties (e.g. nullable flags,
                // additionalProperties) to avoid changing meaning unnecessarily.
                Value::Bool(_) => value.clone(),
                other => other.clone(),
            }
        };
        normalized.insert(key.clone(), next);
    }
    normalized
}

fn is_object_root_schema(schema: &Map<String, Value>) -> bool {
    schema
        .get("type")
        .and_then(Value::as_str)
        .map(|value| value == "object")
        .unwrap_or(false)
}

async fn merged_list_prompts(
    store: &ConfigStore,
    upstream_manager: &upstream::UpstreamManager,
    request: Option<PaginatedRequestParams>,
) -> Result<ListPromptsResult, McpError> {
    if !upstream_discovery_enabled_for_mcp_listing() {
        return Ok(ListPromptsResult::default());
    }

    let cfg = store.load_async().await.map_err(|err| {
        McpError::internal_error(
            format!("failed to load config for prompt listing: {err}"),
            None,
        )
    })?;
    let discovered = discover_prompts_with_auto_refresh(store, upstream_manager, &cfg.servers)
        .await
        .map_err(|err| {
            McpError::internal_error(
                format!("failed to discover upstream prompts: {err:#}"),
                None,
            )
        })?;

    for failure in discovered.failures {
        warn!(
            server = %failure.server_name,
            error = %failure.message,
            "upstream prompt discovery failed during list_prompts"
        );
    }

    let mut prompts = discovered
        .prompts
        .into_iter()
        .map(|prompt| prompt.prompt)
        .collect::<Vec<_>>();
    prompts.sort_by(|a, b| a.name.cmp(&b.name));
    prompts.dedup_by(|a, b| a.name == b.name);

    let (prompts, next_cursor) = paginate_items(prompts, request)?;

    Ok(ListPromptsResult {
        meta: None,
        next_cursor,
        prompts,
    })
}

async fn merged_list_resources(
    store: &ConfigStore,
    upstream_manager: &upstream::UpstreamManager,
    request: Option<PaginatedRequestParams>,
) -> Result<ListResourcesResult, McpError> {
    if !upstream_discovery_enabled_for_mcp_listing() {
        return Ok(ListResourcesResult::default());
    }

    let cfg = store.load_async().await.map_err(|err| {
        McpError::internal_error(
            format!("failed to load config for resource listing: {err}"),
            None,
        )
    })?;
    let discovered = discover_resources_with_auto_refresh(store, upstream_manager, &cfg.servers)
        .await
        .map_err(|err| {
            McpError::internal_error(
                format!("failed to discover upstream resources: {err:#}"),
                None,
            )
        })?;

    for failure in discovered.failures {
        warn!(
            server = %failure.server_name,
            error = %failure.message,
            "upstream resource discovery failed during list_resources"
        );
    }

    let mut resources = discovered
        .resources
        .into_iter()
        .map(|resource| resource.resource)
        .collect::<Vec<_>>();
    resources.sort_by(|a, b| a.raw.uri.cmp(&b.raw.uri));
    resources.dedup_by(|a, b| a.raw.uri == b.raw.uri);

    let (resources, next_cursor) = paginate_items(resources, request)?;

    Ok(ListResourcesResult {
        meta: None,
        next_cursor,
        resources,
    })
}

async fn merged_list_resource_templates(
    store: &ConfigStore,
    upstream_manager: &upstream::UpstreamManager,
    request: Option<PaginatedRequestParams>,
) -> Result<ListResourceTemplatesResult, McpError> {
    if !upstream_discovery_enabled_for_mcp_listing() {
        return Ok(ListResourceTemplatesResult::default());
    }

    let cfg = store.load_async().await.map_err(|err| {
        McpError::internal_error(
            format!("failed to load config for resource template listing: {err}"),
            None,
        )
    })?;
    let discovered = discover_resources_with_auto_refresh(store, upstream_manager, &cfg.servers)
        .await
        .map_err(|err| {
            McpError::internal_error(
                format!("failed to discover upstream resource templates: {err:#}"),
                None,
            )
        })?;

    for failure in discovered.failures {
        warn!(
            server = %failure.server_name,
            error = %failure.message,
            "upstream resource template discovery failed during list_resource_templates"
        );
    }

    let mut resource_templates = discovered
        .resource_templates
        .into_iter()
        .map(|template| template.resource_template)
        .collect::<Vec<_>>();
    resource_templates.sort_by(|a, b| a.raw.uri_template.cmp(&b.raw.uri_template));
    resource_templates.dedup_by(|a, b| a.raw.uri_template == b.raw.uri_template);

    let (resource_templates, next_cursor) = paginate_items(resource_templates, request)?;

    Ok(ListResourceTemplatesResult {
        meta: None,
        next_cursor,
        resource_templates,
    })
}

async fn route_tool_call<S>(
    service: &S,
    tool_router: &ToolRouter<S>,
    request: CallToolRequestParams,
    context: RequestContext<RoleServer>,
) -> Result<CallToolResult, McpError>
where
    S: Send + Sync + 'static,
{
    if tool_router.has_route(request.name.as_ref()) {
        return tool_router
            .call(ToolCallContext::new(service, request, context))
            .await;
    }

    let tool_name = request.name.clone().into_owned();
    if split_namespaced(&tool_name).is_some() {
        return Err(McpError::invalid_params(
            "direct upstream tool invocation is disabled; use gambi_help then gambi_execute (safe) or gambi_execute_escalated",
            None,
        ));
    }
    Err(McpError::method_not_found::<CallToolRequestMethod>())
}

async fn route_get_prompt(
    store: &ConfigStore,
    upstream_manager: &upstream::UpstreamManager,
    request: GetPromptRequestParams,
    context: RequestContext<RoleServer>,
) -> Result<GetPromptResult, McpError> {
    let namespaced_prompt = request.name.clone();
    let Some((server_name, upstream_prompt_name)) = split_namespaced(&namespaced_prompt) else {
        return Err(McpError::invalid_params(
            "prompt name must be namespaced as '<server>:<prompt>'",
            None,
        ));
    };
    let server_name = server_name.to_string();
    let upstream_prompt_name = upstream_prompt_name.to_string();

    let cfg = store.load_async().await.map_err(|err| {
        McpError::internal_error(
            format!("failed to load config for prompt routing: {err}"),
            None,
        )
    })?;
    let auth_headers = load_upstream_auth_headers(store).await?;

    let Some(server) = cfg
        .servers
        .iter()
        .find(|candidate| candidate.name == server_name)
    else {
        return Err(McpError::invalid_params(
            format!("unknown prompt namespace '{server_name}'"),
            None,
        ));
    };

    let mut upstream_request = request;
    upstream_request.name = upstream_prompt_name.clone();
    merge_request_meta(&mut upstream_request.meta, &context.meta);
    let progress_forwarder = context
        .meta
        .get_progress_token()
        .map(|token| upstream::ProgressForwarder::new(context.peer.clone(), token));
    let first_attempt = upstream_manager
        .get_prompt_on_server(
            server,
            &auth_headers,
            upstream_request.clone(),
            context.ct.clone(),
            progress_forwarder.clone(),
        )
        .await;
    let result = match first_attempt {
        Ok(result) => Ok(result),
        Err(err) if should_attempt_auth_retry(server, &err) => {
            let refreshed = refresh_auth_for_server(store, &server_name).await;
            match refreshed {
                Ok(true) => {
                    upstream_manager.invalidate_discovery_cache().await;
                    let refreshed_headers = load_upstream_auth_headers(store).await?;
                    upstream_manager
                        .get_prompt_on_server(
                            server,
                            &refreshed_headers,
                            upstream_request,
                            context.ct,
                            progress_forwarder,
                        )
                        .await
                }
                Ok(false) => Err(err),
                Err(refresh_err) => {
                    warn!(
                        server = %server_name,
                        error = %refresh_err,
                        "upstream prompt request hit auth failure but oauth refresh failed"
                    );
                    Err(err)
                }
            }
        }
        Err(err) => Err(err),
    };
    map_upstream_request_error(result, "prompt", &server_name, &upstream_prompt_name)
}

async fn route_read_resource(
    store: &ConfigStore,
    upstream_manager: &upstream::UpstreamManager,
    request: ReadResourceRequestParams,
    context: RequestContext<RoleServer>,
) -> Result<ReadResourceResult, McpError> {
    let Some((server_name, upstream_uri)) = parse_namespaced_resource_uri(&request.uri) else {
        return Err(McpError::invalid_params(
            "resource uri must use gambi namespaced format",
            None,
        ));
    };

    let cfg = store.load_async().await.map_err(|err| {
        McpError::internal_error(
            format!("failed to load config for resource routing: {err}"),
            None,
        )
    })?;
    let auth_headers = load_upstream_auth_headers(store).await?;

    let Some(server) = cfg
        .servers
        .iter()
        .find(|candidate| candidate.name == server_name)
    else {
        return Err(McpError::invalid_params(
            format!("unknown resource namespace '{server_name}'"),
            None,
        ));
    };

    let mut upstream_request = request;
    upstream_request.uri = upstream_uri.clone();
    merge_request_meta(&mut upstream_request.meta, &context.meta);
    let progress_forwarder = context
        .meta
        .get_progress_token()
        .map(|token| upstream::ProgressForwarder::new(context.peer.clone(), token));
    let first_attempt = upstream_manager
        .read_resource_on_server(
            server,
            &auth_headers,
            upstream_request.clone(),
            context.ct.clone(),
            progress_forwarder.clone(),
        )
        .await;
    let result = match first_attempt {
        Ok(result) => Ok(result),
        Err(err) if should_attempt_auth_retry(server, &err) => {
            let refreshed = refresh_auth_for_server(store, &server_name).await;
            match refreshed {
                Ok(true) => {
                    upstream_manager.invalidate_discovery_cache().await;
                    let refreshed_headers = load_upstream_auth_headers(store).await?;
                    upstream_manager
                        .read_resource_on_server(
                            server,
                            &refreshed_headers,
                            upstream_request,
                            context.ct,
                            progress_forwarder,
                        )
                        .await
                }
                Ok(false) => Err(err),
                Err(refresh_err) => {
                    warn!(
                        server = %server_name,
                        error = %refresh_err,
                        "upstream resource request hit auth failure but oauth refresh failed"
                    );
                    Err(err)
                }
            }
        }
        Err(err) => Err(err),
    };
    let mut result = map_upstream_request_error(result, "resource", &server_name, &upstream_uri)?;
    rewrite_read_resource_result_uris(&mut result, &server_name);
    Ok(result)
}

fn paginate_items<T>(
    items: Vec<T>,
    request: Option<PaginatedRequestParams>,
) -> Result<(Vec<T>, Option<String>), McpError> {
    let start = parse_cursor_offset(request.as_ref())?;
    if start > items.len() {
        return Err(McpError::invalid_params(
            format!(
                "cursor offset '{}' is out of range for {} item(s)",
                start,
                items.len()
            ),
            None,
        ));
    }

    let end = (start + AGGREGATED_PAGE_SIZE).min(items.len());
    let next_cursor = (end < items.len()).then(|| end.to_string());
    let paged = items
        .into_iter()
        .skip(start)
        .take(AGGREGATED_PAGE_SIZE)
        .collect();

    Ok((paged, next_cursor))
}

fn parse_cursor_offset(request: Option<&PaginatedRequestParams>) -> Result<usize, McpError> {
    let Some(cursor) = request.and_then(|req| req.cursor.as_deref()) else {
        return Ok(0);
    };

    cursor.parse::<usize>().map_err(|_| {
        McpError::invalid_params(
            format!("invalid cursor '{cursor}': expected numeric offset"),
            None,
        )
    })
}

fn map_upstream_request_error<T>(
    result: std::result::Result<T, upstream::UpstreamRequestError>,
    capability_kind: &str,
    server_name: &str,
    upstream_id: &str,
) -> Result<T, McpError> {
    result.map_err(|err| match err {
        upstream::UpstreamRequestError::Protocol(err) => err,
        upstream::UpstreamRequestError::Cancelled => {
            McpError::invalid_request("request cancelled", None)
        }
        upstream::UpstreamRequestError::Transport(err) => McpError::internal_error(
            format!(
                "upstream {capability_kind} request failed (server='{server_name}', id='{upstream_id}'): {err:#}"
            ),
            None,
        ),
    })
}

fn rewrite_read_resource_result_uris(result: &mut ReadResourceResult, server_name: &str) {
    for content in &mut result.contents {
        match content {
            ResourceContents::TextResourceContents { uri, .. }
            | ResourceContents::BlobResourceContents { uri, .. } => {
                *uri = namespaced_resource_uri(server_name, uri);
            }
        }
    }
}

fn merge_request_meta(target: &mut Option<rmcp::model::Meta>, source: &rmcp::model::Meta) {
    match target {
        Some(existing) => existing.extend(source.clone()),
        None => *target = Some(source.clone()),
    }
}

async fn load_upstream_auth_headers(
    store: &ConfigStore,
) -> Result<upstream::UpstreamAuthHeaders, McpError> {
    let tokens: TokenState = store.load_tokens_async().await.map_err(|err| {
        McpError::internal_error(
            format!("failed to load token state for upstream routing: {err}"),
            None,
        )
    })?;
    Ok(upstream::auth_headers_from_token_state(&tokens))
}

pub async fn run(
    store: ConfigStore,
    upstream_manager: upstream::UpstreamManager,
    exec_enabled: bool,
    shutdown: CancellationToken,
) -> Result<()> {
    let service = McpServer::new(store, upstream_manager, exec_enabled)
        .serve(rmcp::transport::stdio())
        .await
        .context("failed to start MCP stdio server")?;

    wait_for_shutdown(service, shutdown).await
}

async fn wait_for_shutdown<S>(
    service: rmcp::service::RunningService<rmcp::RoleServer, S>,
    shutdown: CancellationToken,
) -> Result<()>
where
    S: rmcp::service::Service<rmcp::RoleServer>,
{
    let cancellation = service.cancellation_token();
    let mut waiter = tokio::spawn(async move { service.waiting().await });

    tokio::select! {
        _ = shutdown.cancelled() => {
            cancellation.cancel();
            match tokio::time::timeout(Duration::from_secs(3), &mut waiter).await {
                Ok(joined) => handle_waiter_result(joined),
                Err(_) => Err(anyhow!("timed out waiting for MCP service shutdown")),
            }
        }
        joined = &mut waiter => handle_waiter_result(joined),
    }
}

fn handle_waiter_result(
    joined: Result<
        Result<rmcp::service::QuitReason, tokio::task::JoinError>,
        tokio::task::JoinError,
    >,
) -> Result<()> {
    match joined {
        Ok(Ok(reason)) => {
            warn!(reason = ?reason, "MCP service exited");
            Ok(())
        }
        Ok(Err(err)) => Err(anyhow!(err)).context("MCP service wait failed"),
        Err(err) => Err(anyhow!(err)).context("MCP waiter task failed"),
    }
}

async fn list_servers_output(store: &ConfigStore) -> Result<Json<ListServersOutput>, String> {
    let cfg = store.load_async().await.map_err(|err| err.to_string())?;
    let policy_modes = cfg.server_tool_policy_modes.clone();
    let servers = cfg
        .servers
        .into_iter()
        .map(|server| {
            let name = server.name;
            let policy_mode = policy_modes
                .get(&name)
                .copied()
                .unwrap_or_default()
                .as_str()
                .to_string();
            ListedServer {
                name,
                url: server.url,
                exposure_mode: server.exposure_mode.as_str().to_string(),
                policy_mode,
            }
        })
        .collect();

    Ok(Json(ListServersOutput { servers }))
}

async fn list_upstream_tools_output(
    store: &ConfigStore,
    upstream_manager: &upstream::UpstreamManager,
) -> Result<Json<UpstreamDiscoveryOutput>, String> {
    let cfg = store.load_async().await.map_err(|err| err.to_string())?;
    let discovered = discover_tools_with_auto_refresh(store, upstream_manager, &cfg.servers)
        .await
        .map_err(|err| err.to_string())?;

    let tools = discovered
        .tools
        .into_iter()
        .map(|tool| UpstreamToolOutput {
            namespaced_name: tool.namespaced_name,
            server: tool.server_name,
            upstream_name: tool.upstream_name,
        })
        .collect();
    let failures = discovered
        .failures
        .into_iter()
        .map(|failure| UpstreamFailureOutput {
            server: failure.server_name,
            error: failure.message,
        })
        .collect();

    Ok(Json(UpstreamDiscoveryOutput { tools, failures }))
}

async fn gambi_help_output(
    store: &ConfigStore,
    upstream_manager: &upstream::UpstreamManager,
    request: HelpRequest,
) -> Result<Json<HelpResponse>, String> {
    let cfg = store.load_async().await.map_err(|err| err.to_string())?;

    let selected_servers = select_help_servers(&cfg, request.server.as_deref())?;
    let discovery = discover_tools_with_auto_refresh(store, upstream_manager, &selected_servers)
        .await
        .map_err(|err| err.to_string())?;

    let mut by_server = BTreeMap::<String, Vec<upstream::DiscoveredTool>>::new();
    for tool in discovery.tools {
        by_server
            .entry(tool.server_name.clone())
            .or_default()
            .push(tool);
    }
    for tools in by_server.values_mut() {
        tools.sort_by(|a, b| a.namespaced_name.cmp(&b.namespaced_name));
    }

    let selected_server_name = request.server.as_deref();
    let selected_tool_detail = if let Some(raw_tool) = request.tool.as_deref() {
        let selected = resolve_help_tool(raw_tool, selected_server_name, &by_server)?;
        let description = effective_tool_description(&cfg, selected);
        let policy = cfg.evaluate_tool_policy(
            &selected.server_name,
            &selected.upstream_name,
            Some(description.as_str()),
        );
        Some(HelpToolDetailOutput {
            namespaced_name: selected.namespaced_name.clone(),
            server: selected.server_name.clone(),
            upstream_name: selected.upstream_name.clone(),
            description,
            policy_level: policy.level.as_str().to_string(),
            policy_source: policy.source.as_str().to_string(),
            input_schema: selected.tool.input_schema.as_ref().clone(),
            output_schema: selected
                .tool
                .output_schema
                .as_ref()
                .map(|schema| schema.as_ref().clone()),
        })
    } else {
        None
    };

    let mut servers = Vec::new();
    for server in selected_servers {
        let discovered = by_server.get(&server.name).cloned().unwrap_or_default();
        let tool_count = discovered.len();
        let tools = summarize_tools_for_exposure_mode(&cfg, &server, &discovered);
        servers.push(HelpServerOutput {
            name: server.name.clone(),
            url: server.url.clone(),
            exposure_mode: server.exposure_mode.as_str().to_string(),
            tool_count,
            tools,
        });
    }

    let failures = discovery
        .failures
        .into_iter()
        .map(|failure| UpstreamFailureOutput {
            server: failure.server_name,
            error: failure.message,
        })
        .collect::<Vec<_>>();

    let usage = "Use gambi in execute-only mode:\n\
1) Call gambi_help to inspect servers/tools.\n\
2) If you need full metadata, call gambi_help with server/tool.\n\
3) Run workflows through gambi_execute first (safe policy mode).\n\
4) If response includes ESCALATION_REQUIRED, re-run with gambi_execute_escalated.\n\
5) Safe heuristic: names starting with get/list/search/lookup/fetch OR descriptions starting with get.\n\
Direct namespaced upstream tool calls are disabled."
        .to_string();

    Ok(Json(HelpResponse {
        usage,
        servers,
        failures,
        tool: selected_tool_detail,
    }))
}

fn select_help_servers(
    cfg: &crate::config::AppConfig,
    selected_server_name: Option<&str>,
) -> Result<Vec<crate::config::ServerConfig>, String> {
    let mut servers = cfg.servers.clone();
    if let Some(server_name) = selected_server_name {
        servers.retain(|server| server.name == server_name);
        if servers.is_empty() {
            return Err(format!("unknown server '{server_name}'"));
        }
    }
    Ok(servers)
}

fn resolve_help_tool<'a>(
    raw_tool: &str,
    selected_server_name: Option<&str>,
    by_server: &'a BTreeMap<String, Vec<upstream::DiscoveredTool>>,
) -> Result<&'a upstream::DiscoveredTool, String> {
    if let Some((server_name, upstream_tool_name)) = split_namespaced(raw_tool) {
        let Some(tools) = by_server.get(server_name) else {
            return Err(format!("unknown server '{server_name}'"));
        };
        return tools
            .iter()
            .find(|tool| tool.upstream_name == upstream_tool_name)
            .ok_or_else(|| format!("unknown tool '{raw_tool}'"));
    }

    let mut matches = Vec::new();
    for (server_name, tools) in by_server {
        if selected_server_name.is_some_and(|selected| selected != server_name) {
            continue;
        }
        for tool in tools {
            if tool.upstream_name == raw_tool || tool.namespaced_name == raw_tool {
                matches.push(tool);
            }
        }
    }

    match matches.len() {
        0 => Err(format!("unknown tool '{raw_tool}'")),
        1 => Ok(matches[0]),
        _ => Err(format!(
            "tool '{raw_tool}' is ambiguous across servers; specify '<server>:<tool>'"
        )),
    }
}

fn summarize_tools_for_exposure_mode(
    cfg: &crate::config::AppConfig,
    server: &crate::config::ServerConfig,
    discovered: &[upstream::DiscoveredTool],
) -> Vec<HelpToolSummaryOutput> {
    if server.exposure_mode == ExposureMode::ServerOnly {
        return Vec::new();
    }

    discovered
        .iter()
        .map(|tool| {
            let effective = effective_tool_description(cfg, tool);
            let policy = cfg.evaluate_tool_policy(
                &tool.server_name,
                &tool.upstream_name,
                Some(effective.as_str()),
            );
            let description = match server.exposure_mode {
                ExposureMode::Passthrough => (!effective.is_empty()).then_some(effective),
                ExposureMode::Compact => {
                    (!effective.is_empty()).then_some(truncate_description(&effective))
                }
                ExposureMode::NamesOnly => None,
                ExposureMode::ServerOnly => None,
            };
            HelpToolSummaryOutput {
                namespaced_name: tool.namespaced_name.clone(),
                description,
                policy_level: policy.level.as_str().to_string(),
                policy_source: policy.source.as_str().to_string(),
            }
        })
        .collect()
}

fn effective_tool_description(
    cfg: &crate::config::AppConfig,
    tool: &upstream::DiscoveredTool,
) -> String {
    cfg.tool_description_override_for(&tool.server_name, &tool.upstream_name)
        .map(ToOwned::to_owned)
        .or_else(|| {
            tool.tool
                .description
                .as_ref()
                .map(|description| description.to_string())
        })
        .unwrap_or_default()
}

fn truncate_description(description: &str) -> String {
    let mut out = String::new();
    for (index, ch) in description.chars().enumerate() {
        if index >= COMPACT_DESCRIPTION_LIMIT {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

async fn discover_tools_with_auto_refresh(
    store: &ConfigStore,
    upstream_manager: &upstream::UpstreamManager,
    servers: &[crate::config::ServerConfig],
) -> Result<upstream::DiscoveryResult> {
    let tokens: TokenState = store.load_tokens_async().await?;
    let auth_headers = upstream::auth_headers_from_token_state(&tokens);
    let mut discovered = upstream_manager
        .discover_tools_from_servers(servers, &auth_headers)
        .await?;

    let auth_failures: BTreeSet<String> = discovered
        .failures
        .iter()
        .filter(|failure| looks_like_auth_failure(&failure.message))
        .map(|failure| failure.server_name.clone())
        .collect();
    if auth_failures.is_empty() {
        return Ok(discovered);
    }

    let mut refreshed_any = false;
    for server_name in auth_failures {
        if !server_is_http(servers, &server_name) {
            continue;
        }
        match refresh_auth_for_server(store, &server_name).await {
            Ok(true) => refreshed_any = true,
            Ok(false) => {}
            Err(err) => {
                warn!(
                    error = %err,
                    server = %server_name,
                    "upstream reported auth failure during MCP discovery and oauth refresh failed"
                );
            }
        }
    }

    if refreshed_any {
        upstream_manager.invalidate_discovery_cache().await;
        let tokens: TokenState = store.load_tokens_async().await?;
        let auth_headers = upstream::auth_headers_from_token_state(&tokens);
        discovered = upstream_manager
            .discover_tools_from_servers(servers, &auth_headers)
            .await?;
    }

    Ok(discovered)
}

async fn discover_prompts_with_auto_refresh(
    store: &ConfigStore,
    upstream_manager: &upstream::UpstreamManager,
    servers: &[crate::config::ServerConfig],
) -> Result<upstream::PromptDiscoveryResult> {
    let tokens: TokenState = store.load_tokens_async().await?;
    let auth_headers = upstream::auth_headers_from_token_state(&tokens);
    let mut discovered = upstream_manager
        .discover_prompts_from_servers(servers, &auth_headers)
        .await?;

    let auth_failures: BTreeSet<String> = discovered
        .failures
        .iter()
        .filter(|failure| looks_like_auth_failure(&failure.message))
        .map(|failure| failure.server_name.clone())
        .collect();
    if auth_failures.is_empty() {
        return Ok(discovered);
    }

    let mut refreshed_any = false;
    for server_name in auth_failures {
        if !server_is_http(servers, &server_name) {
            continue;
        }
        match refresh_auth_for_server(store, &server_name).await {
            Ok(true) => refreshed_any = true,
            Ok(false) => {}
            Err(err) => {
                warn!(
                    error = %err,
                    server = %server_name,
                    "upstream reported auth failure during MCP prompt discovery and oauth refresh failed"
                );
            }
        }
    }

    if refreshed_any {
        upstream_manager.invalidate_discovery_cache().await;
        let tokens: TokenState = store.load_tokens_async().await?;
        let auth_headers = upstream::auth_headers_from_token_state(&tokens);
        discovered = upstream_manager
            .discover_prompts_from_servers(servers, &auth_headers)
            .await?;
    }

    Ok(discovered)
}

async fn discover_resources_with_auto_refresh(
    store: &ConfigStore,
    upstream_manager: &upstream::UpstreamManager,
    servers: &[crate::config::ServerConfig],
) -> Result<upstream::ResourceDiscoveryResult> {
    let tokens: TokenState = store.load_tokens_async().await?;
    let auth_headers = upstream::auth_headers_from_token_state(&tokens);
    let mut discovered = upstream_manager
        .discover_resources_from_servers(servers, &auth_headers)
        .await?;

    let auth_failures: BTreeSet<String> = discovered
        .failures
        .iter()
        .filter(|failure| looks_like_auth_failure(&failure.message))
        .map(|failure| failure.server_name.clone())
        .collect();
    if auth_failures.is_empty() {
        return Ok(discovered);
    }

    let mut refreshed_any = false;
    for server_name in auth_failures {
        if !server_is_http(servers, &server_name) {
            continue;
        }
        match refresh_auth_for_server(store, &server_name).await {
            Ok(true) => refreshed_any = true,
            Ok(false) => {}
            Err(err) => {
                warn!(
                    error = %err,
                    server = %server_name,
                    "upstream reported auth failure during MCP resource discovery and oauth refresh failed"
                );
            }
        }
    }

    if refreshed_any {
        upstream_manager.invalidate_discovery_cache().await;
        let tokens: TokenState = store.load_tokens_async().await?;
        let auth_headers = upstream::auth_headers_from_token_state(&tokens);
        discovered = upstream_manager
            .discover_resources_from_servers(servers, &auth_headers)
            .await?;
    }

    Ok(discovered)
}

async fn refresh_auth_for_server(store: &ConfigStore, server_name: &str) -> Result<bool> {
    let auth = crate::auth::AuthManager::new(store.clone());
    match auth.refresh(server_name).await {
        Ok(response) => {
            info!(
                server = %server_name,
                refreshed = response.refreshed,
                expires_at_epoch_seconds = ?response.expires_at_epoch_seconds,
                "oauth refresh succeeded after upstream auth failure"
            );
            Ok(true)
        }
        Err(err) => Err(err),
    }
}

fn server_is_http(servers: &[crate::config::ServerConfig], server_name: &str) -> bool {
    servers
        .iter()
        .find(|server| server.name == server_name)
        .is_some_and(|server| {
            server.url.starts_with("http://") || server.url.starts_with("https://")
        })
}

fn should_attempt_auth_retry(
    server: &crate::config::ServerConfig,
    err: &upstream::UpstreamRequestError,
) -> bool {
    (server.url.starts_with("http://") || server.url.starts_with("https://"))
        && looks_like_auth_failure_error(err)
}

fn looks_like_auth_failure_error(err: &upstream::UpstreamRequestError) -> bool {
    match err {
        upstream::UpstreamRequestError::Protocol(protocol) => {
            looks_like_auth_failure(protocol.message.as_ref())
        }
        upstream::UpstreamRequestError::Transport(transport) => {
            looks_like_auth_failure(&transport.to_string())
        }
        upstream::UpstreamRequestError::Cancelled => false,
    }
}

fn looks_like_invalid_token(message: &str) -> bool {
    let msg = message.trim().to_ascii_lowercase();
    msg.contains("invalid_token") || msg.contains("invalid token")
}

fn looks_like_auth_required(message: &str) -> bool {
    let msg = message.trim().to_ascii_lowercase();
    msg.contains("auth required")
        || msg.contains("unauthorized")
        || msg.contains("forbidden")
        || msg.contains("401")
        || msg.contains("403")
}

fn looks_like_auth_failure(message: &str) -> bool {
    looks_like_invalid_token(message) || looks_like_auth_required(message)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rmcp::model::{ErrorCode, PaginatedRequestParams, Tool};
    use serde_json::{Map, Value};

    use super::{
        McpServer, looks_like_auth_failure, normalize_tool_schema_for_clients, paginate_items,
    };

    #[test]
    fn exec_mode_exposes_execute_tools() {
        let server = McpServer::new(
            crate::config::ConfigStore::with_base_dir(
                tempfile::tempdir().expect("tempdir").path().join("gambi"),
            ),
            crate::upstream::UpstreamManager::new(),
            true,
        );
        let names: Vec<String> = server
            .local_tools()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();

        assert!(names.iter().any(|name| name == "gambi_execute"));
        assert!(names.iter().any(|name| name == "gambi_execute_escalated"));
        assert!(names.iter().any(|name| name == "gambi_list_servers"));
        assert!(names.iter().any(|name| name == "gambi_list_upstream_tools"));
    }

    #[test]
    fn no_exec_mode_hides_execute_tools() {
        let server = McpServer::new(
            crate::config::ConfigStore::with_base_dir(
                tempfile::tempdir().expect("tempdir").path().join("gambi"),
            ),
            crate::upstream::UpstreamManager::new(),
            false,
        );
        let names: Vec<String> = server
            .local_tools()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();

        assert!(!names.iter().any(|name| name == "gambi_execute"));
        assert!(!names.iter().any(|name| name == "gambi_execute_escalated"));
        assert!(names.iter().any(|name| name == "gambi_list_servers"));
        assert!(names.iter().any(|name| name == "gambi_list_upstream_tools"));
    }

    #[test]
    fn auth_failure_matcher_detects_common_messages() {
        assert!(looks_like_auth_failure("invalid_token"));
        assert!(looks_like_auth_failure("Auth required"));
        assert!(looks_like_auth_failure(
            "upstream returned 401 unauthorized"
        ));
        assert!(looks_like_auth_failure("http 403 forbidden"));
        assert!(!looks_like_auth_failure("connection reset by peer"));
    }

    #[test]
    fn pagination_uses_numeric_cursor_offsets() {
        let items: Vec<usize> = (0..205).collect();

        let (first_page, first_cursor) = paginate_items(items, None).expect("first page");
        assert_eq!(first_page.len(), 100);
        assert_eq!(first_page[0], 0);
        assert_eq!(first_page[99], 99);
        assert_eq!(first_cursor.as_deref(), Some("100"));
    }

    #[test]
    fn pagination_rejects_invalid_cursor() {
        let items: Vec<usize> = (0..10).collect();
        let err = paginate_items(
            items,
            Some(PaginatedRequestParams {
                meta: None,
                cursor: Some("bad-cursor".to_string()),
            }),
        )
        .expect_err("invalid cursor must fail");

        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
        assert!(err.message.contains("invalid cursor"));
    }

    #[test]
    fn normalize_tool_schema_forces_object_input_schema_root() {
        let mut input_schema = Map::new();
        input_schema.insert("type".to_string(), Value::String("null".to_string()));
        let mut tool = Tool::new("example_tool", "example", Arc::new(input_schema));

        normalize_tool_schema_for_clients(&mut tool);

        assert_eq!(
            tool.input_schema
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            "object"
        );
    }

    #[test]
    fn normalize_tool_schema_drops_non_object_output_schema_root() {
        let mut tool = Tool::new("example_tool", "example", Arc::new(Map::new()));
        let mut output_schema = Map::new();
        output_schema.insert("type".to_string(), Value::String("array".to_string()));
        tool.output_schema = Some(Arc::new(output_schema));

        normalize_tool_schema_for_clients(&mut tool);

        assert!(tool.output_schema.is_none());
    }

    #[test]
    fn normalize_tool_schema_sanitizes_boolean_property_schemas() {
        let mut input_schema = Map::new();
        input_schema.insert("type".to_string(), Value::String("object".to_string()));
        input_schema.insert("additionalProperties".to_string(), Value::Bool(false));
        let mut properties = Map::new();
        properties.insert("result".to_string(), Value::Bool(true));
        input_schema.insert("properties".to_string(), Value::Object(properties));
        let mut tool = Tool::new("example_tool", "example", Arc::new(input_schema));

        normalize_tool_schema_for_clients(&mut tool);

        assert_eq!(
            tool.input_schema
                .get("additionalProperties")
                .and_then(Value::as_bool),
            Some(false)
        );
        let result_schema = tool
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .and_then(|properties| properties.get("result"))
            .and_then(Value::as_object)
            .expect("result schema should be object after normalization");
        assert!(result_schema.is_empty());
    }
}
