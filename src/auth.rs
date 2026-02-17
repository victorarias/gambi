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
    #[serde(default)]
    pub registered_clients: BTreeMap<String, RegisteredClient>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredClient {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_client_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_access_token: Option<String>,
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

#[derive(Debug, Serialize)]
struct DynamicClientRegistrationRequest {
    redirect_uris: Vec<String>,
    grant_types: Vec<String>,
    response_types: Vec<String>,
    token_endpoint_auth_method: String,
    client_name: String,
}

#[derive(Debug, Deserialize)]
struct DynamicClientRegistrationResponse {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    registration_client_uri: Option<String>,
    #[serde(default)]
    registration_access_token: Option<String>,
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
    #[serde(default)]
    registration_endpoint: Option<String>,
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
            let has_registered_client = tokens.registered_clients.contains_key(&server.name);
            let can_discover_oauth = mcp_oauth_discovery_url_for_server_url(&server.url).is_some();
            statuses.push(AuthServerStatus {
                server: server.name,
                oauth_configured: server.oauth.is_some()
                    || has_registered_client
                    || can_discover_oauth,
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
        let code_verifier = random_url_safe(64);
        let state = random_url_safe(48);
        let redirect_uri = format!(
            "{}/auth/callback",
            admin_base_url.trim_end_matches('/').trim_end_matches(' ')
        );

        let server = server_for_name(&cfg, server_name)?;
        let resolved = self
            .resolve_oauth_config(
                server_name,
                &server.url,
                server.oauth.as_ref(),
                Some(&redirect_uri),
            )
            .await?;

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

    /// Best-effort bootstrap: discover OAuth metadata and dynamically register a client if needed.
    /// Intended for calling when an upstream HTTP server reports "auth required" before the user
    /// explicitly starts the OAuth flow.
    pub async fn ensure_registered_client(
        &self,
        server_name: &str,
        admin_base_url: &str,
    ) -> Result<()> {
        let cfg = self.store.load_async().await?;
        let server = server_for_name(&cfg, server_name)?;
        let redirect_uri = format!(
            "{}/auth/callback",
            admin_base_url.trim_end_matches('/').trim_end_matches(' ')
        );
        let _ = self
            .resolve_oauth_config(
                server_name,
                &server.url,
                server.oauth.as_ref(),
                Some(&redirect_uri),
            )
            .await?;
        Ok(())
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
        let server = server_for_name(&cfg, &pending.server_name)?;
        let resolved = self
            .resolve_oauth_config(
                &pending.server_name,
                &server.url,
                server.oauth.as_ref(),
                Some(&pending.redirect_uri),
            )
            .await?;

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

    pub async fn mark_servers_healthy(&self, server_names: &[String]) -> Result<usize> {
        if server_names.is_empty() {
            return Ok(0);
        }

        let server_set = server_names.iter().cloned().collect::<HashSet<_>>();
        let cleared = self
            .store
            .update_tokens_async::<TokenState, _, _>(move |tokens| {
                let mut changed = 0usize;
                for (server_name, state) in &mut tokens.oauth_health {
                    if !server_set.contains(server_name) {
                        continue;
                    }
                    if state.degraded
                        || state.last_error.is_some()
                        || state.next_retry_after_epoch_seconds.is_some()
                    {
                        state.degraded = false;
                        state.last_error = None;
                        state.updated_at_epoch_seconds = Some(now_epoch_seconds());
                        state.next_retry_after_epoch_seconds = None;
                        changed = changed.saturating_add(1);
                    }
                }
                Ok(changed)
            })
            .await?;

        for server_name in server_names {
            self.clear_retry_state(server_name);
        }

        Ok(cleared)
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

        for server in cfg
            .servers
            .into_iter()
            .filter(|item| item.enabled && item.oauth.is_some())
        {
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
        let server = server_for_name(&cfg, server_name)?;
        let resolved = self
            .resolve_oauth_config(server_name, &server.url, server.oauth.as_ref(), None)
            .await?;
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

    async fn resolve_oauth_config(
        &self,
        server_name: &str,
        server_url: &str,
        oauth: Option<&OAuthConfig>,
        redirect_uri: Option<&str>,
    ) -> Result<ResolvedOAuthConfig> {
        let mut authorize_url = oauth
            .map(|value| value.authorize_url.clone())
            .unwrap_or_default();
        let mut token_url = oauth
            .map(|value| value.token_url.clone())
            .unwrap_or_default();
        let scopes = oauth.map(|value| value.scopes.clone()).unwrap_or_default();
        let configured_client_id = oauth.and_then(|value| value.client_id.clone());

        let configured_discovery_url = oauth.and_then(|value| value.discovery_url.clone());
        let derived_discovery_url =
            configured_discovery_url.or_else(|| mcp_oauth_discovery_url_for_server_url(server_url));

        let discovered = match derived_discovery_url.as_deref() {
            Some(discovery_url) => match self.fetch_discovery_document(discovery_url).await {
                Ok(doc) => Some((discovery_url.to_string(), doc)),
                Err(err) => {
                    if oauth.is_some() {
                        warn!(
                            discovery_url,
                            error = %err,
                            "oauth discovery failed; using configured endpoint fallback"
                        );
                        None
                    } else {
                        return Err(err).with_context(|| {
                            format!(
                                "oauth discovery failed for '{server_name}' (no configured oauth endpoints to fall back to)"
                            )
                        });
                    }
                }
            },
            None => {
                if oauth.is_some() {
                    None
                } else {
                    bail!(
                        "oauth discovery is required for server '{server_name}', but server url '{}' cannot be converted to an origin",
                        server_url
                    );
                }
            }
        };

        let mut registration_endpoint: Option<String> = None;
        if let Some((_discovery_url, doc)) = discovered.as_ref() {
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
            registration_endpoint = sanitize_discovered_url(
                doc.registration_endpoint.as_deref(),
                "registration_endpoint",
            );
        }

        if authorize_url.trim().is_empty() {
            bail!("oauth authorize_url is not available for server '{server_name}'");
        }
        if token_url.trim().is_empty() {
            bail!("oauth token_url is not available for server '{server_name}'");
        }

        let tokens: TokenState = self.store.load_tokens_async().await?;
        let persisted_client_id = tokens
            .registered_clients
            .get(server_name)
            .map(|value| value.client_id.clone());

        let mut client_id = configured_client_id.or(persisted_client_id);

        if client_id.is_none() {
            let Some(registration_endpoint) = registration_endpoint else {
                bail!(
                    "oauth client_id is not configured for server '{server_name}', and dynamic client registration is not available"
                );
            };
            let redirect_uri = redirect_uri.ok_or_else(|| {
                anyhow::anyhow!(
                    "oauth client_id is not configured for server '{server_name}', and dynamic registration requires a redirect_uri"
                )
            })?;

            let registered = self
                .dynamic_register_client(&registration_endpoint, redirect_uri)
                .await
                .with_context(|| {
                    format!(
                        "dynamic client registration failed for server '{server_name}' at '{}'",
                        registration_endpoint
                    )
                })?;

            let client_id_value = registered.client_id.clone();
            let server_name_owned = server_name.to_string();
            self.store
                .update_tokens_async::<TokenState, _, _>(move |tokens| {
                    tokens
                        .registered_clients
                        .insert(server_name_owned.clone(), registered.clone());
                    Ok(())
                })
                .await
                .context("failed to persist dynamically registered oauth client")?;

            client_id = Some(client_id_value);
        }

        let client_id = client_id.ok_or_else(|| {
            anyhow::anyhow!("oauth client_id could not be resolved for server '{server_name}'")
        })?;

        Ok(ResolvedOAuthConfig {
            authorize_url,
            token_url,
            client_id,
            scopes,
        })
    }

    async fn dynamic_register_client(
        &self,
        registration_endpoint: &str,
        redirect_uri: &str,
    ) -> Result<RegisteredClient> {
        let request = DynamicClientRegistrationRequest {
            redirect_uris: vec![redirect_uri.to_string()],
            grant_types: vec![
                "authorization_code".to_string(),
                "refresh_token".to_string(),
            ],
            response_types: vec!["code".to_string()],
            token_endpoint_auth_method: "none".to_string(),
            client_name: "gambi".to_string(),
        };

        let response = self
            .http
            .post(registration_endpoint)
            .json(&request)
            .send()
            .await
            .context("client registration request failed")?
            .error_for_status()
            .context("client registration returned error response")?
            .json::<DynamicClientRegistrationResponse>()
            .await
            .context("client registration response JSON is invalid")?;

        let client_id = response.client_id.trim().to_string();
        if client_id.is_empty() {
            bail!("client registration response did not include a client_id");
        }

        Ok(RegisteredClient {
            client_id,
            client_secret: response.client_secret,
            registration_client_uri: response.registration_client_uri,
            registration_access_token: response.registration_access_token,
        })
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

fn server_for_name<'a>(
    cfg: &'a crate::config::AppConfig,
    server_name: &str,
) -> Result<&'a crate::config::ServerConfig> {
    cfg.servers
        .iter()
        .find(|item| item.name == server_name)
        .ok_or_else(|| anyhow::anyhow!("unknown server '{server_name}'"))
}

fn mcp_oauth_discovery_url_for_server_url(server_url: &str) -> Option<String> {
    let parsed = Url::parse(server_url).ok()?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return None,
    }
    let origin = parsed.origin().ascii_serialization();
    if origin == "null" {
        return None;
    }
    Some(format!(
        "{origin}/.well-known/oauth-authorization-server",
        origin = origin.trim_end_matches('/')
    ))
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

    let before_registered_clients = tokens.registered_clients.len();
    tokens
        .registered_clients
        .retain(|server, _| server_names.contains(server));
    let removed_registered_clients =
        before_registered_clients.saturating_sub(tokens.registered_clients.len());

    removed_oauth_tokens
        .saturating_add(removed_oauth_health)
        .saturating_add(removed_registered_clients)
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
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

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
            client_id: Some("client-id".to_string()),
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
            client_id: Some("client-id".to_string()),
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
    async fn mark_servers_healthy_clears_stale_degraded_state() {
        let (_temp, store) = configured_store(None);
        store
            .update_tokens::<TokenState, _, _>(|tokens| {
                tokens.oauth_health.insert(
                    "port".to_string(),
                    OAuthServerHealth {
                        degraded: true,
                        last_error: Some("oauth refresh returned error response".to_string()),
                        updated_at_epoch_seconds: Some(1),
                        next_retry_after_epoch_seconds: Some(2),
                    },
                );
                Ok(())
            })
            .expect("seed degraded oauth health state");

        let manager = AuthManager::new(store);
        let cleared = manager
            .mark_servers_healthy(&["port".to_string()])
            .await
            .expect("mark healthy should succeed");
        assert_eq!(cleared, 1);

        let statuses = manager.list_statuses().await.expect("status should load");
        assert_eq!(statuses.len(), 1);
        assert!(!statuses[0].degraded);
        assert!(statuses[0].last_error.is_none());
    }

    #[tokio::test]
    async fn callback_and_refresh_rotate_tokens_with_simulated_provider() {
        let (token_url, handle) = spawn_token_server().await;

        let oauth = OAuthConfig {
            discovery_url: None,
            authorize_url: "https://example.com/oauth/authorize".to_string(),
            token_url,
            client_id: Some("client-id".to_string()),
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
        assert!(parsed.registered_clients.is_empty());
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
        tokens.registered_clients.insert(
            "remove".to_string(),
            super::RegisteredClient {
                client_id: "client-remove".to_string(),
                client_secret: None,
                registration_client_uri: None,
                registration_access_token: None,
            },
        );

        let mut allowed = std::collections::HashSet::new();
        allowed.insert("keep".to_string());
        let removed = prune_token_state_for_server_names(&mut tokens, &allowed);

        assert_eq!(removed, 3);
        assert!(tokens.oauth_tokens.contains_key("keep"));
        assert!(!tokens.oauth_tokens.contains_key("remove"));
        assert!(!tokens.oauth_health.contains_key("remove"));
        assert!(!tokens.registered_clients.contains_key("remove"));
    }

    fn configured_store(oauth_override: Option<OAuthConfig>) -> (tempfile::TempDir, ConfigStore) {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let store = ConfigStore::with_base_dir(temp_dir.path().join("gambi"));

        let oauth = oauth_override.unwrap_or_else(|| OAuthConfig {
            discovery_url: None,
            authorize_url: "https://example.com/oauth/authorize".to_string(),
            token_url: "https://example.com/oauth/token".to_string(),
            client_id: Some("client-id".to_string()),
            scopes: vec!["read".to_string()],
        });

        let cfg = AppConfig {
            servers: vec![ServerConfig {
                name: "port".to_string(),
                url: "https://example.com/mcp".to_string(),
                oauth: Some(oauth),
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            }],
            server_tool_activation_modes: std::collections::BTreeMap::new(),
            tool_activation_overrides: std::collections::BTreeMap::new(),
            server_tool_policy_modes: std::collections::BTreeMap::new(),
            tool_description_overrides: std::collections::BTreeMap::new(),
            tool_policy_overrides: std::collections::BTreeMap::new(),
        };
        store.replace(cfg).expect("config should persist");

        (temp_dir, store)
    }

    #[tokio::test]
    async fn start_dynamically_registers_client_and_reuses_persisted_registration() {
        let (server_url, register_hits, handle) = spawn_dynamic_registration_provider(true).await;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let store = ConfigStore::with_base_dir(temp_dir.path().join("gambi"));
        store
            .replace(AppConfig {
                servers: vec![ServerConfig {
                    name: "atlassian".to_string(),
                    url: server_url,
                    oauth: None,
                    transport: Default::default(),
                    exposure_mode: Default::default(),
                    enabled: true,
                }],
                server_tool_activation_modes: std::collections::BTreeMap::new(),
                tool_activation_overrides: std::collections::BTreeMap::new(),
                server_tool_policy_modes: std::collections::BTreeMap::new(),
                tool_description_overrides: std::collections::BTreeMap::new(),
                tool_policy_overrides: std::collections::BTreeMap::new(),
            })
            .expect("config should persist");

        let manager = AuthManager::new(store.clone());
        let started = manager
            .start("atlassian", "http://127.0.0.1:3333")
            .await
            .expect("start should succeed");
        assert!(started.auth_url.contains("client_id=dynamic-client"));
        assert_eq!(register_hits.load(Ordering::SeqCst), 1);

        let tokens: TokenState = store.load_tokens().expect("token state should load");
        let registered = tokens
            .registered_clients
            .get("atlassian")
            .expect("registered client should be persisted");
        assert_eq!(registered.client_id, "dynamic-client");

        // New manager instance should reuse the persisted registration (no extra register calls).
        let manager_again = AuthManager::new(store);
        let _ = manager_again
            .start("atlassian", "http://127.0.0.1:3333")
            .await
            .expect("start should still succeed");
        assert_eq!(register_hits.load(Ordering::SeqCst), 1);

        handle.abort();
    }

    #[tokio::test]
    async fn start_fails_when_client_id_missing_and_registration_endpoint_unavailable() {
        let (server_url, register_hits, handle) = spawn_dynamic_registration_provider(false).await;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let store = ConfigStore::with_base_dir(temp_dir.path().join("gambi"));
        store
            .replace(AppConfig {
                servers: vec![ServerConfig {
                    name: "port".to_string(),
                    url: server_url,
                    oauth: None,
                    transport: Default::default(),
                    exposure_mode: Default::default(),
                    enabled: true,
                }],
                server_tool_activation_modes: std::collections::BTreeMap::new(),
                tool_activation_overrides: std::collections::BTreeMap::new(),
                server_tool_policy_modes: std::collections::BTreeMap::new(),
                tool_description_overrides: std::collections::BTreeMap::new(),
                tool_policy_overrides: std::collections::BTreeMap::new(),
            })
            .expect("config should persist");

        let manager = AuthManager::new(store);
        let err = manager
            .start("port", "http://127.0.0.1:3333")
            .await
            .expect_err("start should fail without registration endpoint");
        assert!(
            err.to_string()
                .contains("dynamic client registration is not available")
        );
        assert_eq!(register_hits.load(Ordering::SeqCst), 0);

        handle.abort();
    }

    async fn spawn_dynamic_registration_provider(
        with_registration: bool,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let register_hits = Arc::new(AtomicUsize::new(0));
        let register_hits_handler = Arc::clone(&register_hits);

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind provider listener");
        let addr = listener.local_addr().expect("provider listener addr");

        let origin = format!("http://{addr}");
        let issuer = origin.clone();
        let authorize_url = format!("{origin}/authorize");
        let token_url = format!("{origin}/token");
        let registration_endpoint = format!("{origin}/register");

        let discovery = Router::new().route(
            "/.well-known/oauth-authorization-server",
            get(move || {
                let issuer = issuer.clone();
                let authorize_url = authorize_url.clone();
                let token_url = token_url.clone();
                let registration_endpoint = registration_endpoint.clone();
                async move {
                    if with_registration {
                        Json(json!({
                            "issuer": issuer,
                            "authorization_endpoint": authorize_url,
                            "token_endpoint": token_url,
                            "registration_endpoint": registration_endpoint,
                            "grant_types_supported": ["authorization_code", "refresh_token"],
                            "code_challenge_methods_supported": ["S256"]
                        }))
                    } else {
                        Json(json!({
                            "issuer": issuer,
                            "authorization_endpoint": authorize_url,
                            "token_endpoint": token_url
                        }))
                    }
                }
            }),
        );

        let register_route = Router::new().route(
            "/register",
            post(move |Json(body): Json<serde_json::Value>| {
                let hits = Arc::clone(&register_hits_handler);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);

                    // Minimal validation that the redirect URI was passed through.
                    let redirect_uris = body
                        .get("redirect_uris")
                        .and_then(|value| value.as_array())
                        .cloned()
                        .unwrap_or_default();
                    assert!(
                        redirect_uris.iter().any(|value| value
                            .as_str()
                            .unwrap_or_default()
                            .contains("/auth/callback")),
                        "expected redirect_uris to include /auth/callback"
                    );

                    Json(json!({
                        "client_id": "dynamic-client",
                        "registration_client_uri": "https://example.test/client/123"
                    }))
                }
            }),
        );

        let app = if with_registration {
            discovery.merge(register_route)
        } else {
            discovery
        };

        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("{origin}/v1/sse"), register_hits, handle)
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
