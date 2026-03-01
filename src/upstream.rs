use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rmcp::{
    ClientHandler, ErrorData as McpError, RoleClient, RoleServer, ServiceError, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, CancelledNotificationParam, ClientRequest,
        GetPromptRequestParams, GetPromptResult, ProgressNotificationParam, ProgressToken, Prompt,
        ReadResourceRequestParams, ReadResourceResult, Request, Resource, ResourceTemplate,
        ServerResult, Tool,
    },
    service::{NotificationContext, Peer, PeerRequestOptions, RunningService},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess, common::client_side_sse::SseRetryPolicy,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use url::Url;

use crate::auth::TokenState;
use crate::config::{ServerConfig, TransportMode};
use crate::namespacing::{namespaced, namespaced_resource_template_uri, namespaced_resource_uri};
use crate::sse_transport::LegacySseWorker;

const DEFAULT_UPSTREAM_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_UPSTREAM_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);
const UPSTREAM_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_PARALLEL_DISCOVERY: usize = 8;
const HTTP_RECONNECT_MAX_ATTEMPTS: usize = 6;
const HTTP_RECONNECT_BASE_DELAY: Duration = Duration::from_millis(250);
const HTTP_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(8);
const HTTP_CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_AUTH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const DISCOVERY_CACHE_TTL: Duration = Duration::from_secs(5);
const AUTH_REQUIRED_MARKER: &str = "gambi_auth_required";

#[derive(Debug)]
pub enum UpstreamRequestError {
    Protocol(McpError),
    Transport(anyhow::Error),
    Cancelled,
}

impl fmt::Display for UpstreamRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(err) => write!(f, "upstream MCP protocol error: {}", err.message),
            Self::Transport(err) => write!(f, "upstream transport error: {err}"),
            Self::Cancelled => write!(f, "upstream request cancelled"),
        }
    }
}

impl std::error::Error for UpstreamRequestError {}

#[derive(Debug)]
pub struct UpstreamAuthFailure {
    server_name: String,
    status_code: Option<u16>,
    www_authenticate: Option<String>,
}

impl UpstreamAuthFailure {
    fn new(
        server_name: String,
        status_code: Option<u16>,
        www_authenticate: Option<String>,
    ) -> Self {
        Self {
            server_name,
            status_code,
            www_authenticate,
        }
    }
}

impl fmt::Display for UpstreamAuthFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{AUTH_REQUIRED_MARKER}: server='{}'", self.server_name)?;
        if let Some(status) = self.status_code {
            write!(f, " status={status}")?;
        }
        if let Some(challenge) = &self.www_authenticate {
            write!(f, " www-authenticate={challenge:?}")?;
        }
        Ok(())
    }
}

impl std::error::Error for UpstreamAuthFailure {}

pub fn transport_error_is_auth_failure(err: &anyhow::Error) -> bool {
    err.is::<UpstreamAuthFailure>()
}

pub fn message_has_auth_required_marker(message: &str) -> bool {
    message.contains(AUTH_REQUIRED_MARKER)
}

fn map_service_error(err: ServiceError) -> UpstreamRequestError {
    match err {
        ServiceError::McpError(mcp_error) => UpstreamRequestError::Protocol(mcp_error),
        ServiceError::Cancelled { .. } => UpstreamRequestError::Cancelled,
        other => UpstreamRequestError::Transport(anyhow!(other)),
    }
}

