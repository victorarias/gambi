use std::path::Path;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rmcp::{
    ClientHandler, ServiceExt,
    handler::client::progress::ProgressDispatcher,
    model::{
        CallToolRequestParams, ClientRequest, GetPromptRequestParams, ReadResourceRequestParams,
        Request, ServerResult,
    },
    service::PeerRequestOptions,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use tokio::process::Command;
use tokio_stream::StreamExt;

#[derive(Default)]
struct ProgressClient {
    progress: ProgressDispatcher,
}

impl ClientHandler for ProgressClient {
    async fn on_progress(
        &self,
        params: rmcp::model::ProgressNotificationParam,
        _context: rmcp::service::NotificationContext<rmcp::RoleClient>,
    ) {
        self.progress.handle_notification(params).await;
    }
}

#[tokio::test]
async fn gambi_forwards_tool_progress_notifications_for_routed_calls() -> anyhow::Result<()> {
    let (client, _temp) = spawn_gambi_with_fixture().await?;

    let request = ClientRequest::CallToolRequest(Request::new(CallToolRequestParams {
        meta: None,
        name: "fixture:fixture_progress".to_string().into(),
        arguments: None,
        task: None,
    }));

    let handle = client
        .send_cancellable_request(request, PeerRequestOptions::no_options())
        .await?;

    let observed = collect_progress(&client, handle.progress_token.clone(), 5).await?;
    let response = handle.await_response().await?;

    match response {
        ServerResult::CallToolResult(result) => {
            assert!(!result.is_error.unwrap_or(false));
        }
        other => anyhow::bail!("unexpected response variant: {other:?}"),
    }

    assert_progress_sequence(&observed);

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_forwards_prompt_progress_notifications_for_routed_calls() -> anyhow::Result<()> {
    let (client, _temp) = spawn_gambi_with_fixture().await?;

    let request = ClientRequest::GetPromptRequest(Request::new(GetPromptRequestParams {
        meta: None,
        name: "fixture:fixture_progress_prompt".to_string(),
        arguments: None,
    }));

    let handle = client
        .send_cancellable_request(request, PeerRequestOptions::no_options())
        .await?;

    let observed = collect_progress(&client, handle.progress_token.clone(), 5).await?;
    let response = handle.await_response().await?;

    match response {
        ServerResult::GetPromptResult(result) => {
            assert!(!result.messages.is_empty());
        }
        other => anyhow::bail!("unexpected response variant: {other:?}"),
    }

    assert_progress_sequence(&observed);

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_forwards_resource_progress_notifications_for_routed_calls() -> anyhow::Result<()> {
    let (client, _temp) = spawn_gambi_with_fixture().await?;

    let namespaced_uri = namespaced_resource_uri("fixture", "fixture://progress");

    let request = ClientRequest::ReadResourceRequest(Request::new(ReadResourceRequestParams {
        meta: None,
        uri: namespaced_uri,
    }));

    let handle = client
        .send_cancellable_request(request, PeerRequestOptions::no_options())
        .await?;

    let observed = collect_progress(&client, handle.progress_token.clone(), 5).await?;
    let response = handle.await_response().await?;

    match response {
        ServerResult::ReadResourceResult(result) => {
            assert!(!result.contents.is_empty());
        }
        other => anyhow::bail!("unexpected response variant: {other:?}"),
    }

    assert_progress_sequence(&observed);

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_forwards_tool_progress_notifications_for_execute_nested_calls() -> anyhow::Result<()>
{
    let (client, _temp) = spawn_gambi_with_fixture_for_exec().await?;

    let request = ClientRequest::CallToolRequest(Request::new(CallToolRequestParams {
        meta: None,
        name: "gambi_execute".to_string().into(),
        arguments: Some(
            serde_json::json!({
                "code": r#"
result = fixture.fixture_progress()
return {"status": result["status"]}
"#
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        ),
        task: None,
    }));

    let handle = client
        .send_cancellable_request(request, PeerRequestOptions::no_options())
        .await?;

    let observed = collect_progress(&client, handle.progress_token.clone(), 5).await?;
    let response = handle.await_response().await?;

    match response {
        ServerResult::CallToolResult(result) => {
            assert!(!result.is_error.unwrap_or(false));
        }
        other => anyhow::bail!("unexpected response variant: {other:?}"),
    }

    assert_progress_sequence(&observed);

    let _ = client.cancel().await;
    Ok(())
}

async fn spawn_gambi_with_fixture() -> anyhow::Result<(
    rmcp::service::RunningService<rmcp::RoleClient, ProgressClient>,
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

    let client = ProgressClient::default().serve(transport).await?;
    Ok((client, temp))
}

async fn spawn_gambi_with_fixture_for_exec() -> anyhow::Result<(
    rmcp::service::RunningService<rmcp::RoleClient, ProgressClient>,
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
        cmd.env("XDG_CONFIG_HOME", xdg_config_home);
    }))?;

    let client = ProgressClient::default().serve(transport).await?;
    Ok((client, temp))
}

async fn collect_progress(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ProgressClient>,
    progress_token: rmcp::model::ProgressToken,
    expected: usize,
) -> anyhow::Result<Vec<i64>> {
    let mut progress_stream = client.service().progress.subscribe(progress_token).await;
    let mut observed = Vec::new();
    while observed.len() < expected {
        let next =
            tokio::time::timeout(std::time::Duration::from_secs(2), progress_stream.next()).await?;
        let Some(item) = next else {
            break;
        };
        observed.push(item.progress as i64);
    }

    Ok(observed)
}

fn assert_progress_sequence(observed: &[i64]) {
    assert!(
        observed.len() >= 3,
        "expected multiple progress notifications"
    );
    assert_eq!(observed[0], 1);
    assert_eq!(*observed.last().unwrap_or(&0), 5);
}

fn namespaced_resource_uri(server_name: &str, upstream_uri: &str) -> String {
    let encoded = URL_SAFE_NO_PAD.encode(upstream_uri.as_bytes());
    format!("gambi://resource/{server_name}/{encoded}")
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
