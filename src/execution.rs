use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use monty::{
    CollectStringPrint, ExcType, LimitedTracker, MontyException, MontyObject, MontyRun,
    ResourceLimits, RunProgress,
};
use num_bigint::BigInt;
use num_traits::cast::ToPrimitive;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};
use serde::Serialize;
use serde_json::{Map, Value, json};
use tracing::{info, warn};

use crate::auth::AuthManager;
use crate::config::{AppConfig, ConfigStore, ServerConfig, ToolPolicyLevel};
use crate::namespacing::split_namespaced;
use crate::upstream::{
    ProgressForwarder, UpstreamAuthHeaders, UpstreamManager, UpstreamRequestError,
    auth_headers_from_token_state,
};

const EXTERNAL_CALL_FN: &str = "__gambi_call__";
const EXTERNAL_READ_FILE_FN: &str = "__gambi_read_file__";
const EXTERNAL_JSON_LOADS_FN: &str = "__gambi_json_loads__";
const EXTERNAL_JSON_DUMPS_FN: &str = "__gambi_json_dumps__";

const DEFAULT_MAX_WALL_MS: u64 = 10_000;
const DEFAULT_MAX_CPU_SECONDS: u64 = 10;
const DEFAULT_MAX_MEMORY_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_ALLOCATION_BYTES: u64 = 128 * 1024 * 1024;
const DEFAULT_MAX_STDOUT_BYTES: usize = 1_048_576;
const DEFAULT_MAX_READ_FILE_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_READ_FILE_ROOT: &str = "/tmp";
const MAX_UPSTREAM_TOOL_TIMEOUT_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionPolicy {
    Safe,
    Escalated,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ExecuteOptions {
    pub upstream_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ExecutionLimits {
    pub max_wall: Duration,
    pub max_cpu_seconds: u64,
    pub max_memory_bytes: u64,
    pub max_allocation_bytes: u64,
    pub max_stdout_bytes: usize,
    pub max_read_file_bytes: usize,
    pub read_file_allowed_roots: Option<Vec<PathBuf>>,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            max_wall: Duration::from_millis(
                parse_u64_env("GAMBI_EXEC_MAX_WALL_MS").unwrap_or(DEFAULT_MAX_WALL_MS),
            ),
            max_cpu_seconds: parse_u64_env("GAMBI_EXEC_MAX_CPU_SECS")
                .unwrap_or(DEFAULT_MAX_CPU_SECONDS),
            max_memory_bytes: parse_u64_env("GAMBI_EXEC_MAX_MEM_BYTES")
                .unwrap_or(DEFAULT_MAX_MEMORY_BYTES),
            max_allocation_bytes: parse_u64_env("GAMBI_EXEC_MAX_ALLOC_BYTES")
                .unwrap_or(DEFAULT_MAX_ALLOCATION_BYTES),
            max_stdout_bytes: parse_usize_env("GAMBI_EXEC_MAX_STDOUT_BYTES")
                .unwrap_or(DEFAULT_MAX_STDOUT_BYTES),
            max_read_file_bytes: parse_usize_env("GAMBI_EXEC_MAX_READ_FILE_BYTES")
                .unwrap_or(DEFAULT_MAX_READ_FILE_BYTES),
            read_file_allowed_roots: Some(default_read_file_roots()),
        }
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ExecuteOutput {
    pub result: Value,
    pub tool_calls: usize,
    pub elapsed_ms: u128,
    pub stdout: Vec<String>,
}

#[derive(Debug)]
enum ExecutionError {
    Message(String),
    EscalationRequired(String),
    Cancelled,
    Timeout,
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) => write!(f, "{message}"),
            Self::EscalationRequired(message) => write!(f, "{message}"),
            Self::Cancelled => write!(f, "execution cancelled"),
            Self::Timeout => write!(f, "execution timed out"),
        }
    }
}

pub async fn run_code_execution(
    code: &str,
    store: &ConfigStore,
    upstream: &UpstreamManager,
    context: RequestContext<RoleServer>,
    policy: ExecutionPolicy,
    options: ExecuteOptions,
) -> Result<ExecuteOutput, McpError> {
    let cfg = store.load_async().await.map_err(|err| {
        McpError::internal_error(format!("failed to load config for execution: {err}"), None)
    })?;
    let tokens: crate::auth::TokenState = store.load_tokens_async().await.map_err(|err| {
        McpError::internal_error(
            format!("failed to load oauth token state for execution: {err}"),
            None,
        )
    })?;

    let auth_headers = auth_headers_from_token_state(&tokens);
    let servers_by_name = cfg
        .servers
        .iter()
        .map(|server| (server.name.clone(), server.clone()))
        .collect::<HashMap<_, _>>();

    let script = build_monty_script(code, &cfg.servers).map_err(|err| {
        McpError::invalid_params(format!("failed to build monty script: {err}"), None)
    })?;

    let limits = ExecutionLimits::default();
    let started = Instant::now();
    let tool_descriptions = build_tool_description_index(&cfg, upstream, &auth_headers).await;
    let tool_call_timeout = normalize_upstream_tool_timeout(options.upstream_timeout_ms)
        .map_err(|err| McpError::invalid_params(err, None))?;
    let runtime_context = RuntimeContext {
        store,
        cfg: &cfg,
        servers_by_name: &servers_by_name,
        auth_headers: &auth_headers,
        tool_descriptions: &tool_descriptions,
        policy,
        tool_call_timeout,
        upstream,
        request_context: &context,
    };

    let runtime = run_monty_execution_loop(&script, &runtime_context, &limits);

    let (result, tool_calls, stdout) = match tokio::time::timeout(limits.max_wall, runtime).await {
        Ok(Ok(out)) => out,
        Ok(Err(err)) => return Err(map_execution_error(err)),
        Err(_) => return Err(map_execution_error(ExecutionError::Timeout)),
    };

    Ok(ExecuteOutput {
        result,
        tool_calls,
        elapsed_ms: started.elapsed().as_millis(),
        stdout,
    })
}

struct RuntimeContext<'a> {
    store: &'a ConfigStore,
    cfg: &'a AppConfig,
    servers_by_name: &'a HashMap<String, ServerConfig>,
    auth_headers: &'a UpstreamAuthHeaders,
    tool_descriptions: &'a HashMap<(String, String), String>,
    policy: ExecutionPolicy,
    tool_call_timeout: Option<Duration>,
    upstream: &'a UpstreamManager,
    request_context: &'a RequestContext<RoleServer>,
}