#[derive(Debug, Clone)]
pub struct StdioServerTarget {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpServerTarget {
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseServerTarget {
    pub uri: String,
}

#[derive(Debug, Clone)]
enum UpstreamTransportTarget {
    Stdio(StdioServerTarget),
    Http(HttpServerTarget),
    Sse(SseServerTarget),
}

#[derive(Debug, Clone, Default)]
pub struct UpstreamAuthHeaders {
    bearer_tokens: HashMap<String, String>,
}

impl UpstreamAuthHeaders {
    pub fn bearer_token_for(&self, server_name: &str) -> Option<&str> {
        self.bearer_tokens.get(server_name).map(String::as_str)
    }
}

pub fn auth_headers_from_token_state(tokens: &TokenState) -> UpstreamAuthHeaders {
    let mut headers = HashMap::new();
    for (server_name, token) in &tokens.oauth_tokens {
        if token.access_token.trim().is_empty() {
            continue;
        }

        if token
            .token_type
            .as_deref()
            .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
        {
            continue;
        }

        headers.insert(server_name.clone(), token.access_token.clone());
    }

    UpstreamAuthHeaders {
        bearer_tokens: headers,
    }
}

#[derive(Debug, Clone)]
struct BoundedExponentialBackoff {
    max_attempts: usize,
    base_delay: Duration,
    max_delay: Duration,
}

impl Default for BoundedExponentialBackoff {
    fn default() -> Self {
        Self {
            max_attempts: HTTP_RECONNECT_MAX_ATTEMPTS,
            base_delay: HTTP_RECONNECT_BASE_DELAY,
            max_delay: HTTP_RECONNECT_MAX_DELAY,
        }
    }
}

impl SseRetryPolicy for BoundedExponentialBackoff {
    fn retry(&self, current_times: usize) -> Option<Duration> {
        if current_times >= self.max_attempts {
            return None;
        }

        let exponent = current_times.saturating_sub(1).min(31) as u32;
        let factor = 1u32 << exponent;
        let delay = self
            .base_delay
            .checked_mul(factor)
            .unwrap_or(self.max_delay);
        Some(delay.min(self.max_delay))
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredTool {
    pub tool: Tool,
    pub namespaced_name: String,
    pub server_name: String,
    pub upstream_name: String,
}

#[derive(Debug, Clone)]
pub struct DiscoveryFailure {
    pub server_name: String,
    pub message: String,
}

#[derive(Clone)]
pub struct ProgressForwarder {
    peer: Peer<RoleServer>,
    progress_token: ProgressToken,
}

impl ProgressForwarder {
    pub fn new(peer: Peer<RoleServer>, progress_token: ProgressToken) -> Self {
        Self {
            peer,
            progress_token,
        }
    }
}

#[derive(Clone)]
struct ProgressRegistration {
    id: u64,
    forwarder: ProgressForwarder,
}

#[derive(Default)]
struct ProgressRegistry {
    next_id: AtomicU64,
    by_token: RwLock<HashMap<ProgressToken, Vec<ProgressRegistration>>>,
}

impl ProgressRegistry {
    async fn register(&self, forwarder: ProgressForwarder) -> (ProgressToken, u64) {
        let token = forwarder.progress_token.clone();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.by_token.write().await;
        guard
            .entry(token.clone())
            .or_default()
            .push(ProgressRegistration { id, forwarder });
        (token, id)
    }

    async fn unregister(&self, token: &ProgressToken, id: u64) {
        let mut guard = self.by_token.write().await;
        if let Some(entries) = guard.get_mut(token) {
            entries.retain(|entry| entry.id != id);
            if entries.is_empty() {
                guard.remove(token);
            }
        }
    }

    async fn resolve(&self, token: &ProgressToken) -> Vec<ProgressForwarder> {
        let guard = self.by_token.read().await;
        guard
            .get(token)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| entry.forwarder.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }
}

#[derive(Clone)]
struct UpstreamClient {
    progress_registry: Arc<ProgressRegistry>,
}

impl Default for UpstreamClient {
    fn default() -> Self {
        Self {
            progress_registry: Arc::new(ProgressRegistry::default()),
        }
    }
}

impl UpstreamClient {
    fn progress_registry(&self) -> Arc<ProgressRegistry> {
        Arc::clone(&self.progress_registry)
    }
}

impl ClientHandler for UpstreamClient {
    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let forwarders = self.progress_registry.resolve(&params.progress_token).await;
        for forwarder in forwarders {
            let _ = forwarder.peer.notify_progress(params.clone()).await;
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct DiscoveryResult {
    pub tools: Vec<DiscoveredTool>,
    pub failures: Vec<DiscoveryFailure>,
}

#[derive(Debug, Clone)]
pub struct DiscoveredPrompt {
    pub prompt: Prompt,
    pub namespaced_name: String,
}

#[derive(Debug, Default, Clone)]
pub struct PromptDiscoveryResult {
    pub prompts: Vec<DiscoveredPrompt>,
    pub failures: Vec<DiscoveryFailure>,
}

#[derive(Debug, Clone)]
pub struct DiscoveredResource {
    pub resource: Resource,
    pub namespaced_uri: String,
}

#[derive(Debug, Clone)]
pub struct DiscoveredResourceTemplate {
    pub resource_template: ResourceTemplate,
    pub namespaced_uri_template: String,
}

#[derive(Debug, Default, Clone)]
pub struct ResourceDiscoveryResult {
    pub resources: Vec<DiscoveredResource>,
    pub resource_templates: Vec<DiscoveredResourceTemplate>,
    pub failures: Vec<DiscoveryFailure>,
}

#[derive(Debug, Clone)]
struct DiscoveryCacheEntry<T> {
    key: u64,
    cached_at: Instant,
    value: T,
}

#[derive(Debug, Default)]
struct DiscoveryCaches {
    tools: Option<DiscoveryCacheEntry<DiscoveryResult>>,
    prompts: Option<DiscoveryCacheEntry<PromptDiscoveryResult>>,
    resources: Option<DiscoveryCacheEntry<ResourceDiscoveryResult>>,
}

struct ManagedUpstreamClient {
    server_name: String,
    server_url: String,
    auth_header: Option<String>,
    instructions: Option<String>,
    peer: Peer<RoleClient>,
    progress_registry: Arc<ProgressRegistry>,
    running: Mutex<Option<RunningService<RoleClient, UpstreamClient>>>,
}

impl ManagedUpstreamClient {
    fn is_same_config(&self, server: &ServerConfig, auth_header: Option<&str>) -> bool {
        self.server_url == server.url && self.auth_header.as_deref() == auth_header
    }

    async fn shutdown(&self) {
        let maybe_running = { self.running.lock().await.take() };
        if let Some(running) = maybe_running {
            let _ = timeout(UPSTREAM_SHUTDOWN_TIMEOUT, running.cancel()).await;
        }
    }
}

#[derive(Default)]
struct UpstreamManagerInner {
    clients: Mutex<HashMap<String, Arc<ManagedUpstreamClient>>>,
    client_creation_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    discovery_caches: Mutex<DiscoveryCaches>,
}

#[derive(Clone)]
pub struct UpstreamManager {
    inner: Arc<UpstreamManagerInner>,
    request_timeout: Duration,
    discovery_timeout: Duration,
}

impl fmt::Debug for UpstreamManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpstreamManager").finish_non_exhaustive()
    }
}

impl Default for UpstreamManager {
    fn default() -> Self {
        Self {
            inner: Arc::new(UpstreamManagerInner::default()),
            request_timeout: duration_env_ms(
                "GAMBI_UPSTREAM_REQUEST_TIMEOUT_MS",
                DEFAULT_UPSTREAM_REQUEST_TIMEOUT,
            ),
            discovery_timeout: duration_env_ms(
                "GAMBI_UPSTREAM_DISCOVERY_TIMEOUT_MS",
                DEFAULT_UPSTREAM_DISCOVERY_TIMEOUT,
            ),
        }
    }
}

impl UpstreamManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn shutdown(&self) {
        let clients = {
            let mut guard = self.inner.clients.lock().await;
            guard.drain().map(|(_, client)| client).collect::<Vec<_>>()
        };

        for client in clients {
            client.shutdown().await;
        }
    }

    pub async fn invalidate_discovery_cache(&self) {
        let mut guard = self.inner.discovery_caches.lock().await;
        guard.tools = None;
        guard.prompts = None;
        guard.resources = None;
    }

    pub async fn cached_tool_discovery_failure_count(&self) -> usize {
        let guard = self.inner.discovery_caches.lock().await;
        let Some(entry) = guard.tools.as_ref() else {
            return 0;
        };
        if !Self::cache_entry_is_fresh(entry) {
            return 0;
        }
        entry.value.failures.len()
    }

    pub async fn connected_server_instructions(&self) -> BTreeMap<String, String> {
        let snapshot = {
            let guard = self.inner.clients.lock().await;
            guard
                .iter()
                .filter_map(|(server_name, client)| {
                    client
                        .instructions
                        .as_ref()
                        .map(|instruction| (server_name.clone(), instruction.clone()))
                })
                .collect::<Vec<_>>()
        };
        snapshot.into_iter().collect()
    }

    pub fn connected_server_instructions_snapshot(&self) -> BTreeMap<String, String> {
        let Ok(guard) = self.inner.clients.try_lock() else {
            return BTreeMap::new();
        };
        guard
            .iter()
            .filter_map(|(server_name, client)| {
                client
                    .instructions
                    .as_ref()
                    .map(|instruction| (server_name.clone(), instruction.clone()))
            })
            .collect()
    }

    pub async fn discover_tools_from_servers(
        &self,
        servers: &[ServerConfig],
        auth_headers: &UpstreamAuthHeaders,
    ) -> Result<DiscoveryResult> {
        let cache_key = servers_cache_key(servers, auth_headers);
        if let Some(cached) = self.cached_tools(cache_key).await {
            return Ok(cached);
        }

        let mut result = DiscoveryResult::default();
        let limiter = Arc::new(Semaphore::new(MAX_PARALLEL_DISCOVERY));
        let mut tasks = JoinSet::new();

        for server in servers {
            let server = server.clone();
            let manager = self.clone();
            let limiter = Arc::clone(&limiter);
            let auth_header = auth_headers
                .bearer_token_for(&server.name)
                .map(std::string::ToString::to_string);
            tasks.spawn(async move {
                let server_name = server.name.clone();
                let _permit = match limiter.acquire_owned().await {
                    Ok(permit) => permit,
                    Err(err) => {
                        return (
                            server_name,
                            Err(anyhow!("discovery limiter closed unexpectedly: {err}")),
                        );
                    }
                };
                let outcome = manager
                    .discover_tools_for_server(&server, auth_header.as_deref())
                    .await;
                (server_name, outcome)
            });
        }

        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((_server_name, Ok(mut tools))) => result.tools.append(&mut tools),
                Ok((server_name, Err(err))) => {
                    warn!(server = %server_name, error = %err, "upstream discovery failed");
                    result.failures.push(DiscoveryFailure {
                        server_name,
                        message: format!("{err:#}"),
                    });
                }
                Err(err) => {
                    result.failures.push(DiscoveryFailure {
                        server_name: "<discovery-task>".to_string(),
                        message: format!("discovery task join failed: {err}"),
                    });
                }
            }
        }

