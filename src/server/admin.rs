use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::Path,
    extract::Request,
    extract::{Query, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::auth::{AuthManager, AuthServerStatus, AuthStartResponse, TokenState};
use crate::config::{
    AppConfig, ConfigStore, ExposureMode, ServerConfig, ToolPolicyLevel, ToolPolicyMode,
    ToolPolicySource,
};
use crate::logging;
use crate::upstream;

#[derive(Clone)]
struct AppState {
    store: ConfigStore,
    auth: AuthManager,
    upstream: upstream::UpstreamManager,
    exec_enabled: bool,
    admin_base_url: String,
    activity: ActivityTracker,
    shutdown: CancellationToken,
}

#[derive(Clone)]
struct ActivityTracker(Arc<Mutex<Instant>>);

impl ActivityTracker {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    async fn touch(&self) {
        *self.0.lock().await = Instant::now();
    }

    async fn idle_for(&self) -> Duration {
        self.0.lock().await.elapsed()
    }
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
    server_tool_policy_modes: BTreeMap<String, ToolPolicyMode>,
    tool_description_overrides: BTreeMap<String, BTreeMap<String, String>>,
    tool_policy_overrides: BTreeMap<String, BTreeMap<String, ToolPolicyLevel>>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_name: Option<String>,
    policy_level: String,
    policy_source: String,
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

#[derive(Debug, Deserialize)]
struct ToolPolicyOverrideRequest {
    server: String,
    tool: String,
    level: ToolPolicyLevel,
}

#[derive(Debug, Deserialize)]
struct RemoveToolPolicyOverrideRequest {
    server: String,
    tool: String,
}

#[derive(Debug, Deserialize)]
struct UpdateServerExposureRequest {
    exposure_mode: ExposureMode,
}

#[derive(Debug, Deserialize)]
struct UpdateServerToolPolicyRequest {
    policy_mode: ToolPolicyMode,
}

#[derive(Debug, Deserialize)]
struct AddServerRequest {
    name: String,
    url: String,
    #[serde(default)]
    oauth: Option<crate::config::OAuthConfig>,
    #[serde(default)]
    transport: crate::config::TransportMode,
    #[serde(default)]
    exposure_mode: crate::config::ExposureMode,
    #[serde(default)]
    policy_mode: crate::config::ToolPolicyMode,
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
    idle_timeout: Option<Duration>,
) -> Result<()> {
    let AdminBindOptions {
        host,
        port,
        port_file: admin_port_file,
    } = bind;
    info!(host = %host, port, "starting admin UI listener");
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

    let activity = ActivityTracker::new();
    let state = AppState {
        store,
        auth,
        upstream,
        exec_enabled,
        admin_base_url: format!("http://{local_addr}"),
        activity: activity.clone(),
        shutdown: shutdown.clone(),
    };

    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/heartbeat", post(heartbeat))
        .route("/shutdown", post(shutdown_daemon))
        .route("/status", get(status))
        .route("/servers", get(servers).post(add_server))
        .route("/servers/{name}/exposure", post(update_server_exposure))
        .route("/servers/{name}/policy", post(update_server_tool_policy))
        .route("/servers/{name}", delete(remove_server))
        .route("/tools", get(tools))
        .route("/tool-descriptions", post(set_tool_description_override))
        .route(
            "/tool-descriptions/remove",
            post(remove_tool_description_override),
        )
        .route("/tool-policies", post(set_tool_policy_override))
        .route("/tool-policies/remove", post(remove_tool_policy_override))
        .route("/logs", get(logs))
        .route("/config/export", get(export_config))
        .route("/config/import", post(import_config))
        .route("/auth/status", get(auth_status))
        .route("/auth/start", post(auth_start))
        .route("/auth/refresh", post(auth_refresh))
        .route("/auth/callback", get(auth_callback))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            track_activity,
        ))
        .with_state(state);

    info!(addr = %local_addr, "admin UI listening");

    let idle_cancel = CancellationToken::new();
    if let Some(timeout) = idle_timeout {
        let activity = activity.clone();
        let idle_cancel = idle_cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let idle_for = activity.idle_for().await;
                if idle_for >= timeout {
                    info!(
                        idle_seconds = idle_for.as_secs(),
                        timeout_seconds = timeout.as_secs(),
                        "admin idle timeout reached; shutting down"
                    );
                    idle_cancel.cancel();
                    break;
                }
            }
        });
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = shutdown.cancelled() => {}
                _ = idle_cancel.cancelled() => {}
            }
        })
        .await
        .context("admin server exited unexpectedly")
}

async fn track_activity(State(state): State<AppState>, request: Request, next: Next) -> Response {
    state.activity.touch().await;
    next.run(request).await
}

