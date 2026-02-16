use std::path::Path;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct SseMockState {
    /// Broadcast channel: SSE server -> connected SSE clients
    sse_tx: broadcast::Sender<String>,
}

/// GET /sse — the SSE endpoint.
///
/// Sends `event: endpoint` with the POST URL, then relays all messages
/// from the broadcast channel as `event: message` SSE events.
async fn sse_handler(State(state): State<SseMockState>) -> impl IntoResponse {
    let mut rx = state.sse_tx.subscribe();

    let stream = async_stream::stream! {
        // Send the endpoint event immediately
        yield Ok::<_, std::convert::Infallible>(
            "event: endpoint\ndata: /messages\n\n".to_string()
        );

        // Relay JSON-RPC messages from broadcast channel
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    yield Ok(format!("event: message\ndata: {msg}\n\n"));
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    };

    let body = axum::body::Body::from_stream(stream);

    (
        StatusCode::OK,
        [
            ("content-type", "text/event-stream"),
            ("cache-control", "no-cache"),
        ],
        body,
    )
}

/// POST /messages — receives JSON-RPC messages from clients.
///
/// Processes them and sends responses via the SSE broadcast channel.
async fn messages_handler(State(state): State<SseMockState>, body: String) -> impl IntoResponse {
    let parsed: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST,
    };

    let method = parsed.get("method").and_then(Value::as_str).unwrap_or("");
    let id = parsed.get("id").cloned();

    match method {
        "initialize" => {
            let response = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": { "listChanged": false }
                    },
                    "serverInfo": {
                        "name": "mock-sse-server",
                        "version": "0.1.0"
                    }
                }
            });
            let _ = state.sse_tx.send(response.to_string());
        }
        "notifications/initialized" => {
            // No response needed for notifications
        }
        "tools/list" => {
            let response = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "sse_echo",
                            "description": "Echo back arguments via SSE",
                            "inputSchema": {
                                "type": "object",
                                "properties": {}
                            }
                        }
                    ]
                }
            });
            let _ = state.sse_tx.send(response.to_string());
        }
        "tools/call" => {
            let tool_name = parsed
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let arguments = parsed
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));

            let result = match tool_name {
                "sse_echo" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": serde_json::to_string(&json!({
                                "source": "sse",
                                "echo": arguments
                            })).unwrap_or_default()
                        }],
                        "structuredContent": {
                            "source": "sse",
                            "echo": arguments
                        }
                    }
                }),
                other => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32602,
                        "message": format!("unknown tool '{other}'")
                    }
                }),
            };
            let _ = state.sse_tx.send(result.to_string());
        }
        _ => {
            if let Some(id) = id {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("method not found: {method}")
                    }
                });
                let _ = state.sse_tx.send(response.to_string());
            }
        }
    }

    StatusCode::ACCEPTED
}

async fn spawn_mock_sse_server()
-> anyhow::Result<(u16, CancellationToken, tokio::task::JoinHandle<()>)> {
    let shutdown = CancellationToken::new();
    let (sse_tx, _) = broadcast::channel::<String>(64);

    let state = SseMockState { sse_tx };

    let app = Router::new()
        .route("/sse", get(sse_handler))
        .route("/messages", post(messages_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
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

#[tokio::test]
async fn gambi_discovers_and_calls_tools_over_legacy_sse() -> anyhow::Result<()> {
    let (sse_port, sse_shutdown, sse_handle) = spawn_mock_sse_server().await?;

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
      "name": "mock",
      "url": "http://127.0.0.1:{sse_port}/sse"
    }}
  ]
}}
"#
        ),
    )?;
    write_config(&gambi_config_dir.join("tokens.json"), "{}\n")?;

    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.arg("serve");
        cmd.arg("--admin-port");
        cmd.arg("0");
        cmd.env("GAMBI_CONFIG_DIR", &gambi_config_dir);
    }))?;

    let client = ().serve(transport).await?;

    let help = client
        .call_tool(CallToolRequestParams {
            meta: None,
            name: "gambi_help".to_string().into(),
            arguments: Some(
                json!({
                    "server": "mock",
                    "tool": "sse_echo"
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            ),
            task: None,
        })
        .await?;
    let help_payload = help
        .structured_content
        .ok_or_else(|| anyhow::anyhow!("missing help payload"))?;
    assert_eq!(help_payload["tool"]["namespaced_name"], "mock:sse_echo");

    let result = client
        .call_tool(CallToolRequestParams {
            meta: None,
            name: "gambi_execute".to_string().into(),
            arguments: Some(
                json!({
                    "code": r#"
result = mock.sse_echo(message="hello-sse")
return {"source": result["source"], "message": result["echo"]["message"]}
"#
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
        .ok_or_else(|| anyhow::anyhow!("missing structured payload"))?;
    assert_eq!(payload["result"]["source"], "sse");
    assert_eq!(payload["result"]["message"], "hello-sse");

    sse_shutdown.cancel();
    let _ = sse_handle.await;
    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_uses_sse_transport_for_explicit_config() -> anyhow::Result<()> {
    let (sse_port, sse_shutdown, sse_handle) = spawn_mock_sse_server().await?;

    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;
    let gambi_config_dir = temp.path().join("gambi");
    std::fs::create_dir_all(&gambi_config_dir)?;

    // Use a non-/sse path but set transport explicitly
    write_config(
        &gambi_config_dir.join("config.json"),
        &format!(
            r#"{{
  "servers": [
    {{
      "name": "explicit",
      "url": "http://127.0.0.1:{sse_port}/sse",
      "transport": "sse"
    }}
  ]
}}
"#
        ),
    )?;
    write_config(&gambi_config_dir.join("tokens.json"), "{}\n")?;

    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.arg("serve");
        cmd.arg("--admin-port");
        cmd.arg("0");
        cmd.arg("--no-exec");
        cmd.env("GAMBI_CONFIG_DIR", &gambi_config_dir);
    }))?;

    let client = ().serve(transport).await?;

    let help = client
        .call_tool(CallToolRequestParams {
            meta: None,
            name: "gambi_help".to_string().into(),
            arguments: Some(
                json!({
                    "server": "explicit",
                    "tool": "sse_echo"
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            ),
            task: None,
        })
        .await?;
    let payload = help
        .structured_content
        .ok_or_else(|| anyhow::anyhow!("missing structured help payload"))?;
    assert_eq!(payload["tool"]["namespaced_name"], "explicit:sse_echo");

    sse_shutdown.cancel();
    let _ = sse_handle.await;
    let _ = client.cancel().await;
    Ok(())
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