        result
            .tools
            .sort_by(|a, b| a.namespaced_name.cmp(&b.namespaced_name));

        self.cache_tools(cache_key, result.clone()).await;
        Ok(result)
    }

    pub async fn discover_prompts_from_servers(
        &self,
        servers: &[ServerConfig],
        auth_headers: &UpstreamAuthHeaders,
    ) -> Result<PromptDiscoveryResult> {
        let cache_key = servers_cache_key(servers, auth_headers);
        if let Some(cached) = self.cached_prompts(cache_key).await {
            return Ok(cached);
        }

        let mut result = PromptDiscoveryResult::default();
        let limiter = Arc::new(Semaphore::new(MAX_PARALLEL_DISCOVERY));
        let mut tasks = JoinSet::new();

        for server in servers {
            let server = server.clone();
            let manager = self.clone();
            let limiter = Arc::clone(&limiter);
            let auth_header = auth_headers
                .bearer_token_for(&server.name)
                .map(std::string::ToString::to_string);
            tasks.spawn(async move {
                let server_name = server.name.clone();
                let _permit = match limiter.acquire_owned().await {
                    Ok(permit) => permit,
                    Err(err) => {
                        return (
                            server_name,
                            Err(anyhow!(
                                "prompt discovery limiter closed unexpectedly: {err}"
                            )),
                        );
                    }
                };
                let outcome = manager
                    .discover_prompts_for_server(&server, auth_header.as_deref())
                    .await;
                (server_name, outcome)
            });
        }

        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((_server_name, Ok(mut prompts))) => result.prompts.append(&mut prompts),
                Ok((server_name, Err(err))) => {
                    warn!(server = %server_name, error = %err, "upstream prompt discovery failed");
                    result.failures.push(DiscoveryFailure {
                        server_name,
                        message: format!("{err:#}"),
                    });
                }
                Err(err) => {
                    result.failures.push(DiscoveryFailure {
                        server_name: "<prompt-discovery-task>".to_string(),
                        message: format!("prompt discovery task join failed: {err}"),
                    });
                }
            }
        }

        result
            .prompts
            .sort_by(|a, b| a.namespaced_name.cmp(&b.namespaced_name));