async fn root() -> Html<&'static str> {
    Html(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>gambi admin</title>
  <link rel="icon" href="data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><rect width='100' height='100' rx='20' fill='%23e8a642'/><text x='50' y='70' text-anchor='middle' font-family='monospace' font-weight='bold' font-size='72' fill='%230c0c0e'>g</text></svg>">
  <style>
    *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
    :root {
      --bg: #0c0c0e; --surface: #141418; --raised: #1c1c22;
      --border: #232330; --border-hi: #333342;
      --text: #e4e2df; --text-2: #9594a0; --text-3: #5c5c6a;
      --accent: #e8a642; --accent-bg: rgba(232,166,66,0.08);
      --green: #4ade80; --red: #f87171; --red-bg: rgba(248,113,113,0.08);
      --mono: ui-monospace, 'Cascadia Code', 'SF Mono', Menlo, Consolas, monospace;
      --sans: system-ui, -apple-system, BlinkMacSystemFont, sans-serif;
      --r: 8px; --r-sm: 5px;
    }
    body { font-family: var(--sans); background: var(--bg); color: var(--text); font-size: 14px; line-height: 1.5; min-height: 100vh; }

    header { padding: 0 24px; height: 52px; display: flex; align-items: center; justify-content: space-between; background: var(--surface); border-bottom: 1px solid var(--border); position: sticky; top: 0; z-index: 10; }
    header::after { content: ''; position: absolute; bottom: -1px; left: 0; width: 120px; height: 1px; background: linear-gradient(90deg, var(--accent), transparent); }
    h1 { font-family: var(--mono); font-size: 15px; font-weight: 700; color: var(--accent); letter-spacing: -0.02em; }
    .hdr-r { display: flex; gap: 8px; align-items: center; }
    .badge { font-family: var(--mono); font-size: 11px; padding: 2px 10px; border-radius: 100px; background: var(--raised); border: 1px solid var(--border); color: var(--text-3); white-space: nowrap; display: inline-flex; align-items: center; gap: 5px; }
    .dot { width: 6px; height: 6px; border-radius: 50%; display: inline-block; }
    .dot-ok { background: var(--green); box-shadow: 0 0 6px var(--green); }
    .dot-err { background: var(--red); }
    .dot-off { background: var(--text-3); }

    .err-banner { display: none; padding: 8px 24px; background: var(--red-bg); border-bottom: 1px solid rgba(248,113,113,0.15); }
    .err-banner.show { display: flex; align-items: center; gap: 8px; }
    .err-banner pre { background: none; padding: 0; margin: 0; font-size: 12px; color: var(--red); max-height: none; overflow: visible; }

    main { padding: 20px 24px; display: grid; gap: 16px; grid-template-columns: 1fr 1fr; max-width: 1440px; margin: 0 auto; }
    @media (max-width: 960px) { main { grid-template-columns: 1fr; } }
    .span-2 { grid-column: 1 / -1; }

    .panel { background: var(--surface); border: 1px solid var(--border); border-radius: var(--r); display: flex; flex-direction: column; }
    .ph { padding: 10px 16px; display: flex; align-items: center; justify-content: space-between; border-bottom: 1px solid var(--border); min-height: 40px; }
    .ph h2 { font-family: var(--mono); font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.08em; color: var(--text-2); }
    .ph-r { display: flex; gap: 6px; align-items: center; }
    .pb { padding: 12px 16px; flex: 1; overflow: auto; }
    .pf { padding: 12px 16px; border-top: 1px solid var(--border); background: rgba(0,0,0,0.15); border-radius: 0 0 var(--r) var(--r); }

    .raw-btn { font-family: var(--mono); font-size: 10px; padding: 1px 8px; border-radius: 100px; background: none; border: 1px solid var(--border); color: var(--text-3); cursor: pointer; transition: all 0.15s; }
    .raw-btn:hover { border-color: var(--border-hi); color: var(--text-2); }

    pre { font-family: var(--mono); font-size: 11px; line-height: 1.6; color: var(--text-3); background: var(--bg); padding: 10px 12px; border-radius: var(--r-sm); overflow: auto; max-height: 280px; white-space: pre-wrap; word-break: break-all; margin: 0; }
    .raw { display: none; margin-top: 10px; }
    .raw.show { display: block; }

    .v-empty { font-family: var(--mono); font-size: 12px; color: var(--text-3); padding: 20px 0; text-align: center; }
    .v-item { display: flex; align-items: baseline; gap: 10px; padding: 7px 0; font-family: var(--mono); font-size: 12px; border-bottom: 1px solid var(--border); }
    .v-item:last-child { border-bottom: 0; }
    .v-name { color: var(--text); font-weight: 600; white-space: nowrap; flex-shrink: 0; }
    .v-det { color: var(--text-3); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0; font-size: 11px; }

    .t-row { display: grid; grid-template-columns: minmax(160px, 1fr) 2fr; gap: 12px; padding: 6px 0; border-bottom: 1px solid var(--border); align-items: baseline; font-family: var(--mono); font-size: 12px; }
    .t-row:last-child { border-bottom: 0; }
    .t-name { color: var(--text); font-weight: 500; }
    .t-ns { color: var(--text-3); }
    .t-desc { color: var(--text-3); font-size: 11px; }
    .p-row { display: flex; flex-direction: column; gap: 4px; padding: 8px 0; border-bottom: 1px solid var(--border); font-family: var(--mono); font-size: 11px; }
    .p-row:last-child { border-bottom: 0; }
    .po-top { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 8px; align-items: center; }
    .po-name { color: var(--text); min-width: 0; overflow-wrap: anywhere; }
    .po-config { display: flex; gap: 6px; align-items: center; justify-content: flex-end; flex-wrap: wrap; }
    .po-desc { color: var(--text-3); font-size: 11px; white-space: pre-wrap; overflow-wrap: anywhere; }
    .pill { border: 1px solid var(--border); border-radius: 100px; padding: 1px 8px; font-size: 10px; color: var(--text-2); justify-self: start; }
    .pill-safe { border-color: rgba(74,222,128,0.35); color: var(--green); background: rgba(74,222,128,0.08); }
    .pill-esc { border-color: rgba(248,113,113,0.35); color: var(--red); background: rgba(248,113,113,0.08); }
    .pill-src { color: var(--text-3); }

    .fail { padding: 6px 8px; background: var(--red-bg); border-radius: var(--r-sm); font-family: var(--mono); font-size: 11px; color: var(--red); margin-bottom: 8px; }

    .a-item { display: flex; align-items: center; justify-content: space-between; padding: 8px 0; font-family: var(--mono); font-size: 12px; border-bottom: 1px solid var(--border); }
    .a-item:last-child { border-bottom: 0; }
    .a-right { display: flex; align-items: center; gap: 8px; }
    .a-dot { width: 6px; height: 6px; border-radius: 50%; display: inline-block; margin-right: 6px; }
    .a-ok { background: var(--green); }
    .a-no { background: var(--text-3); }
    .a-bad { background: var(--red); }
    .a-warn { background: var(--accent); }

    form { display: flex; flex-direction: column; gap: 8px; }
    form + form { margin-top: 12px; padding-top: 12px; border-top: 1px solid var(--border); }
    .row { display: grid; gap: 8px; grid-template-columns: 1fr 1fr; }
    .f-label { font-size: 11px; color: var(--text-2); margin-bottom: -2px; }
    input, select { font-family: var(--mono); font-size: 12px; padding: 7px 10px; background: var(--bg); border: 1px solid var(--border); border-radius: var(--r-sm); color: var(--text); outline: none; transition: border-color 0.15s; }
    input::placeholder { color: var(--text-3); }
    input:focus, select:focus { border-color: var(--accent); }
    select { cursor: pointer; }
    select:disabled, input:disabled { opacity: 0.35; cursor: not-allowed; }
    button[type='submit'] { font-family: var(--sans); font-size: 12px; font-weight: 600; padding: 7px 16px; background: var(--accent); color: var(--bg); border: none; border-radius: var(--r-sm); cursor: pointer; transition: opacity 0.15s, transform 0.1s; }
    button[type='submit']:hover { opacity: 0.88; }
    button[type='submit']:active { transform: scale(0.98); }
    button[type='submit']:disabled { opacity: 0.3; cursor: not-allowed; transform: none; }
    .btn-d { background: var(--red-bg); border: 1px solid rgba(248,113,113,0.4); color: var(--red); }
    .btn-d:hover { background: rgba(248,113,113,0.14); border-color: rgba(248,113,113,0.6); opacity: 1; }
    .btn-d:disabled { background: transparent; border-color: rgba(248,113,113,0.15); }
    .note { font-size: 11px; color: var(--text-3); margin-top: 6px; }

    .logs-pre { max-height: 220px; min-height: 48px; border-radius: 0 0 var(--r) var(--r); }
    .logs-pre:empty::after { content: 'no log entries'; color: var(--text-3); }

    @keyframes fadeUp { from { opacity: 0; transform: translateY(6px); } to { opacity: 1; transform: none; } }
    .panel { animation: fadeUp 0.35s ease both; }
    .panel:nth-child(1) { animation-delay: 0s; }
    .panel:nth-child(2) { animation-delay: 0.04s; }
    .panel:nth-child(3) { animation-delay: 0.08s; }
    .panel:nth-child(4) { animation-delay: 0.12s; }
    .panel:nth-child(5) { animation-delay: 0.16s; }

    ::-webkit-scrollbar { width: 5px; height: 5px; }
    ::-webkit-scrollbar-track { background: transparent; }
    ::-webkit-scrollbar-thumb { background: var(--border); border-radius: 3px; }
    ::-webkit-scrollbar-thumb:hover { background: var(--border-hi); }

    .sr-only { position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px; overflow: hidden; clip: rect(0,0,0,0); border: 0; }
  </style>
</head>
<body>
  <header>
    <h1>gambi admin</h1>
    <div class="hdr-r" id="status-bar"></div>
  </header>

  <div class="err-banner" id="error-banner">
    <pre id="errors">(none)</pre>
  </div>

  <main>
    <section class="panel">
      <div class="ph"><h2>Servers</h2><button class="raw-btn" onclick="toggleRaw(this,'servers')">json</button></div>
      <div class="pb">
        <div id="servers-view"></div>
        <pre id="servers" class="raw">loading...</pre>
      </div>
      <div class="pf">
        <form id="server-add-form">
          <div class="row">
            <input id="server-name" placeholder="server name (e.g. github)" required>
            <input id="server-url" placeholder="url (e.g. stdio://... or https://...)" required>
            <select id="server-exposure-add">
              <option value="passthrough">passthrough</option>
              <option value="compact">compact</option>
              <option value="names-only">names-only</option>
              <option value="server-only">server-only</option>
            </select>
            <select id="server-policy-add">
              <option value="heuristic">heuristic</option>
              <option value="all-safe">all-safe</option>
              <option value="all-escalated">all-escalated</option>
              <option value="custom">custom</option>
            </select>
          </div>
          <button type="submit">Add Server</button>
        </form>
        <form id="server-exposure-form">
          <div class="row">
            <select id="server-exposure-name"></select>
            <select id="server-exposure-mode">
              <option value="passthrough">passthrough</option>
              <option value="compact">compact</option>
              <option value="names-only">names-only</option>
              <option value="server-only">server-only</option>
            </select>
            <button type="submit">Set Exposure</button>
          </div>
        </form>
        <form id="server-remove-form">
          <div class="row">
            <select id="server-remove-name"></select>
            <button type="submit" class="btn-d">Remove Server</button>
          </div>
        </form>
      </div>
    </section>

    <section class="panel">
      <div class="ph">
        <h2>Tools</h2>
        <div class="ph-r">
          <span class="badge" id="tool-count"></span>
          <button class="raw-btn" onclick="toggleRaw(this,'tools')">json</button>
        </div>
      </div>
      <div class="pb" style="max-height:480px;overflow:auto;">
        <div id="tools-view"></div>
        <pre id="tools" class="raw">loading...</pre>
      </div>
    </section>

    <section class="panel">
      <div class="ph"><h2>Policy & Overrides</h2><button class="raw-btn" onclick="toggleRaw(this,'policies')">json</button></div>
      <div class="pb">
        <div id="policies-view"></div>
        <pre id="policies" class="raw">loading...</pre>
        <div id="overrides-view" style="margin-top:10px;"></div>
        <pre id="overrides" class="raw">loading...</pre>
        <div class="note">Default safe heuristic: tool name starts with get/list/search/lookup/fetch, or description starts with get.</div>
      </div>
      <div class="pf">
        <form id="server-policy-mode-form">
          <span class="f-label">catalog policy mode</span>
          <div class="row">
            <select id="policy-server-mode-name"></select>
            <select id="policy-server-mode">
              <option value="heuristic">heuristic</option>
              <option value="all-safe">all-safe</option>
              <option value="all-escalated">all-escalated</option>
              <option value="custom">custom</option>
            </select>
          </div>
          <button type="submit">Set Catalog Mode</button>
        </form>
        <form id="policy-override-set-form">
          <span class="f-label">set tool policy override</span>
          <div class="row">
            <select id="policy-override-server"></select>
            <input id="policy-override-tool" placeholder="upstream tool name (e.g. search_issues)" required>
          </div>
          <div class="row">
            <select id="policy-override-level">
              <option value="safe">safe</option>
              <option value="escalated">escalated</option>
            </select>
          </div>
          <button type="submit">Save Policy Override</button>
        </form>
        <form id="policy-override-remove-form">
          <span class="f-label">remove tool policy override</span>
          <div class="row">
            <select id="policy-override-remove-server"></select>
            <input id="policy-override-remove-tool" placeholder="upstream tool name to remove" required>
          </div>
          <button type="submit" class="btn-d">Remove Policy Override</button>
        </form>
        <form id="override-set-form">
          <span class="f-label">set description override</span>
          <div class="row">
            <select id="override-server"></select>
            <input id="override-tool" placeholder="upstream tool name (e.g. search_issues)" required>
          </div>
          <input id="override-description" placeholder="description sent to MCP clients" required>
          <button type="submit">Save Description Override</button>
        </form>
        <form id="override-remove-form">
          <span class="f-label">remove description override</span>
          <div class="row">
            <select id="override-remove-server"></select>
            <input id="override-remove-tool" placeholder="upstream tool name to remove" required>
          </div>
          <button type="submit" class="btn-d">Remove Description Override</button>
        </form>
      </div>
    </section>

    <section class="panel">
      <div class="ph"><h2>Auth</h2><button class="raw-btn" onclick="toggleRaw(this,'auth')">json</button></div>
      <div class="pb">
        <div id="auth-view"></div>
        <pre id="auth" class="raw">loading...</pre>
      </div>
    </section>

    <section class="panel span-2">
      <div class="ph"><h2>Logs</h2></div>
      <pre id="logs" class="logs-pre">loading...</pre>
    </section>
  </main>

  <pre id="status" class="sr-only">loading...</pre>

  <script>
    let currentServers = [];
    let refreshInFlight = false;
    let TOOL_REFRESH_INTERVAL_MS = 15000;
    let REFRESH_INTERVAL_MS = 3000;
    let panelState = {
      toolsPayload: null,
      toolsRefreshAtMs: 0,
      serverPolicyModes: {}
    };

    function esc(s) {
      if (!s) return '';
      const d = document.createElement('div');
      d.textContent = s;
      return d.innerHTML;
    }

    function toggleRaw(btn, id) {
      const el = document.getElementById(id);
      if (!el) return;
      el.classList.toggle('show');
      btn.textContent = el.classList.contains('show') ? 'hide' : 'json';
    }

    function setError(message) {
      document.getElementById('errors').textContent = message || '(none)';
      const b = document.getElementById('error-banner');
      if (message) b.classList.add('show'); else b.classList.remove('show');
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
      document.getElementById('server-exposure-name').disabled = !hasServers;
      document.getElementById('server-exposure-mode').disabled = !hasServers;
      document.querySelector('#server-exposure-form button[type=submit]').disabled = !hasServers;
      document.getElementById('policy-server-mode-name').disabled = !hasServers;
      document.getElementById('policy-server-mode').disabled = !hasServers;
      document.querySelector('#server-policy-mode-form button[type=submit]').disabled = !hasServers;
      document.getElementById('policy-override-server').disabled = !hasServers;
      document.getElementById('policy-override-level').disabled = !hasServers;
      document.querySelector('#policy-override-set-form button[type=submit]').disabled = !hasServers;
      document.getElementById('policy-override-remove-server').disabled = !hasServers;
      document.querySelector('#policy-override-remove-form button[type=submit]').disabled = !hasServers;
      document.getElementById('override-server').disabled = !hasServers;
      document.querySelector('#override-set-form button[type=submit]').disabled = !hasServers;
      document.getElementById('override-remove-server').disabled = !hasServers;
      document.querySelector('#override-remove-form button[type=submit]').disabled = !hasServers;
    }

    function syncSelectedServerPolicyMode() {
      const selectServer = document.getElementById('policy-server-mode-name');
      const selectMode = document.getElementById('policy-server-mode');
      if (!selectServer || !selectMode) return;
      const serverName = selectServer.value;
      const mode = (panelState.serverPolicyModes && panelState.serverPolicyModes[serverName]) || 'heuristic';
      selectMode.value = mode;
    }

    async function fetchJson(path, label) {
      const response = await fetch(path);
      if (!response.ok) {
        const message = await readErrorMessage(response);
        throw new Error(label + ': ' + response.status + ' ' + message);
      }
      return response.json();
    }

    function shouldRefreshTools(forceTools) {
      if (forceTools) return true;
      if (!panelState.toolsPayload) return true;
      return Date.now() - panelState.toolsRefreshAtMs >= TOOL_REFRESH_INTERVAL_MS;
    }

    function renderStatusBar(p) {
      const bar = document.getElementById('status-bar');
      if (!p) { bar.innerHTML = '<span class="badge">connecting</span>'; return; }
      const dc = p.status === 'ok' ? 'dot-ok' : 'dot-err';
      const ex = p.exec_enabled ? 'exec on' : 'exec off';
      let h = '<span class="badge"><span class="dot ' + dc + '"></span>' + esc(p.status) + '</span>';
      h += '<span class="badge">' + esc(ex) + '</span>';
      h += '<span class="badge">' + p.server_count + ' server' + (p.server_count !== 1 ? 's' : '') + '</span>';
      if (p.discovery_failure_count > 0) {
        h += '<span class="badge" style="border-color:rgba(248,113,113,0.3);color:var(--red)">' + p.discovery_failure_count + ' failure' + (p.discovery_failure_count !== 1 ? 's' : '') + '</span>';
      }
      bar.innerHTML = h;
    }

    function renderServersView(servers) {
      const v = document.getElementById('servers-view');
      if (!servers || !servers.length) {
        v.innerHTML = '<div class="v-empty">no servers configured</div>';
        return;
      }
      v.innerHTML = servers.map(function(s) {
        const exposure = s.exposure_mode || 'passthrough';
        const policy = (panelState.serverPolicyModes && panelState.serverPolicyModes[s.name]) || 'heuristic';
        return '<div class="v-item"><span class="v-name">' + esc(s.name) + '</span><span class="v-det">' + esc(s.url) + ' · exposure=' + esc(exposure) + ' · policy=' + esc(policy) + '</span></div>';
      }).join('');
    }

    function renderToolsView(p) {
      const v = document.getElementById('tools-view');
      const c = document.getElementById('tool-count');
      if (!p || !p.tool_details) {
        v.innerHTML = '<div class="v-empty">discovering tools</div>';
        c.textContent = '';
        return;
      }
      c.textContent = p.tool_details.length;
      let h = '';
      if (p.failures && p.failures.length) {
        for (let i = 0; i < p.failures.length; i++) {
          h += '<div class="fail">' + esc(p.failures[i].server) + ': ' + esc(p.failures[i].error) + '</div>';
        }
      }
      for (let i = 0; i < p.tool_details.length; i++) {
        const t = p.tool_details[i];
        const idx = t.name.indexOf(':');
        let nm;
        if (idx > -1) {
          nm = '<span class="t-ns">' + esc(t.name.substring(0, idx + 1)) + '</span>' + esc(t.name.substring(idx + 1));
        } else {
          nm = esc(t.name);
        }
        h += '<div class="t-row"><span class="t-name">' + nm + '</span><span class="t-desc">' + esc(t.description) + '</span></div>';
      }
      v.innerHTML = h;
    }

    function renderPoliciesView(toolsPayload, serversPayload) {
      const v = document.getElementById('policies-view');
      const servers = Array.isArray(currentServers) ? currentServers : [];
      if (!servers.length) {
        v.innerHTML = '<div class="v-empty">no policy data</div>';
        return;
      }
      const byServer = {};
      const details = (toolsPayload && Array.isArray(toolsPayload.tool_details)) ? toolsPayload.tool_details : [];
      for (let i = 0; i < details.length; i++) {
        const t = details[i];
        if (!t || !t.server) continue;
        if (!byServer[t.server]) byServer[t.server] = [];
        byServer[t.server].push(t);
      }
      const serverModes = (serversPayload && serversPayload.server_tool_policy_modes) || {};
      const policyOverrides = (serversPayload && serversPayload.tool_policy_overrides) || {};
      let h = '';
      for (let i = 0; i < servers.length; i++) {
        const serverName = servers[i].name;
        const mode = serverModes[serverName] || 'heuristic';
        const tools = (byServer[serverName] || []).slice().sort(function(a, b) {
          return (a.upstream_name || '').localeCompare(b.upstream_name || '');
        });
        const safeCount = tools.filter(function(t) { return t.policy_level === 'safe'; }).length;
        const escalatedCount = tools.length - safeCount;
        const overrideCount = Object.keys(policyOverrides[serverName] || {}).length;
        h += '<div class="t-row"><span class="t-name">' + esc(serverName) + '</span><span class="t-desc">mode=' + esc(mode) + ' · safe=' + safeCount + ' · escalated=' + escalatedCount + ' · overrides=' + overrideCount + '</span></div>';
        if (!tools.length) {
          h += '<div class="v-item"><span class="v-det">no discovered tools</span></div>';
          continue;
        }
        for (let j = 0; j < tools.length; j++) {
          const level = tools[j].policy_level || 'escalated';
          const source = tools[j].policy_source || 'heuristic';
          const levelClass = level === 'safe' ? 'pill-safe' : 'pill-esc';
          const toolName = tools[j].upstream_name || tools[j].name || '';
          h += '<div class="p-row">';
          h += '<div class="po-top"><span class="po-name"><span class="t-ns">' + esc(serverName) + ':</span>' + esc(toolName) + '</span><span class="po-config"><span class="pill ' + levelClass + '">' + esc(level) + '</span><span class="pill pill-src">' + esc(source) + '</span></span></div>';
          h += '<div class="po-desc">' + esc(tools[j].description || '') + '</div>';
          h += '</div>';
        }
      }
      v.innerHTML = h;
    }

    function renderOverridesView(overrides) {
      const v = document.getElementById('overrides-view');
      if (!overrides || !Object.keys(overrides).length) {
        v.innerHTML = '<div class="v-empty">no overrides</div>';
        return;
      }
      let h = '';
      const srvs = Object.keys(overrides).sort();
      for (let i = 0; i < srvs.length; i++) {
        const tools = Object.keys(overrides[srvs[i]]).sort();
        for (let j = 0; j < tools.length; j++) {
          h += '<div class="p-row">';
          h += '<div class="po-top"><span class="po-name"><span class="t-ns">' + esc(srvs[i]) + ':</span>' + esc(tools[j]) + '</span><span class="po-config"><span class="pill pill-src">description override</span></span></div>';
          h += '<div class="po-desc">' + esc(overrides[srvs[i]][tools[j]]) + '</div>';
          h += '</div>';
        }
      }
      v.innerHTML = h;
    }

    function renderAuthView(p) {
      const v = document.getElementById('auth-view');
      if (!p || !p.statuses || !p.statuses.length) {
        v.innerHTML = '<div class="v-empty">no auth configured</div>';
        return;
      }
      let h = '';
      for (let i = 0; i < p.statuses.length; i++) {
        const s = p.statuses[i];
        let dot = 'a-no', label = 'not configured';
        if (s.oauth_configured && !s.has_token) { dot = 'a-no'; label = 'no token'; }
        if (s.has_token) { dot = 'a-ok'; label = 'authenticated'; }
        if (s.degraded) { dot = 'a-warn'; label = 'degraded'; }
        if (s.last_error) { dot = 'a-bad'; label = s.last_error; }
        if (!s.oauth_configured) { label = 'not configured'; }
        const canLogin = Boolean(s.oauth_configured) && !Boolean(s.has_token);
        const loginBtn = canLogin ? '<button class="raw-btn" data-server="' + escAttrHtml(s.server) + '" onclick="startOauth(this.getAttribute(&#39;data-server&#39;))">login</button>' : '';
        h += '<div class="a-item"><span><span class="a-dot ' + dot + '"></span>' + esc(s.server) + '</span><span class="a-right"><span style="color:var(--text-3)">' + esc(label) + '</span>' + loginBtn + '</span></div>';
      }
      v.innerHTML = h;
    }

    function escAttrHtml(value) {
      return String(value)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
    }

    async function startOauth(server) {
      await runAction('start oauth', async function() {
        const response = await fetch('/auth/start', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ server: server })
        });
        if (!response.ok) {
          const message = await readErrorMessage(response);
          throw new Error(response.status + ': ' + message);
        }
        const payload = await response.json();
        if (!payload || !payload.auth_url) {
          throw new Error('missing auth_url in /auth/start response');
        }
        window.open(payload.auth_url, '_blank', 'noopener');
      });
    }

    async function refresh(options) {
      options = options || {};
      const forceTools = Boolean(options.forceTools);
      if (refreshInFlight) return;
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
        panelState.serverPolicyModes = serversPayload.server_tool_policy_modes || {};
        document.getElementById('status').textContent = JSON.stringify(statusPayload, null, 2);
        document.getElementById('tools').textContent = JSON.stringify(panelState.toolsPayload || {}, null, 2);
        document.getElementById('servers').textContent = JSON.stringify(currentServers, null, 2);
        document.getElementById('policies').textContent = JSON.stringify({
          server_tool_policy_modes: serversPayload.server_tool_policy_modes || {},
          tool_policy_overrides: serversPayload.tool_policy_overrides || {}
        }, null, 2);
        document.getElementById('overrides').textContent = JSON.stringify(serversPayload.tool_description_overrides || {}, null, 2);
        document.getElementById('logs').textContent = (logsPayload.logs || []).join('\n');
        document.getElementById('auth').textContent = JSON.stringify(authPayload, null, 2);

        renderStatusBar(statusPayload);
        renderServersView(currentServers);
        renderToolsView(panelState.toolsPayload);
        renderPoliciesView(panelState.toolsPayload, serversPayload);
        renderOverridesView(serversPayload.tool_description_overrides || {});
        renderAuthView(authPayload);

        setServerOptions('server-remove-name', currentServers);
        setServerOptions('server-exposure-name', currentServers);
        setServerOptions('policy-server-mode-name', currentServers);
        setServerOptions('policy-override-server', currentServers);
        setServerOptions('policy-override-remove-server', currentServers);
        setServerOptions('override-server', currentServers);
        setServerOptions('override-remove-server', currentServers);
        setMutationFormState();
        syncSelectedServerPolicyMode();
      } catch (error) {
        setError('refresh failed: ' + (error instanceof Error ? error.message : String(error)));
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
        throw new Error(response.status + ': ' + message);
      }
    }

    async function deletePath(path) {
      const response = await fetch(path, { method: 'DELETE' });
      if (!response.ok) {
        const message = await readErrorMessage(response);
        throw new Error(response.status + ': ' + message);
      }
    }

    async function runAction(action, fn) {
      try {
        await fn();
        setError('');
      } catch (error) {
        setError(action + ' failed: ' + (error instanceof Error ? error.message : String(error)));
      }
    }

    document.getElementById('server-add-form').addEventListener('submit', async function(event) {
      event.preventDefault();
      await runAction('add server', async function() {
        await postJson('/servers', {
          name: document.getElementById('server-name').value,
          url: document.getElementById('server-url').value,
          exposure_mode: document.getElementById('server-exposure-add').value,
          policy_mode: document.getElementById('server-policy-add').value
        });
        event.target.reset();
        await refresh({ forceTools: true });
      });
    });

    document.getElementById('server-exposure-form').addEventListener('submit', async function(event) {
      event.preventDefault();
      await runAction('set exposure mode', async function() {
        const name = document.getElementById('server-exposure-name').value;
        if (!name) {
          throw new Error('no server selected');
        }
        await postJson('/servers/' + encodeURIComponent(name) + '/exposure', {
          exposure_mode: document.getElementById('server-exposure-mode').value
        });
        await refresh({ forceTools: true });
      });
    });

    document.getElementById('policy-server-mode-name').addEventListener('change', function() {
      syncSelectedServerPolicyMode();
    });

    document.getElementById('server-policy-mode-form').addEventListener('submit', async function(event) {
      event.preventDefault();
      await runAction('set catalog policy mode', async function() {
        const name = document.getElementById('policy-server-mode-name').value;
        if (!name) {
          throw new Error('no server selected');
        }
        await postJson('/servers/' + encodeURIComponent(name) + '/policy', {
          policy_mode: document.getElementById('policy-server-mode').value
        });
        await refresh({ forceTools: true });
      });
    });

    document.getElementById('server-remove-form').addEventListener('submit', async function(event) {
      event.preventDefault();
      await runAction('remove server', async function() {
        const name = document.getElementById('server-remove-name').value;
        if (!name) {
          throw new Error('no server selected');
        }
        await deletePath('/servers/' + encodeURIComponent(name));
        await refresh({ forceTools: true });
      });
    });

    document.getElementById('policy-override-set-form').addEventListener('submit', async function(event) {
      event.preventDefault();
      await runAction('save policy override', async function() {
        const server = document.getElementById('policy-override-server').value;
        if (!server) {
          throw new Error('no server selected');
        }
        await postJson('/tool-policies', {
          server: server,
          tool: document.getElementById('policy-override-tool').value,
          level: document.getElementById('policy-override-level').value
        });
        event.target.reset();
        await refresh({ forceTools: true });
      });
    });

    document.getElementById('policy-override-remove-form').addEventListener('submit', async function(event) {
      event.preventDefault();
      await runAction('remove policy override', async function() {
        const server = document.getElementById('policy-override-remove-server').value;
        if (!server) {
          throw new Error('no server selected');
        }
        await postJson('/tool-policies/remove', {
          server: server,
          tool: document.getElementById('policy-override-remove-tool').value
        });
        event.target.reset();
        await refresh({ forceTools: true });
      });
    });

    document.getElementById('override-set-form').addEventListener('submit', async function(event) {
      event.preventDefault();
      await runAction('save override', async function() {
        const server = document.getElementById('override-server').value;
        if (!server) {
          throw new Error('no server selected');
        }
        await postJson('/tool-descriptions', {
          server: server,
          tool: document.getElementById('override-tool').value,
          description: document.getElementById('override-description').value
        });
        event.target.reset();
        await refresh({ forceTools: true });
      });
    });

    document.getElementById('override-remove-form').addEventListener('submit', async function(event) {
      event.preventDefault();
      await runAction('remove override', async function() {
        const server = document.getElementById('override-remove-server').value;
        if (!server) {
          throw new Error('no server selected');
        }
        await postJson('/tool-descriptions/remove', {
          server: server,
          tool: document.getElementById('override-remove-tool').value
        });
        event.target.reset();
        await refresh({ forceTools: true });
      });
    });

    void refresh({ forceTools: true });
    setInterval(function() {
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

async fn heartbeat() -> Json<MutationResponse> {
    Json(MutationResponse { status: "ok" })
}

async fn shutdown_daemon(State(state): State<AppState>) -> Json<MutationResponse> {
    state.shutdown.cancel();
    Json(MutationResponse { status: "ok" })
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
        server_tool_policy_modes: cfg.server_tool_policy_modes,
        tool_description_overrides: cfg.tool_description_overrides,
        tool_policy_overrides: cfg.tool_policy_overrides,
    }))
}

