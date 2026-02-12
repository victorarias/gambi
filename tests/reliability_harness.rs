mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    http::{StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::IntoResponse,
};
use futures::future::try_join_all;
use rmcp::{
    ErrorData as McpError, RoleClient, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo, Tool,
    },
    service::RunningService,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde::Deserialize;
use serde_json::{Map, json};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Deserialize)]
struct ExecuteResponse {
    result: serde_json::Value,
    tool_calls: usize,
    elapsed_ms: u128,
    stdout: Vec<String>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gambi_handles_parallel_mixed_operations_without_errors() -> anyhow::Result<()> {
    let fixture_url = support::fixture_server_url();
    let config = format!(
        "{{\n  \"servers\": [\n    {{ \"name\": \"fixture\", \"url\": \"{}\" }}\n  ]\n}}\n",
        fixture_url
    );

    let mut env = BTreeMap::new();
    env.insert("GAMBI_EXEC_MAX_WALL_MS".to_string(), "1500".to_string());
    env.insert(
        "GAMBI_EXEC_MAX_MEM_BYTES".to_string(),
        "134217728".to_string(),
    );

    let (client, _temp) =
        support::spawn_gambi_with_config((), &config, "{}\n", false, &env).await?;

    let mixed_jobs = env_usize("GAMBI_RELIABILITY_MIXED_JOBS", 72, 1, 512);
    let mixed_rounds = env_usize("GAMBI_RELIABILITY_MIXED_ROUNDS", 1, 1, 32);

    for round in 0..mixed_rounds {
        let jobs = (0usize..mixed_jobs)
            .map(|idx| run_mixed_operation(&client, round * mixed_jobs + idx))
            .collect::<Vec<_>>();
        try_join_all(jobs).await?;
    }

    let _ = client.cancel().await;
    Ok(())
}

async fn run_mixed_operation(
    client: &RunningService<RoleClient, ()>,
    idx: usize,
) -> anyhow::Result<()> {
    match idx % 3 {
        0 => {
            let tools = client.list_all_tools().await?;
            anyhow::ensure!(
                tools
                    .iter()
                    .any(|tool| tool.name.as_ref() == "fixture:fixture_echo"),
                "fixture tool missing from aggregated list"
            );
            anyhow::ensure!(
                tools
                    .iter()
                    .any(|tool| tool.name.as_ref() == "gambi_execute"),
                "gambi_execute missing from aggregated list"
            );
        }
        1 => {
            let result = client
                .call_tool(CallToolRequestParams {
                    meta: None,
                    name: "fixture:fixture_echo".to_string().into(),
                    arguments: Some(
                        json!({
                            "idx": idx,
                            "parity": if idx.is_multiple_of(2) { "even" } else { "odd" }
                        })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                    ),
                    task: None,
                })
                .await?;

            let payload = result
                .structured_content
                .ok_or_else(|| anyhow::anyhow!("missing structured payload for fixture_echo"))?;
            anyhow::ensure!(payload["echo"]["idx"] == idx);
        }
        _ => {
            let code = format!(
                r#"
first = fixture.fixture_echo(step={idx})
second = fixture.fixture_echo(step={idx} + 1)
print("run", {idx})
return {{"first": first["echo"]["step"], "second": second["echo"]["step"]}}
"#
            );

            let output = execute(client, &code).await?;
            anyhow::ensure!(output.result["first"] == idx);
            anyhow::ensure!(output.result["second"] == idx + 1);
            anyhow::ensure!(output.tool_calls == 2);
            anyhow::ensure!(output.elapsed_ms > 0);
            anyhow::ensure!(!output.stdout.is_empty());
        }
    }

    Ok(())
}

async fn execute(
    client: &RunningService<RoleClient, ()>,
    code: &str,
) -> anyhow::Result<ExecuteResponse> {
    let result = client
        .call_tool(CallToolRequestParams {
            meta: None,
            name: "gambi_execute".to_string().into(),
            arguments: Some(
                json!({ "code": code })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            task: None,
        })
        .await?;

    if result.is_error.unwrap_or(false) {
        let first_text = result
            .content
            .first()
            .and_then(|content| content.as_text())
            .map(|text| text.text.clone())
            .unwrap_or_else(|| "gambi_execute returned an error".to_string());
        anyhow::bail!(first_text);
    }

    if let Some(structured) = result.structured_content {
        return Ok(serde_json::from_value(structured)?);
    }

    let first_text = result
        .content
        .first()
        .and_then(|content| content.as_text())
        .map(|text| text.text.clone())
        .ok_or_else(|| anyhow::anyhow!("gambi_execute did not return parseable output"))?;

    Ok(serde_json::from_str(&first_text)?)
}

#[derive(Debug, Clone, Copy)]
struct RemoteFixture;

impl RemoteFixture {
    fn tool_descriptor() -> Tool {
        Tool::new("remote_echo", "Echo arguments", Arc::new(Map::new()))
    }
}

impl ServerHandler for RemoteFixture {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some("remote fixture server".into()),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: vec![Self::tool_descriptor()],
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        match request.name.as_ref() {
            "remote_echo" => Ok(CallToolResult::structured(json!({
                "source": "remote",
                "echo": request.arguments.unwrap_or_default()
            }))),
            _ => Err(McpError::invalid_params(
                format!("unknown tool '{}'", request.name),
                None,
            )),
        }
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        if name == "remote_echo" {
            Some(Self::tool_descriptor())
        } else {
            None
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gambi_stays_healthy_during_remote_bounce_under_load() -> anyhow::Result<()> {
    let (remote_port, mut remote_shutdown, mut remote_handle) =
        spawn_authenticated_remote_server(None, "test-token").await?;

    let config = format!(
        r#"{{
  "servers": [
    {{
      "name": "remote",
      "url": "http://127.0.0.1:{remote_port}/mcp",
      "oauth": {{
        "authorize_url": "https://example.com/oauth/authorize",
        "token_url": "https://example.com/oauth/token",
        "client_id": "client-id",
        "scopes": ["read"]
      }}
    }}
  ]
}}
"#
    );

    let tokens = r#"{
  "oauth_tokens": {
    "remote": {
      "access_token": "test-token",
      "token_type": "Bearer",
      "scopes": ["read"]
    }
  },
  "oauth_health": {}
}
"#;

    let (client, _temp) =
        support::spawn_gambi_with_config((), &config, tokens, true, &BTreeMap::new()).await?;

    let bounce_cycles = env_usize("GAMBI_RELIABILITY_BOUNCE_CYCLES", 12, 1, 128);
    let bounce_every = env_usize("GAMBI_RELIABILITY_BOUNCE_EVERY", 3, 1, 32);
    let bounce_workers = env_usize("GAMBI_RELIABILITY_BOUNCE_WORKERS", 6, 1, 64);
    let retry_attempts = env_usize("GAMBI_RELIABILITY_RETRY_ATTEMPTS", 8, 1, 32);
    let retry_backoff_ms = env_u64("GAMBI_RELIABILITY_RETRY_BACKOFF_MS", 125, 10, 2_000);
    let bounce_pause_ms = env_u64("GAMBI_RELIABILITY_BOUNCE_PAUSE_MS", 120, 10, 5_000);

    for cycle in 0..bounce_cycles {
        if cycle > 0 && cycle % bounce_every == 0 {
            remote_shutdown.cancel();
            let _ = remote_handle.await;
            tokio::time::sleep(Duration::from_millis(bounce_pause_ms)).await;
            let (rebound_port, next_shutdown, next_handle) =
                spawn_authenticated_remote_server(Some(remote_port), "test-token").await?;
            anyhow::ensure!(
                rebound_port == remote_port,
                "remote restart rebound to different port"
            );
            remote_shutdown = next_shutdown;
            remote_handle = next_handle;
        }

        let batch = (0usize..bounce_workers)
            .map(|worker| {
                call_remote_echo_with_retry(
                    &client,
                    cycle,
                    worker,
                    retry_attempts,
                    retry_backoff_ms,
                )
            })
            .collect::<Vec<_>>();
        try_join_all(batch).await?;
    }

    remote_shutdown.cancel();
    let _ = remote_handle.await;
    let _ = client.cancel().await;
    Ok(())
}

async fn call_remote_echo_with_retry(
    client: &RunningService<RoleClient, ()>,
    cycle: usize,
    worker: usize,
    retry_attempts: usize,
    retry_backoff_ms: u64,
) -> anyhow::Result<()> {
    for attempt in 0..retry_attempts {
        let result = client
            .call_tool(CallToolRequestParams {
                meta: None,
                name: "remote:remote_echo".to_string().into(),
                arguments: Some(
                    json!({
                        "cycle": cycle,
                        "worker": worker,
                        "attempt": attempt,
                    })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                ),
                task: None,
            })
            .await;

        match result {
            Ok(value) => {
                let payload = value
                    .structured_content
                    .ok_or_else(|| anyhow::anyhow!("missing structured payload for remote echo"))?;
                anyhow::ensure!(payload["source"] == "remote");
                anyhow::ensure!(payload["echo"]["cycle"] == cycle);
                anyhow::ensure!(payload["echo"]["worker"] == worker);
                return Ok(());
            }
            Err(err) if attempt + 1 < retry_attempts => {
                tracing::debug!(error = %err, cycle, worker, attempt, "remote echo failed, retrying");
                tokio::time::sleep(Duration::from_millis(retry_backoff_ms)).await;
            }
            Err(err) => return Err(err.into()),
        }
    }

    anyhow::bail!("remote echo retry loop exhausted")
}

fn env_usize(name: &str, default: usize, min: usize, max: usize) -> usize {
    let parsed = std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(default);
    parsed.clamp(min, max)
}

fn env_u64(name: &str, default: u64, min: u64, max: u64) -> u64 {
    let parsed = std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(default);
    parsed.clamp(min, max)
}

async fn spawn_authenticated_remote_server(
    preferred_port: Option<u16>,
    token: &'static str,
) -> anyhow::Result<(u16, CancellationToken, tokio::task::JoinHandle<()>)> {
    let shutdown = CancellationToken::new();

    let service: StreamableHttpService<RemoteFixture, LocalSessionManager> =
        StreamableHttpService::new(
            || Ok(RemoteFixture),
            Default::default(),
            StreamableHttpServerConfig {
                stateful_mode: true,
                sse_keep_alive: None,
                cancellation_token: shutdown.child_token(),
                ..Default::default()
            },
        );

    let app = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn(move |req, next| {
            require_bearer(req, next, token)
        }));

    let listener = {
        let mut attempts = 0u32;
        loop {
            let bind_port = preferred_port.unwrap_or(0);
            match tokio::net::TcpListener::bind(("127.0.0.1", bind_port)).await {
                Ok(listener) => break listener,
                Err(err) if preferred_port.is_some() && attempts < 20 => {
                    attempts += 1;
                    tracing::debug!(
                        error = %err,
                        attempts,
                        "retrying remote fixture bind on preferred port"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
    };
    let bound_port = listener.local_addr()?.port();

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
                .await;
        }
    });

    Ok((bound_port, shutdown, handle))
}

async fn require_bearer(
    req: axum::extract::Request,
    next: Next,
    token: &'static str,
) -> axum::response::Response {
    let expected = format!("Bearer {token}");
    let provided = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    if provided != Some(expected.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    next.run(req).await
}
