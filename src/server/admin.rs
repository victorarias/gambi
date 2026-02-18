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
    AppConfig, ConfigStore, ExposureMode, ServerConfig, ToolActivationMode, ToolPolicyLevel,
    ToolPolicyMode, ToolPolicySource,
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
    server_tool_activation_modes: BTreeMap<String, ToolActivationMode>,
    tool_activation_overrides: BTreeMap<String, BTreeMap<String, bool>>,
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
    original_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_name: Option<String>,
    enabled: bool,
    activation_source: String,
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
struct UpdateServerToolActivationRequest {
    tool_activation_mode: ToolActivationMode,
}

#[derive(Debug, Deserialize)]
struct UpdateServerEnabledRequest {
    enabled: bool,
}

fn default_true() -> bool {
    true
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
    #[serde(default)]
    tool_activation_mode: crate::config::ToolActivationMode,
    #[serde(default = "default_true")]
    enabled: bool,
}

#[derive(Debug, Deserialize)]
struct ToolActivationOverrideRequest {
    server: String,
    tool: String,
    enabled: bool,
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
        .route(
            "/servers/{name}/tool-default",
            post(update_server_tool_activation),
        )
        .route("/servers/{name}/enabled", post(update_server_enabled))
        .route("/servers/{name}", delete(remove_server))
        .route("/tools", get(tools))
        .route("/tool-activation", post(set_tool_activation_override))
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
  <link rel="icon" href="data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><rect width='100' height='100' rx='20' fill='%23d47a5a'/><text x='50' y='70' text-anchor='middle' font-family='monospace' font-weight='bold' font-size='72' fill='%230e0b10'>g</text></svg>">
  <style>
    *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
    :root {
      --bg: #0e0b10; --surface: #161318; --raised: #1e1a22;
      --border: #2a2630; --border-hi: #3b3544;
      --text: #dbd5d0; --text-2: #8e868a; --text-3: #585058;
      --accent: #d47a5a; --accent-bg: rgba(212,122,90,0.08);
      --green: #7abf8a; --red: #d46a6a; --red-bg: rgba(212,106,106,0.09);
      --mono: 'JetBrains Mono', 'Cascadia Code', ui-monospace, 'SF Mono', Menlo, Consolas, monospace;
      --sans: 'Avenir Next', 'Inter', system-ui, -apple-system, BlinkMacSystemFont, sans-serif;
      --r: 6px; --r-sm: 4px;
    }
    body {
      font-family: var(--sans);
      background:
        repeating-linear-gradient(0deg, transparent, transparent 23px, rgba(255,255,255,0.012) 23px, rgba(255,255,255,0.012) 24px),
        repeating-linear-gradient(90deg, transparent, transparent 23px, rgba(255,255,255,0.012) 23px, rgba(255,255,255,0.012) 24px),
        var(--bg);
      color: var(--text);
      font-size: 14px;
      line-height: 1.5;
      min-height: 100vh;
    }
    header { padding: 0 24px; height: 52px; display: flex; align-items: center; justify-content: space-between; background: var(--surface); border-bottom: 1px dashed var(--border); position: sticky; top: 0; z-index: 20; }
    header::after {
      content: '';
      position: absolute;
      bottom: -1px;
      left: 0;
      width: 100%;
      height: 1px;
      background: repeating-linear-gradient(90deg, var(--accent) 0 3px, transparent 3px 7px);
      opacity: 0.32;
    }
    h1 { font-family: var(--mono); font-size: 15px; font-weight: 700; color: var(--accent); letter-spacing: -0.03em; }
    .hdr-r { display: flex; gap: 8px; align-items: center; }
    .badge { font-family: var(--mono); font-size: 11px; padding: 2px 10px; border-radius: 100px; background: var(--raised); border: 1px dashed var(--border); color: var(--text-3); white-space: nowrap; display: inline-flex; align-items: center; gap: 5px; }
    .dot { width: 6px; height: 6px; border-radius: 50%; display: inline-block; }
    .dot-ok { background: var(--green); box-shadow: 0 0 5px rgba(122,191,138,0.45); }
    .dot-err { background: var(--red); }
    .dot-warn { background: var(--accent); }
    .dot-off { background: var(--text-3); }

    .status-strip { position: sticky; top: 52px; z-index: 18; background: var(--surface); border-top: 1px dashed rgba(255,255,255,0.04); border-bottom: 1px dashed var(--border); padding: 8px 24px; display: flex; gap: 8px; flex-wrap: wrap; }
    .status-chip { font-family: var(--mono); font-size: 11px; padding: 3px 9px; border-radius: 100px; border: 1px solid var(--border); background: var(--raised); color: var(--text-2); display: inline-flex; align-items: center; gap: 6px; }

    .err-banner { display: none; padding: 8px 24px; background: var(--red-bg); border-bottom: 1px solid rgba(248,113,113,0.15); }
    .err-banner.show { display: flex; align-items: center; gap: 8px; }
    .err-banner pre { background: none; padding: 0; margin: 0; font-size: 12px; color: var(--red); max-height: none; overflow: visible; }

    main { padding: 16px 24px 24px; max-width: 1440px; margin: 0 auto; }
    .tabs { display: flex; gap: 8px; margin-bottom: 12px; }
    .tab-btn { font-family: var(--mono); font-size: 12px; padding: 7px 12px; border-radius: 100px; border: 1px dashed var(--border); background: var(--surface); color: var(--text-2); cursor: pointer; transition: border-color 0.15s, color 0.15s, background 0.15s; }
    .tab-btn:hover { border-color: var(--border-hi); color: var(--text); }
    .tab-btn.active { border-color: rgba(212,122,90,0.5); color: var(--accent); background: var(--accent-bg); }

