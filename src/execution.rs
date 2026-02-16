use std::collections::HashMap;
use std::fmt;
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

const DEFAULT_MAX_WALL_MS: u64 = 10_000;
const DEFAULT_MAX_CPU_SECONDS: u64 = 10;
const DEFAULT_MAX_MEMORY_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_ALLOCATION_BYTES: u64 = 128 * 1024 * 1024;
const DEFAULT_MAX_STDOUT_BYTES: usize = 1_048_576;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionPolicy {
    Safe,
    Escalated,
}

#[derive(Debug, Clone)]
pub struct ExecutionLimits {
    pub max_wall: Duration,
    pub max_cpu_seconds: u64,
    pub max_memory_bytes: u64,
    pub max_allocation_bytes: u64,
    pub max_stdout_bytes: usize,
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
    let runtime_context = RuntimeContext {
        store,
        cfg: &cfg,
        servers_by_name: &servers_by_name,
        auth_headers: &auth_headers,
        tool_descriptions: &tool_descriptions,
        policy,
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
    upstream: &'a UpstreamManager,
    request_context: &'a RequestContext<RoleServer>,
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
        vec![EXTERNAL_CALL_FN.to_string()],
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
                if function_name != EXTERNAL_CALL_FN {
                    return Err(ExecutionError::Message(format!(
                        "unexpected external function call '{function_name}'"
                    )));
                }

                let tool_call = parse_external_tool_call(args, kwargs)?;
                tool_calls = tool_calls.saturating_add(1);
                let result_json =
                    handle_tool_call(tool_call.name, tool_call.arguments, runtime).await?;

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
            looks_like_auth_failure_message(&transport.to_string())
        }
        UpstreamRequestError::Cancelled => false,
    }
}

fn looks_like_auth_failure_message(message: &str) -> bool {
    let msg = message.trim().to_ascii_lowercase();
    msg.contains("invalid_token")
        || msg.contains("invalid token")
        || msg.contains("auth required")
        || msg.contains("unauthorized")
        || msg.contains("forbidden")
        || msg.contains("401")
        || msg.contains("403")
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
    let Ok(discovered) = upstream
        .discover_tools_from_servers(&cfg.servers, auth_headers)
        .await
    else {
        return descriptions;
    };
    for discovered_tool in discovered.tools {
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

    use super::{
        build_monty_script, looks_like_auth_failure_error, looks_like_auth_failure_message,
        parse_external_tool_call, rewrite_namespaced_tool_calls,
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
            }],
        )
        .expect("script build should succeed");

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
            }],
        )
        .expect("rewrite should succeed");

        assert!(rewritten.contains("container.fixture.fixture_echo(value=1)"));
        assert!(
            !rewritten.contains("__gambi_call__(\"fixture:fixture_echo\""),
            "member access chains must not be rewritten as upstream tool calls"
        );
    }
}