        self.cache_prompts(cache_key, result.clone()).await;
        Ok(result)
    }

    pub async fn discover_resources_from_servers(
        &self,
        servers: &[ServerConfig],
        auth_headers: &UpstreamAuthHeaders,
    ) -> Result<ResourceDiscoveryResult> {
        let cache_key = servers_cache_key(servers, auth_headers);
        if let Some(cached) = self.cached_resources(cache_key).await {
            return Ok(cached);
        }

        let mut result = ResourceDiscoveryResult::default();
        let limiter = Arc::new(Semaphore::new(MAX_PARALLEL_DISCOVERY));
        let mut tasks = JoinSet::new();

        for server in servers {
            let server = server.clone();
            let manager = self.clone();
            let limiter = Arc::clone(&limiter);
            let auth_header = auth_headers
                .bearer_token_for(&server.name)
                .map(std::string::ToString::to_string);
            tasks.spawn(async move {
                let server_name = server.name.clone();
                let _permit = match limiter.acquire_owned().await {
                    Ok(permit) => permit,
                    Err(err) => {
                        return (
                            server_name,
                            Err(anyhow!(
                                "resource discovery limiter closed unexpectedly: {err}"
                            )),
                        );
                    }
                };
                let outcome = manager
                    .discover_resources_for_server(&server, auth_header.as_deref())
                    .await;
                (server_name, outcome)
            });
        }

        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((_, Ok((mut resources, mut templates)))) => {
                    result.resources.append(&mut resources);
                    result.resource_templates.append(&mut templates);
                }
                Ok((server_name, Err(err))) => {
                    warn!(server = %server_name, error = %err, "upstream resource discovery failed");
                    result.failures.push(DiscoveryFailure {
                        server_name,
                        message: format!("{err:#}"),
                    });
                }
                Err(err) => {
                    result.failures.push(DiscoveryFailure {
                        server_name: "<resource-discovery-task>".to_string(),
                        message: format!("resource discovery task join failed: {err}"),
                    });
                }
            }
        }

        result
            .resources
            .sort_by(|a, b| a.namespaced_uri.cmp(&b.namespaced_uri));
        result
            .resource_templates
            .sort_by(|a, b| a.namespaced_uri_template.cmp(&b.namespaced_uri_template));

        self.cache_resources(cache_key, result.clone()).await;
        Ok(result)
    }

    pub async fn call_tool_on_server(
        &self,
        server: &ServerConfig,
        auth_headers: &UpstreamAuthHeaders,
        mut request: CallToolRequestParams,
        cancel: CancellationToken,
        progress_forwarder: Option<ProgressForwarder>,
    ) -> std::result::Result<CallToolResult, UpstreamRequestError> {
        let client = self
            .managed_client(server, auth_headers.bearer_token_for(&server.name))
            .await
            .map_err(UpstreamRequestError::Transport)?;

        let registration = self
            .register_progress_forwarder(&client, progress_forwarder)
            .await;

        let meta = request.meta.take();
        let handle = client
            .peer
            .send_cancellable_request(
                ClientRequest::CallToolRequest(Request::new(request)),
                PeerRequestOptions {
                    timeout: Some(self.request_timeout),
                    meta,
                },
            )
            .await
            .map_err(map_service_error);

        let result = match handle {
            Ok(handle) => self.await_server_result(server, cancel, handle).await,
            Err(err) => Err(err),
        }
        .and_then(|response| match response {
            ServerResult::CallToolResult(result) => Ok(result),
            other => Err(UpstreamRequestError::Transport(anyhow!(
                "unexpected upstream response for tools/call on '{}': {other:?}",
                server.name
            ))),
        });

        self.unregister_progress_forwarder(&client, registration)
            .await;
        self.evict_if_transport_error(&server.name, &result).await;
        result
    }

    pub async fn get_prompt_on_server(
        &self,
        server: &ServerConfig,
        auth_headers: &UpstreamAuthHeaders,
        mut request: GetPromptRequestParams,
        cancel: CancellationToken,
        progress_forwarder: Option<ProgressForwarder>,
    ) -> std::result::Result<GetPromptResult, UpstreamRequestError> {
        let client = self
            .managed_client(server, auth_headers.bearer_token_for(&server.name))
            .await
            .map_err(UpstreamRequestError::Transport)?;

        let registration = self
            .register_progress_forwarder(&client, progress_forwarder)
            .await;

        let meta = request.meta.take();
        let handle = client
            .peer
            .send_cancellable_request(
                ClientRequest::GetPromptRequest(Request::new(request)),
                PeerRequestOptions {
                    timeout: Some(self.request_timeout),
                    meta,
                },
            )
            .await
            .map_err(map_service_error);

        let result = match handle {
            Ok(handle) => self.await_server_result(server, cancel, handle).await,
            Err(err) => Err(err),
        }
        .and_then(|response| match response {
            ServerResult::GetPromptResult(result) => Ok(result),
            other => Err(UpstreamRequestError::Transport(anyhow!(
                "unexpected upstream response for prompts/get on '{}': {other:?}",
                server.name
            ))),
        });

        self.unregister_progress_forwarder(&client, registration)
            .await;
        self.evict_if_transport_error(&server.name, &result).await;
        result
    }

    pub async fn read_resource_on_server(
        &self,
        server: &ServerConfig,
        auth_headers: &UpstreamAuthHeaders,
        mut request: ReadResourceRequestParams,
        cancel: CancellationToken,
        progress_forwarder: Option<ProgressForwarder>,
    ) -> std::result::Result<ReadResourceResult, UpstreamRequestError> {
        let client = self
            .managed_client(server, auth_headers.bearer_token_for(&server.name))
            .await
            .map_err(UpstreamRequestError::Transport)?;

        let registration = self
            .register_progress_forwarder(&client, progress_forwarder)
            .await;

        let meta = request.meta.take();
        let handle = client
            .peer
            .send_cancellable_request(
                ClientRequest::ReadResourceRequest(Request::new(request)),
                PeerRequestOptions {
                    timeout: Some(self.request_timeout),
                    meta,
                },
            )
            .await
            .map_err(map_service_error);

        let result = match handle {
            Ok(handle) => self.await_server_result(server, cancel, handle).await,
            Err(err) => Err(err),
        }
        .and_then(|response| match response {
            ServerResult::ReadResourceResult(result) => Ok(result),
            other => Err(UpstreamRequestError::Transport(anyhow!(
                "unexpected upstream response for resources/read on '{}': {other:?}",
                server.name
            ))),
        });

        self.unregister_progress_forwarder(&client, registration)
            .await;
        self.evict_if_transport_error(&server.name, &result).await;
        result
    }

    async fn await_server_result(
        &self,
        server: &ServerConfig,
        cancel: CancellationToken,
        handle: rmcp::service::RequestHandle<RoleClient>,
    ) -> std::result::Result<ServerResult, UpstreamRequestError> {
        let request_id = handle.id.clone();
        let peer = handle.peer.clone();
        let response = handle.await_response();
        tokio::pin!(response);

        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = peer.notify_cancelled(CancelledNotificationParam {
                    request_id,
                    reason: Some("request cancelled".to_string()),
                }).await;
                Err(UpstreamRequestError::Cancelled)
            },
            response = &mut response => {
                response.map_err(map_service_error).map_err(|err| {
                    if matches!(err, UpstreamRequestError::Transport(_)) {
                        let server_name = server.name.clone();
                        let manager = self.clone();
                        tokio::spawn(async move {
                            manager.evict_client(&server_name).await;
                        });
                    }
                    err
                })
            },
        }
    }

    async fn evict_if_transport_error<T>(
        &self,
        server_name: &str,
        result: &std::result::Result<T, UpstreamRequestError>,
    ) {
        if matches!(result, Err(UpstreamRequestError::Transport(_))) {
            self.evict_client(server_name).await;
        }
    }

    pub async fn evict_server(&self, server_name: &str) {
        self.evict_client(server_name).await;
    }

    async fn register_progress_forwarder(
        &self,
        client: &ManagedUpstreamClient,
        progress_forwarder: Option<ProgressForwarder>,
    ) -> Option<(ProgressToken, u64)> {
        match progress_forwarder {
            Some(forwarder) => Some(client.progress_registry.register(forwarder).await),
            None => None,
        }
    }

    async fn unregister_progress_forwarder(
        &self,
        client: &ManagedUpstreamClient,
        registration: Option<(ProgressToken, u64)>,
    ) {
        if let Some((token, registration_id)) = registration {
            client
                .progress_registry
                .unregister(&token, registration_id)
                .await;
        }
    }

    async fn managed_client(
        &self,
        server: &ServerConfig,
        auth_header: Option<&str>,
    ) -> Result<Arc<ManagedUpstreamClient>> {
        let creation_lock = self.client_creation_lock(&server.name).await;
        let _creation_guard = creation_lock.lock().await;

        if let Some(existing) = {
            let guard = self.inner.clients.lock().await;
            guard.get(&server.name).cloned()
        } {
            if existing.is_same_config(server, auth_header) {
                return Ok(existing);
            }
            self.evict_client(&server.name).await;
        }

        let spawned = Arc::new(spawn_managed_client(server, auth_header).await?);
        let mut guard = self.inner.clients.lock().await;
        match guard.get(&server.name) {
            Some(existing) if existing.is_same_config(server, auth_header) => Ok(existing.clone()),
            _ => {
                guard.insert(server.name.clone(), spawned.clone());
                Ok(spawned)
            }
        }
    }

    async fn client_creation_lock(&self, server_name: &str) -> Arc<Mutex<()>> {
        let mut guard = self.inner.client_creation_locks.lock().await;
        guard
            .entry(server_name.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn evict_client(&self, server_name: &str) {
        let client = {
            let mut guard = self.inner.clients.lock().await;
            guard.remove(server_name)
        };
        if let Some(client) = client {
            debug!(server = %client.server_name, "evicting managed upstream client");
            client.shutdown().await;
        }
        self.invalidate_discovery_cache().await;
    }

    async fn discover_tools_for_server(
        &self,
        server: &ServerConfig,
        auth_header: Option<&str>,
    ) -> Result<Vec<DiscoveredTool>> {
        let discovered = match self
            .discover_tools_for_server_once(server, auth_header)
            .await
        {
            Ok(discovered) => discovered,
            Err(first_err) => {
                warn!(
                    server = %server.name,
                    error = %first_err,
                    "tool discovery failed on first attempt; evicting client and retrying once"
                );
                self.evict_client(&server.name).await;
                match self
                    .discover_tools_for_server_once(server, auth_header)
                    .await
                {
                    Ok(discovered) => {
                        debug!(
                            server = %server.name,
                            "tool discovery recovered after client reconnect retry"
                        );
                        discovered
                    }
                    Err(retry_err) => {
                        self.evict_client(&server.name).await;
                        return Err(retry_err);
                    }
                }
            }
        };

        let mut tools = Vec::new();
        for tool in discovered {
            let upstream_name = tool.name.clone().into_owned();
            let namespaced_name = namespaced(&server.name, &upstream_name);
            let mut namespaced_tool = tool;
            namespaced_tool.name = namespaced_name.clone().into();
            tools.push(DiscoveredTool {
                tool: namespaced_tool,
                namespaced_name,
                server_name: server.name.clone(),
                upstream_name,
            });
        }

        Ok(tools)
    }

    async fn discover_prompts_for_server(
        &self,
        server: &ServerConfig,
        auth_header: Option<&str>,
    ) -> Result<Vec<DiscoveredPrompt>> {
        let discovered = match self
            .discover_prompts_for_server_once(server, auth_header)
            .await
        {
            Ok(discovered) => discovered,
            Err(first_err) => {
                warn!(
                    server = %server.name,
                    error = %first_err,
                    "prompt discovery failed on first attempt; evicting client and retrying once"
                );
                self.evict_client(&server.name).await;
                match self
                    .discover_prompts_for_server_once(server, auth_header)
                    .await
                {
                    Ok(discovered) => {
                        debug!(
                            server = %server.name,
                            "prompt discovery recovered after client reconnect retry"
                        );
                        discovered
                    }
                    Err(retry_err) => {
                        self.evict_client(&server.name).await;
                        return Err(retry_err);
                    }
                }
            }
        };

        let mut prompts = Vec::new();
        for prompt in discovered {
            let namespaced_name = namespaced(&server.name, &prompt.name);
            let mut namespaced_prompt = prompt;
            namespaced_prompt.name = namespaced_name.clone();
            prompts.push(DiscoveredPrompt {
                prompt: namespaced_prompt,
                namespaced_name,
            });
        }

        Ok(prompts)
    }

    async fn discover_resources_for_server(
        &self,
        server: &ServerConfig,
        auth_header: Option<&str>,
    ) -> Result<(Vec<DiscoveredResource>, Vec<DiscoveredResourceTemplate>)> {
        let (discovered_resources, discovered_templates) = match self
            .discover_resources_for_server_once(server, auth_header)
            .await
        {
            Ok(discovered) => discovered,
            Err(first_err) => {
                warn!(
                    server = %server.name,
                    error = %first_err,
                    "resource discovery failed on first attempt; evicting client and retrying once"
                );
                self.evict_client(&server.name).await;
                match self
                    .discover_resources_for_server_once(server, auth_header)
                    .await
                {
                    Ok(discovered) => {
                        debug!(
                            server = %server.name,
                            "resource discovery recovered after client reconnect retry"
                        );
                        discovered
                    }
                    Err(retry_err) => {
                        self.evict_client(&server.name).await;
                        return Err(retry_err);
                    }
                }
            }
        };

        let mut resources = Vec::new();
        for resource in discovered_resources {
            let upstream_uri = resource.raw.uri.clone();
            let namespaced_uri = namespaced_resource_uri(&server.name, &upstream_uri);
            let mut namespaced_resource = resource;
            namespaced_resource.raw.uri = namespaced_uri.clone();
            let meta = namespaced_resource
                .raw
                .meta
                .get_or_insert_with(rmcp::model::Meta::new);
            meta.0.insert(
                "x-gambi-upstream-uri".to_string(),
                Value::String(upstream_uri.clone()),
            );
            resources.push(DiscoveredResource {
                resource: namespaced_resource,
                namespaced_uri,
            });
        }

        let mut templates = Vec::new();
        for template in discovered_templates {
            let upstream_uri_template = template.raw.uri_template.clone();
            let namespaced_uri_template =
                namespaced_resource_template_uri(&server.name, &upstream_uri_template);
            let mut namespaced_template = template;
            namespaced_template.raw.uri_template = namespaced_uri_template.clone();
            templates.push(DiscoveredResourceTemplate {
                resource_template: namespaced_template,
                namespaced_uri_template,
            });
        }

        Ok((resources, templates))
    }

    async fn discover_tools_for_server_once(
        &self,
        server: &ServerConfig,
        auth_header: Option<&str>,
    ) -> Result<Vec<Tool>> {
        let client = self.managed_client(server, auth_header).await?;
        timeout(self.discovery_timeout, client.peer.list_all_tools())
            .await
            .with_context(|| format!("tool discovery timed out for '{}'", server.name))
            .and_then(|response| {
                response
                    .with_context(|| format!("tool discovery request failed for '{}'", server.name))
            })
    }

    async fn discover_prompts_for_server_once(
        &self,
        server: &ServerConfig,
        auth_header: Option<&str>,
    ) -> Result<Vec<Prompt>> {
        let client = self.managed_client(server, auth_header).await?;
        timeout(self.discovery_timeout, client.peer.list_all_prompts())
            .await
            .with_context(|| format!("prompt discovery timed out for '{}'", server.name))
            .and_then(|response| {
                response.with_context(|| {
                    format!("prompt discovery request failed for '{}'", server.name)
                })
            })
    }

    async fn discover_resources_for_server_once(
        &self,
        server: &ServerConfig,
        auth_header: Option<&str>,
    ) -> Result<(Vec<Resource>, Vec<ResourceTemplate>)> {
        let client = self.managed_client(server, auth_header).await?;

        let discovered_resources =
            timeout(self.discovery_timeout, client.peer.list_all_resources())
                .await
                .with_context(|| format!("resource discovery timed out for '{}'", server.name))
                .and_then(|response| {
                    response.with_context(|| {
                        format!("resource discovery request failed for '{}'", server.name)
                    })
                })?;

        let discovered_templates = timeout(
            self.discovery_timeout,
            client.peer.list_all_resource_templates(),
        )
        .await
        .with_context(|| {
            format!(
                "resource template discovery timed out for '{}'",
                server.name
            )
        })
        .and_then(|response| {
            response.with_context(|| {
                format!(
                    "resource template discovery request failed for '{}'",
                    server.name
                )
            })
        })?;

        Ok((discovered_resources, discovered_templates))
    }

    async fn cached_tools(&self, key: u64) -> Option<DiscoveryResult> {
        let guard = self.inner.discovery_caches.lock().await;
        let entry = guard.tools.as_ref()?;
        (entry.key == key && Self::cache_entry_is_fresh(entry)).then(|| entry.value.clone())
    }

    async fn cache_tools(&self, key: u64, value: DiscoveryResult) {
        let mut guard = self.inner.discovery_caches.lock().await;
        guard.tools = Some(DiscoveryCacheEntry {
            key,
            cached_at: Instant::now(),
            value,
        });
    }

    async fn cached_prompts(&self, key: u64) -> Option<PromptDiscoveryResult> {
        let guard = self.inner.discovery_caches.lock().await;
        let entry = guard.prompts.as_ref()?;
        (entry.key == key && Self::cache_entry_is_fresh(entry)).then(|| entry.value.clone())
    }

    async fn cache_prompts(&self, key: u64, value: PromptDiscoveryResult) {
        let mut guard = self.inner.discovery_caches.lock().await;
        guard.prompts = Some(DiscoveryCacheEntry {
            key,
            cached_at: Instant::now(),
            value,
        });
    }

    async fn cached_resources(&self, key: u64) -> Option<ResourceDiscoveryResult> {
        let guard = self.inner.discovery_caches.lock().await;
        let entry = guard.resources.as_ref()?;
        (entry.key == key && Self::cache_entry_is_fresh(entry)).then(|| entry.value.clone())
    }

    async fn cache_resources(&self, key: u64, value: ResourceDiscoveryResult) {
        let mut guard = self.inner.discovery_caches.lock().await;
        guard.resources = Some(DiscoveryCacheEntry {
            key,
            cached_at: Instant::now(),
            value,
        });
    }

    fn cache_entry_is_fresh<T>(entry: &DiscoveryCacheEntry<T>) -> bool {
        entry.cached_at.elapsed() <= DISCOVERY_CACHE_TTL
    }
}

async fn spawn_managed_client(
    server: &ServerConfig,
    auth_header: Option<&str>,
) -> Result<ManagedUpstreamClient> {
    let target = parse_upstream_target(server)?;
    match target {
        UpstreamTransportTarget::Stdio(target) => spawn_managed_stdio_client(server, &target).await,
        UpstreamTransportTarget::Http(target) => {
            match spawn_managed_http_client(server, &target, auth_header).await {
                Ok(client) => Ok(client),
                Err(http_err)
                    if server.transport == TransportMode::Auto
                        && looks_like_http_transport_decode_error(&http_err) =>
                {
                    warn!(
                        server = %server.name,
                        uri = %target.uri,
                        error = %http_err,
                        "streamable-http transport initialization failed in auto mode with decode/protocol error; retrying with legacy-sse"
                    );
                    let sse_target = SseServerTarget {
                        uri: target.uri.clone(),
                    };
                    match spawn_managed_sse_client(server, &sse_target, auth_header).await {
                        Ok(client) => {
                            warn!(
                                server = %server.name,
                                uri = %target.uri,
                                "auto transport fallback to legacy-sse succeeded"
                            );
                            Ok(client)
                        }
                        Err(sse_err) => Err(anyhow!(
                            "auto transport failed for '{}': streamable-http error: {http_err:#}; legacy-sse fallback error: {sse_err:#}",
                            server.name
                        )),
                    }
                }
                Err(http_err) => Err(http_err),
            }
        }
        UpstreamTransportTarget::Sse(target) => {
            match spawn_managed_sse_client(server, &target, auth_header).await {
                Ok(client) => Ok(client),
                Err(sse_err) if server.transport == TransportMode::Auto => {
                    warn!(
                        server = %server.name,
                        uri = %target.uri,
                        error = %sse_err,
                        "legacy-sse transport initialization failed in auto mode; retrying with streamable-http"
                    );
                    let http_target = HttpServerTarget {
                        uri: target.uri.clone(),
                    };
                    match spawn_managed_http_client(server, &http_target, auth_header).await {
                        Ok(client) => {
                            warn!(
                                server = %server.name,
                                uri = %target.uri,
                                "auto transport fallback to streamable-http succeeded"
                            );
                            Ok(client)
                        }
                        Err(http_err) => Err(anyhow!(
                            "auto transport failed for '{}': legacy-sse error: {sse_err:#}; streamable-http fallback error: {http_err:#}",
                            server.name
                        )),
                    }
                }
                Err(sse_err) => Err(sse_err),
            }
        }
    }
}

fn looks_like_http_transport_decode_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("error decoding response body")
        || msg.contains("unexpected content type")
        || msg.contains("data did not match any variant of untagged enum jsonrpcmessage")
}