    .panel { background: var(--surface); border: 1px solid var(--border); border-radius: var(--r); display: flex; flex-direction: column; min-height: 460px; position: relative; overflow: hidden; }
    .panel::before {
      content: '';
      position: absolute;
      top: 0;
      left: 12px;
      right: 12px;
      height: 2px;
      background: repeating-linear-gradient(90deg, var(--accent) 0 10px, transparent 10px 14px);
      opacity: 0.18;
      pointer-events: none;
    }
    .ph { padding: 10px 16px; display: flex; align-items: center; justify-content: space-between; border-bottom: 1px dashed var(--border); min-height: 40px; }
    .ph h2 { font-family: var(--mono); font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.08em; color: var(--text-2); }
    .pb { padding: 12px 16px; flex: 1; overflow: auto; }
    .hidden { display: none !important; }

    .raw-btn { font-family: var(--mono); font-size: 10px; padding: 1px 8px; border-radius: 100px; background: none; border: 1px dashed var(--border); color: var(--text-3); cursor: pointer; }
    pre { font-family: var(--mono); font-size: 11px; line-height: 1.6; color: var(--text-3); background: var(--bg); border-left: 2px solid rgba(212,122,90,0.14); padding: 10px 12px; border-radius: var(--r-sm); overflow: auto; max-height: 320px; white-space: pre-wrap; word-break: break-all; margin: 10px 0 0; }
    .v-empty { font-family: var(--mono); font-size: 12px; color: var(--text-3); padding: 20px 0; text-align: center; }

