use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{Rng, distributions::Alphanumeric};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use url::Url;

use crate::config::{ConfigStore, OAuthConfig};

const PENDING_AUTH_TTL: Duration = Duration::from_secs(600);
const AUTO_REFRESH_LOOP_INTERVAL: Duration = Duration::from_secs(15);
const AUTO_REFRESH_LEAD_SECONDS: u64 = 120;
const REFRESH_BACKOFF_BASE_SECONDS: u64 = 5;
const REFRESH_BACKOFF_MAX_SECONDS: u64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenState {
    #[serde(default)]
    pub oauth_tokens: BTreeMap<String, OAuthToken>,
    #[serde(default)]
    pub oauth_health: BTreeMap<String, OAuthServerHealth>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthToken {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_epoch_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OAuthServerHealth {
    #[serde(default)]
    pub degraded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_epoch_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry_after_epoch_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthServerStatus {
    pub server: String,
    pub oauth_configured: bool,
    pub has_token: bool,
    pub expires_at_epoch_seconds: Option<u64>,
    pub degraded: bool,
    pub last_error: Option<String>,
    pub next_retry_after_epoch_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthStartResponse {
    pub server: String,
    pub auth_url: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthRefreshResponse {
    pub server: String,
    pub refreshed: bool,
    pub expires_at_epoch_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthCallbackResponse {
    pub server: String,
    pub expires_at_epoch_seconds: Option<u64>,
}

#[derive(Debug, Clone)]
struct PendingAuthRequest {
    server_name: String,
    code_verifier: String,
    redirect_uri: String,
    created_at: Instant,
}

#[derive(Debug, Deserialize)]
struct OAuthTokenExchangeResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthDiscoveryDocument {
    #[serde(default)]
    authorization_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint: Option<String>,
}

#[derive(Debug, Clone)]
struct ResolvedOAuthConfig {
    authorize_url: String,
    token_url: String,
    client_id: String,
    scopes: Vec<String>,
}

#[derive(Debug, Clone)]
struct RefreshRetryState {
    consecutive_failures: u32,
    next_attempt_at: Instant,
}

#[derive(Clone)]
pub struct AuthManager {
    store: ConfigStore,
    pending: Arc<Mutex<HashMap<String, PendingAuthRequest>>>,
    retry_state: Arc<Mutex<HashMap<String, RefreshRetryState>>>,
    http: Client,
}

impl AuthManager {
    pub fn new(store: ConfigStore) -> Self {
        let http = match Client::builder().timeout(Duration::from_secs(10)).build() {
            Ok(client) => client,
            Err(err) => {
                warn!(error = %err, "failed to build configured HTTP client; falling back to default reqwest client");
                Client::new()
            }
        };

        Self {
            store,
            pending: Arc::new(Mutex::new(HashMap::new())),
            retry_state: Arc::new(Mutex::new(HashMap::new())),
            http,
        }
    }

    pub async fn run_refresh_loop(&self, shutdown: CancellationToken) {
        let mut interval = tokio::time::interval(AUTO_REFRESH_LOOP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(err) = self.refresh_due_tokens_once().await {
                        warn!(error = %err, "oauth auto-refresh sweep failed");
                    }
                }
            }
        }
    }

    pub async fn list_statuses(&self) -> Result<Vec<AuthServerStatus>> {
        let cfg = self.store.load_async().await?;
        let tokens: TokenState = self.store.load_tokens_async().await?;

        let mut statuses = Vec::with_capacity(cfg.servers.len());
        for server in cfg.servers {
            let token = tokens.oauth_tokens.get(&server.name);
            let health = tokens.oauth_health.get(&server.name);
            statuses.push(AuthServerStatus {
                server: server.name,
                oauth_configured: server.oauth.is_some(),
                has_token: token.is_some(),
                expires_at_epoch_seconds: token.and_then(|value| value.expires_at_epoch_seconds),
                degraded: health.map(|state| state.degraded).unwrap_or(false),
                last_error: health.and_then(|state| state.last_error.clone()),
                next_retry_after_epoch_seconds: health
                    .and_then(|state| state.next_retry_after_epoch_seconds),
            });
        }
        statuses.sort_by(|a, b| a.server.cmp(&b.server));
        Ok(statuses)
    }

    pub async fn start(
        &self,
        server_name: &str,
        admin_base_url: &str,
    ) -> Result<AuthStartResponse> {
        let cfg = self.store.load_async().await?;
        let oauth = oauth_for_server(&cfg, server_name)?;
        let resolved = self.resolve_oauth_config(oauth).await;

        let code_verifier = random_url_safe(64);
        let state = random_url_safe(48);
        let redirect_uri = format!(
            "{}/auth/callback",
            admin_base_url.trim_end_matches('/').trim_end_matches(' ')
        );

        let code_challenge = {
            let hash = Sha256::digest(code_verifier.as_bytes());
            URL_SAFE_NO_PAD.encode(hash)
        };

        let mut authorize_url = Url::parse(&resolved.authorize_url)
            .with_context(|| format!("invalid authorize URL '{}'", resolved.authorize_url))?;
        {
            let mut query = authorize_url.query_pairs_mut();
            query.append_pair("response_type", "code");
            query.append_pair("client_id", &resolved.client_id);
            query.append_pair("redirect_uri", &redirect_uri);
            query.append_pair("state", &state);
            query.append_pair("code_challenge", &code_challenge);
            query.append_pair("code_challenge_method", "S256");
            if !resolved.scopes.is_empty() {
                query.append_pair("scope", &resolved.scopes.join(" "));
            }
        }

        {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| anyhow::anyhow!("failed to lock pending oauth state"))?;
            pending.retain(|_, item| item.created_at.elapsed() <= PENDING_AUTH_TTL);
            pending.insert(
                state.clone(),
                PendingAuthRequest {
                    server_name: server_name.to_string(),
                    code_verifier,
                    redirect_uri,
                    created_at: Instant::now(),
                },
            );
        }

        Ok(AuthStartResponse {
            server: server_name.to_string(),
            auth_url: authorize_url.to_string(),
            state,
        })
    }

    pub async fn callback(&self, state: &str, code: &str) -> Result<AuthCallbackResponse> {
        if code.trim().is_empty() {
            bail!("authorization code is required");
        }

        let pending = {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| anyhow::anyhow!("failed to lock pending oauth state"))?;
            let Some(value) = pending.remove(state) else {
                bail!("invalid or expired oauth state");
            };
            value
        };

        if pending.created_at.elapsed() > PENDING_AUTH_TTL {
            bail!("oauth state expired; restart authorization");
        }

        let cfg = self.store.load_async().await?;
        let oauth = oauth_for_server(&cfg, &pending.server_name)?;
        let resolved = self.resolve_oauth_config(oauth).await;

        let token_response = exchange_authorization_code(
            &self.http,
            &resolved,
            &pending.redirect_uri,
            code,
            &pending.code_verifier,
        )
        .await?;

        let token = build_token(token_response, &resolved.scopes);
        let server_name = pending.server_name.clone();
        let server_name_for_store = server_name.clone();
        let expires_at = token.expires_at_epoch_seconds;

        self.store
            .update_tokens_async::<TokenState, _, _>(move |tokens| {
                tokens
                    .oauth_tokens
                    .insert(server_name_for_store.clone(), token.clone());
                set_server_healthy(tokens, &server_name_for_store);
                Ok(())
            })
            .await
            .context("failed to persist oauth token")?;
        self.clear_retry_state(&server_name);

        Ok(AuthCallbackResponse {
            server: pending.server_name,
            expires_at_epoch_seconds: expires_at,
        })
    }

    pub async fn refresh(&self, server_name: &str) -> Result<AuthRefreshResponse> {
        match self.refresh_once(server_name).await {
            Ok(response) => {
                self.clear_retry_state(server_name);
                Ok(response)
            }
            Err(err) => {
                let _ = self
                    .mark_server_degraded(server_name, err.to_string(), None)
                    .await;
                Err(err)
            }
        }
    }

    pub async fn prune_orphaned_state(&self) -> Result<usize> {
        let cfg = self.store.load_async().await?;
        let server_names = cfg
            .servers
            .into_iter()
            .map(|server| server.name)
            .collect::<HashSet<_>>();

        let server_names_for_tokens = server_names.clone();
        let removed = self
            .store
            .update_tokens_async::<TokenState, _, _>(move |tokens| {
                Ok(prune_token_state_for_server_names(
                    tokens,
                    &server_names_for_tokens,
                ))
            })
            .await?;

        if let Ok(mut pending) = self.pending.lock() {
            pending.retain(|_, state| server_names.contains(&state.server_name));
        }
        if let Ok(mut retry_state) = self.retry_state.lock() {
            retry_state.retain(|server, _| server_names.contains(server));
        }

        Ok(removed)
    }

    async fn refresh_due_tokens_once(&self) -> Result<()> {
        let cfg = self.store.load_async().await?;
        let tokens: TokenState = self.store.load_tokens_async().await?;
        let now = now_epoch_seconds();

        for server in cfg.servers.into_iter().filter(|item| item.oauth.is_some()) {
            let Some(token) = tokens.oauth_tokens.get(&server.name) else {
                continue;
            };

            if token.refresh_token.is_none() {
                continue;
            }

            let Some(expires_at) = token.expires_at_epoch_seconds else {
                continue;
            };

            if expires_at > now.saturating_add(AUTO_REFRESH_LEAD_SECONDS) {
                continue;
            }

            if !self.retry_allows_attempt(&server.name) {
                continue;
            }

            match self.refresh_once(&server.name).await {
                Ok(_) => self.clear_retry_state(&server.name),
                Err(err) => {
                    let (attempt, next_retry_epoch) = self.record_retry_failure(&server.name);
                    let _ = self
                        .mark_server_degraded(
                            &server.name,
                            format!("auto-refresh failed: {err}"),
                            Some(next_retry_epoch),
                        )
                        .await;
                    warn!(
                        server = %server.name,
                        attempt,
                        next_retry_after_epoch_seconds = next_retry_epoch,
                        error = %err,
                        "oauth auto-refresh attempt failed"
                    );
                }
            }
        }

        Ok(())
    }

    async fn refresh_once(&self, server_name: &str) -> Result<AuthRefreshResponse> {
        let cfg = self.store.load_async().await?;
        let oauth = oauth_for_server(&cfg, server_name)?;
        let resolved = self.resolve_oauth_config(oauth).await;
        let current: TokenState = self.store.load_tokens_async().await?;

        let Some(existing) = current.oauth_tokens.get(server_name) else {
            bail!("no oauth token found for server '{server_name}'");
        };
        let Some(refresh_token) = existing.refresh_token.clone() else {
            bail!("server '{server_name}' token does not include refresh_token");
        };

        let response = self
            .http
            .post(&resolved.token_url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.as_str()),
                ("client_id", resolved.client_id.as_str()),
            ])
            .send()
            .await
            .context("oauth refresh request failed")?
            .error_for_status()
            .context("oauth refresh returned error response")?
            .json::<OAuthTokenExchangeResponse>()
            .await
            .context("oauth refresh response JSON is invalid")?;

        let mut updated = build_token(response, &resolved.scopes);
        if updated.refresh_token.is_none() {
            updated.refresh_token = Some(refresh_token);
        }
        let expires_at = updated.expires_at_epoch_seconds;

        let server_name_owned = server_name.to_string();
        self.store
            .update_tokens_async::<TokenState, _, _>(move |tokens| {
                tokens
                    .oauth_tokens
                    .insert(server_name_owned.clone(), updated.clone());
                set_server_healthy(tokens, &server_name_owned);
                Ok(())
            })
            .await
            .context("failed to persist refreshed oauth token")?;

        Ok(AuthRefreshResponse {
            server: server_name.to_string(),
            refreshed: true,
            expires_at_epoch_seconds: expires_at,
        })
    }

    async fn resolve_oauth_config(&self, oauth: &OAuthConfig) -> ResolvedOAuthConfig {
        let mut authorize_url = oauth.authorize_url.clone();
        let mut token_url = oauth.token_url.clone();

        if let Some(discovery_url) = oauth.discovery_url.as_deref() {
            match self.fetch_discovery_document(discovery_url).await {
                Ok(doc) => {
                    if let Some(value) = sanitize_discovered_url(
                        doc.authorization_endpoint.as_deref(),
                        "authorization_endpoint",
                    ) {
                        authorize_url = value;
                    }
                    if let Some(value) =
                        sanitize_discovered_url(doc.token_endpoint.as_deref(), "token_endpoint")
                    {
                        token_url = value;
                    }
                }
                Err(err) => {
                    warn!(
                        discovery_url,
                        error = %err,
                        "oauth discovery failed; using configured endpoint fallback"
                    );
                }
            }
        }

        ResolvedOAuthConfig {
            authorize_url,
            token_url,
            client_id: oauth.client_id.clone(),
            scopes: oauth.scopes.clone(),
        }
    }

    async fn fetch_discovery_document(
        &self,
        discovery_url: &str,
    ) -> Result<OAuthDiscoveryDocument> {
        self.http
            .get(discovery_url)
            .send()
            .await
            .with_context(|| format!("oauth discovery request failed for '{discovery_url}'"))?
            .error_for_status()
            .with_context(|| {
                format!("oauth discovery response returned error for '{discovery_url}'")
            })?
            .json::<OAuthDiscoveryDocument>()
            .await
            .with_context(|| {
                format!("oauth discovery response is invalid JSON for '{discovery_url}'")
            })
    }

    async fn mark_server_degraded(
        &self,
        server_name: &str,
        error_message: String,
        next_retry_after_epoch_seconds: Option<u64>,
    ) -> Result<()> {
        let server_name = server_name.to_string();
        self.store
            .update_tokens_async::<TokenState, _, _>(move |tokens| {
                let state = tokens.oauth_health.entry(server_name.clone()).or_default();
                state.degraded = true;
                state.last_error = Some(error_message.clone());
                state.updated_at_epoch_seconds = Some(now_epoch_seconds());
                state.next_retry_after_epoch_seconds = next_retry_after_epoch_seconds;
                Ok(())
            })
            .await
    }

    fn retry_allows_attempt(&self, server_name: &str) -> bool {
        let Ok(state) = self.retry_state.lock() else {
            return true;
        };

        match state.get(server_name) {
            Some(value) => Instant::now() >= value.next_attempt_at,
            None => true,
        }
    }

    fn clear_retry_state(&self, server_name: &str) {
        if let Ok(mut state) = self.retry_state.lock() {
            state.remove(server_name);
        }
    }

    fn record_retry_failure(&self, server_name: &str) -> (u32, u64) {
        let mut state = match self.retry_state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!("retry state lock was poisoned; continuing with recovered state");
                poisoned.into_inner()
            }
        };

        let entry = state
            .entry(server_name.to_string())
            .or_insert(RefreshRetryState {
                consecutive_failures: 0,
                next_attempt_at: Instant::now(),
            });

        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        let exponent = entry.consecutive_failures.saturating_sub(1).min(8);
        let base_delay = REFRESH_BACKOFF_BASE_SECONDS.saturating_mul(2u64.saturating_pow(exponent));
        let jitter = rand::thread_rng().gen_range(0..=3);
        let delay = (base_delay.saturating_add(jitter)).min(REFRESH_BACKOFF_MAX_SECONDS);

        entry.next_attempt_at = Instant::now() + Duration::from_secs(delay);

        (
            entry.consecutive_failures,
            now_epoch_seconds().saturating_add(delay),
        )
    }
}