fn normalize_upstream_tool_timeout(
    requested_ms: Option<u64>,
) -> std::result::Result<Option<Duration>, String> {
    let Some(requested_ms) = requested_ms else {
        return Ok(None);
    };
    if requested_ms == 0 {
        return Err("upstream_timeout_ms must be greater than 0".to_string());
    }
    Ok(Some(Duration::from_millis(
        requested_ms.min(MAX_UPSTREAM_TOOL_TIMEOUT_MS),
    )))
}

async fn run_monty_execution_loop(
    script: &str,
    runtime: &RuntimeContext<'_>,
    limits: &ExecutionLimits,
) -> std::result::Result<(Value, usize, Vec<String>), ExecutionError> {
    let runner = MontyRun::new(
        script.to_string(),
        "gambi_exec.py",
        vec![],
        vec![
            EXTERNAL_CALL_FN.to_string(),
            EXTERNAL_READ_FILE_FN.to_string(),
            EXTERNAL_JSON_LOADS_FN.to_string(),
            EXTERNAL_JSON_DUMPS_FN.to_string(),
        ],
    )
    .map_err(map_monty_exception)?;

    let tracker = LimitedTracker::new(build_monty_limits(limits));
    let mut print = BoundedPrint::new(limits.max_stdout_bytes);
    let mut progress = runner
        .start(vec![], tracker, &mut print)
        .map_err(map_monty_exception)?;

    let mut tool_calls = 0usize;

    loop {
        if runtime.request_context.ct.is_cancelled() {
            return Err(ExecutionError::Cancelled);
        }

        match progress {
            RunProgress::Complete(object) => {
                let result = monty_object_to_json(&object).map_err(ExecutionError::Message)?;
                return Ok((result, tool_calls, print.into_lines()));
            }
            RunProgress::FunctionCall {
                function_name,
                args,
                kwargs,
                state,
                ..
            } => {
                let result_json = if function_name == EXTERNAL_CALL_FN {
                    let tool_call = parse_external_tool_call(args, kwargs)?;
                    tool_calls = tool_calls.saturating_add(1);
                    handle_tool_call(tool_call.name, tool_call.arguments, runtime).await?
                } else if function_name == EXTERNAL_READ_FILE_FN {
                    let path = parse_read_file_call(args, kwargs)?;
                    Value::String(read_file_for_execution(&path, limits).await?)
                } else if function_name == EXTERNAL_JSON_LOADS_FN {
                    let input = parse_json_loads_call(args, kwargs)?;
                    serde_json::from_str(&input).map_err(|err| {
                        ExecutionError::Message(format!("json_loads failed: {err}"))
                    })?
                } else if function_name == EXTERNAL_JSON_DUMPS_FN {
                    let object = parse_json_dumps_call(args, kwargs)?;
                    let json_value =
                        monty_object_to_json(&object).map_err(ExecutionError::Message)?;
                    let serialized = serde_json::to_string(&json_value).map_err(|err| {
                        ExecutionError::Message(format!("json_dumps failed: {err}"))
                    })?;
                    Value::String(serialized)
                } else {
                    return Err(ExecutionError::Message(format!(
                        "unexpected external function call '{function_name}'"
                    )));
                };

                let monty_result =
                    json_to_monty_object(&result_json).map_err(ExecutionError::Message)?;
                progress = state
                    .run(monty_result, &mut print)
                    .map_err(map_monty_exception)?;
            }
            RunProgress::ResolveFutures(_) => {
                return Err(ExecutionError::Message(
                    "monty execution entered unresolved async future state".to_string(),
                ));
            }
            RunProgress::OsCall { function, .. } => {
                return Err(ExecutionError::Message(format!(
                    "monty requested unsupported OS call: {function}"
                )));
            }
        }
    }
}

fn parse_read_file_call(
    args: Vec<MontyObject>,
    kwargs: Vec<(MontyObject, MontyObject)>,
) -> std::result::Result<String, ExecutionError> {
    let positional_path = match args.as_slice() {
        [] => None,
        [MontyObject::String(path)] => Some(path.clone()),
        [single] => {
            return Err(ExecutionError::Message(format!(
                "read_file path must be a string, got {}",
                single.type_name()
            )));
        }
        _ => {
            return Err(ExecutionError::Message(
                "read_file accepts at most one positional argument".to_string(),
            ));
        }
    };

    let mut keyword_path: Option<String> = None;
    for (key, value) in kwargs {
        let key = match key {
            MontyObject::String(value) => value,
            other => {
                return Err(ExecutionError::Message(format!(
                    "read_file argument key must be string, got {}",
                    other.type_name()
                )));
            }
        };
        if key != "path" {
            return Err(ExecutionError::Message(format!(
                "read_file received unknown keyword argument '{key}'"
            )));
        }
        if keyword_path.is_some() {
            return Err(ExecutionError::Message(
                "read_file received duplicate 'path' argument".to_string(),
            ));
        }
        let path = match value {
            MontyObject::String(value) => value,
            other => {
                return Err(ExecutionError::Message(format!(
                    "read_file path must be a string, got {}",
                    other.type_name()
                )));
            }
        };
        keyword_path = Some(path);
    }

    if positional_path.is_some() && keyword_path.is_some() {
        return Err(ExecutionError::Message(
            "read_file path must be provided once (positional or keyword)".to_string(),
        ));
    }

    let path = positional_path.or(keyword_path).ok_or_else(|| {
        ExecutionError::Message("read_file requires a path string argument".to_string())
    })?;
    if path.trim().is_empty() {
        return Err(ExecutionError::Message(
            "read_file path cannot be empty".to_string(),
        ));
    }

    Ok(path)
}

fn parse_json_loads_call(
    args: Vec<MontyObject>,
    kwargs: Vec<(MontyObject, MontyObject)>,
) -> std::result::Result<String, ExecutionError> {
    if !kwargs.is_empty() {
        return Err(ExecutionError::Message(
            "json_loads does not accept keyword arguments".to_string(),
        ));
    }
    match args.as_slice() {
        [MontyObject::String(s)] => Ok(s.clone()),
        [single] => Err(ExecutionError::Message(format!(
            "json_loads argument must be a string, got {}",
            single.type_name()
        ))),
        [] => Err(ExecutionError::Message(
            "json_loads requires a string argument".to_string(),
        )),
        _ => Err(ExecutionError::Message(
            "json_loads accepts exactly one positional argument".to_string(),
        )),
    }
}

