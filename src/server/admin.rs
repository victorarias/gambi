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
    server_instruction_overrides: BTreeMap<String, String>,
    upstream_server_instructions: BTreeMap<String, String>,
    effective_server_instructions: BTreeMap<String, String>,
    server_instruction_sources: BTreeMap<String, String>,
    tool_description_overrides: BTreeMap<String, BTreeMap<String, String>>,
    tool_policy_overrides: BTreeMap<String, BTreeMap<String, ToolPolicyLevel>>,
}

#[derive(Debug, Serialize)]
struct ToolsResponse {
    tools: Vec<String>,
    tool_details: Vec<ToolDetail>,
    failures: Vec<ToolDiscoveryFailure>,
}

#[derive(Debug, Clone, Serialize)]
struct ToolDiscoveryFailure {
    server: String,
    error: String,
}

#[derive(Debug, Clone, Serialize)]
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
struct EffectiveOutputResponse {
    mcp_initialize: serde_json::Value,
    mcp_tools_list: serde_json::Value,
    gambi_list_servers: serde_json::Value,
    gambi_list_upstream_tools: serde_json::Value,
    gambi_help: serde_json::Value,
    tool_details: Vec<ToolDetail>,
    discovery_failures: Vec<ToolDiscoveryFailure>,
    impacted_servers: Vec<EffectiveServerImpact>,
    auth_statuses: Vec<AuthServerStatus>,
}

