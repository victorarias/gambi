mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
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
}

#[derive(Debug, Clone, Copy)]
struct HttpFixture;

impl HttpFixture {
    fn tool_descriptor() -> Tool {
        Tool::new(
            "remote_echo",
            "Echo arguments from HTTP fixture",
            Arc::new(Map::new()),
        )
    }
}

impl ServerHandler for HttpFixture {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some("http fixture server".into()),
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
                "source": "http",
                "echo": request.arguments.unwrap_or_default(),
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
async fn gambi_execute_calls_multi_protocol_upstreams() -> anyhow::Result<()> {
    let (http_port, http_shutdown, http_handle) = spawn_http_fixture_server().await?;

    let fixture_url = support::fixture_server_url();
    let config = format!(
        r#"{{
  "servers": [
    {{ "name": "fixture_a", "url": "{fixture_url}" }},
    {{ "name": "fixture_b", "url": "{fixture_url}" }},
    {{ "name": "remote", "url": "http://127.0.0.1:{http_port}/mcp" }}
  ]
}}
"#
    );

    let (client, _temp) =
        support::spawn_gambi_with_config((), &config, "{}\n", false, &BTreeMap::new()).await?;

    let result = execute(
        &client,
        r#"
a = fixture_a.fixture_echo(source="fixture_a", value=1)
b = fixture_b.fixture_echo(source="fixture_b", value=2)
c = remote.remote_echo(source="remote", value=3)
return {
    "fixture_a": a["echo"],
    "fixture_b": b["echo"],
    "remote": c,
}
"#,
    )
    .await?;

    assert_eq!(result.result["fixture_a"]["source"], "fixture_a");
    assert_eq!(result.result["fixture_a"]["value"], 1);

    assert_eq!(result.result["fixture_b"]["source"], "fixture_b");
    assert_eq!(result.result["fixture_b"]["value"], 2);

    assert_eq!(result.result["remote"]["source"], "http");
    assert_eq!(result.result["remote"]["echo"]["source"], "remote");
    assert_eq!(result.result["remote"]["echo"]["value"], 3);

    assert_eq!(result.tool_calls, 3);

    let _ = client.cancel().await;
    http_shutdown.cancel();
    let _ = http_handle.await;

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

async fn spawn_http_fixture_server()
-> anyhow::Result<(u16, CancellationToken, tokio::task::JoinHandle<()>)> {
    let shutdown = CancellationToken::new();

    let service: StreamableHttpService<HttpFixture, LocalSessionManager> =
        StreamableHttpService::new(
            || Ok(HttpFixture),
            Default::default(),
            StreamableHttpServerConfig {
                stateful_mode: true,
                sse_keep_alive: None,
                cancellation_token: shutdown.child_token(),
                ..Default::default()
            },
        );

    let app = Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
                .await;
        }
    });

    Ok((port, shutdown, handle))
}