async fn spawn_managed_stdio_client(
    server: &ServerConfig,
    target: &StdioServerTarget,
) -> Result<ManagedUpstreamClient> {
    debug!(
        server = %server.name,
        command = %target.command,
        args = ?target.args,
        cwd = ?target.cwd,
        "spawning managed stdio upstream client"
    );

    let command = build_upstream_command(target);
    let transport = TokioChildProcess::new(command)
        .with_context(|| format!("failed to spawn stdio server command '{}'", target.command))?;

    let handler = UpstreamClient::default();
    let service = handler
        .clone()
        .serve(transport)
        .await
        .with_context(|| format!("failed to initialize MCP client for '{}'", server.name))?;

    let peer = service.peer().clone();
    let instructions = peer
        .peer_info()
        .and_then(|info| info.instructions.as_deref())
        .map(str::trim)
        .filter(|instruction| !instruction.is_empty())
        .map(std::string::ToString::to_string);

    Ok(ManagedUpstreamClient {
        server_name: server.name.clone(),
        server_url: server.url.clone(),
        auth_header: None,
        instructions,
        peer,
        progress_registry: handler.progress_registry(),
        running: Mutex::new(Some(service)),
    })
}

async fn spawn_managed_http_client(
    server: &ServerConfig,
    target: &HttpServerTarget,
    auth_header: Option<&str>,
) -> Result<ManagedUpstreamClient> {
    debug!(
        server = %server.name,
        uri = %target.uri,
        has_auth = auth_header.is_some(),
        "spawning managed streamable-http upstream client"
    );

    let retry_config: Arc<dyn SseRetryPolicy> = Arc::new(BoundedExponentialBackoff::default());
    let mut transport_config = StreamableHttpClientTransportConfig::with_uri(target.uri.clone());
    transport_config.retry_config = retry_config;
    transport_config.auth_header = auth_header.map(std::string::ToString::to_string);

    let http_client = reqwest::Client::builder()
        .timeout(HTTP_CLIENT_TIMEOUT)
        .build()
        .context("failed to construct reqwest client for streamable-http transport")?;

    let transport =
        StreamableHttpClientTransport::with_client(http_client.clone(), transport_config.clone());

    let handler = UpstreamClient::default();
    let service = match handler.clone().serve(transport).await {
        Ok(service) => service,
        Err(err) => {
            if let Some(auth_failure) =
                probe_http_auth_failure(server, target, &http_client, auth_header).await
            {
                return Err(anyhow!(auth_failure)).with_context(|| {
                    format!("failed to initialize MCP HTTP client for '{}'", server.name)
                });
            }
            return Err(err).with_context(|| {
                format!("failed to initialize MCP HTTP client for '{}'", server.name)
            });
        }
    };

    let peer = service.peer().clone();
    let instructions = peer
        .peer_info()
        .and_then(|info| info.instructions.as_deref())
        .map(str::trim)
        .filter(|instruction| !instruction.is_empty())
        .map(std::string::ToString::to_string);

    Ok(ManagedUpstreamClient {
        server_name: server.name.clone(),
        server_url: server.url.clone(),
        auth_header: transport_config.auth_header,
        instructions,
        peer,
        progress_registry: handler.progress_registry(),
        running: Mutex::new(Some(service)),
    })
}

