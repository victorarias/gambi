use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    http::{StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::IntoResponse,
};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo, Tool,
    },
    transport::{
        ConfigureCommandExt, TokioChildProcess,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use serde::Deserialize;
use serde_json::{Map, json};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Deserialize)]
struct ExecuteResponse {
    result: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct HelpToolSummary {
    namespaced_name: String,
}

#[derive(Debug, Deserialize)]
struct HelpServerSummary {
    name: String,
    tools: Vec<HelpToolSummary>,
}

#[derive(Debug, Deserialize)]
struct HelpResponse {
    servers: Vec<HelpServerSummary>,
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

#[tokio::test]
async fn gambi_routes_http_upstream_with_bearer_token() -> anyhow::Result<()> {
    let (remote_port, remote_shutdown, remote_handle) =
        spawn_authenticated_remote_server(0, "test-token").await?;

    let (client, _temp) = spawn_gambi_with_remote(remote_port).await?;

    let tools = client.list_all_tools().await?;
    assert!(
        tools.iter().any(|tool| tool.name.as_ref() == "gambi_help"),
        "expected gambi_help to be listed"
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.name.as_ref() == "gambi_execute"),
        "expected gambi_execute to be listed"
    );

    let help = help(&client, Some("remote")).await?;
    let remote = help
        .servers
        .iter()
        .find(|server| server.name == "remote")
        .ok_or_else(|| anyhow::anyhow!("remote server missing from gambi_help"))?;
    assert!(
        remote
            .tools
            .iter()
            .any(|tool| tool.namespaced_name == "remote:remote_echo"),
        "expected remote:remote_echo in gambi_help discovery"
    );

    let output = execute(
        &client,
        r#"
result = remote.remote_echo(message="hello-http", count=2)
return result
"#,
    )
    .await?;
    assert_eq!(output.result["source"], "remote");
    assert_eq!(output.result["echo"]["message"], "hello-http");
    assert_eq!(output.result["echo"]["count"], 2);

    remote_shutdown.cancel();
    let _ = remote_handle.await;
    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_reconnects_http_upstream_after_remote_restart() -> anyhow::Result<()> {
    let (remote_port, first_shutdown, first_handle) =
        spawn_authenticated_remote_server(0, "test-token").await?;

    let (client, _temp) = spawn_gambi_with_remote(remote_port).await?;

    // Prime initial connection.
    let _ = execute(&client, "return remote.remote_echo()").await?;

    first_shutdown.cancel();
    let _ = first_handle.await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (restarted_port, second_shutdown, second_handle) =
        spawn_authenticated_remote_server(remote_port, "test-token").await?;
    assert_eq!(restarted_port, remote_port);

    let mut succeeded = false;
    for _attempt in 0..5 {
        match execute(&client, "return remote.remote_echo()").await {
            Ok(output) => {
                assert_eq!(output.result["source"], "remote");
                succeeded = true;
                break;
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }

    assert!(
        succeeded,
        "expected reconnect to succeed after remote restart"
    );

    second_shutdown.cancel();
    let _ = second_handle.await;
    let _ = client.cancel().await;
    Ok(())
}

async fn spawn_authenticated_remote_server(
    port: u16,
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
            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(listener) => break listener,
                Err(err) if port != 0 && attempts < 200 => {
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    tracing::debug!(
                        error = %err,
                        attempts,
                        "retrying authenticated remote bind on preferred port"
                    );
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

async fn spawn_gambi_with_remote(
    remote_port: u16,
) -> anyhow::Result<(
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tempfile::TempDir,
)> {
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;

    let gambi_config_dir = temp.path().join("gambi");
    std::fs::create_dir_all(&gambi_config_dir)?;

    write_config(
        &gambi_config_dir.join("config.json"),
        &format!(
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
        ),
    )?;

    write_config(
        &gambi_config_dir.join("tokens.json"),
        r#"{
  "oauth_tokens": {
    "remote": {
      "access_token": "test-token",
      "token_type": "Bearer",
      "scopes": ["read"]
    }
  },
  "oauth_health": {}
}
"#,
    )?;

    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.arg("serve");
        cmd.arg("--admin-port");
        cmd.arg("0");
        cmd.env("GAMBI_CONFIG_DIR", &gambi_config_dir);
    }))?;

    let client = ().serve(transport).await?;
    Ok((client, temp))
}

async fn execute(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    code: &str,
) -> anyhow::Result<ExecuteResponse> {
    let result = client
        .call_tool(CallToolRequestParams {
            meta: None,
            name: "gambi_execute_escalated".to_string().into(),
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

async fn help(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    server: Option<&str>,
) -> anyhow::Result<HelpResponse> {
    let arguments = server.map(|name| {
        json!({ "server": name })
            .as_object()
            .cloned()
            .unwrap_or_default()
    });

    let result = client
        .call_tool(CallToolRequestParams {
            meta: None,
            name: "gambi_help".to_string().into(),
            arguments,
            task: None,
        })
        .await?;

    if result.is_error.unwrap_or(false) {
        let first_text = result
            .content
            .first()
            .and_then(|content| content.as_text())
            .map(|text| text.text.clone())
            .unwrap_or_else(|| "gambi_help returned an error".to_string());
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
        .ok_or_else(|| anyhow::anyhow!("gambi_help did not return parseable output"))?;

    Ok(serde_json::from_str(&first_text)?)
}

fn write_config(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