async fn add_server(
    State(state): State<AppState>,
    Json(request): Json<AddServerRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    let server_name = request.name.clone();
    let server_name_for_update = server_name.clone();
    let policy_mode = request.policy_mode;
    let server = ServerConfig {
        name: request.name,
        url: request.url,
        oauth: request.oauth,
        transport: request.transport,
        exposure_mode: request.exposure_mode,
    };
    state
        .store
        .update_async(move |cfg| {
            cfg.add_server(server)?;
            cfg.set_server_tool_policy_mode(&server_name_for_update, policy_mode)
        })
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

async fn update_server_exposure(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<UpdateServerExposureRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    let target_name = name.clone();
    state
        .store
        .update_async(move |cfg| cfg.set_server_exposure_mode(&target_name, request.exposure_mode))
        .await
        .map_err(|err| {
            error!(
                error = %err,
                server = %name,
                "failed to update server exposure mode"
            );
            ApiError::bad_request(format!("failed to update exposure mode: {err}"))
        })?;
    state.upstream.invalidate_discovery_cache().await;
    Ok(Json(MutationResponse { status: "ok" }))
}

async fn update_server_tool_policy(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<UpdateServerToolPolicyRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    let target_name = name.clone();
    state
        .store
        .update_async(move |cfg| cfg.set_server_tool_policy_mode(&target_name, request.policy_mode))
        .await
        .map_err(|err| {
            error!(
                error = %err,
                server = %name,
                "failed to update server tool policy mode"
            );
            ApiError::bad_request(format!("failed to update policy mode: {err}"))
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

async fn set_tool_policy_override(
    State(state): State<AppState>,
    Json(request): Json<ToolPolicyOverrideRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    let req_server = request.server.clone();
    let req_tool = request.tool.clone();
    let req_level = request.level;
    state
        .store
        .update_async(move |cfg| cfg.set_tool_policy_override(&req_server, &req_tool, req_level))
        .await
        .map_err(|err| {
            error!(
                error = %err,
                server = %request.server,
                tool = %request.tool,
                "failed to set tool policy override"
            );
            ApiError::bad_request(format!("failed to set tool policy override: {err}"))
        })?;
    state.upstream.invalidate_discovery_cache().await;

    Ok(Json(MutationResponse { status: "ok" }))
}

async fn remove_tool_policy_override(
    State(state): State<AppState>,
    Json(request): Json<RemoveToolPolicyOverrideRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    let req_server = request.server.clone();
    let req_tool = request.tool.clone();
    let removed = state
        .store
        .update_async(move |cfg| Ok(cfg.remove_tool_policy_override(&req_server, &req_tool)))
        .await
        .map_err(|err| {
            error!(
                error = %err,
                server = %request.server,
                tool = %request.tool,
                "failed to remove tool policy override"
            );
            ApiError::internal("failed to remove tool policy override")
        })?;

    if !removed {
        return Err(ApiError::bad_request("policy override not found"));
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

    // If an HTTP upstream says "auth required" and we don't have a client_id yet, attempt
    // MCP-spec OAuth discovery + RFC 7591 dynamic client registration in the background.
    for failure in &discovery.failures {
        if !looks_like_auth_required(&failure.message) {
            continue;
        }
        let should_try_refresh = looks_like_invalid_token(&failure.message);
        let is_http = cfg
            .servers
            .iter()
            .find(|server| server.name == failure.server_name)
            .is_some_and(|server| {
                server.url.starts_with("http://") || server.url.starts_with("https://")
            });
        if !is_http {
            continue;
        }

        let auth = state.auth.clone();
        let upstream = state.upstream.clone();
        let server_name = failure.server_name.clone();
        let admin_base_url = state.admin_base_url.clone();
        tokio::spawn(async move {
            if let Err(err) = auth
                .ensure_registered_client(&server_name, &admin_base_url)
                .await
            {
                error!(
                    error = %err,
                    server = %server_name,
                    "oauth bootstrap after auth-required upstream failure did not succeed"
                );
            }
            if should_try_refresh {
                match auth.refresh(&server_name).await {
                    Ok(response) => {
                        info!(
                            server = %server_name,
                            refreshed = response.refreshed,
                            expires_at_epoch_seconds = ?response.expires_at_epoch_seconds,
                            "upstream reported invalid_token; oauth refresh succeeded"
                        );
                        upstream.invalidate_discovery_cache().await;
                    }
                    Err(err) => {
                        warn!(
                            error = %err,
                            server = %server_name,
                            "upstream reported invalid_token and oauth refresh failed"
                        );
                    }
                }
            }
        });
    }

    let mut tool_details = BTreeMap::<String, ToolDetail>::new();
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
        let policy = cfg.evaluate_tool_policy(
            &tool.server_name,
            &tool.upstream_name,
            Some(description.as_str()),
        );
        tool_details.insert(
            tool.namespaced_name.clone(),
            ToolDetail {
                name: tool.namespaced_name.clone(),
                description,
                server: Some(tool.server_name.clone()),
                upstream_name: Some(tool.upstream_name.clone()),
                policy_level: policy.level.as_str().to_string(),
                policy_source: policy.source.as_str().to_string(),
            },
        );
    }

    tool_details.insert(
        "gambi_list_servers".to_string(),
        ToolDetail {
            name: "gambi_list_servers".to_string(),
            description: "List configured upstream servers".to_string(),
            server: None,
            upstream_name: None,
            policy_level: ToolPolicyLevel::Safe.as_str().to_string(),
            policy_source: ToolPolicySource::System.as_str().to_string(),
        },
    );
    tool_details.insert(
        "gambi_list_upstream_tools".to_string(),
        ToolDetail {
            name: "gambi_list_upstream_tools".to_string(),
            description: "Discover upstream tool names and discovery failures for troubleshooting"
                .to_string(),
            server: None,
            upstream_name: None,
            policy_level: ToolPolicyLevel::Safe.as_str().to_string(),
            policy_source: ToolPolicySource::System.as_str().to_string(),
        },
    );
    tool_details.insert(
        "gambi_help".to_string(),
        ToolDetail {
            name: "gambi_help".to_string(),
            description:
                "Explain upstream MCP capabilities and return full tool metadata on demand (execute-only workflow)"
                    .to_string(),
            server: None,
            upstream_name: None,
            policy_level: ToolPolicyLevel::Safe.as_str().to_string(),
            policy_source: ToolPolicySource::System.as_str().to_string(),
        },
    );
    if state.exec_enabled {
        tool_details.insert(
            "gambi_execute".to_string(),
            ToolDetail {
                name: "gambi_execute".to_string(),
                description:
                    "Safe execution path with policy-aware tool-call bridge; escalated calls are blocked with ESCALATION_REQUIRED"
                        .to_string(),
                server: None,
                upstream_name: None,
                policy_level: ToolPolicyLevel::Safe.as_str().to_string(),
                policy_source: ToolPolicySource::System.as_str().to_string(),
            },
        );
        tool_details.insert(
            "gambi_execute_escalated".to_string(),
            ToolDetail {
                name: "gambi_execute_escalated".to_string(),
                description:
                    "Escalated execution path for workflows that require non-safe upstream tools"
                        .to_string(),
                server: None,
                upstream_name: None,
                policy_level: ToolPolicyLevel::Escalated.as_str().to_string(),
                policy_source: ToolPolicySource::System.as_str().to_string(),
            },
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
    let has_gambi_execute = tools.iter().any(|tool| tool == "gambi_execute");
    let has_gambi_execute_escalated = tools.iter().any(|tool| tool == "gambi_execute_escalated");
    if state.exec_enabled && (!has_gambi_execute || !has_gambi_execute_escalated) {
        warn!(
            tool_count = tools.len(),
            failure_count = failures.len(),
            has_gambi_execute,
            has_gambi_execute_escalated,
            "admin tools snapshot missing execute tool(s) while exec mode is enabled"
        );
    }
    info!(
        tool_count = tools.len(),
        failure_count = failures.len(),
        has_gambi_execute,
        has_gambi_execute_escalated,
        "admin tools snapshot prepared"
    );
    let tool_details = tool_details.into_values().collect::<Vec<_>>();

    Ok(Json(ToolsResponse {
        tools,
        tool_details,
        failures,
    }))
}

fn looks_like_auth_required(message: &str) -> bool {
    let msg = message.trim().to_ascii_lowercase();
    msg.contains("auth required")
        || msg.contains("unauthorized")
        || msg.contains("forbidden")
        || msg.contains("401")
        || msg.contains("403")
}

fn looks_like_invalid_token(message: &str) -> bool {
    message
        .trim()
        .to_ascii_lowercase()
        .contains("invalid_token")
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