    .server-add { display: grid; grid-template-columns: 1fr 2fr 1fr 1fr 1fr auto; gap: 8px; margin-bottom: 12px; }
    .server-list { display: grid; gap: 10px; }
    .server-card { border: 1px dashed var(--border); border-radius: var(--r-sm); background: rgba(0,0,0,0.16); padding: 10px; display: grid; gap: 8px; position: relative; }
    .server-card::before, .server-card::after {
      content: '+';
      position: absolute;
      font-family: var(--mono);
      font-size: 8px;
      line-height: 1;
      color: var(--text-3);
      opacity: 0.35;
    }
    .server-card::before { top: 2px; left: 4px; }
    .server-card::after { bottom: 2px; right: 4px; }
    .server-head { display: flex; align-items: baseline; justify-content: space-between; gap: 12px; }
    .server-title { display: inline-flex; align-items: center; gap: 8px; font-family: var(--mono); font-size: 12px; min-width: 0; }
    .server-name { color: var(--text); font-weight: 600; }
    .server-url { color: var(--text-3); font-size: 11px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .server-controls { display: grid; grid-template-columns: 1fr 1fr 1fr auto auto auto; gap: 8px; align-items: center; }
    .auth-line { display: flex; align-items: center; justify-content: space-between; gap: 8px; font-family: var(--mono); font-size: 11px; color: var(--text-2); }
    .chip-auth { display: inline-flex; align-items: center; gap: 6px; }

    input, select, textarea { font-family: var(--mono); font-size: 12px; padding: 7px 10px; background: var(--bg); border: 1px solid var(--border); border-radius: var(--r-sm); color: var(--text); outline: none; }
    input:focus, select:focus, textarea:focus { border-color: var(--accent); box-shadow: 0 0 0 1px rgba(212,122,90,0.15); }
    input::placeholder, textarea::placeholder { color: var(--text-3); }
    select:disabled, input:disabled, textarea:disabled { opacity: 0.45; cursor: not-allowed; }
    textarea { min-height: 72px; resize: vertical; }

    button { font-family: var(--sans); font-size: 12px; font-weight: 600; padding: 7px 12px; border: none; border-radius: var(--r-sm); cursor: pointer; background: var(--accent); color: #0e0b10; }
    button:hover { opacity: 0.9; }
    button:disabled { opacity: 0.45; cursor: wait; }
    .btn-quiet { background: transparent; color: var(--text-2); border: 1px dashed var(--border); }
    .btn-quiet:hover { border-color: var(--border-hi); color: var(--text); }
    .btn-d { background: var(--red-bg); border: 1px solid rgba(248,113,113,0.4); color: var(--red); }

    .tool-toolbar { display: grid; grid-template-columns: 2fr 1fr 1fr auto; gap: 8px; margin-bottom: 12px; }
    .tool-list { display: grid; gap: 8px; }
    .tool-row { border: 1px dashed var(--border); border-radius: var(--r-sm); background: rgba(0,0,0,0.16); padding: 10px; display: grid; gap: 8px; position: relative; }
    .tool-row.tool-row-disabled { opacity: 0.6; }
    .tool-row::before, .tool-row::after {
      content: '+';
      position: absolute;
      font-family: var(--mono);
      font-size: 8px;
      line-height: 1;
      color: var(--text-3);
      opacity: 0.35;
    }
    .tool-row::before { top: 2px; left: 4px; }
    .tool-row::after { bottom: 2px; right: 4px; }
    .tool-top { display: grid; grid-template-columns: auto minmax(0,1fr) auto; gap: 10px; align-items: center; }
    .tool-actions { display: inline-flex; gap: 6px; }
    .tool-name { font-family: var(--mono); font-size: 12px; color: var(--text); min-width: 0; overflow-wrap: anywhere; }
    .tool-ns { color: var(--text-3); }
    .tool-desc { font-family: var(--mono); font-size: 11px; color: var(--text-2); display: grid; gap: 7px; }
    .desc-value { cursor: pointer; border: 1px solid transparent; border-radius: var(--r-sm); padding: 6px; margin: -6px; }
    .desc-value:hover { border-color: var(--border); background: rgba(255,255,255,0.01); }
    .desc-edit-actions { display: inline-flex; gap: 6px; align-items: center; }
    .desc-diff { border: 1px solid var(--border); border-radius: var(--r-sm); padding: 6px; background: rgba(0,0,0,0.2); display: grid; gap: 4px; font-size: 10px; }
    .diff-old { color: var(--text-3); }
    .diff-new { color: var(--accent); }
    .pill { border: 1px solid var(--border); border-radius: 100px; padding: 1px 8px; font-size: 10px; color: var(--text-2); background: transparent; display: inline-flex; align-items: center; line-height: 1; }
    .pill-btn { font-family: var(--mono); cursor: pointer; border: 1px solid var(--border); border-radius: 100px; padding: 2px 8px; background: transparent; color: var(--text-2); }
    .pill-safe { border-color: rgba(122,191,138,0.35); color: var(--green); background: rgba(122,191,138,0.08); }
    .pill-esc { border-color: rgba(212,106,106,0.35); color: var(--red); background: rgba(212,106,106,0.09); }
    .pill-src { color: var(--text-3); }
    .pill-override { border-color: rgba(212,122,90,0.5); color: var(--accent); background: var(--accent-bg); }
    .fail { padding: 6px 8px; background: var(--red-bg); border-left: 2px solid var(--red); border-radius: 0 var(--r-sm) var(--r-sm) 0; font-family: var(--mono); font-size: 11px; color: var(--red); margin-bottom: 8px; }

    .logs-pre { max-height: 72vh; min-height: 260px; margin: 0; }
    .sr-only { position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px; overflow: hidden; clip: rect(0,0,0,0); border: 0; }
    @media (max-width: 960px) {
      .server-add { grid-template-columns: 1fr; }
      .server-controls { grid-template-columns: 1fr; }
      .tool-toolbar { grid-template-columns: 1fr; }
      .tool-top { grid-template-columns: 1fr; }
    }
  </style>
</head>
<body>
  <header>
    <h1>gambi admin</h1>
    <div class="hdr-r" id="status-bar"></div>
  </header>

  <div class="status-strip" id="status-strip"></div>

  <div class="err-banner" id="error-banner">
    <pre id="errors">(none)</pre>
  </div>

  <main>
    <div class="tabs">
      <button class="tab-btn" data-tab="servers">Servers</button>
      <button class="tab-btn" data-tab="tools">Tools</button>
      <button class="tab-btn" data-tab="logs">Logs</button>
    </div>

    <section class="panel" id="tab-servers">
      <div class="ph"><h2>Servers</h2><button class="raw-btn" data-raw-toggle="servers">json</button></div>
      <div class="pb">
        <form id="server-add-form" class="server-add">
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
          <select id="server-tool-default-add">
            <option value="all">all tools active</option>
            <option value="none">no tools active</option>
          </select>
          <button type="submit">Add Server</button>
        </form>
        <div id="servers-view" class="server-list"></div>
        <pre id="servers-raw" class="hidden">loading...</pre>
      </div>
    </section>

    <section class="panel hidden" id="tab-tools">
      <div class="ph">
        <h2>Tools</h2>
        <div class="hdr-r">
          <span class="badge" id="tool-count"></span>
          <button class="raw-btn" data-raw-toggle="tools">json</button>
        </div>
      </div>
      <div class="pb">
        <div class="tool-toolbar">
          <input id="tool-filter-text" placeholder="filter by name or description">
          <select id="tool-filter-server"></select>
          <select id="tool-filter-policy">
            <option value="all">all policies</option>
            <option value="safe">safe</option>
            <option value="escalated">escalated</option>
          </select>
          <button type="button" id="tool-filter-reset" class="btn-quiet">Reset</button>
        </div>
        <div id="tools-failures"></div>
        <div id="tools-view" class="tool-list"></div>
        <pre id="tools-raw" class="hidden">loading...</pre>
      </div>
    </section>

    <section class="panel hidden" id="tab-logs">
      <div class="ph"><h2>Logs</h2><button class="raw-btn" data-raw-toggle="logs">json</button></div>
      <div class="pb">
        <pre id="logs" class="logs-pre">loading...</pre>
        <pre id="logs-raw" class="hidden">loading...</pre>
      </div>
    </section>
  </main>

  <pre id="status" class="sr-only">loading...</pre>

  <script>
    const TOOL_REFRESH_INTERVAL_MS = 15000;
    const REFRESH_INTERVAL_MS = 3000;
    let refreshInFlight = false;
    let clearRemoveConfirmTimer = null;

    const state = {
      currentServers: [],
      serverActivationModes: {},
      toolActivationOverrides: {},
      serverPolicyModes: {},
      descriptionOverrides: {},
      authStatuses: [],
      toolsPayload: null,
      toolsRefreshAtMs: 0,
      latestLogsPayload: null,
      logsDirty: false,
      activeTab: 'servers',
      rawVisible: { servers: false, tools: false, logs: false },
      filterText: '',
      filterServer: 'all',
      filterPolicy: 'all',
      editingToolKey: null,
      editingDraft: '',
      showDiff: false,
      confirmRemoveServer: null
    };

    function esc(s) {
      if (!s) return '';
      const d = document.createElement('div');
      d.textContent = s;
      return d.innerHTML;
    }

    function escAttrHtml(value) {
      return String(value)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
    }

    function readHashTab() {
      const hash = window.location.hash.replace('#', '').trim();
      if (hash === 'servers' || hash === 'tools' || hash === 'logs') {
        return hash;
      }
      return 'servers';
    }

    function setActiveTab(tab) {
      state.activeTab = tab;
      window.location.hash = tab;
      const tabs = ['servers', 'tools', 'logs'];
      for (const t of tabs) {
        const btn = document.querySelector('.tab-btn[data-tab="' + t + '"]');
        const pane = document.getElementById('tab-' + t);
        if (!btn || !pane) continue;
        const active = t === tab;
        btn.classList.toggle('active', active);
        pane.classList.toggle('hidden', !active);
      }
    }

    function toggleRaw(kind) {
      state.rawVisible[kind] = !state.rawVisible[kind];
      const el = document.getElementById(kind + '-raw');
      if (!el) return;
      el.classList.toggle('hidden', !state.rawVisible[kind]);
      const btn = document.querySelector('[data-raw-toggle="' + kind + '"]');
      if (btn) {
        btn.textContent = state.rawVisible[kind] ? 'hide' : 'json';
      }
    }

    function selectionTouchesElement(element) {
      if (!element) return false;
      const selection = window.getSelection();
      if (!selection || selection.rangeCount === 0 || selection.isCollapsed) return false;
      const anchorNode = selection.anchorNode;
      const focusNode = selection.focusNode;
      return (anchorNode && element.contains(anchorNode)) || (focusNode && element.contains(focusNode));
    }

    function isSelectingLogsText() {
      const logs = document.getElementById('logs');
      const logsRaw = document.getElementById('logs-raw');
      return selectionTouchesElement(logs) || selectionTouchesElement(logsRaw);
    }

    function renderLogsFromState() {
      const payload = state.latestLogsPayload || {};
      document.getElementById('logs-raw').textContent = JSON.stringify(payload, null, 2);
      document.getElementById('logs').textContent = (payload.logs || []).join('\n');
      state.logsDirty = false;
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
      if (!state.toolsPayload) return true;
      return Date.now() - state.toolsRefreshAtMs >= TOOL_REFRESH_INTERVAL_MS;
    }

    function captureEditingDraftFromDom() {
      if (!state.editingToolKey) return;
      const textarea = document.getElementById('edit-desc-text');
      if (!textarea) return;
      state.editingDraft = textarea.value || '';
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

    function authViewModelByServer(serverName) {
      for (let i = 0; i < state.authStatuses.length; i++) {
        if (state.authStatuses[i].server === serverName) {
          return state.authStatuses[i];
        }
      }
      return null;
    }

    function statusChipForAuth(auth, serverEnabled) {
      if (!serverEnabled) {
        return { dot: 'dot-off', text: 'disabled', action: '' };
      }
      if (!auth || !auth.oauth_configured) {
        return { dot: 'dot-off', text: 'not configured', action: '' };
      }
      if (!auth.has_token) {
        return { dot: 'dot-off', text: 'no token', action: 'login' };
      }
      if (auth.last_error) {
        return { dot: 'dot-err', text: auth.last_error, action: 'refresh' };
      }
      if (auth.degraded) {
        return { dot: 'dot-warn', text: 'degraded', action: 'refresh' };
      }
      return { dot: 'dot-ok', text: 'authenticated', action: '' };
    }

    function renderStatusStrip() {
      const strip = document.getElementById('status-strip');
      const servers = Array.isArray(state.currentServers) ? state.currentServers : [];
      if (!servers.length) {
        strip.innerHTML = '<span class="status-chip"><span class="dot dot-off"></span>no servers configured</span>';
        return;
      }
      let h = '';
      for (const server of servers) {
        const auth = authViewModelByServer(server.name);
        const status = statusChipForAuth(auth, server.enabled !== false);
        h += '<span class="status-chip"><span class="dot ' + status.dot + '"></span>' + esc(server.name) + ': ' + esc(status.text) + '</span>';
        if (status.action === 'login') {
          h += '<button class="btn-quiet" type="button" data-action="status-login" data-server="' + escAttrHtml(server.name) + '">login</button>';
        }
        if (status.action === 'refresh') {
          h += '<button class="btn-quiet" type="button" data-action="status-refresh" data-server="' + escAttrHtml(server.name) + '">refresh</button>';
        }
      }
      strip.innerHTML = h;
    }

    function renderServersView() {
      const v = document.getElementById('servers-view');
      const servers = Array.isArray(state.currentServers) ? state.currentServers : [];
      if (!servers.length) {
        v.innerHTML = '<div class="v-empty">no servers configured</div>';
        return;
      }
      let h = '';
      for (const s of servers) {
        const exposure = s.exposure_mode || 'passthrough';
        const toolDefault = (state.serverActivationModes && state.serverActivationModes[s.name]) || 'all';
        const policy = (state.serverPolicyModes && state.serverPolicyModes[s.name]) || 'heuristic';
        const enabled = s.enabled !== false;
        const auth = authViewModelByServer(s.name);
        const authStatus = statusChipForAuth(auth, enabled);
        const toggleLabel = enabled ? 'Deactivate' : 'Activate';
        const wantsConfirm = state.confirmRemoveServer === s.name;
        h += '<article class="server-card">';
        h += '<div class="server-head"><div class="server-title"><span class="dot ' + authStatus.dot + '"></span><span class="server-name">' + esc(s.name) + '</span></div><span class="server-url">' + esc(s.url) + '</span></div>';
        h += '<div class="server-controls">';
        h += '<select class="server-exposure-select" data-server="' + escAttrHtml(s.name) + '">' + exposureOptions(exposure) + '</select>';
        h += '<select class="server-policy-select" data-server="' + escAttrHtml(s.name) + '">' + policyOptions(policy) + '</select>';
        h += '<select class="server-tool-default-select" data-server="' + escAttrHtml(s.name) + '">' + toolDefaultOptions(toolDefault) + '</select>';
        if (authStatus.action === 'login') {
          h += '<button type="button" class="btn-quiet" data-action="server-login" data-server="' + escAttrHtml(s.name) + '">Login</button>';
        } else if (authStatus.action === 'refresh') {
          h += '<button type="button" class="btn-quiet" data-action="server-refresh" data-server="' + escAttrHtml(s.name) + '">Refresh</button>';
        } else {
          h += '<span class="pill pill-src">' + (enabled ? 'auth ok' : 'disabled') + '</span>';
        }
        h += '<button type="button" class="btn-quiet" data-action="toggle-server" data-server="' + escAttrHtml(s.name) + '" data-enabled="' + escAttrHtml(String(enabled)) + '">' + toggleLabel + '</button>';
        h += '<button type="button" class="btn-d" data-action="remove-server" data-server="' + escAttrHtml(s.name) + '">' + (wantsConfirm ? 'Confirm Remove' : 'Remove') + '</button>';
        h += '</div>';
        h += '<div class="auth-line"><span class="chip-auth"><span class="dot ' + authStatus.dot + '"></span>' + esc(authStatus.text) + '</span><span class="pill pill-src">state=' + (enabled ? 'enabled' : 'disabled') + ' · exposure=' + esc(exposure) + ' · tools=' + esc(toolDefault) + ' · policy=' + esc(policy) + '</span></div>';
        h += '</article>';
      }
      v.innerHTML = h;
    }

    function exposureOptions(selected) {
      const options = ['passthrough', 'compact', 'names-only', 'server-only'];
      return options.map(function(value) {
        const sel = value === selected ? ' selected' : '';
        return '<option value="' + value + '"' + sel + '>' + value + '</option>';
      }).join('');
    }

    function policyOptions(selected) {
      const options = ['heuristic', 'all-safe', 'all-escalated', 'custom'];
      return options.map(function(value) {
        const sel = value === selected ? ' selected' : '';
        return '<option value="' + value + '"' + sel + '>' + value + '</option>';
      }).join('');
    }

    function toolDefaultOptions(selected) {
      const options = ['all', 'none'];
      return options.map(function(value) {
        const sel = value === selected ? ' selected' : '';
        return '<option value="' + value + '"' + sel + '>' + value + '</option>';
      }).join('');
    }

    function toolKey(server, tool) {
      return server + '::' + tool;
    }

    function overrideText(server, tool) {
      const byServer = state.descriptionOverrides && state.descriptionOverrides[server];
      if (!byServer) return null;
      const value = byServer[tool];
      return typeof value === 'string' ? value : null;
    }

    function renderToolFilters() {
      const filterServer = document.getElementById('tool-filter-server');
      if (!filterServer) return;
      const selected = state.filterServer || 'all';
      let h = '<option value="all">all servers</option>';
      for (const server of state.currentServers) {
        const sel = server.name === selected ? ' selected' : '';
        h += '<option value="' + escAttrHtml(server.name) + '"' + sel + '>' + esc(server.name) + '</option>';
      }
      filterServer.innerHTML = h;
      document.getElementById('tool-filter-text').value = state.filterText || '';
      document.getElementById('tool-filter-policy').value = state.filterPolicy || 'all';
    }

    function filteredToolDetails() {
      if (!state.toolsPayload || !Array.isArray(state.toolsPayload.tool_details)) return [];
      const text = (state.filterText || '').toLowerCase();
      const server = state.filterServer || 'all';
      const policy = state.filterPolicy || 'all';
      return state.toolsPayload.tool_details.filter(function(t) {
        if (!t.server) return false;
        if (server !== 'all' && t.server !== server) return false;
        if (policy !== 'all' && t.policy_level !== policy) return false;
        if (!text) return true;
        const hay = (t.name + ' ' + (t.description || '')).toLowerCase();
        return hay.includes(text);
      });
    }

    function renderToolsView() {
      const p = state.toolsPayload;
      const v = document.getElementById('tools-view');
      const c = document.getElementById('tool-count');
      const failures = document.getElementById('tools-failures');
      if (!p || !p.tool_details) {
        v.innerHTML = '<div class="v-empty">discovering tools</div>';
        failures.innerHTML = '';
        c.textContent = '';
        return;
      }
      c.textContent = p.tool_details.length;
      let failuresHtml = '';
      let h = '';
      if (p.failures && p.failures.length) {
        for (let i = 0; i < p.failures.length; i++) {
          failuresHtml += '<div class="fail">' + esc(p.failures[i].server) + ': ' + esc(p.failures[i].error) + '</div>';
        }
      }
      failures.innerHTML = failuresHtml;

      const tools = filteredToolDetails();
      if (!tools.length) {
        v.innerHTML = '<div class="v-empty">no tools match this filter</div>';
        return;
      }

      for (const t of tools) {
        const fullName = t.name || '';
        const idx = fullName.indexOf(':');
        let nsPrefix = '';
        let toolName = fullName;
        if (idx > -1) {
          nsPrefix = fullName.substring(0, idx + 1);
          toolName = fullName.substring(idx + 1);
        }
        const key = toolKey(t.server, t.upstream_name || toolName);
        const activeSafe = t.policy_level === 'safe';
        const enabled = t.enabled !== false;
        const activationSource = t.activation_source || 'catalog-all';
        const safeClass = activeSafe ? 'pill-btn pill-safe' : 'pill-btn';
        const escClass = !activeSafe ? 'pill-btn pill-esc' : 'pill-btn';
        const enabledClass = enabled ? 'pill-btn pill-safe' : 'pill-btn pill-esc';
        const override = overrideText(t.server, t.upstream_name || toolName);
        const isEditing = state.editingToolKey === key;
        const original = typeof t.original_description === 'string' ? t.original_description : '';
        const source = t.policy_source || 'heuristic';
        h += '<article class="tool-row' + (enabled ? '' : ' tool-row-disabled') + '">';
        h += '<div class="tool-top">';
        h += '<div class="tool-actions">';
        h += '<button type="button" class="' + enabledClass + '" data-action="toggle-tool-enabled" data-server="' + escAttrHtml(t.server) + '" data-tool="' + escAttrHtml(t.upstream_name || toolName) + '" data-enabled="' + escAttrHtml(String(enabled)) + '">' + (enabled ? 'active' : 'inactive') + '</button>';
        h += '<button type="button" class="' + safeClass + '" data-action="set-policy" data-server="' + escAttrHtml(t.server) + '" data-tool="' + escAttrHtml(t.upstream_name || toolName) + '" data-level="safe">safe</button>';
        h += '<button type="button" class="' + escClass + '" data-action="set-policy" data-server="' + escAttrHtml(t.server) + '" data-tool="' + escAttrHtml(t.upstream_name || toolName) + '" data-level="escalated">escalated</button>';
        h += '</div>';
        h += '<div class="tool-name"><span class="tool-ns">' + esc(nsPrefix) + '</span>' + esc(toolName) + '</div>';
        h += '<span class="pill pill-src">' + esc(activationSource) + ' · ' + esc(source) + '</span>';
        h += '</div>';
        h += '<div class="tool-desc">';
        if (isEditing) {
          h += '<textarea id="edit-desc-text">' + esc(state.editingDraft) + '</textarea>';
          h += '<div class="desc-edit-actions">';
          h += '<button type="button" data-action="save-desc" data-server="' + escAttrHtml(t.server) + '" data-tool="' + escAttrHtml(t.upstream_name || toolName) + '">Save Override</button>';
          h += '<button type="button" class="btn-quiet" data-action="cancel-desc">Cancel</button>';
          if (override && original) {
            h += '<button type="button" class="btn-quiet" data-action="toggle-diff">' + (state.showDiff ? 'Hide Diff' : 'Show Diff') + '</button>';
          }
          h += '</div>';
          if (state.showDiff && override && original) {
            h += '<div class="desc-diff"><div class="diff-old">- ' + esc(original) + '</div><div class="diff-new">+ ' + esc(state.editingDraft) + '</div></div>';
          }
        } else {
          h += '<div class="desc-value" data-action="edit-desc" data-key="' + escAttrHtml(key) + '">' + esc(t.description || '') + '</div>';
          h += '<div class="desc-edit-actions">';
          if (override) {
            h += '<span class="pill pill-override">override</span>';
            h += '<button type="button" class="btn-quiet" data-action="restore-desc" data-server="' + escAttrHtml(t.server) + '" data-tool="' + escAttrHtml(t.upstream_name || toolName) + '">Restore</button>';
          }
          h += '</div>';
        }
        h += '</div>';
        h += '</article>';
      }
      v.innerHTML = h;
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

    async function refreshOauth(server) {
      await runAction('refresh oauth', async function() {
        await postJson('/auth/refresh', { server: server });
      });
    }

    async function refresh(options) {
      options = options || {};
      const forceTools = Boolean(options.forceTools);
      if (refreshInFlight) return;
      refreshInFlight = true;
      try {
        captureEditingDraftFromDom();
        const refreshToolsNow = shouldRefreshTools(forceTools);
        const [statusPayload, serversPayload, logsPayload, authPayload] = await Promise.all([
          fetchJson('/status', 'status'),
          fetchJson('/servers', 'servers'),
          fetchJson('/logs?limit=200', 'logs'),
          fetchJson('/auth/status', 'auth/status')
        ]);
        if (refreshToolsNow) {
          state.toolsPayload = await fetchJson('/tools', 'tools');
          state.toolsRefreshAtMs = Date.now();
        }
        state.currentServers = serversPayload.servers || [];
        state.serverActivationModes = serversPayload.server_tool_activation_modes || {};
        state.toolActivationOverrides = serversPayload.tool_activation_overrides || {};
        state.serverPolicyModes = serversPayload.server_tool_policy_modes || {};
        state.descriptionOverrides = serversPayload.tool_description_overrides || {};
        state.authStatuses = authPayload.statuses || [];
        document.getElementById('status').textContent = JSON.stringify(statusPayload, null, 2);
        document.getElementById('tools-raw').textContent = JSON.stringify(state.toolsPayload || {}, null, 2);
        document.getElementById('servers-raw').textContent = JSON.stringify({
          servers: state.currentServers,
          server_tool_activation_modes: state.serverActivationModes,
          tool_activation_overrides: state.toolActivationOverrides,
          server_tool_policy_modes: serversPayload.server_tool_policy_modes || {},
          tool_description_overrides: state.descriptionOverrides,
          tool_policy_overrides: serversPayload.tool_policy_overrides || {}
        }, null, 2);
        state.latestLogsPayload = logsPayload || {};
        if (isSelectingLogsText()) {
          state.logsDirty = true;
        } else {
          renderLogsFromState();
        }

        renderStatusBar(statusPayload);
        renderStatusStrip();
        renderServersView();
        renderToolFilters();
        if (state.editingToolKey && !forceTools && !refreshToolsNow) {
          captureEditingDraftFromDom();
        } else {
          renderToolsView();
        }
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

    document.querySelector('.tabs').addEventListener('click', function(event) {
      const target = event.target instanceof Element ? event.target.closest('[data-tab]') : null;
      if (!target) return;
      event.preventDefault();
      const tab = target.getAttribute('data-tab');
      if (tab) setActiveTab(tab);
    });

    document.body.addEventListener('click', async function(event) {
      const target = event.target instanceof Element ? event.target.closest('[data-action], [data-raw-toggle]') : null;
      if (!target) return;

      const rawToggle = target.getAttribute('data-raw-toggle');
      if (rawToggle) {
        toggleRaw(rawToggle);
        return;
      }

      const action = target.getAttribute('data-action');
      if (!action) return;
      const server = target.getAttribute('data-server') || '';
      const tool = target.getAttribute('data-tool') || '';

      if (action === 'status-login' || action === 'server-login') {
        await startOauth(server);
        await refresh({ forceTools: true });
        return;
      }
      if (action === 'status-refresh' || action === 'server-refresh') {
        await refreshOauth(server);
        await refresh({ forceTools: true });
        return;
      }
      if (action === 'toggle-server') {
        const enabled = target.getAttribute('data-enabled') === 'true';
        await runAction('toggle server enabled state', async function() {
          await postJson('/servers/' + encodeURIComponent(server) + '/enabled', { enabled: !enabled });
          await refresh({ forceTools: true });
        });
        return;
      }
      if (action === 'remove-server') {
        if (state.confirmRemoveServer !== server) {
          state.confirmRemoveServer = server;
          renderServersView();
          if (clearRemoveConfirmTimer) clearTimeout(clearRemoveConfirmTimer);
          clearRemoveConfirmTimer = setTimeout(function() {
            state.confirmRemoveServer = null;
            renderServersView();
          }, 3000);
          return;
        }
        await runAction('remove server', async function() {
          await deletePath('/servers/' + encodeURIComponent(server));
          state.confirmRemoveServer = null;
          await refresh({ forceTools: true });
        });
        return;
      }
      if (action === 'set-policy') {
        const level = target.getAttribute('data-level') || 'safe';
        await runAction('set tool policy', async function() {
          await postJson('/tool-policies', { server: server, tool: tool, level: level });
          await refresh({ forceTools: true });
        });
        return;
      }
      if (action === 'toggle-tool-enabled') {
        const enabled = target.getAttribute('data-enabled') === 'true';
        await runAction('set tool activation', async function() {
          await postJson('/tool-activation', { server: server, tool: tool, enabled: !enabled });
          await refresh({ forceTools: true });
        });
        return;
      }
      if (action === 'edit-desc') {
        const key = target.getAttribute('data-key') || '';
        state.editingToolKey = key;
        const selected = filteredToolDetails().find(function(detail) {
          return toolKey(detail.server, detail.upstream_name || (detail.name || '').split(':').pop()) === key;
        });
        state.editingDraft = selected ? (selected.description || '') : '';
        state.showDiff = false;
        renderToolsView();
        return;
      }
      if (action === 'cancel-desc') {
        state.editingToolKey = null;
        state.editingDraft = '';
        state.showDiff = false;
        renderToolsView();
        return;
      }
      if (action === 'toggle-diff') {
        state.showDiff = !state.showDiff;
        renderToolsView();
        return;
      }
      if (action === 'save-desc') {
        const textarea = document.getElementById('edit-desc-text');
        const description = textarea ? textarea.value : state.editingDraft;
        await runAction('save description override', async function() {
          await postJson('/tool-descriptions', { server: server, tool: tool, description: description });
          state.editingToolKey = null;
          state.editingDraft = '';
          state.showDiff = false;
          await refresh({ forceTools: true });
        });
        return;
      }
      if (action === 'restore-desc') {
        await runAction('restore description override', async function() {
          await postJson('/tool-descriptions/remove', { server: server, tool: tool });
          state.editingToolKey = null;
          state.editingDraft = '';
          state.showDiff = false;
          await refresh({ forceTools: true });
        });
        return;
      }
    });

    document.getElementById('servers-view').addEventListener('change', async function(event) {
      const target = event.target;
      if (!(target instanceof Element)) return;
      if (target.classList.contains('server-exposure-select')) {
        const server = target.getAttribute('data-server') || '';
        const value = target.value;
        await runAction('set exposure mode', async function() {
          await postJson('/servers/' + encodeURIComponent(server) + '/exposure', { exposure_mode: value });
          await refresh({ forceTools: true });
        });
      }
      if (target.classList.contains('server-policy-select')) {
        const server = target.getAttribute('data-server') || '';
        const value = target.value;
        await runAction('set catalog policy mode', async function() {
          await postJson('/servers/' + encodeURIComponent(server) + '/policy', { policy_mode: value });
          await refresh({ forceTools: true });
        });
      }
      if (target.classList.contains('server-tool-default-select')) {
        const server = target.getAttribute('data-server') || '';
        const value = target.value;
        await runAction('set tool default mode', async function() {
          await postJson('/servers/' + encodeURIComponent(server) + '/tool-default', { tool_activation_mode: value });
          await refresh({ forceTools: true });
        });
      }
    });

    document.getElementById('server-add-form').addEventListener('submit', async function(event) {
      event.preventDefault();
      await runAction('add server', async function() {
        await postJson('/servers', {
          name: document.getElementById('server-name').value,
          url: document.getElementById('server-url').value,
          exposure_mode: document.getElementById('server-exposure-add').value,
          policy_mode: document.getElementById('server-policy-add').value,
          tool_activation_mode: document.getElementById('server-tool-default-add').value
        });
        event.target.reset();
        await refresh({ forceTools: true });
      });
    });

    document.getElementById('tool-filter-text').addEventListener('input', function(event) {
      state.filterText = event.target.value || '';
      renderToolsView();
    });

    document.body.addEventListener('input', function(event) {
      const target = event.target;
      if (!(target instanceof Element)) return;
      if (target.id !== 'edit-desc-text') return;
      state.editingDraft = target.value || '';
    });

    document.getElementById('tool-filter-server').addEventListener('change', function(event) {
      state.filterServer = event.target.value || 'all';
      renderToolsView();
    });

    document.getElementById('tool-filter-policy').addEventListener('change', function(event) {
      state.filterPolicy = event.target.value || 'all';
      renderToolsView();
    });

    document.getElementById('tool-filter-reset').addEventListener('click', function() {
      state.filterText = '';
      state.filterServer = 'all';
      state.filterPolicy = 'all';
      renderToolFilters();
      renderToolsView();
    });

    setActiveTab(readHashTab());
    window.addEventListener('hashchange', function() {
      setActiveTab(readHashTab());
    });
    document.addEventListener('selectionchange', function() {
      if (!state.logsDirty) return;
      if (isSelectingLogsText()) return;
      renderLogsFromState();
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
        server_tool_activation_modes: cfg.server_tool_activation_modes,
        tool_activation_overrides: cfg.tool_activation_overrides,
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
    let tool_activation_mode = request.tool_activation_mode;
    let server = ServerConfig {
        name: request.name,
        url: request.url,
        oauth: request.oauth,
        transport: request.transport,
        exposure_mode: request.exposure_mode,
        enabled: request.enabled,
    };
    state
        .store
        .update_async(move |cfg| {
            cfg.add_server(server)?;
            cfg.set_server_tool_policy_mode(&server_name_for_update, policy_mode)?;
            cfg.set_server_tool_activation_mode(&server_name_for_update, tool_activation_mode)
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

async fn update_server_tool_activation(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<UpdateServerToolActivationRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    let target_name = name.clone();
    state
        .store
        .update_async(move |cfg| {
            cfg.set_server_tool_activation_mode(&target_name, request.tool_activation_mode)
        })
        .await
        .map_err(|err| {
            error!(
                error = %err,
                server = %name,
                "failed to update server tool activation mode"
            );
            ApiError::bad_request(format!("failed to update tool default: {err}"))
        })?;
    state.upstream.invalidate_discovery_cache().await;
    Ok(Json(MutationResponse { status: "ok" }))
}

async fn update_server_enabled(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<UpdateServerEnabledRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    let target_name = name.clone();
    let enabled = request.enabled;
    state
        .store
        .update_async(move |cfg| cfg.set_server_enabled(&target_name, enabled))
        .await
        .map_err(|err| {
            error!(
                error = %err,
                server = %name,
                enabled,
                "failed to update server enabled state"
            );
            ApiError::bad_request(format!("failed to update server enabled state: {err}"))
        })?;
    if !enabled {
        state.upstream.evict_server(&name).await;
    }
    state.upstream.invalidate_discovery_cache().await;
    Ok(Json(MutationResponse { status: "ok" }))
}

async fn set_tool_activation_override(
    State(state): State<AppState>,
    Json(request): Json<ToolActivationOverrideRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    let req_server = request.server.clone();
    let req_tool = request.tool.clone();
    let req_enabled = request.enabled;
    state
        .store
        .update_async(move |cfg| {
            cfg.set_tool_activation_override(&req_server, &req_tool, req_enabled)
        })
        .await
        .map_err(|err| {
            error!(
                error = %err,
                server = %request.server,
                tool = %request.tool,
                enabled = request.enabled,
                "failed to set tool activation override"
            );
            ApiError::bad_request(format!("failed to set tool activation override: {err}"))
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
    let enabled_servers = cfg.enabled_servers();
    let auth_headers = load_upstream_auth_headers(&state).await?;
    let discovery = state
        .upstream
        .discover_tools_from_servers(&enabled_servers, &auth_headers)
        .await
        .map_err(|err| {
            error!(error = %err, "failed to discover upstream tools for tools request");
            ApiError::internal("failed to discover upstream tools")
        })?;

    let failed_servers = discovery
        .failures
        .iter()
        .map(|failure| failure.server_name.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let healthy_servers = enabled_servers
        .iter()
        .map(|server| server.name.clone())
        .filter(|server_name| !failed_servers.contains(server_name))
        .collect::<Vec<_>>();
    if !healthy_servers.is_empty() {
        match state.auth.mark_servers_healthy(&healthy_servers).await {
            Ok(cleared) => {
                if cleared > 0 {
                    info!(
                        cleared_server_count = cleared,
                        "cleared stale oauth degraded state after successful upstream discovery"
                    );
                }
            }
            Err(err) => {
                warn!(
                    error = %err,
                    "failed to clear oauth degraded state after successful upstream discovery"
                );
            }
        }
    }

    // If an HTTP upstream says "auth required" and we don't have a client_id yet, attempt
    // MCP-spec OAuth discovery + RFC 7591 dynamic client registration in the background.
    for failure in &discovery.failures {
        if !looks_like_auth_failure(&failure.message) {
            continue;
        }
        let should_try_refresh = looks_like_invalid_token(&failure.message)
            || looks_like_initialize_decode_auth_failure(&failure.message);
        let is_http = enabled_servers
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
        let upstream_description = tool
            .tool
            .description
            .as_ref()
            .map(|value| value.to_string())
            .unwrap_or_default();
        let override_description = cfg
            .tool_description_override_for(&tool.server_name, &tool.upstream_name)
            .map(ToOwned::to_owned);
        let description = override_description
            .clone()
            .unwrap_or_else(|| upstream_description.clone());
        let activation = cfg.evaluate_tool_activation(&tool.server_name, &tool.upstream_name);
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
                original_description: override_description
                    .as_ref()
                    .map(|_| upstream_description.clone()),
                server: Some(tool.server_name.clone()),
                upstream_name: Some(tool.upstream_name.clone()),
                enabled: activation.enabled,
                activation_source: activation.source.as_str().to_string(),
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
            original_description: None,
            server: None,
            upstream_name: None,
            enabled: true,
            activation_source: "system".to_string(),
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
            original_description: None,
            server: None,
            upstream_name: None,
            enabled: true,
            activation_source: "system".to_string(),
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
            original_description: None,
            server: None,
            upstream_name: None,
            enabled: true,
            activation_source: "system".to_string(),
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
                original_description: None,
                server: None,
                upstream_name: None,
                enabled: true,
                activation_source: "system".to_string(),
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
                original_description: None,
                server: None,
                upstream_name: None,
                enabled: true,
                activation_source: "system".to_string(),
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

fn looks_like_initialize_decode_auth_failure(message: &str) -> bool {
    let msg = message.trim().to_ascii_lowercase();
    msg.contains("send initialize request")
        && (msg.contains("error decoding response body") || msg.contains("unexpected content type"))
}

fn looks_like_invalid_token(message: &str) -> bool {
    message
        .trim()
        .to_ascii_lowercase()
        .contains("invalid_token")
}

fn looks_like_auth_failure(message: &str) -> bool {
    crate::upstream::message_has_auth_required_marker(message)
        || looks_like_auth_required(message)
        || looks_like_invalid_token(message)
        || looks_like_initialize_decode_auth_failure(message)
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