fn parse_json_dumps_call(
    args: Vec<MontyObject>,
    kwargs: Vec<(MontyObject, MontyObject)>,
) -> std::result::Result<MontyObject, ExecutionError> {
    if !kwargs.is_empty() {
        return Err(ExecutionError::Message(
            "json_dumps does not accept keyword arguments".to_string(),
        ));
    }
    match args.len() {
        1 => Ok(args.into_iter().next().expect("checked length")),
        0 => Err(ExecutionError::Message(
            "json_dumps requires one argument".to_string(),
        )),
        _ => Err(ExecutionError::Message(
            "json_dumps accepts exactly one positional argument".to_string(),
        )),
    }
}

async fn read_file_for_execution(
    requested_path: &str,
    limits: &ExecutionLimits,
) -> std::result::Result<String, ExecutionError> {
    let requested = PathBuf::from(requested_path);
    let resolved = tokio::fs::canonicalize(&requested).await.map_err(|err| {
        ExecutionError::Message(format!("read_file failed for '{requested_path}': {err}"))
    })?;

    if let Some(roots) = limits.read_file_allowed_roots.as_ref() {
        let normalized_roots = normalize_allowed_roots(roots).await;
        let allowed = normalized_roots
            .iter()
            .any(|root| resolved.starts_with(root));
        if !allowed {
            let allowed_roots = normalized_roots
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ExecutionError::Message(format!(
                "read_file denied for '{}': outside allowed roots ({allowed_roots})",
                requested_path
            )));
        }
    }

    let metadata = tokio::fs::metadata(&resolved).await.map_err(|err| {
        ExecutionError::Message(format!(
            "read_file failed to inspect '{}': {err}",
            resolved.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(ExecutionError::Message(format!(
            "read_file path '{}' is not a file",
            resolved.display()
        )));
    }
    if metadata.len() > limits.max_read_file_bytes as u64 {
        return Err(ExecutionError::Message(format!(
            "read_file rejected '{}': file size {} bytes exceeds limit {} bytes",
            resolved.display(),
            metadata.len(),
            limits.max_read_file_bytes
        )));
    }

    let bytes = tokio::fs::read(&resolved).await.map_err(|err| {
        ExecutionError::Message(format!(
            "read_file failed for '{}': {err}",
            resolved.display()
        ))
    })?;
    if bytes.len() > limits.max_read_file_bytes {
        return Err(ExecutionError::Message(format!(
            "read_file rejected '{}': file size {} bytes exceeds limit {} bytes",
            resolved.display(),
            bytes.len(),
            limits.max_read_file_bytes
        )));
    }
    String::from_utf8(bytes).map_err(|_| {
        ExecutionError::Message(format!(
            "read_file supports UTF-8 text files only: '{}'",
            resolved.display()
        ))
    })
}

async fn normalize_allowed_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut normalized = Vec::with_capacity(roots.len());
    for root in roots {
        let resolved = tokio::fs::canonicalize(root)
            .await
            .unwrap_or_else(|_| root.clone());
        normalized.push(resolved);
    }
    normalized
}

#[derive(Debug)]
struct ExternalToolCall {
    name: String,
    arguments: Map<String, Value>,
}

fn parse_external_tool_call(
    args: Vec<MontyObject>,
    kwargs: Vec<(MontyObject, MontyObject)>,
) -> std::result::Result<ExternalToolCall, ExecutionError> {
    let mut args = args.into_iter();
    let name = match args.next() {
        Some(MontyObject::String(value)) => value,
        Some(other) => {
            return Err(ExecutionError::Message(format!(
                "tool name must be a string, got {}",
                other.type_name()
            )));
        }
        None => {
            return Err(ExecutionError::Message(
                "tool name argument is required".to_string(),
            ));
        }
    };
    let positional_args_after_name = args.count();
    if positional_args_after_name > 0 {
        return Err(ExecutionError::Message(format!(
            "tool calls inside gambi_execute support keyword arguments only; received {positional_args_after_name} positional argument(s) after tool name"
        )));
    }

    let mut arguments = Map::new();
    for (key, value) in kwargs {
        let key = match key {
            MontyObject::String(value) => value,
            other => {
                return Err(ExecutionError::Message(format!(
                    "tool argument key must be string, got {}",
                    other.type_name()
                )));
            }
        };

        let value = monty_object_to_json(&value).map_err(ExecutionError::Message)?;
        arguments.insert(key, value);
    }

    Ok(ExternalToolCall { name, arguments })
}

async fn handle_tool_call(
    namespaced_name: String,
    arguments: Map<String, Value>,
    runtime: &RuntimeContext<'_>,
) -> std::result::Result<Value, ExecutionError> {
    let Some((server_name, upstream_tool_name)) = split_namespaced(&namespaced_name) else {
        return Err(ExecutionError::Message(format!(
            "tool '{namespaced_name}' must be namespaced as '<server>:<tool>'"
        )));
    };

    let Some(server) = runtime.servers_by_name.get(server_name) else {
        return Err(ExecutionError::Message(format!(
            "unknown tool namespace '{server_name}'"
        )));
    };
    if !server.enabled {
        return Err(ExecutionError::Message(format!(
            "server '{server_name}' is disabled"
        )));
    }
    if !runtime.cfg.is_tool_enabled(server_name, upstream_tool_name) {
        return Err(ExecutionError::Message(format!(
            "tool '{namespaced_name}' is disabled in gambi admin"
        )));
    }
    if runtime.policy == ExecutionPolicy::Safe {
        let description = runtime
            .tool_descriptions
            .get(&(server_name.to_string(), upstream_tool_name.to_string()))
            .map(String::as_str)
            .or_else(|| {
                runtime
                    .cfg
                    .tool_description_override_for(server_name, upstream_tool_name)
            });
        let decision =
            runtime
                .cfg
                .evaluate_tool_policy(server_name, upstream_tool_name, description);
        if decision.level == ToolPolicyLevel::Escalated {
            let reason = format!(
                "ESCALATION_REQUIRED: '{}' is policy-level '{}' (source '{}'). Re-run with gambi_execute_escalated or reclassify this tool in admin policy settings.",
                namespaced_name,
                decision.level.as_str(),
                decision.source.as_str()
            );
            return Err(ExecutionError::EscalationRequired(reason));
        }
    }

    let request = CallToolRequestParams {
        meta: Some(runtime.request_context.meta.clone()),
        name: upstream_tool_name.to_string().into(),
        arguments: Some(arguments),
        task: None,
    };

    let progress_forwarder = runtime
        .request_context
        .meta
        .get_progress_token()
        .map(|token| ProgressForwarder::new(runtime.request_context.peer.clone(), token));

    let first_attempt = runtime
        .upstream
        .call_tool_on_server(
            server,
            runtime.auth_headers,
            request.clone(),
            runtime.tool_call_timeout,
            runtime.request_context.ct.clone(),
            progress_forwarder.clone(),
        )
        .await;
    let result = match first_attempt {
        Ok(result) => result,
        Err(err) if looks_like_auth_failure_error(&err) => {
            let auth = AuthManager::new(runtime.store.clone());
            match auth.refresh(server_name).await {
                Ok(response) => {
                    info!(
                        server = server_name,
                        refreshed = response.refreshed,
                        expires_at_epoch_seconds = ?response.expires_at_epoch_seconds,
                        "upstream tool call hit auth failure; oauth refresh succeeded, retrying"
                    );
                    runtime.upstream.invalidate_discovery_cache().await;
                    let tokens: crate::auth::TokenState = runtime
                        .store
                        .load_tokens_async()
                        .await
                        .map_err(|load_err| {
                        ExecutionError::Message(format!(
                            "failed to load oauth token state after refresh: {load_err}"
                        ))
                    })?;
                    let refreshed_headers = auth_headers_from_token_state(&tokens);
                    runtime
                        .upstream
                        .call_tool_on_server(
                            server,
                            &refreshed_headers,
                            request,
                            runtime.tool_call_timeout,
                            runtime.request_context.ct.clone(),
                            progress_forwarder,
                        )
                        .await
                        .map_err(format_upstream_error)?
                }
                Err(refresh_err) => {
                    warn!(
                        server = server_name,
                        error = %refresh_err,
                        "upstream tool call hit auth failure; oauth refresh failed"
                    );
                    return Err(format_upstream_error(err));
                }
            }
        }
        Err(err) => return Err(format_upstream_error(err)),
    };
    if result.is_error.unwrap_or(false) {
        return Err(ExecutionError::Message(format!(
            "upstream tool '{namespaced_name}' returned an error: {}",
            summarize_upstream_tool_error(&result)
        )));
    }

    if let Some(value) = result.structured_content {
        return Ok(value);
    }

    if !result.content.is_empty() {
        return serde_json::to_value(&result.content).map_err(|err| {
            ExecutionError::Message(format!("failed to serialize tool content result: {err}"))
        });
    }

    Ok(json!({
        "is_error": result.is_error.unwrap_or(false)
    }))
}

fn format_upstream_error(err: UpstreamRequestError) -> ExecutionError {
    match err {
        UpstreamRequestError::Protocol(protocol) => {
            ExecutionError::Message(protocol.message.to_string())
        }
        UpstreamRequestError::Transport(transport) => {
            ExecutionError::Message(format!("upstream transport error: {transport:#}"))
        }
        UpstreamRequestError::Cancelled => ExecutionError::Cancelled,
    }
}

fn looks_like_auth_failure_error(err: &UpstreamRequestError) -> bool {
    match err {
        UpstreamRequestError::Protocol(protocol) => {
            looks_like_auth_failure_message(protocol.message.as_ref())
        }
        UpstreamRequestError::Transport(transport) => {
            crate::upstream::transport_error_is_auth_failure(transport)
                || looks_like_auth_failure_message(&transport.to_string())
        }
        UpstreamRequestError::Cancelled => false,
    }
}

fn looks_like_auth_failure_message(message: &str) -> bool {
    let msg = message.trim().to_ascii_lowercase();
    let initialize_decode_auth_failure = msg.contains("send initialize request")
        && (msg.contains("error decoding response body")
            || msg.contains("unexpected content type"));
    crate::upstream::message_has_auth_required_marker(message)
        || msg.contains("invalid_token")
        || msg.contains("invalid token")
        || msg.contains("auth required")
        || msg.contains("unauthorized")
        || msg.contains("forbidden")
        || msg.contains("401")
        || msg.contains("403")
        || initialize_decode_auth_failure
}

fn map_execution_error(err: ExecutionError) -> McpError {
    match err {
        ExecutionError::Cancelled => McpError::invalid_request("execution cancelled", None),
        ExecutionError::Timeout => McpError::invalid_request("execution timed out", None),
        ExecutionError::EscalationRequired(message) => McpError::invalid_request(message, None),
        ExecutionError::Message(message) => McpError::invalid_request(message, None),
    }
}

async fn build_tool_description_index(
    cfg: &AppConfig,
    upstream: &UpstreamManager,
    auth_headers: &UpstreamAuthHeaders,
) -> HashMap<(String, String), String> {
    let mut descriptions = HashMap::new();
    let enabled_servers = cfg.enabled_servers();
    let Ok(discovered) = upstream
        .discover_tools_from_servers(&enabled_servers, auth_headers)
        .await
    else {
        return descriptions;
    };
    for discovered_tool in discovered.tools {
        if !cfg.is_tool_enabled(&discovered_tool.server_name, &discovered_tool.upstream_name) {
            continue;
        }
        let effective = cfg
            .tool_description_override_for(
                &discovered_tool.server_name,
                &discovered_tool.upstream_name,
            )
            .map(ToOwned::to_owned)
            .or_else(|| {
                discovered_tool
                    .tool
                    .description
                    .as_ref()
                    .map(|value| value.to_string())
            })
            .unwrap_or_default();
        descriptions.insert(
            (
                discovered_tool.server_name.clone(),
                discovered_tool.upstream_name.clone(),
            ),
            effective,
        );
    }
    descriptions
}

fn summarize_upstream_tool_error(result: &CallToolResult) -> String {
    if let Some(value) = &result.structured_content {
        return value.to_string();
    }
    if !result.content.is_empty() {
        return serde_json::to_string(&result.content)
            .unwrap_or_else(|_| "unable to serialize upstream error content".to_string());
    }
    "upstream tool call marked as error without content".to_string()
}

fn map_monty_exception(err: MontyException) -> ExecutionError {
    match err.exc_type() {
        ExcType::TimeoutError => ExecutionError::Timeout,
        _ => ExecutionError::Message(err.summary()),
    }
}

fn build_monty_limits(limits: &ExecutionLimits) -> ResourceLimits {
    let effective_duration = limits
        .max_wall
        .min(Duration::from_secs(limits.max_cpu_seconds.max(1)));

    let mut resource_limits = ResourceLimits::new()
        .max_duration(effective_duration)
        .max_memory(usize::try_from(limits.max_memory_bytes).unwrap_or(usize::MAX));

    // Backward-compatible mapping from historic alloc-bytes control.
    let allocation_limit = parse_u64_env("GAMBI_EXEC_MAX_ALLOCATIONS").unwrap_or_else(|| {
        let bytes = limits.max_allocation_bytes.max(256);
        bytes / 256
    });
    if allocation_limit > 0 {
        resource_limits = resource_limits.max_allocations(allocation_limit as usize);
    }

    resource_limits
}

fn build_monty_script(code: &str, servers: &[ServerConfig]) -> Result<String> {
    let rewritten = rewrite_namespaced_tool_calls(code, servers)?;
    let mut script = String::new();
    script.push_str("def tool(name, **kwargs):\n");
    script.push_str("    return __gambi_call__(name, **kwargs)\n\n");
    script.push_str("def call_tool(name, **kwargs):\n");
    script.push_str("    return __gambi_call__(name, **kwargs)\n\n");
    script.push_str("def read_file(path):\n");
    script.push_str("    return __gambi_read_file__(path)\n\n");
    script.push_str("def json_loads(s):\n");
    script.push_str("    return __gambi_json_loads__(s)\n\n");
    script.push_str("def json_dumps(v):\n");
    script.push_str("    return __gambi_json_dumps__(v)\n\n");
    script.push_str("def __gambi_user_main__():\n");

    if rewritten.trim().is_empty() {
        script.push_str("    return None\n");
    } else {
        for line in rewritten.lines() {
            script.push_str("    ");
            script.push_str(line);
            script.push('\n');
        }
    }

    script.push_str("\n__gambi_user_main__()\n");
    Ok(script)
}

fn rewrite_namespaced_tool_calls(code: &str, servers: &[ServerConfig]) -> Result<String> {
    if servers.is_empty() || code.is_empty() {
        return Ok(code.to_string());
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ParseState {
        Normal,
        LineComment,
        SingleQuoted,
        DoubleQuoted,
        TripleSingleQuoted,
        TripleDoubleQuoted,
    }

    let mut rewritten = String::with_capacity(code.len().saturating_add(32));
    let mut index = 0usize;
    let mut state = ParseState::Normal;

    while index < code.len() {
        match state {
            ParseState::Normal => {
                if let Some((replacement, consumed_to)) =
                    try_rewrite_server_call_at(code, index, servers)
                {
                    rewritten.push_str(&replacement);
                    index = consumed_to;
                    continue;
                }

                if code[index..].starts_with("'''") {
                    rewritten.push_str("'''");
                    index += 3;
                    state = ParseState::TripleSingleQuoted;
                    continue;
                }
                if code[index..].starts_with("\"\"\"") {
                    rewritten.push_str("\"\"\"");
                    index += 3;
                    state = ParseState::TripleDoubleQuoted;
                    continue;
                }

                let ch = next_char(code, index);
                match ch {
                    '#' => state = ParseState::LineComment,
                    '\'' => state = ParseState::SingleQuoted,
                    '"' => state = ParseState::DoubleQuoted,
                    _ => {}
                }
                rewritten.push(ch);
                index += ch.len_utf8();
            }
            ParseState::LineComment => {
                let ch = next_char(code, index);
                rewritten.push(ch);
                index += ch.len_utf8();
                if ch == '\n' {
                    state = ParseState::Normal;
                }
            }
            ParseState::SingleQuoted => {
                if code[index..].starts_with("\\'") {
                    rewritten.push_str("\\'");
                    index += 2;
                    continue;
                }
                let ch = next_char(code, index);
                rewritten.push(ch);
                index += ch.len_utf8();
                if ch == '\'' {
                    state = ParseState::Normal;
                }
            }
            ParseState::DoubleQuoted => {
                if code[index..].starts_with("\\\"") {
                    rewritten.push_str("\\\"");
                    index += 2;
                    continue;
                }
                let ch = next_char(code, index);
                rewritten.push(ch);
                index += ch.len_utf8();
                if ch == '"' {
                    state = ParseState::Normal;
                }
            }
            ParseState::TripleSingleQuoted => {
                if code[index..].starts_with("'''") {
                    rewritten.push_str("'''");
                    index += 3;
                    state = ParseState::Normal;
                    continue;
                }
                let ch = next_char(code, index);
                rewritten.push(ch);
                index += ch.len_utf8();
            }
            ParseState::TripleDoubleQuoted => {
                if code[index..].starts_with("\"\"\"") {
                    rewritten.push_str("\"\"\"");
                    index += 3;
                    state = ParseState::Normal;
                    continue;
                }
                let ch = next_char(code, index);
                rewritten.push(ch);
                index += ch.len_utf8();
            }
        }
    }

    Ok(rewritten)
}

fn try_rewrite_server_call_at(
    code: &str,
    start_index: usize,
    servers: &[ServerConfig],
) -> Option<(String, usize)> {
    for server in servers {
        let server_name = server.name.as_str();
        if !code[start_index..].starts_with(server_name) {
            continue;
        }

        if start_index > 0 {
            let immediate_before = code[..start_index].chars().next_back()?;
            if is_python_identifier_char(immediate_before) {
                continue;
            }
            if previous_non_whitespace_char(code, start_index).is_some_and(|ch| ch == '.') {
                continue;
            }
        }

        let mut cursor = start_index + server_name.len();
        cursor = skip_python_whitespace(code, cursor);
        if cursor >= code.len() {
            continue;
        }

        if next_char(code, cursor) != '.' {
            continue;
        }
        cursor += 1;
        cursor = skip_python_whitespace(code, cursor);
        if cursor >= code.len() {
            continue;
        }

        let tool_start = cursor;
        let first = next_char(code, cursor);
        if !is_python_identifier_start(first) {
            continue;
        }
        cursor += first.len_utf8();
        while cursor < code.len() {
            let ch = next_char(code, cursor);
            if !is_python_identifier_char(ch) {
                break;
            }
            cursor += ch.len_utf8();
        }
        let tool_name = &code[tool_start..cursor];
        cursor = skip_python_whitespace(code, cursor);
        if cursor >= code.len() {
            continue;
        }

        if next_char(code, cursor) != '(' {
            continue;
        }
        let args_start = cursor + 1;
        let after_open = skip_python_whitespace(code, args_start);
        if after_open < code.len() && next_char(code, after_open) == ')' {
            let replacement = format!("__gambi_call__(\"{}:{}\")", server_name, tool_name);
            return Some((replacement, after_open + 1));
        }

        let replacement = format!("__gambi_call__(\"{}:{}\", ", server_name, tool_name);
        return Some((replacement, args_start));
    }

    None
}

fn skip_python_whitespace(code: &str, mut index: usize) -> usize {
    while index < code.len() {
        let ch = next_char(code, index);
        if !ch.is_whitespace() {
            break;
        }
        index += ch.len_utf8();
    }
    index
}

fn is_python_identifier_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_python_identifier_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn next_char(code: &str, index: usize) -> char {
    code[index..]
        .chars()
        .next()
        .expect("parser index should always be a valid character boundary")
}

fn previous_non_whitespace_char(code: &str, mut index: usize) -> Option<char> {
    while index > 0 {
        let ch = code[..index].chars().next_back()?;
        if !ch.is_whitespace() {
            return Some(ch);
        }
        index -= ch.len_utf8();
    }
    None
}

fn monty_object_to_json(object: &MontyObject) -> std::result::Result<Value, String> {
    match object {
        MontyObject::None => Ok(Value::Null),
        MontyObject::Bool(v) => Ok(Value::Bool(*v)),
        MontyObject::Int(v) => Ok(Value::Number((*v).into())),
        MontyObject::BigInt(v) => {
            if let Some(as_i64) = v.to_i64() {
                Ok(Value::Number(serde_json::Number::from(as_i64)))
            } else if let Some(as_u64) = v.to_u64() {
                Ok(Value::Number(serde_json::Number::from(as_u64)))
            } else {
                Ok(Value::String(v.to_string()))
            }
        }
        MontyObject::Float(v) => serde_json::Number::from_f64(*v)
            .map(Value::Number)
            .ok_or_else(|| "cannot convert non-finite float to JSON".to_string()),
        MontyObject::String(v) => Ok(Value::String(v.clone())),
        MontyObject::Bytes(v) => Ok(Value::Array(
            v.iter()
                .map(|b| Value::Number((*b as u64).into()))
                .collect(),
        )),
        MontyObject::List(values) | MontyObject::Tuple(values) => values
            .iter()
            .map(monty_object_to_json)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(Value::Array),
        MontyObject::Dict(pairs) => {
            let mut map = Map::new();
            for (k, v) in pairs {
                let key = match k {
                    MontyObject::String(s) => s.clone(),
                    other => {
                        return Err(format!(
                            "cannot convert non-string dict key '{}' to JSON",
                            other.type_name()
                        ));
                    }
                };
                map.insert(key, monty_object_to_json(v)?);
            }
            Ok(Value::Object(map))
        }
        other => Err(format!(
            "unsupported monty output type '{}' for JSON conversion",
            other.type_name()
        )),
    }
}

fn json_to_monty_object(value: &Value) -> std::result::Result<MontyObject, String> {
    match value {
        Value::Null => Ok(MontyObject::None),
        Value::Bool(v) => Ok(MontyObject::Bool(*v)),
        Value::Number(v) => {
            if let Some(i) = v.as_i64() {
                return Ok(MontyObject::Int(i));
            }
            if let Some(u) = v.as_u64() {
                if let Ok(i) = i64::try_from(u) {
                    return Ok(MontyObject::Int(i));
                }
                return Ok(MontyObject::BigInt(BigInt::from(u)));
            }
            if let Some(f) = v.as_f64() {
                return Ok(MontyObject::Float(f));
            }
            Err("unsupported JSON number representation".to_string())
        }
        Value::String(v) => Ok(MontyObject::String(v.clone())),
        Value::Array(values) => values
            .iter()
            .map(json_to_monty_object)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(MontyObject::List),
        Value::Object(values) => {
            let mut dict = Vec::with_capacity(values.len());
            for (k, v) in values {
                dict.push((MontyObject::String(k.clone()), json_to_monty_object(v)?));
            }
            Ok(MontyObject::dict(dict))
        }
    }
}

fn parse_u64_env(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.parse::<u64>().ok()
}

fn parse_usize_env(key: &str) -> Option<usize> {
    std::env::var(key).ok()?.parse::<usize>().ok()
}

fn default_read_file_roots() -> Vec<PathBuf> {
    let root = PathBuf::from(DEFAULT_READ_FILE_ROOT);
    vec![std::fs::canonicalize(&root).unwrap_or(root)]
}

#[derive(Debug)]
struct BoundedPrint {
    output: CollectStringPrint,
    max_bytes: usize,
    bytes: usize,
}

impl BoundedPrint {
    fn new(max_bytes: usize) -> Self {
        Self {
            output: CollectStringPrint::new(),
            max_bytes,
            bytes: 0,
        }
    }

    fn into_lines(self) -> Vec<String> {
        self.output
            .into_output()
            .lines()
            .map(|line| line.to_string())
            .collect()
    }

    fn consume_bytes(&mut self, additional: usize) -> std::result::Result<(), MontyException> {
        self.bytes = self.bytes.saturating_add(additional);
        if self.bytes > self.max_bytes {
            return Err(MontyException::new(
                ExcType::MemoryError,
                Some(format!(
                    "execution stdout exceeded configured cap ({} bytes)",
                    self.max_bytes
                )),
            ));
        }
        Ok(())
    }
}

impl monty::PrintWriter for BoundedPrint {
    fn stdout_write(
        &mut self,
        output: std::borrow::Cow<'_, str>,
    ) -> std::result::Result<(), MontyException> {
        self.consume_bytes(output.len())?;
        self.output.stdout_write(output)
    }

    fn stdout_push(&mut self, end: char) -> std::result::Result<(), MontyException> {
        self.consume_bytes(end.len_utf8())?;
        self.output.stdout_push(end)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use monty::MontyObject;
    use std::time::Duration;

    use super::{
        ExecutionLimits, build_monty_script, looks_like_auth_failure_error,
        looks_like_auth_failure_message, normalize_upstream_tool_timeout, parse_external_tool_call,
        parse_json_dumps_call, parse_json_loads_call, parse_read_file_call,
        read_file_for_execution, rewrite_namespaced_tool_calls,
    };
    use crate::config::ServerConfig;
    use crate::upstream::UpstreamRequestError;

    #[test]
    fn script_builder_rewrites_server_tool_calls() {
        let rewritten = rewrite_namespaced_tool_calls(
            "return port.sample_tool(value=1)",
            &[ServerConfig {
                name: "port".to_string(),
                url: "https://example.com/mcp".to_string(),
                oauth: None,
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            }],
        )
        .expect("rewrite should succeed");

        assert!(rewritten.contains("__gambi_call__(\"port:sample_tool\", value=1)"));
    }

    #[test]
    fn script_builder_wraps_user_main() {
        let script = build_monty_script(
            "return fixture.fixture_echo(value=1)",
            &[ServerConfig {
                name: "fixture".to_string(),
                url: "stdio://fixture".to_string(),
                oauth: None,
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            }],
        )
        .expect("script build should succeed");

        assert!(script.contains("def tool(name, **kwargs):"));
        assert!(script.contains("return __gambi_call__(name, **kwargs)"));
        assert!(script.contains("def call_tool(name, **kwargs):"));
        assert!(script.contains("return __gambi_call__(name, **kwargs)"));
        assert!(script.contains("def read_file(path):"));
        assert!(script.contains("return __gambi_read_file__(path)"));
        assert!(script.contains("def json_loads(s):"));
        assert!(script.contains("return __gambi_json_loads__(s)"));
        assert!(script.contains("def json_dumps(v):"));
        assert!(script.contains("return __gambi_json_dumps__(v)"));
        assert!(script.contains("def __gambi_user_main__():"));
        assert!(script.contains("__gambi_user_main__()"));
        assert!(script.contains("__gambi_call__(\"fixture:fixture_echo\", value=1)"));
    }

    #[test]
    fn parse_external_tool_call_keeps_empty_object_arguments() {
        let parsed = parse_external_tool_call(
            vec![MontyObject::String("fixture:fixture_echo".to_string())],
            vec![],
        )
        .expect("parse should succeed");

        assert!(parsed.arguments.is_empty());
    }

    #[test]
    fn normalize_upstream_tool_timeout_clamps_to_five_minutes() {
        let capped = normalize_upstream_tool_timeout(Some(999_999))
            .expect("timeout should parse")
            .expect("timeout should be present");
        assert_eq!(capped, Duration::from_millis(300_000));
    }

    #[test]
    fn normalize_upstream_tool_timeout_rejects_zero() {
        let err = normalize_upstream_tool_timeout(Some(0)).expect_err("zero should fail");
        assert!(err.contains("greater than 0"));
    }

    #[test]
    fn parse_read_file_call_accepts_positional_or_keyword_path() {
        let positional =
            parse_read_file_call(vec![MontyObject::String("/tmp/doc.md".to_string())], vec![])
                .expect("positional path should parse");
        assert_eq!(positional, "/tmp/doc.md");

        let keyword = parse_read_file_call(
            vec![],
            vec![(
                MontyObject::String("path".to_string()),
                MontyObject::String("/tmp/doc.md".to_string()),
            )],
        )
        .expect("keyword path should parse");
        assert_eq!(keyword, "/tmp/doc.md");
    }

    #[test]
    fn parse_read_file_call_rejects_invalid_shapes() {
        let missing = parse_read_file_call(vec![], vec![]).expect_err("missing path must fail");
        assert!(missing.to_string().contains("requires a path"));

        let non_string = parse_read_file_call(vec![MontyObject::Int(1)], vec![])
            .expect_err("non-string path must fail");
        assert!(non_string.to_string().contains("must be a string"));

        let duplicate = parse_read_file_call(
            vec![MontyObject::String("/tmp/a".to_string())],
            vec![(
                MontyObject::String("path".to_string()),
                MontyObject::String("/tmp/b".to_string()),
            )],
        )
        .expect_err("duplicate path must fail");
        assert!(duplicate.to_string().contains("provided once"));
    }

    #[test]
    fn default_limits_restrict_read_file_to_tmp_root() {
        let limits = ExecutionLimits::default();
        let roots = limits
            .read_file_allowed_roots
            .as_ref()
            .expect("default read_file roots should be configured");
        assert_eq!(roots.len(), 1);
        let expected =
            std::fs::canonicalize("/tmp").unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
        assert_eq!(roots[0], expected);
    }

    #[tokio::test]
    async fn read_file_for_execution_reads_utf8_when_allowed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("payload.md");
        std::fs::write(&target, "hello gambi").expect("write payload");

        let limits = ExecutionLimits {
            max_read_file_bytes: 1024,
            read_file_allowed_roots: Some(vec![temp.path().to_path_buf()]),
            ..ExecutionLimits::default()
        };
        let value = read_file_for_execution(target.to_str().expect("utf8 path"), &limits)
            .await
            .expect("file should be readable");
        assert_eq!(value, "hello gambi");
    }

    #[tokio::test]
    async fn read_file_for_execution_enforces_roots_and_size_limit() {
        let allowed = tempfile::tempdir().expect("allowed tempdir");
        let denied = tempfile::tempdir().expect("denied tempdir");
        let denied_file = denied.path().join("payload.md");
        std::fs::write(&denied_file, "outside root").expect("write denied payload");

        let limits = ExecutionLimits {
            max_read_file_bytes: 8,
            read_file_allowed_roots: Some(vec![allowed.path().to_path_buf()]),
            ..ExecutionLimits::default()
        };
        let denied_err = read_file_for_execution(denied_file.to_str().expect("utf8 path"), &limits)
            .await
            .expect_err("outside root must fail");
        assert!(denied_err.to_string().contains("outside allowed roots"));

        let allowed_big_file = allowed.path().join("big.md");
        std::fs::write(&allowed_big_file, "1234567890").expect("write large payload");
        let too_large =
            read_file_for_execution(allowed_big_file.to_str().expect("utf8 path"), &limits)
                .await
                .expect_err("file over limit must fail");
        assert!(too_large.to_string().contains("exceeds limit"));
    }

    #[test]
    fn rewrite_skips_comments_and_strings() {
        let rewritten = rewrite_namespaced_tool_calls(
            r#"
note = "fixture.fixture_echo(value=1)"
# fixture.fixture_echo(value=2)
result = fixture.fixture_echo(value=3)
"#,
            &[ServerConfig {
                name: "fixture".to_string(),
                url: "stdio://fixture".to_string(),
                oauth: None,
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            }],
        )
        .expect("rewrite should succeed");

        assert!(
            rewritten.contains("result = __gambi_call__(\"fixture:fixture_echo\", value=3)"),
            "expected real call to be rewritten"
        );
        assert!(
            rewritten.contains("\"fixture.fixture_echo(value=1)\""),
            "expected string literal call text to remain unchanged"
        );
        assert!(
            rewritten.contains("# fixture.fixture_echo(value=2)"),
            "expected comment call text to remain unchanged"
        );
    }

    #[test]
    fn rewrite_skips_triple_quoted_strings() {
        let rewritten = rewrite_namespaced_tool_calls(
            r#"
doc = """
fixture.fixture_echo(value=1)
"""
return fixture.fixture_echo(value=2)
"#,
            &[ServerConfig {
                name: "fixture".to_string(),
                url: "stdio://fixture".to_string(),
                oauth: None,
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            }],
        )
        .expect("rewrite should succeed");

        assert!(
            rewritten.contains("return __gambi_call__(\"fixture:fixture_echo\", value=2)"),
            "expected real call to be rewritten"
        );
        assert!(
            rewritten.contains("fixture.fixture_echo(value=1)"),
            "expected triple-quoted string content to remain unchanged"
        );
    }

    #[test]
    fn rewrite_supports_zero_arg_calls() {
        let rewritten = rewrite_namespaced_tool_calls(
            "return fixture.fixture_echo()",
            &[ServerConfig {
                name: "fixture".to_string(),
                url: "stdio://fixture".to_string(),
                oauth: None,
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            }],
        )
        .expect("rewrite should succeed");

        assert!(rewritten.contains("return __gambi_call__(\"fixture:fixture_echo\")"));
    }

    #[test]
    fn auth_failure_message_matcher_covers_common_cases() {
        assert!(looks_like_auth_failure_message("invalid_token"));
        assert!(looks_like_auth_failure_message("Auth required"));
        assert!(looks_like_auth_failure_message("401 unauthorized"));
        assert!(looks_like_auth_failure_message("403 forbidden"));
        assert!(looks_like_auth_failure_message(
            "gambi_auth_required: server='port' status=401"
        ));
        assert!(looks_like_auth_failure_message(
            "Client error: error decoding response body, when send initialize request"
        ));
        assert!(!looks_like_auth_failure_message("connection reset by peer"));
    }

    #[test]
    fn auth_failure_error_matcher_detects_transport_errors() {
        let err =
            UpstreamRequestError::Transport(anyhow!("Auth required, when send initialize request"));
        assert!(looks_like_auth_failure_error(&err));
    }

    #[test]
    fn rewrite_skips_prefixed_triple_quoted_strings() {
        let rewritten = rewrite_namespaced_tool_calls(
            r#"
doc_raw = r"""fixture.fixture_echo(value=1)"""
doc_fmt = f"""fixture.fixture_echo(value=2)"""
return fixture.fixture_echo(value=3)
"#,
            &[ServerConfig {
                name: "fixture".to_string(),
                url: "stdio://fixture".to_string(),
                oauth: None,
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            }],
        )
        .expect("rewrite should succeed");

        assert!(rewritten.contains(r#"r"""fixture.fixture_echo(value=1)""""#));
        assert!(rewritten.contains(r#"f"""fixture.fixture_echo(value=2)""""#));
        assert!(rewritten.contains("return __gambi_call__(\"fixture:fixture_echo\", value=3)"));
    }

    #[test]
    fn rewrite_skips_member_access_chains() {
        let rewritten = rewrite_namespaced_tool_calls(
            "return container.fixture.fixture_echo(value=1)",
            &[ServerConfig {
                name: "fixture".to_string(),
                url: "stdio://fixture".to_string(),
                oauth: None,
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            }],
        )
        .expect("rewrite should succeed");

        assert!(rewritten.contains("container.fixture.fixture_echo(value=1)"));
        assert!(
            !rewritten.contains("__gambi_call__(\"fixture:fixture_echo\""),
            "member access chains must not be rewritten as upstream tool calls"
        );
    }

    #[test]
    fn parse_json_loads_call_accepts_string_arg() {
        let result =
            parse_json_loads_call(vec![MontyObject::String("{\"a\": 1}".to_string())], vec![])
                .expect("valid json_loads call should parse");
        assert_eq!(result, "{\"a\": 1}");
    }

    #[test]
    fn parse_json_loads_call_rejects_non_string() {
        let err = parse_json_loads_call(vec![MontyObject::Int(42)], vec![])
            .expect_err("non-string arg must fail");
        assert!(err.to_string().contains("must be a string"));
    }

    #[test]
    fn parse_json_loads_call_rejects_missing_arg() {
        let err = parse_json_loads_call(vec![], vec![]).expect_err("missing arg must fail");
        assert!(err.to_string().contains("requires a string argument"));
    }

    #[test]
    fn parse_json_loads_call_rejects_too_many_args() {
        let err = parse_json_loads_call(
            vec![
                MontyObject::String("a".to_string()),
                MontyObject::String("b".to_string()),
            ],
            vec![],
        )
        .expect_err("too many args must fail");
        assert!(err.to_string().contains("exactly one"));
    }

    #[test]
    fn parse_json_loads_call_rejects_kwargs() {
        let err = parse_json_loads_call(
            vec![],
            vec![(
                MontyObject::String("s".to_string()),
                MontyObject::String("{}".to_string()),
            )],
        )
        .expect_err("kwargs must fail");
        assert!(err.to_string().contains("keyword arguments"));
    }

    #[test]
    fn parse_json_dumps_call_accepts_any_type() {
        let result = parse_json_dumps_call(vec![MontyObject::Int(42)], vec![])
            .expect("valid json_dumps call should parse");
        assert_eq!(result, MontyObject::Int(42));

        let result = parse_json_dumps_call(vec![MontyObject::String("hello".to_string())], vec![])
            .expect("string arg should parse");
        assert_eq!(result, MontyObject::String("hello".to_string()));
    }

    #[test]
    fn parse_json_dumps_call_rejects_missing_arg() {
        let err = parse_json_dumps_call(vec![], vec![]).expect_err("missing arg must fail");
        assert!(err.to_string().contains("requires one argument"));
    }

    #[test]
    fn parse_json_dumps_call_rejects_too_many_args() {
        let err = parse_json_dumps_call(vec![MontyObject::Int(1), MontyObject::Int(2)], vec![])
            .expect_err("too many args must fail");
        assert!(err.to_string().contains("exactly one"));
    }

    #[test]
    fn parse_json_dumps_call_rejects_kwargs() {
        let err = parse_json_dumps_call(
            vec![],
            vec![(MontyObject::String("v".to_string()), MontyObject::Int(1))],
        )
        .expect_err("kwargs must fail");
        assert!(err.to_string().contains("keyword arguments"));
    }
}
