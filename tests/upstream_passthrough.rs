use std::path::Path;

use rmcp::{
    ServiceError, ServiceExt,
    model::{CallToolRequestParams, ErrorCode, PaginatedRequestParams},
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use tokio::process::Command;

#[tokio::test]
async fn gambi_routes_namespaced_upstream_stdio_tool_calls() -> anyhow::Result<()> {
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;

    let xdg_config_home = temp.path();
    let gambi_config_dir = xdg_config_home.join("gambi");
    std::fs::create_dir_all(&gambi_config_dir)?;

    let self_url = format!(
        "stdio:///{bin}?arg=serve&arg=--no-exec&arg=--admin-port&arg=0",
        bin = bin
    );

    write_config(
        &gambi_config_dir.join("config.json"),
        &format!(
            "{{\n  \"servers\": [\n    {{ \"name\": \"self\", \"url\": \"{}\" }}\n  ]\n}}\n",
            self_url
        ),
    )?;
    write_config(&gambi_config_dir.join("tokens.json"), "{}\n")?;

    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.arg("serve");
        cmd.arg("--admin-port");
        cmd.arg("0");
        cmd.env("XDG_CONFIG_HOME", xdg_config_home);
    }))?;

    let client = ().serve(transport).await?;

    let tools = client.list_all_tools().await?;
    assert!(
        tools
            .iter()
            .any(|tool| tool.name.as_ref() == "self:gambi_list_servers")
    );

    let routed = client
        .call_tool(CallToolRequestParams {
            meta: None,
            name: "self:gambi_list_servers".to_string().into(),
            arguments: None,
            task: None,
        })
        .await?;

    assert!(routed.structured_content.is_some());

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_preserves_upstream_mcp_error_codes_for_routed_calls() -> anyhow::Result<()> {
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;

    let xdg_config_home = temp.path();
    let gambi_config_dir = xdg_config_home.join("gambi");
    std::fs::create_dir_all(&gambi_config_dir)?;

    let self_url = format!(
        "stdio:///{bin}?arg=serve&arg=--no-exec&arg=--admin-port&arg=0",
        bin = bin
    );

    write_config(
        &gambi_config_dir.join("config.json"),
        &format!(
            "{{\n  \"servers\": [\n    {{ \"name\": \"self\", \"url\": \"{}\" }}\n  ]\n}}\n",
            self_url
        ),
    )?;
    write_config(&gambi_config_dir.join("tokens.json"), "{}\n")?;

    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.arg("serve");
        cmd.arg("--admin-port");
        cmd.arg("0");
        cmd.env("XDG_CONFIG_HOME", xdg_config_home);
    }))?;

    let client = ().serve(transport).await?;
    let err = client
        .call_tool(CallToolRequestParams {
            meta: None,
            name: "self:tool_does_not_exist".to_string().into(),
            arguments: None,
            task: None,
        })
        .await
        .expect_err("unknown upstream tool should fail");

    match err {
        ServiceError::McpError(data) => {
            assert_ne!(data.code, ErrorCode::INTERNAL_ERROR);
            assert!(
                data.code == ErrorCode::INVALID_PARAMS || data.code == ErrorCode::METHOD_NOT_FOUND
            );
        }
        other => panic!("expected MCP protocol error, got: {other}"),
    }

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_rejects_invalid_list_cursor_with_mcp_invalid_params() -> anyhow::Result<()> {
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;

    let xdg_config_home = temp.path();
    let gambi_config_dir = xdg_config_home.join("gambi");
    std::fs::create_dir_all(&gambi_config_dir)?;
    write_config(
        &gambi_config_dir.join("config.json"),
        "{\n  \"servers\": []\n}\n",
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
    let err = client
        .list_tools(Some(PaginatedRequestParams {
            meta: None,
            cursor: Some("not-a-number".to_string()),
        }))
        .await
        .expect_err("invalid cursor must fail");

    match err {
        ServiceError::McpError(data) => {
            assert_eq!(data.code, ErrorCode::INVALID_PARAMS);
            assert!(data.message.contains("invalid cursor"));
        }
        other => panic!("expected MCP invalid params error, got: {other}"),
    }

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_preserves_input_schema_for_namespaced_tools() -> anyhow::Result<()> {
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;

    let xdg_config_home = temp.path();
    let gambi_config_dir = xdg_config_home.join("gambi");
    std::fs::create_dir_all(&gambi_config_dir)?;

    let self_url = format!(
        "stdio:///{bin}?arg=serve&arg=--no-exec&arg=--admin-port&arg=0",
        bin = bin
    );

    write_config(
        &gambi_config_dir.join("config.json"),
        &format!(
            "{{\n  \"servers\": [\n    {{ \"name\": \"self\", \"url\": \"{}\" }}\n  ]\n}}\n",
            self_url
        ),
    )?;
    write_config(&gambi_config_dir.join("tokens.json"), "{}\n")?;

    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.arg("serve");
        cmd.arg("--admin-port");
        cmd.arg("0");
        cmd.env("XDG_CONFIG_HOME", xdg_config_home);
    }))?;

    let client = ().serve(transport).await?;
    let tools = client.list_all_tools().await?;

    let local = tools
        .iter()
        .find(|tool| tool.name.as_ref() == "gambi_list_servers")
        .expect("local tool should exist");
    let namespaced = tools
        .iter()
        .find(|tool| tool.name.as_ref() == "self:gambi_list_servers")
        .expect("namespaced upstream tool should exist");

    assert_eq!(local.input_schema, namespaced.input_schema);

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_applies_configured_tool_description_overrides() -> anyhow::Result<()> {
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;

    let xdg_config_home = temp.path();
    let gambi_config_dir = xdg_config_home.join("gambi");
    std::fs::create_dir_all(&gambi_config_dir)?;

    let self_url = format!(
        "stdio:///{bin}?arg=serve&arg=--no-exec&arg=--admin-port&arg=0",
        bin = bin
    );

    write_config(
        &gambi_config_dir.join("config.json"),
        &format!(
            r#"{{
  "servers": [
    {{ "name": "self", "url": "{self_url}" }}
  ],
  "tool_description_overrides": {{
    "self": {{
      "gambi_list_servers": "Custom server list description"
    }}
  }}
}}
"#,
        ),
    )?;
    write_config(&gambi_config_dir.join("tokens.json"), "{}\n")?;

    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.arg("serve");
        cmd.arg("--admin-port");
        cmd.arg("0");
        cmd.env("XDG_CONFIG_HOME", xdg_config_home);
    }))?;

    let client = ().serve(transport).await?;
    let tools = client.list_all_tools().await?;

    let namespaced = tools
        .iter()
        .find(|tool| tool.name.as_ref() == "self:gambi_list_servers")
        .expect("namespaced upstream tool should exist");

    assert_eq!(
        namespaced.description.as_deref(),
        Some("Custom server list description")
    );

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
