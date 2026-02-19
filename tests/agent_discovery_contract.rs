mod support;

use rmcp::model::CallToolRequestParams;
use serde_json::{Map, Value, json};

#[tokio::test]
async fn agent_discovery_applies_compact_mode_in_help_summaries() -> anyhow::Result<()> {
    let fixture_url = support::fixture_server_url();
    let long_description = "This is a very long fixture tool description used to verify compact exposure behavior through the agent-visible gambi_help response. ".repeat(4);

    let config = json!({
        "servers": [
            {
                "name": "fixture",
                "url": fixture_url,
                "exposure_mode": "compact"
            }
        ],
        "tool_description_overrides": {
            "fixture": {
                "fixture_echo": long_description
            }
        }
    });
    let config_json = format!("{}\n", serde_json::to_string_pretty(&config)?);

    let (client, _temp) =
        support::spawn_gambi_with_config((), &config_json, "{}\n", false, &Default::default())
            .await?;

    let tools = client.list_all_tools().await?;
    assert!(tools.iter().any(|tool| tool.name.as_ref() == "gambi_help"));

    let help_all = gambi_help(&client, None, None).await?;
    let summary_all = find_summary_description(&help_all, "fixture", "fixture:fixture_echo")
        .ok_or_else(|| anyhow::anyhow!("fixture:fixture_echo missing from gambi_help()"))?;
    assert!(summary_all.ends_with("..."));
    assert!(summary_all.chars().count() <= 183);

    let help_server = gambi_help(&client, Some("fixture"), None).await?;
    let summary_server = find_summary_description(&help_server, "fixture", "fixture:fixture_echo")
        .ok_or_else(|| {
            anyhow::anyhow!("fixture:fixture_echo missing from gambi_help(server=fixture)")
        })?;
    assert!(summary_server.ends_with("..."));
    assert!(summary_server.chars().count() <= 183);

    let help_detail = gambi_help(&client, Some("fixture"), Some("fixture:fixture_echo")).await?;
    let detail_description = help_detail
        .pointer("/tool/description")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("gambi_help(tool=...) missing tool.description"))?;
    assert_eq!(detail_description, long_description);

    assert_no_agent_internal_fields(&help_all);

    let _ = client.cancel().await;
    Ok(())
}

fn find_summary_description<'a>(
    help: &'a Value,
    server_name: &str,
    namespaced_tool: &str,
) -> Option<&'a str> {
    let servers = help.get("servers")?.as_array()?;
    let server = servers
        .iter()
        .find(|server| server.get("name").and_then(Value::as_str) == Some(server_name))?;
    let tools = server.get("tools")?.as_array()?;
    tools
        .iter()
        .find(|tool| {
            tool.get("namespaced_name")
                .and_then(Value::as_str)
                .map(|name| name == namespaced_tool)
                .unwrap_or(false)
        })
        .and_then(|tool| tool.get("description"))
        .and_then(Value::as_str)
}

fn assert_no_agent_internal_fields(help: &Value) {
    if let Some(servers) = help.get("servers").and_then(Value::as_array) {
        for server in servers {
            assert!(server.get("exposure_mode").is_none());
            assert!(server.get("instruction_source").is_none());
            if let Some(tools) = server.get("tools").and_then(Value::as_array) {
                for tool in tools {
                    assert!(tool.get("policy_source").is_none());
                }
            }
        }
    }
    if let Some(tool) = help.get("tool") {
        assert!(tool.get("policy_source").is_none());
    }
}

async fn gambi_help(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    server: Option<&str>,
    tool: Option<&str>,
) -> anyhow::Result<Value> {
    let arguments = {
        let mut args = Map::new();
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
        return Ok(structured);
    }

    let first_text = result
        .content
        .first()
        .and_then(|content| content.as_text())
        .map(|text| text.text.clone())
        .ok_or_else(|| anyhow::anyhow!("gambi_help did not return parseable output"))?;

    Ok(serde_json::from_str(&first_text)?)
}