async fn spawn_managed_sse_client(
    server: &ServerConfig,
    target: &SseServerTarget,
    auth_header: Option<&str>,
) -> Result<ManagedUpstreamClient> {
    debug!(
        server = %server.name,
        uri = %target.uri,
        has_auth = auth_header.is_some(),
        "spawning managed legacy-sse upstream client"
    );

    // No client-level timeout: the SSE GET connection is long-lived.
    // POST requests use per-request timeouts in the worker.
    let http_client = reqwest::Client::builder()
        .build()
        .context("failed to construct reqwest client for legacy-sse transport")?;

    let worker = LegacySseWorker::new(
        http_client.clone(),
        target.uri.clone(),
        auth_header.map(std::string::ToString::to_string),
    );

    let handler = UpstreamClient::default();
    let service = match handler.clone().serve(worker).await {
        Ok(service) => service,
        Err(err) => {
            let probe_target = HttpServerTarget {
                uri: target.uri.clone(),
            };
            // Legacy SSE often surfaces auth failures as opaque channel-close errors.
            // Probe the endpoint directly to classify 401/403 + WWW-Authenticate reliably.
            if let Some(auth_failure) =
                probe_http_auth_failure(server, &probe_target, &http_client, auth_header).await
            {
                return Err(anyhow!(auth_failure)).with_context(|| {
                    format!("failed to initialize MCP SSE client for '{}'", server.name)
                });
            }
            return Err(err).with_context(|| {
                format!("failed to initialize MCP SSE client for '{}'", server.name)
            });
        }
    };

    let peer = service.peer().clone();
    let instructions = peer
        .peer_info()
        .and_then(|info| info.instructions.as_deref())
        .map(str::trim)
        .filter(|instruction| !instruction.is_empty())
        .map(std::string::ToString::to_string);

    Ok(ManagedUpstreamClient {
        server_name: server.name.clone(),
        server_url: server.url.clone(),
        auth_header: auth_header.map(std::string::ToString::to_string),
        instructions,
        peer,
        progress_registry: handler.progress_registry(),
        running: Mutex::new(Some(service)),
    })
}

