use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::Path,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::auth::{AuthManager, AuthServerStatus, AuthStartResponse, TokenState};
use crate::config::{AppConfig, ConfigStore, ServerConfig};
use crate::logging;
use crate::upstream;

#[derive(Clone)]
struct AppState {
    store: ConfigStore,
    auth: AuthManager,
    upstream: upstream::UpstreamManager,
    exec_enabled: bool,
    admin_base_url: String,
}

#[derive(Debug, Clone)]
pub struct AdminBindOptions {
    pub host: IpAddr,
    pub port: u16,
    pub port_file: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    status: &'static str,
    exec_enabled: bool,
    server_count: usize,
    discovery_failure_count: usize,
}

#[derive(Debug, Serialize)]
struct ServersResponse {
    servers: Vec<ServerConfig>,
    tool_description_overrides: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Serialize)]
struct ToolsResponse {
    tools: Vec<String>,
    tool_details: Vec<ToolDetail>,
    failures: Vec<ToolDiscoveryFailure>,
}

#[derive(Debug, Serialize)]
struct ToolDiscoveryFailure {
    server: String,
    error: String,
}

#[derive(Debug, Serialize)]
struct ToolDetail {
    name: String,
    description: String,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug, Serialize)]
struct LogsResponse {
    logs: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct LogsQuery {
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct MutationResponse {
    status: &'static str,
}

#[derive(Debug, Deserialize)]
struct ToolDescriptionOverrideRequest {
    server: String,
    tool: String,
    description: String,
}

#[derive(Debug, Deserialize)]
struct RemoveToolDescriptionOverrideRequest {
    server: String,
    tool: String,
}

#[derive(Debug, Serialize)]
struct AuthStatusesResponse {
    statuses: Vec<AuthServerStatus>,
}

#[derive(Debug, Deserialize)]
struct AuthStartRequest {
    server: String,
}

#[derive(Debug, Deserialize)]
struct AuthRefreshRequest {
    server: String,
}

#[derive(Debug, Deserialize)]
struct AuthCallbackQuery {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

pub async fn run(
    bind: AdminBindOptions,
    store: ConfigStore,
    auth: AuthManager,
    upstream: upstream::UpstreamManager,
    exec_enabled: bool,
    shutdown: CancellationToken,
) -> Result<()> {
    let AdminBindOptions {
        host,
        port,
        port_file: admin_port_file,
    } = bind;
    let admin_addr = SocketAddr::from((host, port));
    let listener = tokio::net::TcpListener::bind(admin_addr)
        .await
        .with_context(|| format!("failed to bind admin UI on {admin_addr}"))?;
    let local_addr = listener
        .local_addr()
        .context("failed to resolve local address for admin listener")?;
    if let Some(path) = admin_port_file {
        tokio::fs::write(&path, format!("{}\n", local_addr.port()))
            .await
            .with_context(|| {
                format!("failed to write admin listener port to {}", path.display())
            })?;
    }

    let state = AppState {
        store,
        auth,
        upstream,
        exec_enabled,
        admin_base_url: format!("http://{local_addr}"),
    };

    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/servers", get(servers).post(add_server))
        .route("/servers/{name}", delete(remove_server))
        .route("/tools", get(tools))
        .route("/tool-descriptions", post(set_tool_description_override))
        .route(
            "/tool-descriptions/remove",
            post(remove_tool_description_override),
        )
        .route("/logs", get(logs))
        .route("/config/export", get(export_config))
        .route("/config/import", post(import_config))
        .route("/auth/status", get(auth_status))
        .route("/auth/start", post(auth_start))
        .route("/auth/refresh", post(auth_refresh))
        .route("/auth/callback", get(auth_callback))
        .with_state(state);

    info!(addr = %local_addr, "admin UI listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.cancelled().await;
        })
        .await
        .context("admin server exited unexpectedly")
}

async fn root() -> Html<&'static str> {
    Html(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>gambi admin</title>
  <style>
    :root { color-scheme: light; font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
    body { margin: 0; padding: 16px; background: #f3f5f7; color: #1f2933; }
    h1 { margin-top: 0; }
    section { background: #ffffff; border: 1px solid #d8dee4; border-radius: 8px; padding: 12px; margin-bottom: 12px; }
    pre { margin: 0; overflow: auto; max-height: 320px; background: #0f172a; color: #f8fafc; padding: 10px; border-radius: 6px; }
    form { display: grid; gap: 8px; margin-top: 8px; }
    input, select, button { font: inherit; padding: 6px; border-radius: 6px; border: 1px solid #cbd5e1; }
    button { cursor: pointer; background: #0f172a; color: #f8fafc; border: 0; }
    button:disabled { cursor: not-allowed; opacity: 0.65; }
    .row { display: grid; gap: 8px; grid-template-columns: 1fr 1fr; }
    .note { font-size: 12px; color: #475569; }
  </style>
</head>
<body>
  <h1>gambi admin</h1>
  <section>
    <strong>status</strong>
    <pre id="status">loading...</pre>
  </section>
  <section>
    <strong>tools</strong>
    <pre id="tools">loading...</pre>
  </section>
  <section>
    <strong>servers</strong>
    <pre id="servers">loading...</pre>
    <form id="server-add-form">
      <div class="row">
        <input id="server-name" placeholder="server name (e.g. github)" required>
        <input id="server-url" placeholder="url (e.g. stdio://... or https://...)" required>
      </div>
      <button type="submit">Add Server</button>
    </form>
    <form id="server-remove-form">
      <div class="row">
        <select id="server-remove-name"></select>
        <button type="submit">Remove Server</button>
      </div>
    </form>
  </section>
  <section>
    <strong>tool description overrides</strong>
    <pre id="overrides">loading...</pre>
    <form id="override-set-form">
      <div class="row">
        <select id="override-server"></select>
        <input id="override-tool" placeholder="upstream tool name (e.g. search_issues)" required>
      </div>
      <input id="override-description" placeholder="description sent to MCP clients" required>
      <button type="submit">Save Override</button>
    </form>
    <form id="override-remove-form">
      <div class="row">
        <select id="override-remove-server"></select>
        <input id="override-remove-tool" placeholder="upstream tool name to remove" required>
      </div>
      <button type="submit">Remove Override</button>
    </form>
    <div class="note">Overrides apply to tool discovery responses sent to connected agents.</div>
  </section>
  <section>
    <strong>errors</strong>
    <pre id="errors">(none)</pre>
  </section>
  <section>
    <strong>logs</strong>
    <pre id="logs">loading...</pre>
  </section>
  <section>
    <strong>auth</strong>
    <pre id="auth">loading...</pre>
  </section>
  <script>
    let currentServers = [];
    let refreshInFlight = false;
    let TOOL_REFRESH_INTERVAL_MS = 15000;
    let REFRESH_INTERVAL_MS = 3000;
    let panelState = {
      toolsPayload: null,
      toolsRefreshAtMs: 0
    };

    function setError(message) {
      document.getElementById('errors').textContent = message || '(none)';
    }

    async function readErrorMessage(response) {
      const body = await response.text();
      if (!body) return response.statusText;
      try {
        const parsed = JSON.parse(body);
        if (parsed && typeof parsed.error === 'string' && parsed.error.length > 0) {
          return parsed.error;
        }
      } catch (_) {
        // fallback to raw body
      }
      return body;
    }

    function setServerOptions(selectId, servers) {
      const select = document.getElementById(selectId);
      if (!select) return;
      select.replaceChildren();
      if (!servers.length) {
        const option = document.createElement('option');
        option.value = '';
        option.selected = true;
        option.disabled = true;
        option.textContent = '(no servers)';
        select.append(option);
        return;
      }
      for (const server of servers) {
        const option = document.createElement('option');
        option.value = server.name;
        option.textContent = server.name;
        select.append(option);
      }
      select.selectedIndex = 0;
    }

    function hasConfiguredServers() {
      return Array.isArray(currentServers) && currentServers.length > 0;
    }

    function setMutationFormState() {
      const hasServers = hasConfiguredServers();
      document.getElementById('server-remove-name').disabled = !hasServers;
      document.querySelector('#server-remove-form button[type=submit]').disabled = !hasServers;
      document.getElementById('override-server').disabled = !hasServers;
      document.querySelector('#override-set-form button[type=submit]').disabled = !hasServers;
      document.getElementById('override-remove-server').disabled = !hasServers;
      document.querySelector('#override-remove-form button[type=submit]').disabled = !hasServers;
    }

    async function fetchJson(path, label) {
      const response = await fetch(path);
      if (!response.ok) {
        const message = await readErrorMessage(response);
        throw new Error(`${label}: ${response.status} ${message}`);
      }
      return response.json();
    }

    function shouldRefreshTools(forceTools) {
      if (forceTools) {
        return true;
      }
      if (!panelState.toolsPayload) {
        return true;
      }
      return Date.now() - panelState.toolsRefreshAtMs >= TOOL_REFRESH_INTERVAL_MS;
    }

    async function refresh(options = {}) {
      const forceTools = Boolean(options.forceTools);
      if (refreshInFlight) {
        return;
      }
      refreshInFlight = true;
      try {
        const [statusPayload, serversPayload, logsPayload, authPayload] = await Promise.all([
          fetchJson('/status', 'status'),
          fetchJson('/servers', 'servers'),
          fetchJson('/logs?limit=200', 'logs'),
          fetchJson('/auth/status', 'auth/status')
        ]);
        if (shouldRefreshTools(forceTools)) {
          panelState.toolsPayload = await fetchJson('/tools', 'tools');
          panelState.toolsRefreshAtMs = Date.now();
        }
        currentServers = serversPayload.servers || [];
        document.getElementById('status').textContent = JSON.stringify(statusPayload, null, 2);
        document.getElementById('tools').textContent = JSON.stringify(panelState.toolsPayload || {}, null, 2);
        document.getElementById('servers').textContent = JSON.stringify(currentServers, null, 2);
        document.getElementById('overrides').textContent = JSON.stringify(serversPayload.tool_description_overrides || {}, null, 2);
        document.getElementById('logs').textContent = (logsPayload.logs || []).join('\n');
        document.getElementById('auth').textContent = JSON.stringify(authPayload, null, 2);

        setServerOptions('server-remove-name', currentServers);
        setServerOptions('override-server', currentServers);
        setServerOptions('override-remove-server', currentServers);
        setMutationFormState();
      } catch (error) {
        setError(`refresh failed: ${error instanceof Error ? error.message : String(error)}`);
      } finally {
        refreshInFlight = false;
      }
    }

    async function postJson(path, payload) {
      const response = await fetch(path, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(payload)
      });
      if (!response.ok) {
        const message = await readErrorMessage(response);
        throw new Error(`${response.status}: ${message}`);
      }
    }

    async function deletePath(path) {
      const response = await fetch(path, { method: 'DELETE' });
      if (!response.ok) {
        const message = await readErrorMessage(response);
        throw new Error(`${response.status}: ${message}`);
      }
    }

    async function runAction(action, fn) {
      try {
        await fn();
        setError('');
      } catch (error) {
        setError(`${action} failed: ${error instanceof Error ? error.message : String(error)}`);
      }
    }

    document.getElementById('server-add-form').addEventListener('submit', async (event) => {
      event.preventDefault();
      await runAction('add server', async () => {
        await postJson('/servers', {
          name: document.getElementById('server-name').value,
          url: document.getElementById('server-url').value
        });
        event.target.reset();
        await refresh({ forceTools: true });
      });
    });

    document.getElementById('server-remove-form').addEventListener('submit', async (event) => {
      event.preventDefault();
      await runAction('remove server', async () => {
        const name = document.getElementById('server-remove-name').value;
        if (!name) {
          throw new Error('no server selected');
        }
        await deletePath(`/servers/${encodeURIComponent(name)}`);
        await refresh({ forceTools: true });
      });
    });

    document.getElementById('override-set-form').addEventListener('submit', async (event) => {
      event.preventDefault();
      await runAction('save override', async () => {
        const server = document.getElementById('override-server').value;
        if (!server) {
          throw new Error('no server selected');
        }
        await postJson('/tool-descriptions', {
          server,
          tool: document.getElementById('override-tool').value,
          description: document.getElementById('override-description').value
        });
        event.target.reset();
        await refresh({ forceTools: true });
      });
    });

    document.getElementById('override-remove-form').addEventListener('submit', async (event) => {
      event.preventDefault();
      await runAction('remove override', async () => {
        const server = document.getElementById('override-remove-server').value;
        if (!server) {
          throw new Error('no server selected');
        }
        await postJson('/tool-descriptions/remove', {
          server,
          tool: document.getElementById('override-remove-tool').value
        });
        event.target.reset();
        await refresh({ forceTools: true });
      });
    });

    void refresh({ forceTools: true });
    setInterval(() => {
      void refresh();
    }, REFRESH_INTERVAL_MS);
  </script>
</body>
</html>
"#,
    )
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn status(State(state): State<AppState>) -> Result<Json<StatusResponse>, ApiError> {
    let cfg = load_config(&state).await?;
    let discovery_failure_count = state.upstream.cached_tool_discovery_failure_count().await;
    Ok(Json(StatusResponse {
        status: "ok",
        exec_enabled: state.exec_enabled,
        server_count: cfg.servers.len(),
        discovery_failure_count,
    }))
}

async fn servers(State(state): State<AppState>) -> Result<Json<ServersResponse>, ApiError> {
    let cfg = load_config(&state).await?;
    Ok(Json(ServersResponse {
        servers: cfg.servers,
        tool_description_overrides: cfg.tool_description_overrides,
    }))
}

async fn add_server(
    State(state): State<AppState>,
    Json(server): Json<ServerConfig>,
) -> Result<Json<MutationResponse>, ApiError> {
    let server_name = server.name.clone();
    state
        .store
        .update_async(move |cfg| cfg.add_server(server))
        .await
        .map_err(|err| {
            error!(error = %err, server = %server_name, "failed to add server");
            ApiError::bad_request(format!("failed to add server: {err}"))
        })?;
    state.upstream.invalidate_discovery_cache().await;

    Ok(Json(MutationResponse { status: "ok" }))
}

async fn remove_server(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<MutationResponse>, ApiError> {
    let lookup_name = name.clone();
    let removed = state
        .store
        .update_async(move |cfg| Ok(cfg.remove_server(&lookup_name)))
        .await
        .map_err(|err| {
            error!(error = %err, server = %name, "failed to remove server");
            ApiError::internal("failed to remove server")
        })?;

    if !removed {
        return Err(ApiError::bad_request(format!(
            "server '{name}' was not found"
        )));
    }
    state.auth.prune_orphaned_state().await.map_err(|err| {
        error!(
            error = %err,
            server = %name,
            "failed to prune oauth state after server removal"
        );
        ApiError::internal("failed to prune oauth state after server removal")
    })?;
    state.upstream.invalidate_discovery_cache().await;

    Ok(Json(MutationResponse { status: "ok" }))
}

async fn set_tool_description_override(
    State(state): State<AppState>,
    Json(request): Json<ToolDescriptionOverrideRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    let req_server = request.server.clone();
    let req_tool = request.tool.clone();
    let req_description = request.description.clone();
    state
        .store
        .update_async(move |cfg| {
            cfg.set_tool_description_override(&req_server, &req_tool, &req_description)
        })
        .await
        .map_err(|err| {
            error!(
                error = %err,
                server = %request.server,
                tool = %request.tool,
                "failed to set tool description override"
            );
            ApiError::bad_request(format!("failed to set tool description override: {err}"))
        })?;
    state.upstream.invalidate_discovery_cache().await;

    Ok(Json(MutationResponse { status: "ok" }))
}

async fn remove_tool_description_override(
    State(state): State<AppState>,
    Json(request): Json<RemoveToolDescriptionOverrideRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    let req_server = request.server.clone();
    let req_tool = request.tool.clone();
    let removed = state
        .store
        .update_async(move |cfg| Ok(cfg.remove_tool_description_override(&req_server, &req_tool)))
        .await
        .map_err(|err| {
            error!(
                error = %err,
                server = %request.server,
                tool = %request.tool,
                "failed to remove tool description override"
            );
            ApiError::internal("failed to remove tool description override")
        })?;

    if !removed {
        return Err(ApiError::bad_request("override not found"));
    }
    state.upstream.invalidate_discovery_cache().await;

    Ok(Json(MutationResponse { status: "ok" }))
}

async fn tools(State(state): State<AppState>) -> Result<Json<ToolsResponse>, ApiError> {
    let cfg = load_config(&state).await?;
    let auth_headers = load_upstream_auth_headers(&state).await?;
    let discovery = state
        .upstream
        .discover_tools_from_servers(&cfg.servers, &auth_headers)
        .await
        .map_err(|err| {
            error!(error = %err, "failed to discover upstream tools for tools request");
            ApiError::internal("failed to discover upstream tools")
        })?;

    let mut tool_details = BTreeMap::<String, String>::new();
    for tool in &discovery.tools {
        let description = cfg
            .tool_description_override_for(&tool.server_name, &tool.upstream_name)
            .map(ToOwned::to_owned)
            .or_else(|| {
                tool.tool
                    .description
                    .as_ref()
                    .map(|value| value.to_string())
            })
            .unwrap_or_default();
        tool_details.insert(tool.namespaced_name.clone(), description);
    }

    tool_details.insert(
        "gambi_list_servers".to_string(),
        "List configured upstream servers".to_string(),
    );
    tool_details.insert(
        "gambi_list_upstream_tools".to_string(),
        "Discover tools from configured upstream MCP servers".to_string(),
    );
    if state.exec_enabled {
        tool_details.insert(
            "gambi_execute".to_string(),
            "Execute Python workflow in gambi (Monty runtime) with tool-call bridge and resource limits"
                .to_string(),
        );
    }

    let failures = discovery
        .failures
        .iter()
        .map(|failure| ToolDiscoveryFailure {
            server: failure.server_name.clone(),
            error: failure.message.clone(),
        })
        .collect::<Vec<_>>();

    let tools = tool_details.keys().cloned().collect::<Vec<_>>();
    let tool_details = tool_details
        .into_iter()
        .map(|(name, description)| ToolDetail { name, description })
        .collect::<Vec<_>>();

    Ok(Json(ToolsResponse {
        tools,
        tool_details,
        failures,
    }))
}

async fn logs(Query(query): Query<LogsQuery>) -> Result<Json<LogsResponse>, ApiError> {
    let log_buffer = logging::global_log_buffer().ok_or_else(|| {
        ApiError::internal("log buffer not initialized; restart gambi to enable log viewer")
    })?;
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);
    Ok(Json(LogsResponse {
        logs: log_buffer.recent_lines(limit),
    }))
}

async fn export_config(State(state): State<AppState>) -> Result<Json<AppConfig>, ApiError> {
    let cfg = load_config(&state).await?;
    Ok(Json(cfg))
}

async fn import_config(
    State(state): State<AppState>,
    Json(cfg): Json<AppConfig>,
) -> Result<Json<MutationResponse>, ApiError> {
    state.store.replace_async(cfg).await.map_err(|err| {
        error!(error = %err, "failed to import config");
        ApiError::bad_request(format!("failed to import config: {err}"))
    })?;
    state.auth.prune_orphaned_state().await.map_err(|err| {
        error!(error = %err, "failed to prune oauth state after config import");
        ApiError::internal("failed to prune oauth state after config import")
    })?;
    state.upstream.invalidate_discovery_cache().await;

    Ok(Json(MutationResponse { status: "ok" }))
}

async fn auth_status(
    State(state): State<AppState>,
) -> Result<Json<AuthStatusesResponse>, ApiError> {
    let statuses = state.auth.list_statuses().await.map_err(|err| {
        error!(error = %err, "failed to load auth statuses");
        ApiError::internal("failed to load auth statuses")
    })?;
    Ok(Json(AuthStatusesResponse { statuses }))
}

async fn auth_start(
    State(state): State<AppState>,
    Json(request): Json<AuthStartRequest>,
) -> Result<Json<AuthStartResponse>, ApiError> {
    let response = state
        .auth
        .start(&request.server, &state.admin_base_url)
        .await
        .map_err(|err| {
            error!(error = %err, server = %request.server, "failed to start oauth");
            ApiError::bad_request(format!("failed to start oauth: {err}"))
        })?;
    Ok(Json(response))
}

async fn auth_refresh(
    State(state): State<AppState>,
    Json(request): Json<AuthRefreshRequest>,
) -> Result<Json<crate::auth::AuthRefreshResponse>, ApiError> {
    let response = state.auth.refresh(&request.server).await.map_err(|err| {
        error!(error = %err, server = %request.server, "failed to refresh oauth token");
        ApiError::bad_request(format!("failed to refresh oauth token: {err}"))
    })?;
    Ok(Json(response))
}

async fn auth_callback(
    State(state): State<AppState>,
    Query(query): Query<AuthCallbackQuery>,
) -> Result<Html<String>, ApiError> {
    if let Some(error) = query.error {
        let escaped_error = escape_html(&error);
        let message = query
            .error_description
            .unwrap_or_else(|| "oauth provider returned an error".to_string());
        let escaped_message = escape_html(&message);
        return Ok(Html(format!(
            "<html><body><h1>OAuth failed</h1><p>{}: {}</p></body></html>",
            escaped_error, escaped_message
        )));
    }

    let state_param = query
        .state
        .ok_or_else(|| ApiError::bad_request("missing query parameter 'state'"))?;
    let code = query
        .code
        .ok_or_else(|| ApiError::bad_request("missing query parameter 'code'"))?;

    let result = state
        .auth
        .callback(&state_param, &code)
        .await
        .map_err(|err| {
            error!(error = %err, "oauth callback handling failed");
            ApiError::bad_request(format!("oauth callback failed: {err}"))
        })?;

    Ok(Html(format!(
        "<html><body><h1>OAuth complete</h1><p>server: {}</p><p>expires_at_epoch_seconds: {:?}</p></body></html>",
        escape_html(&result.server),
        result.expires_at_epoch_seconds
    )))
}

async fn load_config(state: &AppState) -> Result<crate::config::AppConfig, ApiError> {
    state.store.load_async().await.map_err(|err| {
        error!(error = %err, "failed to load config for admin API request");
        ApiError::internal("failed to load config")
    })
}

async fn load_upstream_auth_headers(
    state: &AppState,
) -> Result<upstream::UpstreamAuthHeaders, ApiError> {
    let tokens: TokenState = state.store.load_tokens_async().await.map_err(|err| {
        error!(error = %err, "failed to load token state for admin API request");
        ApiError::internal("failed to load token state")
    })?;
    Ok(upstream::auth_headers_from_token_state(&tokens))
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(c),
        }
    }
    escaped
}
