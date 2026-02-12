use std::path::Path;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rmcp::{
    ServiceError, ServiceExt,
    model::{
        CallToolRequestParams, CancelledNotificationParam, ClientRequest, ErrorCode,
        GetPromptRequestParams, ReadResourceRequestParams, Request,
    },
    service::PeerRequestOptions,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use tokio::process::Command;

#[tokio::test]
async fn gambi_cancels_routed_tool_calls() -> anyhow::Result<()> {
    let (client, _temp) = spawn_gambi_with_fixture().await?;

    let request = ClientRequest::CallToolRequest(Request::new(CallToolRequestParams {
        meta: None,
        name: "fixture:fixture_cancel".to_string().into(),
        arguments: None,
        task: None,
    }));

    let handle = client
        .send_cancellable_request(request, PeerRequestOptions::no_options())
        .await?;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    client
        .notify_cancelled(CancelledNotificationParam {
            request_id: handle.id.clone(),
            reason: Some("test cancellation".to_string()),
        })
        .await?;

    let err = handle
        .await_response()
        .await
        .expect_err("tool call must cancel");
    assert_cancelled_error(err);

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_cancels_routed_prompt_calls() -> anyhow::Result<()> {
    let (client, _temp) = spawn_gambi_with_fixture().await?;

    let request = ClientRequest::GetPromptRequest(Request::new(GetPromptRequestParams {
        meta: None,
        name: "fixture:fixture_cancel_prompt".to_string(),
        arguments: None,
    }));

    let handle = client
        .send_cancellable_request(request, PeerRequestOptions::no_options())
        .await?;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    client
        .notify_cancelled(CancelledNotificationParam {
            request_id: handle.id.clone(),
            reason: Some("test cancellation".to_string()),
        })
        .await?;

    let err = handle
        .await_response()
        .await
        .expect_err("prompt call must cancel");
    assert_cancelled_error(err);

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_cancels_routed_resource_calls() -> anyhow::Result<()> {
    let (client, _temp) = spawn_gambi_with_fixture().await?;

    let namespaced_uri = namespaced_resource_uri("fixture", "fixture://cancel");

    let request = ClientRequest::ReadResourceRequest(Request::new(ReadResourceRequestParams {
        meta: None,
        uri: namespaced_uri,
    }));

    let handle = client
        .send_cancellable_request(request, PeerRequestOptions::no_options())
        .await?;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    client
        .notify_cancelled(CancelledNotificationParam {
            request_id: handle.id.clone(),
            reason: Some("test cancellation".to_string()),
        })
        .await?;

    let err = handle
        .await_response()
        .await
        .expect_err("resource call must cancel");
    assert_cancelled_error(err);

    let _ = client.cancel().await;
    Ok(())
}

fn assert_cancelled_error(err: ServiceError) {
    match err {
        ServiceError::McpError(data) => {
            assert_eq!(data.code, ErrorCode::INVALID_REQUEST);
            assert!(data.message.to_lowercase().contains("cancel"));
        }
        ServiceError::Cancelled { .. } => {}
        other => panic!("expected cancellation-related error, got: {other}"),
    }
}

fn namespaced_resource_uri(server_name: &str, upstream_uri: &str) -> String {
    let encoded = URL_SAFE_NO_PAD.encode(upstream_uri.as_bytes());
    format!("gambi://resource/{server_name}/{encoded}")
}

async fn spawn_gambi_with_fixture() -> anyhow::Result<(
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tempfile::TempDir,
)> {
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;

    let xdg_config_home = temp.path().to_path_buf();
    let gambi_config_dir = xdg_config_home.join("gambi");
    std::fs::create_dir_all(&gambi_config_dir)?;

    let fixture_url = format!("stdio:///{bin}?arg=__fixture_progress_server", bin = bin);

    write_config(
        &gambi_config_dir.join("config.json"),
        &format!(
            "{{\n  \"servers\": [\n    {{ \"name\": \"fixture\", \"url\": \"{}\" }}\n  ]\n}}\n",
            fixture_url
        ),
    )?;
    write_config(&gambi_config_dir.join("tokens.json"), "{}\n")?;

    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.arg("serve");
        cmd.arg("--admin-port");
        cmd.arg("0");
        cmd.arg("--no-exec");
        cmd.env("XDG_CONFIG_HOME", xdg_config_home);
    }))?;

    let client = ().serve(transport).await?;
    Ok((client, temp))
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