fn parse_upstream_target(server: &ServerConfig) -> Result<UpstreamTransportTarget> {
    if server.url.starts_with("stdio://") {
        let target = parse_stdio_server(server)
            .with_context(|| format!("invalid stdio config for server '{}'", server.name))?;
        return Ok(UpstreamTransportTarget::Stdio(target));
    }

    let parsed = Url::parse(&server.url).with_context(|| {
        format!(
            "invalid upstream url '{}' for '{}'",
            server.url, server.name
        )
    })?;

    match parsed.scheme() {
        "http" | "https" => {
            if parsed.host_str().is_none() {
                bail!(
                    "invalid upstream url '{}' for '{}': missing host",
                    server.url,
                    server.name
                );
            }

            let use_sse = match server.transport {
                TransportMode::Sse => true,
                TransportMode::StreamableHttp => false,
                TransportMode::Auto => parsed.path().ends_with("/sse"),
            };

            if use_sse {
                Ok(UpstreamTransportTarget::Sse(SseServerTarget {
                    uri: server.url.clone(),
                }))
            } else {
                Ok(UpstreamTransportTarget::Http(HttpServerTarget {
                    uri: server.url.clone(),
                }))
            }
        }
        other => bail!(
            "unsupported transport scheme '{}' for server '{}'; expected stdio/http/https",
            other,
            server.name
        ),
    }
}

fn servers_cache_key(servers: &[ServerConfig], auth_headers: &UpstreamAuthHeaders) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for server in servers {
        server.name.hash(&mut hasher);
        server.url.hash(&mut hasher);
        auth_headers
            .bearer_token_for(&server.name)
            .map(str::len)
            .hash(&mut hasher);
        auth_headers
            .bearer_token_for(&server.name)
            .hash(&mut hasher);
    }
    hasher.finish()
}

pub fn parse_stdio_target(url: &str) -> Result<StdioServerTarget> {
    let parsed = Url::parse(url).with_context(|| format!("invalid stdio url '{url}'"))?;
    if parsed.scheme() != "stdio" {
        bail!("url scheme '{}' is not stdio", parsed.scheme());
    }

    let command = if let Some(host) = parsed.host_str() {
        if host.is_empty() {
            bail!("stdio url must include a command host or path");
        }
        let path_suffix = parsed.path().trim_start_matches('/');
        if path_suffix.is_empty() {
            host.to_string()
        } else {
            format!("{host}/{path_suffix}")
        }
    } else {
        let path_command = parsed.path();
        if path_command.is_empty() || path_command == "/" {
            bail!("stdio url must include a command host or path");
        }
        path_command.to_string()
    };

    let mut args = Vec::new();
    let mut cwd = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "arg" => args.push(value.into_owned()),
            "cwd" if cwd.is_none() => cwd = Some(PathBuf::from(value.into_owned())),
            _ => {}
        }
    }

    Ok(StdioServerTarget { command, args, cwd })
}

pub fn parse_stdio_server(server: &ServerConfig) -> Result<StdioServerTarget> {
    parse_stdio_target(&server.url)
        .with_context(|| format!("invalid stdio config for server '{}'", server.name))
}

fn build_upstream_command(target: &StdioServerTarget) -> Command {
    let mut command = Command::new(&target.command);
    command.args(&target.args);
    command.env("GAMBI_DISABLE_UPSTREAM_DISCOVERY", "1");
    if let Some(cwd) = &target.cwd {
        command.current_dir(cwd);
    }
    command
}

fn duration_env_ms(key: &str, fallback: Duration) -> Duration {
    std::env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(fallback)
}