fn oauth_for_server<'a>(
    cfg: &'a crate::config::AppConfig,
    server_name: &str,
) -> Result<&'a OAuthConfig> {
    let Some(server) = cfg.servers.iter().find(|item| item.name == server_name) else {
        bail!("unknown server '{server_name}'");
    };
    let Some(oauth) = server.oauth.as_ref() else {
        bail!("server '{server_name}' does not have oauth configured");
    };
    Ok(oauth)
}

async fn exchange_authorization_code(
    http: &Client,
    oauth: &ResolvedOAuthConfig,
    redirect_uri: &str,
    code: &str,
    code_verifier: &str,
) -> Result<OAuthTokenExchangeResponse> {
    http.post(&oauth.token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", oauth.client_id.as_str()),
            ("code_verifier", code_verifier),
        ])
        .send()
        .await
        .context("oauth token exchange request failed")?
        .error_for_status()
        .context("oauth token exchange returned error response")?
        .json::<OAuthTokenExchangeResponse>()
        .await
        .context("oauth token exchange response JSON is invalid")
}

fn build_token(response: OAuthTokenExchangeResponse, default_scopes: &[String]) -> OAuthToken {
    let expires_at_epoch_seconds = response
        .expires_in
        .map(|seconds| now_epoch_seconds().saturating_add(seconds));
    let scopes = response
        .scope
        .as_deref()
        .map(|scope| {
            scope
                .split_whitespace()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| default_scopes.to_vec());

    OAuthToken {
        access_token: response.access_token,
        refresh_token: response.refresh_token,
        token_type: response.token_type,
        expires_at_epoch_seconds,
        scopes,
    }
}

fn sanitize_discovered_url(url: Option<&str>, field_name: &str) -> Option<String> {
    let url = url?.trim();
    if url.is_empty() {
        warn!(
            field_name,
            "oauth discovery endpoint is empty; ignoring discovered value"
        );
        return None;
    }

    match Url::parse(url) {
        Ok(parsed)
            if (parsed.scheme() == "http" || parsed.scheme() == "https")
                && parsed.host_str().is_some() =>
        {
            Some(url.to_string())
        }
        Ok(_) => {
            warn!(
                field_name,
                value = url,
                "oauth discovery endpoint has invalid scheme/host"
            );
            None
        }
        Err(err) => {
            warn!(field_name, value = url, error = %err, "oauth discovery endpoint is not a valid URL");
            None
        }
    }
}

fn set_server_healthy(tokens: &mut TokenState, server_name: &str) {
    let state = tokens
        .oauth_health
        .entry(server_name.to_string())
        .or_default();
    state.degraded = false;
    state.last_error = None;
    state.updated_at_epoch_seconds = Some(now_epoch_seconds());
    state.next_retry_after_epoch_seconds = None;
}

pub fn prune_token_state_for_server_names(
    tokens: &mut TokenState,
    server_names: &HashSet<String>,
) -> usize {
    let before_oauth_tokens = tokens.oauth_tokens.len();
    tokens
        .oauth_tokens
        .retain(|server, _| server_names.contains(server));
    let removed_oauth_tokens = before_oauth_tokens.saturating_sub(tokens.oauth_tokens.len());

    let before_oauth_health = tokens.oauth_health.len();
    tokens
        .oauth_health
        .retain(|server, _| server_names.contains(server));
    let removed_oauth_health = before_oauth_health.saturating_sub(tokens.oauth_health.len());

    removed_oauth_tokens.saturating_add(removed_oauth_health)
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn random_url_safe(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use axum::{
        Form, Json, Router,
        routing::{get, post},
    };
    use serde_json::json;
    use std::collections::HashMap;

    use super::{
        AuthManager, OAuthServerHealth, OAuthToken, TokenState, prune_token_state_for_server_names,
    };
    use crate::config::{AppConfig, ConfigStore, OAuthConfig, ServerConfig};

    #[tokio::test]
    async fn start_builds_pkce_auth_url() {
        let (_temp, store) = configured_store(None);
        let manager = AuthManager::new(store);

        let started = manager
            .start("port", "http://127.0.0.1:3333")
            .await
            .expect("start should succeed");

        assert_eq!(started.server, "port");
        assert!(!started.state.is_empty());
        assert!(started.auth_url.contains("code_challenge="));
        assert!(started.auth_url.contains("code_challenge_method=S256"));
        assert!(
            started
                .auth_url
                .contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A3333%2Fauth%2Fcallback")
        );
    }

    #[tokio::test]
    async fn start_uses_discovery_authorization_endpoint_when_available() {
        let (discovery_url, handle) = spawn_discovery_server(
            "https://issuer.example/authorize-discovered",
            "https://issuer.example/token-discovered",
        )
        .await;

        let oauth = OAuthConfig {
            discovery_url: Some(discovery_url),
            authorize_url: "https://fallback.example/authorize".to_string(),
            token_url: "https://fallback.example/token".to_string(),
            client_id: "client-id".to_string(),
            scopes: vec!["read".to_string()],
        };

        let (_temp, store) = configured_store(Some(oauth));
        let manager = AuthManager::new(store);
        let started = manager
            .start("port", "http://127.0.0.1:3333")
            .await
            .expect("start should succeed");

        handle.abort();
        assert!(
            started
                .auth_url
                .starts_with("https://issuer.example/authorize-discovered")
        );
    }

    #[tokio::test]
    async fn start_falls_back_to_configured_endpoint_when_discovery_fails() {
        let oauth = OAuthConfig {
            discovery_url: Some("http://127.0.0.1:9/.well-known/openid-configuration".to_string()),
            authorize_url: "https://fallback.example/authorize".to_string(),
            token_url: "https://fallback.example/token".to_string(),
            client_id: "client-id".to_string(),
            scopes: vec!["read".to_string()],
        };

        let (_temp, store) = configured_store(Some(oauth));
        let manager = AuthManager::new(store);
        let started = manager
            .start("port", "http://127.0.0.1:3333")
            .await
            .expect("start should succeed");

        assert!(
            started
                .auth_url
                .starts_with("https://fallback.example/authorize")
        );
    }

    #[tokio::test]
    async fn list_statuses_reflects_oauth_configuration() {
        let (_temp, store) = configured_store(None);
        let manager = AuthManager::new(store);

        let statuses = manager.list_statuses().await.expect("status should load");
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].server, "port");
        assert!(statuses[0].oauth_configured);
        assert!(!statuses[0].has_token);
        assert!(!statuses[0].degraded);
        assert!(statuses[0].last_error.is_none());
    }

    #[tokio::test]
    async fn callback_rejects_unknown_state() {
        let (_temp, store) = configured_store(None);
        let manager = AuthManager::new(store);
        let err = manager
            .callback("missing-state", "code")
            .await
            .expect_err("unknown state must fail");
        assert!(err.to_string().contains("invalid or expired oauth state"));
    }

    #[tokio::test]
    async fn refresh_rejects_missing_tokens_and_marks_degraded() {
        let (_temp, store) = configured_store(None);
        let manager = AuthManager::new(store);
        let err = manager
            .refresh("port")
            .await
            .expect_err("refresh requires stored token");
        assert!(err.to_string().contains("no oauth token found"));

        let statuses = manager.list_statuses().await.expect("status should load");
        assert!(statuses[0].degraded);
        assert!(
            statuses[0]
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("no oauth token found")
        );
    }

    #[tokio::test]
    async fn callback_and_refresh_rotate_tokens_with_simulated_provider() {
        let (token_url, handle) = spawn_token_server().await;

        let oauth = OAuthConfig {
            discovery_url: None,
            authorize_url: "https://example.com/oauth/authorize".to_string(),
            token_url,
            client_id: "client-id".to_string(),
            scopes: vec!["read".to_string(), "write".to_string()],
        };

        let (_temp, store) = configured_store(Some(oauth));
        let manager = AuthManager::new(store.clone());

        let started = manager
            .start("port", "http://127.0.0.1:3333")
            .await
            .expect("start should succeed");

        let callback = manager
            .callback(&started.state, "auth-code")
            .await
            .expect("callback should exchange token");
        assert!(callback.expires_at_epoch_seconds.is_some());

        let tokens_after_callback: TokenState =
            store.load_tokens().expect("token state should load");
        let initial = tokens_after_callback
            .oauth_tokens
            .get("port")
            .expect("token must be persisted after callback");
        assert_eq!(initial.access_token, "access-initial");
        assert_eq!(
            initial.refresh_token.as_deref(),
            Some("refresh-initial-rotatable")
        );

        let refreshed = manager
            .refresh("port")
            .await
            .expect("refresh should succeed");
        assert!(refreshed.refreshed);
        assert!(refreshed.expires_at_epoch_seconds.is_some());

        let tokens_after_refresh: TokenState =
            store.load_tokens().expect("token state should load");
        let updated = tokens_after_refresh
            .oauth_tokens
            .get("port")
            .expect("token must remain persisted");
        assert_eq!(updated.access_token, "access-refreshed");
        assert_eq!(updated.refresh_token.as_deref(), Some("refresh-rotated"));

        let statuses = manager.list_statuses().await.expect("status should load");
        assert_eq!(statuses.len(), 1);
        assert!(!statuses[0].degraded);
        assert!(statuses[0].last_error.is_none());

        handle.abort();
    }

    #[test]
    fn token_state_defaults_from_empty_json() {
        let parsed: TokenState = serde_json::from_str("{}").expect("empty json should parse");
        assert!(parsed.oauth_tokens.is_empty());
        assert!(parsed.oauth_health.is_empty());
    }

    #[test]
    fn prune_token_state_removes_unknown_servers() {
        let mut tokens = TokenState::default();
        tokens.oauth_tokens.insert(
            "keep".to_string(),
            OAuthToken {
                access_token: "token-keep".to_string(),
                refresh_token: None,
                token_type: Some("Bearer".to_string()),
                expires_at_epoch_seconds: None,
                scopes: vec![],
            },
        );
        tokens.oauth_tokens.insert(
            "remove".to_string(),
            OAuthToken {
                access_token: "token-remove".to_string(),
                refresh_token: None,
                token_type: Some("Bearer".to_string()),
                expires_at_epoch_seconds: None,
                scopes: vec![],
            },
        );
        tokens.oauth_health.insert(
            "remove".to_string(),
            OAuthServerHealth {
                degraded: true,
                last_error: Some("boom".to_string()),
                updated_at_epoch_seconds: Some(1),
                next_retry_after_epoch_seconds: Some(2),
            },
        );

        let mut allowed = std::collections::HashSet::new();
        allowed.insert("keep".to_string());
        let removed = prune_token_state_for_server_names(&mut tokens, &allowed);

        assert_eq!(removed, 2);
        assert!(tokens.oauth_tokens.contains_key("keep"));
        assert!(!tokens.oauth_tokens.contains_key("remove"));
        assert!(!tokens.oauth_health.contains_key("remove"));
    }

    fn configured_store(oauth_override: Option<OAuthConfig>) -> (tempfile::TempDir, ConfigStore) {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let store = ConfigStore::with_base_dir(temp_dir.path().join("gambi"));

        let oauth = oauth_override.unwrap_or_else(|| OAuthConfig {
            discovery_url: None,
            authorize_url: "https://example.com/oauth/authorize".to_string(),
            token_url: "https://example.com/oauth/token".to_string(),
            client_id: "client-id".to_string(),
            scopes: vec!["read".to_string()],
        });

        let cfg = AppConfig {
            servers: vec![ServerConfig {
                name: "port".to_string(),
                url: "https://example.com/mcp".to_string(),
                oauth: Some(oauth),
            }],
            tool_description_overrides: std::collections::BTreeMap::new(),
        };
        store.replace(cfg).expect("config should persist");

        (temp_dir, store)
    }

    async fn spawn_discovery_server(
        authorize_url: &'static str,
        token_url: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/.well-known/openid-configuration",
            get(move || async move {
                Json(json!({
                    "authorization_endpoint": authorize_url,
                    "token_endpoint": token_url
                }))
            }),
        );

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind discovery listener");
        let addr = listener.local_addr().expect("discovery listener addr");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (
            format!(
                "http://{addr}/.well-known/openid-configuration",
                addr = addr
            ),
            handle,
        )
    }

    async fn spawn_token_server() -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/token",
            post(|Form(form): Form<HashMap<String, String>>| async move {
                let grant_type = form.get("grant_type").cloned().unwrap_or_default();
                match grant_type.as_str() {
                    "authorization_code" => Json(json!({
                        "access_token": "access-initial",
                        "refresh_token": "refresh-initial-rotatable",
                        "token_type": "Bearer",
                        "expires_in": 1800,
                        "scope": "read write"
                    })),
                    "refresh_token" => {
                        let presented = form.get("refresh_token").cloned().unwrap_or_default();
                        if presented != "refresh-initial-rotatable" {
                            return Json(json!({
                                "error": "invalid_grant"
                            }));
                        }

                        Json(json!({
                            "access_token": "access-refreshed",
                            "refresh_token": "refresh-rotated",
                            "token_type": "Bearer",
                            "expires_in": 3600,
                            "scope": "read write"
                        }))
                    }
                    _ => Json(json!({
                        "error": "unsupported_grant_type"
                    })),
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind token listener");
        let addr = listener.local_addr().expect("token listener addr");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{addr}/token"), handle)
    }
}