#[derive(Debug, Serialize)]
struct EffectiveServerImpact {
    server: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    discovery_error: Option<String>,
    oauth_configured: bool,
    has_token: bool,
    degraded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_error: Option<String>,
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
struct ServerInstructionOverrideRequest {
    server: String,
    instruction: String,
}

#[derive(Debug, Deserialize)]
struct RemoveServerInstructionOverrideRequest {
    server: String,
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

fn is_user_config_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("unknown server")
        || msg.contains("already exists")
        || msg.contains("cannot be empty")
        || msg.contains("exceeds max length")
        || msg.contains("invalid")
        || msg.contains("unsupported")
        || msg.contains("not found")
}

fn map_config_mutation_error(action: &str, err: anyhow::Error) -> ApiError {
    if is_user_config_error(&err) {
        ApiError::bad_request(format!("failed to {action}: {err}"))
    } else {
        ApiError::internal(format!("failed to {action}: {err}"))
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
        .route("/effective", get(effective))
        .route("/tool-activation", post(set_tool_activation_override))
        .route(
            "/server-instructions",
            post(set_server_instruction_override),
        )
        .route(
            "/server-instructions/remove",
            post(remove_server_instruction_override),
        )
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
    .effective-summary { display: flex; gap: 8px; flex-wrap: wrap; margin-bottom: 10px; }

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
    .server-instruction { display: grid; gap: 6px; padding-top: 6px; border-top: 1px dashed var(--border); }
    .server-instruction-value { font-family: var(--mono); font-size: 11px; color: var(--text-2); cursor: pointer; border: 1px solid transparent; border-radius: var(--r-sm); padding: 6px; margin: -6px; white-space: pre-wrap; overflow-wrap: anywhere; }
    .server-instruction-value:hover { border-color: var(--border); background: rgba(255,255,255,0.01); }
    .server-instruction-actions { display: inline-flex; align-items: center; gap: 6px; flex-wrap: wrap; }

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
    .effective-pre { max-height: 260px; min-height: 0; margin: 0; }
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
      <button class="tab-btn" data-tab="effective">Effective</button>
      <button class="tab-btn" data-tab="logs">Logs</button>
    </div>

    <section class="panel" id="tab-servers">
      <div class="ph"><h2>Servers</h2><button class="raw-btn" data-raw-toggle="servers">json</button></div>
      <div class="pb">
        <form id="server-add-form" class="server-add">
          <input id="server-name" placeholder="server name (e.g. github)" required>
          <input id="server-url" placeholder="url or command (e.g. https://... or npx -y @railway/mcp-server)" required>
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

    <section class="panel hidden" id="tab-effective">
      <div class="ph"><h2>Effective Output</h2><button class="raw-btn" data-raw-toggle="effective">json</button></div>
      <div class="pb">
        <div id="effective-summary" class="effective-summary"></div>
        <div id="effective-view" class="server-list"></div>
        <pre id="effective-raw" class="hidden">loading...</pre>
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
    const SERVER_INSTRUCTION_MAX_LENGTH = 8192;
    let refreshInFlight = false;
    let clearRemoveConfirmTimer = null;

    const state = {
      currentServers: [],
      serverActivationModes: {},
      toolActivationOverrides: {},
      serverPolicyModes: {},
      serverInstructionOverrides: {},
      upstreamServerInstructions: {},
      effectiveServerInstructions: {},
      serverInstructionSources: {},
      descriptionOverrides: {},
      authStatuses: [],
      toolsPayload: null,
      effectivePayload: null,
      toolsRefreshAtMs: 0,
      latestLogsPayload: null,
      latestStatusPayload: null,
      logsDirty: false,
      statusBarDirty: false,
      serversDirty: false,
      statusStripDirty: false,
      toolFiltersDirty: false,
      toolsDirty: false,
      effectiveDirty: false,
      activeTab: 'servers',
      rawVisible: { servers: false, tools: false, effective: false, logs: false },
      filterText: '',
      filterServer: 'all',
      filterPolicy: 'all',
      editingServerInstructionServer: null,
      editingServerInstructionDraft: '',
      showServerInstructionDiff: false,
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

    function looksLikeServerUrl(value) {
      return value.startsWith('stdio://') || value.startsWith('http://') || value.startsWith('https://');
    }

    function splitShellWords(input) {
      const parts = [];
      let current = '';
      let quote = '';
      let escaping = false;

      for (const ch of input) {
        if (escaping) {
          current += ch;
          escaping = false;
          continue;
        }

        if (quote === '\'') {
          if (ch === '\'') {
            quote = '';
          } else {
            current += ch;
          }
          continue;
        }

        if (ch === '\\') {
          escaping = true;
          continue;
        }

        if (quote === '"') {
          if (ch === '"') {
            quote = '';
          } else {
            current += ch;
          }
          continue;
        }

        if (ch === '\'' || ch === '"') {
          quote = ch;
          continue;
        }

        if (/\s/.test(ch)) {
          if (current) {
            parts.push(current);
            current = '';
          }
          continue;
        }

        current += ch;
      }

      if (escaping) {
        throw new Error('unterminated escape sequence in command');
      }
      if (quote) {
        throw new Error('unterminated quoted string in command');
      }
      if (current) {
        parts.push(current);
      }
      return parts;
    }

    function normalizeServerTarget(rawInput) {
      const trimmed = (rawInput || '').trim();
      if (!trimmed) {
        throw new Error('server target is required');
      }
      if (looksLikeServerUrl(trimmed)) {
        return trimmed;
      }

      const argv = splitShellWords(trimmed);
      if (!argv.length) {
        throw new Error('server target is required');
      }
      const command = argv[0];
      if (command.includes('?') || command.includes('#')) {
        throw new Error('command target cannot include ? or #');
      }

      const params = new URLSearchParams();
      for (let i = 1; i < argv.length; i++) {
        params.append('arg', argv[i]);
      }
      const query = params.toString();
      return query ? ('stdio://' + command + '?' + query) : ('stdio://' + command);
    }

    function readHashTab() {
      const hash = window.location.hash.replace('#', '').trim();
      if (hash === 'servers' || hash === 'tools' || hash === 'effective' || hash === 'logs') {
        return hash;
      }
      return 'servers';
    }

    function setActiveTab(tab) {
      state.activeTab = tab;
      window.location.hash = tab;
      const tabs = ['servers', 'tools', 'effective', 'logs'];
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

    function hasNonCollapsedSelection() {
      const selection = window.getSelection();
      return Boolean(selection && selection.rangeCount > 0 && !selection.isCollapsed);
    }

    function isEditableElement(element) {
      if (!(element instanceof Element)) return false;
      const tag = element.tagName.toLowerCase();
      return tag === 'input' || tag === 'textarea' || tag === 'select' || element.isContentEditable;
    }

    function isInteractingWithin(element) {
      if (!element) return false;
      const active = document.activeElement;
      if (active && element.contains(active) && isEditableElement(active)) {
        return true;
      }
      return hasNonCollapsedSelection() && selectionTouchesElement(element);
    }

    function renderLogsFromState() {
      const payload = state.latestLogsPayload || {};
      document.getElementById('logs-raw').textContent = JSON.stringify(payload, null, 2);
      document.getElementById('logs').textContent = (payload.logs || []).join('\n');
      state.logsDirty = false;
    }

    function flushDeferredRenders() {
      if (refreshInFlight) return;
      if (state.logsDirty && !isSelectingLogsText()) {
        renderLogsFromState();
      }

      if (state.statusBarDirty && !isEditableElement(document.activeElement)) {
        renderStatusBar(state.latestStatusPayload);
        state.statusBarDirty = false;
      }

      const statusStrip = document.getElementById('status-strip');
      if (state.statusStripDirty && !isInteractingWithin(statusStrip)) {
        renderStatusStrip();
        state.statusStripDirty = false;
      }

      const serversTab = document.getElementById('tab-servers');
      if (state.serversDirty && !isInteractingWithin(serversTab)) {
        renderServersView();
        state.serversDirty = false;
      }

      const toolToolbar = document.querySelector('#tab-tools .tool-toolbar');
      if (state.toolFiltersDirty && !isInteractingWithin(toolToolbar)) {
        renderToolFilters();
        state.toolFiltersDirty = false;
      }

      const toolsTab = document.getElementById('tab-tools');
      if (state.toolsDirty && !isInteractingWithin(toolsTab)) {
        renderToolsView();
        state.toolsDirty = false;
      }

      const effectiveTab = document.getElementById('tab-effective');
      if (state.effectiveDirty && !isInteractingWithin(effectiveTab)) {
        renderEffectiveView();
        state.effectiveDirty = false;
      }
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
      if (!state.toolsPayload || !state.effectivePayload) return true;
      return Date.now() - state.toolsRefreshAtMs >= TOOL_REFRESH_INTERVAL_MS;
    }

    function captureEditingDraftFromDom() {
      if (state.editingToolKey) {
        const textarea = document.getElementById('edit-desc-text');
        if (textarea) {
          state.editingDraft = textarea.value || '';
        }
      }
      if (state.editingServerInstructionServer) {
        const textarea = document.getElementById('edit-server-instruction-text');
        if (textarea) {
          state.editingServerInstructionDraft = textarea.value || '';
        }
      }
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
        return { dot: 'dot-off', text: 'disabled', action: '', canLogin: false, loginLabel: 'login' };
      }
      if (!auth || !auth.oauth_configured) {
        return { dot: 'dot-off', text: 'not configured', action: '', canLogin: false, loginLabel: 'login' };
      }
      if (!auth.has_token) {
        return { dot: 'dot-off', text: 'no token', action: 'login', canLogin: true, loginLabel: 'login' };
      }
      if (auth.last_error) {
        return { dot: 'dot-err', text: auth.last_error, action: 'refresh', canLogin: true, loginLabel: 'relogin' };
      }
      if (auth.degraded) {
        return { dot: 'dot-warn', text: 'degraded', action: 'refresh', canLogin: true, loginLabel: 'relogin' };
      }
      return { dot: 'dot-ok', text: 'authenticated', action: '', canLogin: true, loginLabel: 'relogin' };
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
        if (status.canLogin) {
          h += '<button class="btn-quiet" type="button" data-action="status-login" data-server="' + escAttrHtml(server.name) + '">' + esc(status.loginLabel) + '</button>';
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
        const instructionOverride = serverInstructionOverride(s.name);
        const upstreamInstruction = upstreamServerInstruction(s.name);
        const effectiveInstruction = effectiveServerInstruction(s.name);
        const instructionSource = serverInstructionSource(s.name);
        const isEditingInstruction = state.editingServerInstructionServer === s.name;
        const canSaveInstruction = Boolean(
          state.editingServerInstructionDraft && state.editingServerInstructionDraft.trim()
        );
        h += '<article class="server-card">';
        h += '<div class="server-head"><div class="server-title"><span class="dot ' + authStatus.dot + '"></span><span class="server-name">' + esc(s.name) + '</span></div><span class="server-url">' + esc(s.url) + '</span></div>';
        h += '<div class="server-controls">';
        h += '<select class="server-exposure-select" data-server="' + escAttrHtml(s.name) + '">' + exposureOptions(exposure) + '</select>';
        h += '<select class="server-policy-select" data-server="' + escAttrHtml(s.name) + '">' + policyOptions(policy) + '</select>';
        h += '<select class="server-tool-default-select" data-server="' + escAttrHtml(s.name) + '">' + toolDefaultOptions(toolDefault) + '</select>';
        if (authStatus.action === 'refresh') {
          h += '<button type="button" class="btn-quiet" data-action="server-refresh" data-server="' + escAttrHtml(s.name) + '">Refresh</button>';
        }
        if (authStatus.canLogin) {
          h += '<button type="button" class="btn-quiet" data-action="server-login" data-server="' + escAttrHtml(s.name) + '">' + esc(authStatus.loginLabel) + '</button>';
        }
        if (!authStatus.canLogin && authStatus.action !== 'refresh') {
          h += '<span class="pill pill-src">' + (enabled ? 'auth ok' : 'disabled') + '</span>';
        }
        h += '<button type="button" class="btn-quiet" data-action="toggle-server" data-server="' + escAttrHtml(s.name) + '" data-enabled="' + escAttrHtml(String(enabled)) + '">' + toggleLabel + '</button>';
        h += '<button type="button" class="btn-d" data-action="remove-server" data-server="' + escAttrHtml(s.name) + '">' + (wantsConfirm ? 'Confirm Remove' : 'Remove') + '</button>';
        h += '</div>';
        h += '<div class="auth-line"><span class="chip-auth"><span class="dot ' + authStatus.dot + '"></span>' + esc(authStatus.text) + '</span><span class="pill pill-src">state=' + (enabled ? 'enabled' : 'disabled') + ' · exposure=' + esc(exposure) + ' · tools=' + esc(toolDefault) + ' · policy=' + esc(policy) + '</span></div>';
        h += '<div class="server-instruction">';
        h += '<span class="pill pill-src">instruction</span>';
        if (isEditingInstruction) {
          h += '<textarea id="edit-server-instruction-text" placeholder="Describe how this server should be used...">' + esc(state.editingServerInstructionDraft) + '</textarea>';
          h += '<div class="server-instruction-actions">';
          h += '<button type="button" data-action="save-server-instruction" data-server="' + escAttrHtml(s.name) + '"' + (canSaveInstruction ? '' : ' disabled') + '>Save Override</button>';
          h += '<button type="button" class="btn-quiet" data-action="cancel-server-instruction">Cancel</button>';
          if (instructionOverride !== null && upstreamInstruction !== null) {
            h += '<button type="button" class="btn-quiet" data-action="toggle-server-instruction-diff">' + (state.showServerInstructionDiff ? 'Hide Diff' : 'Show Diff') + '</button>';
          }
          h += '</div>';
          if (state.showServerInstructionDiff && instructionOverride !== null && upstreamInstruction !== null) {
            h += '<div class="desc-diff"><div class="diff-old">- ' + esc(upstreamInstruction) + '</div><div class="diff-new">+ ' + esc(state.editingServerInstructionDraft) + '</div></div>';
          }
        } else {
          const instructionText = effectiveInstruction || 'No instruction set. Click to add an override.';
          h += '<div class="server-instruction-value" data-action="edit-server-instruction" data-server="' + escAttrHtml(s.name) + '">' + esc(instructionText) + '</div>';
          h += '<div class="server-instruction-actions">';
          h += '<span class="pill ' + (instructionSource === 'override' ? 'pill-override' : 'pill-src') + '">' + esc(instructionSource) + '</span>';
          if (instructionOverride !== null) {
            h += '<button type="button" class="btn-quiet" data-action="restore-server-instruction" data-server="' + escAttrHtml(s.name) + '">Restore</button>';
          }
          h += '</div>';
        }
        h += '</div>';
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

    function serverInstructionOverride(server) {
      const value = state.serverInstructionOverrides && state.serverInstructionOverrides[server];
      return typeof value === 'string' ? value : null;
    }

    function upstreamServerInstruction(server) {
      const value = state.upstreamServerInstructions && state.upstreamServerInstructions[server];
      return typeof value === 'string' ? value : null;
    }

    function effectiveServerInstruction(server) {
      const value = state.effectiveServerInstructions && state.effectiveServerInstructions[server];
      return typeof value === 'string' ? value : '';
    }

    function serverInstructionSource(server) {
      const value = state.serverInstructionSources && state.serverInstructionSources[server];
      return typeof value === 'string' ? value : 'none';
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

    function renderEffectiveView() {
      const payload = state.effectivePayload;
      const summary = document.getElementById('effective-summary');
      const view = document.getElementById('effective-view');
      const raw = document.getElementById('effective-raw');
      if (raw) {
        raw.textContent = JSON.stringify(payload || {}, null, 2);
      }
      if (!payload) {
        summary.innerHTML = '<span class="status-chip"><span class="dot dot-off"></span>building effective output</span>';
        view.innerHTML = '<div class="v-empty">effective output not ready</div>';
        return;
      }

      const helpServers = (payload.gambi_help && payload.gambi_help.servers) || [];
      const localTools = (payload.mcp_tools_list && payload.mcp_tools_list.tools) || [];
      const failures = payload.discovery_failures || [];
      const impacts = payload.impacted_servers || [];
      summary.innerHTML =
        '<span class="status-chip"><span class="dot dot-ok"></span>initialize + tools/list preview</span>' +
        '<span class="status-chip"><span class="dot dot-ok"></span>' + localTools.length + ' local tool(s)</span>' +
        '<span class="status-chip"><span class="dot dot-ok"></span>' + helpServers.length + ' server(s) in gambi_help</span>' +
        '<span class="status-chip"><span class="dot ' + (failures.length ? 'dot-err' : 'dot-ok') + '"></span>' + failures.length + ' discovery failure(s)</span>';

      let html = '';
      const instructions = payload.mcp_initialize && payload.mcp_initialize.instructions
        ? payload.mcp_initialize.instructions
        : 'No server instructions set';
      html += '<article class="server-card">';
      html += '<div class="server-head"><div class="server-title"><span class="dot dot-ok"></span><span class="server-name">initialize.instructions</span></div></div>';
      html += '<pre class="effective-pre">' + esc(instructions) + '</pre>';
      html += '</article>';

      html += '<article class="server-card">';
      html += '<div class="server-head"><div class="server-title"><span class="dot dot-ok"></span><span class="server-name">tools/list (local gambi tools)</span></div></div>';
      if (!localTools.length) {
        html += '<div class="v-empty">no local tools exposed</div>';
      } else {
        html += '<div class="tool-list">';
        for (const tool of localTools) {
          html += '<div class="tool-row"><div class="tool-name">' + esc(tool.name || '') + '</div><div class="tool-desc">' + esc(tool.description || '') + '</div></div>';
        }
        html += '</div>';
      }
      html += '</article>';

      html += '<article class="server-card">';
      html += '<div class="server-head"><div class="server-title"><span class="dot dot-warn"></span><span class="server-name">upstream impact</span></div></div>';
      if (!impacts.length) {
        html += '<div class="v-empty">no auth/discovery impact detected</div>';
      } else {
        html += '<div class="tool-list">';
        for (const impact of impacts) {
          const status = impact.discovery_error ? 'discovery error' : (impact.degraded ? 'degraded' : (impact.has_token ? 'healthy' : 'token missing'));
          const dot = impact.discovery_error ? 'dot-err' : (impact.degraded ? 'dot-warn' : (impact.has_token ? 'dot-ok' : 'dot-off'));
          html += '<div class="tool-row">';
          html += '<div class="tool-top">';
          html += '<div class="tool-name"><span class="dot ' + dot + '"></span> ' + esc(impact.server || '') + '</div>';
          html += '<span class="pill pill-src">' + esc(status) + '</span>';
          html += '</div>';
          if (impact.discovery_error) {
            html += '<div class="tool-desc">' + esc(impact.discovery_error) + '</div>';
          } else if (impact.auth_error) {
            html += '<div class="tool-desc">' + esc(impact.auth_error) + '</div>';
          }
          html += '</div>';
        }
        html += '</div>';
      }
      html += '</article>';

      html += '<article class="server-card">';
      html += '<div class="server-head"><div class="server-title"><span class="dot dot-ok"></span><span class="server-name">gambi effective tool routing</span></div></div>';
      const toolDetails = Array.isArray(payload.tool_details) ? payload.tool_details.filter(function(detail) { return Boolean(detail.server); }) : [];
      if (!toolDetails.length) {
        html += '<div class="v-empty">no upstream tools currently exposed</div>';
      } else {
        html += '<div class="tool-list">';
        for (const detail of toolDetails) {
          const levelClass = detail.policy_level === 'safe' ? 'pill-safe' : 'pill-esc';
          const activeClass = detail.enabled ? 'pill-safe' : 'pill-esc';
          html += '<div class="tool-row">';
          html += '<div class="tool-top">';
          html += '<div class="tool-name">' + esc(detail.name || '') + '</div>';
          html += '<div class="tool-actions"><span class="pill ' + activeClass + '">' + (detail.enabled ? 'active' : 'inactive') + '</span><span class="pill ' + levelClass + '">' + esc(detail.policy_level || '') + '</span><span class="pill pill-src">' + esc(detail.activation_source || '') + ' · ' + esc(detail.policy_source || '') + '</span></div>';
          html += '</div>';
          html += '<div class="tool-desc">' + esc(detail.description || '') + '</div>';
          html += '</div>';
        }
        html += '</div>';
      }
      html += '</article>';

      view.innerHTML = html;
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
          const [toolsPayload, effectivePayload] = await Promise.all([
            fetchJson('/tools', 'tools'),
            fetchJson('/effective', 'effective')
          ]);
          state.toolsPayload = toolsPayload;
          state.effectivePayload = effectivePayload;
          state.toolsRefreshAtMs = Date.now();
        }
        state.currentServers = serversPayload.servers || [];
        state.serverActivationModes = serversPayload.server_tool_activation_modes || {};
        state.toolActivationOverrides = serversPayload.tool_activation_overrides || {};
        state.serverPolicyModes = serversPayload.server_tool_policy_modes || {};
        state.serverInstructionOverrides = serversPayload.server_instruction_overrides || {};
        state.upstreamServerInstructions = serversPayload.upstream_server_instructions || {};
        state.effectiveServerInstructions = serversPayload.effective_server_instructions || {};
        state.serverInstructionSources = serversPayload.server_instruction_sources || {};
        state.descriptionOverrides = serversPayload.tool_description_overrides || {};
        state.authStatuses = authPayload.statuses || [];
        state.latestStatusPayload = statusPayload;
        if (state.editingServerInstructionServer) {
          const stillPresent = state.currentServers.some(function(server) {
            return server && server.name === state.editingServerInstructionServer;
          });
          if (!stillPresent) {
            state.editingServerInstructionServer = null;
            state.editingServerInstructionDraft = '';
            state.showServerInstructionDiff = false;
          }
        }
        document.getElementById('status').textContent = JSON.stringify(statusPayload, null, 2);
        document.getElementById('tools-raw').textContent = JSON.stringify(state.toolsPayload || {}, null, 2);
        document.getElementById('servers-raw').textContent = JSON.stringify({
          servers: state.currentServers,
          server_tool_activation_modes: state.serverActivationModes,
          tool_activation_overrides: state.toolActivationOverrides,
          server_tool_policy_modes: serversPayload.server_tool_policy_modes || {},
          server_instruction_overrides: state.serverInstructionOverrides,
          upstream_server_instructions: state.upstreamServerInstructions,
          effective_server_instructions: state.effectiveServerInstructions,
          server_instruction_sources: state.serverInstructionSources,
          tool_description_overrides: state.descriptionOverrides,
          tool_policy_overrides: serversPayload.tool_policy_overrides || {}
        }, null, 2);
        state.latestLogsPayload = logsPayload || {};
        if (isSelectingLogsText()) {
          state.logsDirty = true;
        } else {
          renderLogsFromState();
        }

        const freezeInteractiveRenders = !forceTools && (
          hasNonCollapsedSelection() || isEditableElement(document.activeElement)
        );
        if (freezeInteractiveRenders) {
          captureEditingDraftFromDom();
          state.statusBarDirty = true;
          state.statusStripDirty = true;
          state.serversDirty = true;
          state.toolFiltersDirty = true;
          state.toolsDirty = true;
          state.effectiveDirty = true;
        } else {
          renderStatusBar(statusPayload);
          state.statusBarDirty = false;

          const preserveStatusStrip = !forceTools && isInteractingWithin(document.getElementById('status-strip'));
          if (preserveStatusStrip) {
            state.statusStripDirty = true;
          } else {
            renderStatusStrip();
            state.statusStripDirty = false;
          }

          const preserveServersView = !forceTools && (
            Boolean(state.editingServerInstructionServer) || isInteractingWithin(document.getElementById('tab-servers'))
          );
          if (preserveServersView) {
            captureEditingDraftFromDom();
            state.serversDirty = true;
          } else {
            renderServersView();
            state.serversDirty = false;
          }

          const preserveToolFilters = !forceTools && isInteractingWithin(document.querySelector('#tab-tools .tool-toolbar'));
          if (preserveToolFilters) {
            state.toolFiltersDirty = true;
          } else {
            renderToolFilters();
            state.toolFiltersDirty = false;
          }

          const preserveToolsView = !forceTools && (
            Boolean(state.editingToolKey) || isInteractingWithin(document.getElementById('tab-tools'))
          );
          if (preserveToolsView) {
            captureEditingDraftFromDom();
            state.toolsDirty = true;
          } else {
            renderToolsView();
            state.toolsDirty = false;
          }

          const preserveEffectiveView = !forceTools && isInteractingWithin(document.getElementById('tab-effective'));
          if (preserveEffectiveView) {
            state.effectiveDirty = true;
          } else {
            renderEffectiveView();
            state.effectiveDirty = false;
          }
        }
        flushDeferredRenders();
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
          if (state.editingServerInstructionServer === server) {
            state.editingServerInstructionServer = null;
            state.editingServerInstructionDraft = '';
            state.showServerInstructionDiff = false;
          }
          await refresh({ forceTools: true });
        });
        return;
      }
      if (action === 'edit-server-instruction') {
        state.editingServerInstructionServer = server;
        state.editingServerInstructionDraft = effectiveServerInstruction(server);
        state.showServerInstructionDiff = false;
        renderServersView();
        return;
      }
      if (action === 'cancel-server-instruction') {
        state.editingServerInstructionServer = null;
        state.editingServerInstructionDraft = '';
        state.showServerInstructionDiff = false;
        renderServersView();
        return;
      }
      if (action === 'toggle-server-instruction-diff') {
        state.showServerInstructionDiff = !state.showServerInstructionDiff;
        renderServersView();
        return;
      }
      if (action === 'save-server-instruction') {
        const textarea = document.getElementById('edit-server-instruction-text');
        const instruction = textarea ? textarea.value : state.editingServerInstructionDraft;
        if (!instruction || !instruction.trim()) {
          setError('save server instruction override failed: instruction cannot be empty');
          return;
        }
        if (instruction.trim().length > SERVER_INSTRUCTION_MAX_LENGTH) {
          setError('save server instruction override failed: instruction exceeds max length of ' + SERVER_INSTRUCTION_MAX_LENGTH + ' characters');
          return;
        }
        await runAction('save server instruction override', async function() {
          await postJson('/server-instructions', { server: server, instruction: instruction.trim() });
          state.editingServerInstructionServer = null;
          state.editingServerInstructionDraft = '';
          state.showServerInstructionDiff = false;
          await refresh({ forceTools: true });
        });
        return;
      }
      if (action === 'restore-server-instruction') {
        await runAction('restore server instruction override', async function() {
          await postJson('/server-instructions/remove', { server: server });
          state.editingServerInstructionServer = null;
          state.editingServerInstructionDraft = '';
          state.showServerInstructionDiff = false;
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
        const rawTarget = document.getElementById('server-url').value;
        const normalizedTarget = normalizeServerTarget(rawTarget);
        await postJson('/servers', {
          name: document.getElementById('server-name').value,
          url: normalizedTarget,
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
      if (target.id === 'edit-desc-text') {
        state.editingDraft = target.value || '';
        return;
      }
      if (target.id === 'edit-server-instruction-text') {
        state.editingServerInstructionDraft = target.value || '';
        const saveButton = document.querySelector('[data-action="save-server-instruction"]');
        if (saveButton) {
          saveButton.disabled = !state.editingServerInstructionDraft.trim();
        }
      }
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
      flushDeferredRenders();
    });
    document.body.addEventListener('focusout', function() {
      setTimeout(flushDeferredRenders, 0);
    });
    document.body.addEventListener('mouseup', function() {
      flushDeferredRenders();
    });
    document.body.addEventListener('keyup', function() {
      flushDeferredRenders();
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
    let upstream_server_instructions = state.upstream.connected_server_instructions().await;
    let mut effective_server_instructions = BTreeMap::new();
    let mut server_instruction_sources = BTreeMap::new();
    for server in &cfg.servers {
        let decision = cfg.resolve_server_instruction(&upstream_server_instructions, &server.name);
        if let Some(instruction) = decision.instruction {
            effective_server_instructions.insert(server.name.clone(), instruction);
        }
        server_instruction_sources
            .insert(server.name.clone(), decision.source.as_str().to_string());
    }
    Ok(Json(ServersResponse {
        servers: cfg.servers,
        server_tool_activation_modes: cfg.server_tool_activation_modes,
        tool_activation_overrides: cfg.tool_activation_overrides,
        server_tool_policy_modes: cfg.server_tool_policy_modes,
        server_instruction_overrides: cfg.server_instruction_overrides,
        upstream_server_instructions,
        effective_server_instructions,
        server_instruction_sources,
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
            map_config_mutation_error("add server", err)
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
            map_config_mutation_error("update exposure mode", err)
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
            map_config_mutation_error("update policy mode", err)
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
            map_config_mutation_error("update tool default", err)
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
            map_config_mutation_error("update server enabled state", err)
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
            map_config_mutation_error("set tool activation override", err)
        })?;
    state.upstream.invalidate_discovery_cache().await;

    Ok(Json(MutationResponse { status: "ok" }))
}

async fn set_server_instruction_override(
    State(state): State<AppState>,
    Json(request): Json<ServerInstructionOverrideRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    let req_server = request.server.clone();
    let req_instruction = request.instruction.clone();
    state
        .store
        .update_async(move |cfg| cfg.set_server_instruction_override(&req_server, &req_instruction))
        .await
        .map_err(|err| {
            error!(
                error = %err,
                server = %request.server,
                "failed to set server instruction override"
            );
            map_config_mutation_error("set server instruction override", err)
        })?;
    state.upstream.invalidate_discovery_cache().await;

    Ok(Json(MutationResponse { status: "ok" }))
}

async fn remove_server_instruction_override(
    State(state): State<AppState>,
    Json(request): Json<RemoveServerInstructionOverrideRequest>,
) -> Result<Json<MutationResponse>, ApiError> {
    let req_server = request.server.clone();
    let removed = state
        .store
        .update_async(move |cfg| Ok(cfg.remove_server_instruction_override(&req_server)))
        .await
        .map_err(|err| {
            error!(
                error = %err,
                server = %request.server,
                "failed to remove server instruction override"
            );
            ApiError::internal("failed to remove server instruction override")
        })?;

    if !removed {
        return Err(ApiError::bad_request(
            "server instruction override not found",
        ));
    }
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
            map_config_mutation_error("set tool description override", err)
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
            map_config_mutation_error("set tool policy override", err)
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
    let snapshot = build_tools_snapshot(&state, &cfg, &auth_headers).await?;
    Ok(Json(snapshot))
}

async fn effective(
    State(state): State<AppState>,
) -> Result<Json<EffectiveOutputResponse>, ApiError> {
    let cfg = load_config(&state).await?;
    let auth_headers = load_upstream_auth_headers(&state).await?;
    let tools_snapshot = build_tools_snapshot(&state, &cfg, &auth_headers).await?;
    let preview =
        super::mcp::effective_mcp_preview(&state.store, &state.upstream, state.exec_enabled)
            .await
            .map_err(|err| {
                error!(error = %err, "failed to build effective mcp output preview");
                ApiError::internal("failed to build effective output preview")
            })?;
    let auth_statuses = state.auth.list_statuses().await.map_err(|err| {
        error!(error = %err, "failed to load auth statuses for effective output");
        ApiError::internal("failed to load auth statuses")
    })?;
    let impacted_servers =
        derive_effective_server_impacts(&tools_snapshot.failures, &auth_statuses);

    Ok(Json(EffectiveOutputResponse {
        mcp_initialize: preview.initialize,
        mcp_tools_list: preview.tools_list,
        gambi_list_servers: preview.gambi_list_servers,
        gambi_list_upstream_tools: preview.gambi_list_upstream_tools,
        gambi_help: preview.gambi_help,
        tool_details: tools_snapshot.tool_details,
        discovery_failures: tools_snapshot.failures,
        impacted_servers,
        auth_statuses,
    }))
}

async fn build_tools_snapshot(
    state: &AppState,
    cfg: &AppConfig,
    auth_headers: &upstream::UpstreamAuthHeaders,
) -> Result<ToolsResponse, ApiError> {
    let enabled_servers = cfg.enabled_servers();
    let discovery = state
        .upstream
        .discover_tools_from_servers(&enabled_servers, auth_headers)
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
        let store = state.store.clone();
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
            match refresh_auth_for_server_if_possible(&store, &auth, &server_name).await {
                Ok(true) => {
                    info!(
                        server = %server_name,
                        "upstream reported auth failure; oauth refresh succeeded"
                    );
                    upstream.invalidate_discovery_cache().await;
                }
                Ok(false) => {}
                Err(err) => {
                    warn!(
                        error = %err,
                        server = %server_name,
                        "upstream reported auth failure and oauth refresh failed"
                    );
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

    Ok(ToolsResponse {
        tools,
        tool_details,
        failures,
    })
}

fn derive_effective_server_impacts(
    failures: &[ToolDiscoveryFailure],
    auth_statuses: &[AuthServerStatus],
) -> Vec<EffectiveServerImpact> {
    let mut failure_by_server = BTreeMap::<String, String>::new();
    for failure in failures {
        failure_by_server
            .entry(failure.server.clone())
            .or_insert_with(|| failure.error.clone());
    }

    let mut rows = auth_statuses
        .iter()
        .map(|status| EffectiveServerImpact {
            server: status.server.clone(),
            discovery_error: failure_by_server.remove(&status.server),
            oauth_configured: status.oauth_configured,
            has_token: status.has_token,
            degraded: status.degraded,
            auth_error: status.last_error.clone(),
        })
        .collect::<Vec<_>>();

    for (server, discovery_error) in failure_by_server {
        rows.push(EffectiveServerImpact {
            server,
            discovery_error: Some(discovery_error),
            oauth_configured: false,
            has_token: false,
            degraded: false,
            auth_error: None,
        });
    }

    rows.sort_by(|a, b| a.server.cmp(&b.server));
    rows
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

async fn refresh_auth_for_server_if_possible(
    store: &ConfigStore,
    auth: &AuthManager,
    server_name: &str,
) -> Result<bool> {
    let tokens: TokenState = store.load_tokens_async().await?;
    let Some(token) = tokens.oauth_tokens.get(server_name) else {
        return Ok(false);
    };
    if token.refresh_token.is_none() {
        return Ok(false);
    }

    let response = auth.refresh(server_name).await?;
    info!(
        server = %server_name,
        refreshed = response.refreshed,
        expires_at_epoch_seconds = ?response.expires_at_epoch_seconds,
        "oauth refresh succeeded after upstream auth failure"
    );
    Ok(true)
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