async fn probe_http_auth_failure(
    server: &ServerConfig,
    target: &HttpServerTarget,
    http_client: &reqwest::Client,
    auth_header: Option<&str>,
) -> Option<UpstreamAuthFailure> {
    let mut request = http_client
        .get(&target.uri)
        .timeout(HTTP_AUTH_PROBE_TIMEOUT)
        .header(
            "accept",
            "application/json, text/event-stream;q=0.9, */*;q=0.1",
        );

    if let Some(token) = auth_header.map(str::trim).filter(|value| !value.is_empty()) {
        let authorization = if token.to_ascii_lowercase().starts_with("bearer ") {
            token.to_string()
        } else {
            format!("Bearer {token}")
        };
        request = request.header("authorization", authorization);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            debug!(
                server = %server.name,
                uri = %target.uri,
                error = %err,
                "http auth probe request failed"
            );
            return None;
        }
    };

    let status = response.status().as_u16();
    let challenge = response
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    let challenge_implies_auth = challenge
        .as_deref()
        .map(|value| {
            let lower = value.to_ascii_lowercase();
            lower.contains("bearer")
                || lower.contains("invalid_token")
                || lower.contains("insufficient_scope")
                || lower.contains("error=")
        })
        .unwrap_or(false);

    let status_implies_auth = status == 401 || status == 403;

    if !status_implies_auth && !challenge_implies_auth {
        return None;
    }

    Some(UpstreamAuthFailure::new(
        server.name.clone(),
        Some(status),
        challenge,
    ))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{
        auth::{OAuthToken, TokenState},
        config::ServerConfig,
    };
    use anyhow::anyhow;
    use rmcp::transport::common::client_side_sse::SseRetryPolicy;

    use super::{
        BoundedExponentialBackoff, DISCOVERY_CACHE_TTL, DiscoveryCacheEntry, DiscoveryFailure,
        DiscoveryResult, UpstreamAuthFailure, UpstreamManager, UpstreamTransportTarget,
        auth_headers_from_token_state, message_has_auth_required_marker, parse_stdio_target,
        parse_upstream_target, transport_error_is_auth_failure,
    };

    #[test]
    fn parses_stdio_host_and_args() {
        let parsed = parse_stdio_target("stdio://uvx?arg=mcp-server-git&arg=--verbose")
            .expect("parse should succeed");
        assert_eq!(parsed.command, "uvx");
        assert_eq!(parsed.args, vec!["mcp-server-git", "--verbose"]);
    }

    #[test]
    fn parses_stdio_path_command() {
        let parsed = parse_stdio_target("stdio:///usr/local/bin/mcp-server?arg=--stdio")
            .expect("parse should succeed");
        assert_eq!(parsed.command, "/usr/local/bin/mcp-server");
        assert_eq!(parsed.args, vec!["--stdio"]);
    }

    #[test]
    fn parses_stdio_host_with_path_command() {
        let parsed = parse_stdio_target("stdio://node_modules/.bin/mcp-server?arg=--stdio")
            .expect("parse should succeed");
        assert_eq!(parsed.command, "node_modules/.bin/mcp-server");
        assert_eq!(parsed.args, vec!["--stdio"]);
    }

    #[test]
    fn rejects_non_stdio_url() {
        let err = parse_stdio_target("https://example.com/mcp").expect_err("should fail");
        assert!(err.to_string().contains("not stdio"));
    }

    #[test]
    fn rejects_empty_stdio_command() {
        let err = parse_stdio_target("stdio:///").expect_err("should fail");
        assert!(
            err.to_string()
                .contains("must include a command host or path")
        );
    }

    #[test]
    fn parses_http_target() {
        let server = ServerConfig {
            name: "remote".to_string(),
            url: "https://example.com/mcp".to_string(),
            oauth: None,
            transport: Default::default(),
            exposure_mode: Default::default(),
            enabled: true,
        };

        let parsed = parse_upstream_target(&server).expect("parse should succeed");
        match parsed {
            UpstreamTransportTarget::Http(target) => {
                assert_eq!(target.uri, "https://example.com/mcp");
            }
            UpstreamTransportTarget::Stdio(_) | UpstreamTransportTarget::Sse(_) => {
                panic!("expected http target")
            }
        }
    }

    #[test]
    fn builds_auth_headers_from_bearer_tokens() {
        let mut tokens = TokenState::default();
        tokens.oauth_tokens.insert(
            "one".to_string(),
            OAuthToken {
                access_token: "token-one".to_string(),
                refresh_token: None,
                token_type: Some("Bearer".to_string()),
                expires_at_epoch_seconds: None,
                scopes: Vec::new(),
            },
        );
        tokens.oauth_tokens.insert(
            "two".to_string(),
            OAuthToken {
                access_token: "token-two".to_string(),
                refresh_token: None,
                token_type: None,
                expires_at_epoch_seconds: None,
                scopes: Vec::new(),
            },
        );
        tokens.oauth_tokens.insert(
            "ignored".to_string(),
            OAuthToken {
                access_token: "token-three".to_string(),
                refresh_token: None,
                token_type: Some("mac".to_string()),
                expires_at_epoch_seconds: None,
                scopes: Vec::new(),
            },
        );

        let headers = auth_headers_from_token_state(&tokens);
        assert_eq!(headers.bearer_token_for("one"), Some("token-one"));
        assert_eq!(headers.bearer_token_for("two"), Some("token-two"));
        assert_eq!(headers.bearer_token_for("ignored"), None);
    }

    #[test]
    fn bounded_http_retry_policy_stops_and_caps_backoff() {
        let retry = BoundedExponentialBackoff {
            max_attempts: 4,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(350),
        };

        assert_eq!(
            retry.retry(1),
            Some(Duration::from_millis(100)),
            "first retry should use base delay"
        );
        assert_eq!(
            retry.retry(2),
            Some(Duration::from_millis(200)),
            "second retry should double"
        );
        assert_eq!(
            retry.retry(3),
            Some(Duration::from_millis(350)),
            "retry delay should cap at max"
        );
        assert_eq!(
            retry.retry(4),
            None,
            "retry should stop when max is reached"
        );
    }

    #[test]
    fn auth_failure_marker_helpers_roundtrip() {
        let err = anyhow!(UpstreamAuthFailure::new(
            "port".to_string(),
            Some(401),
            Some("Bearer realm=\"OAuth\"".to_string())
        ));
        assert!(transport_error_is_auth_failure(&err));
        assert!(message_has_auth_required_marker(&err.to_string()));
    }

    #[tokio::test]
    async fn tool_discovery_cache_entries_expire_when_stale() {
        let manager = UpstreamManager::new();
        {
            let mut caches = manager.inner.discovery_caches.lock().await;
            caches.tools = Some(DiscoveryCacheEntry {
                key: 7,
                cached_at: std::time::Instant::now() - DISCOVERY_CACHE_TTL - Duration::from_secs(1),
                value: DiscoveryResult {
                    tools: Vec::new(),
                    failures: vec![DiscoveryFailure {
                        server_name: "stale".to_string(),
                        message: "stale failure".to_string(),
                    }],
                },
            });
        }

        assert!(
            manager.cached_tools(7).await.is_none(),
            "stale cache entry should not be returned"
        );
        assert_eq!(
            manager.cached_tool_discovery_failure_count().await,
            0,
            "stale cache should not drive status failure counts"
        );
    }

    #[tokio::test]
    async fn discovery_cache_can_be_explicitly_invalidated() {
        let manager = UpstreamManager::new();
        manager
            .cache_tools(
                11,
                DiscoveryResult {
                    tools: Vec::new(),
                    failures: vec![DiscoveryFailure {
                        server_name: "alpha".to_string(),
                        message: "boom".to_string(),
                    }],
                },
            )
            .await;
        assert!(
            manager.cached_tools(11).await.is_some(),
            "expected tools cache entry before invalidation"
        );

        manager.invalidate_discovery_cache().await;

        assert!(
            manager.cached_tools(11).await.is_none(),
            "expected tools cache to clear on invalidation"
        );
    }
}
