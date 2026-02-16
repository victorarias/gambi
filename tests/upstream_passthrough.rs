use std::path::Path;

use rmcp::{
    ServiceError, ServiceExt,
    model::{CallToolRequestParams, ErrorCode, PaginatedRequestParams},
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::process::Command;

#[derive(Debug, Deserialize)]
struct HelpToolSummary {
    namespaced_name: String,
    #[allow(dead_code)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HelpServerSummary {
    name: String,
    tools: Vec<HelpToolSummary>,
}

#[derive(Debug, Deserialize)]
struct HelpToolDetail {
    #[allow(dead_code)]
    namespaced_name: String,
    description: String,
    input_schema: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct HelpResponse {
    servers: Vec<HelpServerSummary>,
    tool: Option<HelpToolDetail>,
}

#[tokio::test]
async fn gambi_routes_namespaced_upstream_stdio_tool_calls() -> anyhow::Result<()> {
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;

    let gambi_config_dir = temp.path().join("gambi");
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
        cmd.env("GAMBI_CONFIG_DIR", &gambi_config_dir);
    }))?;

    let client = ().serve(transport).await?;

    let tools = client.list_all_tools().await?;
    assert!(tools.iter().any(|tool| tool.name.as_ref() == "gambi_help"));
    let help = gambi_help(&client, Some("self"), None).await?;
    let self_server = help
        .servers
        .iter()
        .find(|server| server.name == "self")
        .expect("self server should exist");
    assert!(
        self_server
            .tools
            .iter()
            .any(|tool| tool.namespaced_name == "self:gambi_list_servers")
    );

    let err = client
        .call_tool(CallToolRequestParams {
            meta: None,
            name: "self:gambi_list_servers".to_string().into(),
            arguments: None,
            task: None,
        })
        .await
        .expect_err("direct namespaced invocation must be rejected");
    match err {
        ServiceError::McpError(data) => {
            assert_eq!(data.code, ErrorCode::INVALID_PARAMS);
            assert!(
                data.message
                    .contains("direct upstream tool invocation is disabled")
            );
        }
        other => panic!("expected MCP invalid params error, got: {other}"),
    }

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_preserves_upstream_mcp_error_codes_for_routed_calls() -> anyhow::Result<()> {
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;

    let gambi_config_dir = temp.path().join("gambi");
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
        cmd.env("GAMBI_CONFIG_DIR", &gambi_config_dir);
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
            assert_eq!(data.code, ErrorCode::INVALID_PARAMS);
            assert!(
                data.message
                    .contains("direct upstream tool invocation is disabled")
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

    let gambi_config_dir = temp.path().join("gambi");
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
        cmd.env("GAMBI_CONFIG_DIR", &gambi_config_dir);
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

    let gambi_config_dir = temp.path().join("gambi");
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
        cmd.env("GAMBI_CONFIG_DIR", &gambi_config_dir);
    }))?;

    let client = ().serve(transport).await?;
    let tools = client.list_all_tools().await?;

    let local = tools
        .iter()
        .find(|tool| tool.name.as_ref() == "gambi_list_servers")
        .expect("local tool should exist");
    let help = gambi_help(&client, Some("self"), Some("self:gambi_list_servers")).await?;
    let detail = help.tool.expect("expected tool detail in gambi_help");

    assert_eq!(local.input_schema.as_ref(), &detail.input_schema);

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_applies_configured_tool_description_overrides() -> anyhow::Result<()> {
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;

    let gambi_config_dir = temp.path().join("gambi");
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
        cmd.env("GAMBI_CONFIG_DIR", &gambi_config_dir);
    }))?;

    let client = ().serve(transport).await?;
    let help = gambi_help(&client, Some("self"), Some("self:gambi_list_servers")).await?;
    let namespaced = help.tool.expect("expected tool detail in gambi_help");

    assert_eq!(namespaced.description, "Custom server list description");

    let _ = client.cancel().await;
    Ok(())
}

async fn gambi_help(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    server: Option<&str>,
    tool: Option<&str>,
) -> anyhow::Result<HelpResponse> {
    let arguments = {
        let mut args = serde_json::Map::new();
        if let Some(server_name) = server {
            args.insert("server".to_string(), json!(server_name));
        }
        if let Some(tool_name) = tool {
            args.insert("tool".to_string(), json!(tool_name));
        }
        if args.is_empty() { None } else { Some(args) }
    };

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
