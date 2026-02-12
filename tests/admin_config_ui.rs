use std::path::Path;
use std::time::Duration;

use rmcp::{
    ServiceExt,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde::Deserialize;
use tokio::process::Command;

#[derive(Debug, Deserialize)]
struct ServersResponse {
    servers: Vec<ServerEntry>,
}

#[derive(Debug, Deserialize)]
struct ServerEntry {
    name: String,
    url: String,
}

#[tokio::test]
async fn admin_can_add_remove_servers_and_set_tool_description_overrides() -> anyhow::Result<()> {
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;
    let xdg_config_home = temp.path();
    let gambi_config_dir = xdg_config_home.join("gambi");
    std::fs::create_dir_all(&gambi_config_dir)?;

    write_config(
        &gambi_config_dir.join("config.json"),
        "{\n  \"servers\": []\n}\n",
    )?;
    write_config(
        &gambi_config_dir.join("tokens.json"),
        r#"{
  "oauth_tokens": {
    "fixture": {
      "access_token": "fixture-token",
      "token_type": "Bearer"
    },
    "orphan": {
      "access_token": "orphan-token",
      "token_type": "Bearer"
    }
  },
  "oauth_health": {
    "fixture": { "degraded": true, "last_error": "boom" },
    "orphan": { "degraded": true, "last_error": "boom" }
  }
}
"#,
    )?;

    let admin_port_file = temp.path().join("admin-port.txt");
    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.arg("serve");
        cmd.arg("--admin-port");
        cmd.arg("0");
        cmd.arg("--admin-port-file");
        cmd.arg(&admin_port_file);
        cmd.arg("--no-exec");
        cmd.env("XDG_CONFIG_HOME", xdg_config_home);
    }))?;

    let client = ().serve(transport).await?;
    let http = reqwest::Client::new();
    let admin_port = wait_for_admin_port(&http, &admin_port_file).await?;

    let fixture_url = format!("stdio:///{bin}?arg=__fixture_progress_server", bin = bin);
    http.post(format!("http://127.0.0.1:{admin_port}/servers"))
        .json(&serde_json::json!({
            "name": "fixture",
            "url": fixture_url,
        }))
        .send()
        .await?
        .error_for_status()?;

    let servers = http
        .get(format!("http://127.0.0.1:{admin_port}/servers"))
        .send()
        .await?
        .error_for_status()?
        .json::<ServersResponse>()
        .await?;
    assert_eq!(servers.servers.len(), 1);
    assert_eq!(servers.servers[0].name, "fixture");
    assert!(
        servers.servers[0].url.starts_with("stdio://"),
        "expected stdio url, got {}",
        servers.servers[0].url
    );

    http.post(format!("http://127.0.0.1:{admin_port}/tool-descriptions"))
        .json(&serde_json::json!({
            "server": "fixture",
            "tool": "fixture_echo",
            "description": "Custom fixture echo description",
        }))
        .send()
        .await?
        .error_for_status()?;

    // wait for discovery to observe latest config update
    let mut saw_override = false;
    for _ in 0..20 {
        let tools = client.list_all_tools().await?;
        if let Some(tool) = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "fixture:fixture_echo")
            && tool.description.as_deref() == Some("Custom fixture echo description")
        {
            saw_override = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        saw_override,
        "expected overridden description to appear in list_all_tools"
    );

    http.delete(format!("http://127.0.0.1:{admin_port}/servers/fixture"))
        .send()
        .await?
        .error_for_status()?;

    let servers_after_delete = http
        .get(format!("http://127.0.0.1:{admin_port}/servers"))
        .send()
        .await?
        .error_for_status()?
        .json::<ServersResponse>()
        .await?;
    assert!(servers_after_delete.servers.is_empty());

    let persisted_tokens = std::fs::read_to_string(gambi_config_dir.join("tokens.json"))?;
    let parsed: serde_json::Value = serde_json::from_str(&persisted_tokens)?;
    assert_eq!(parsed["oauth_tokens"], serde_json::json!({}));
    assert_eq!(parsed["oauth_health"], serde_json::json!({}));

    let _ = client.cancel().await;
    Ok(())
}

async fn wait_for_health(http: &reqwest::Client, admin_port: u16) -> anyhow::Result<()> {
    for _ in 0..50 {
        let response = http
            .get(format!("http://127.0.0.1:{admin_port}/health"))
            .send()
            .await;
        if let Ok(resp) = response
            && resp.status().is_success()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("admin health endpoint did not become ready in time")
}

async fn wait_for_admin_port(http: &reqwest::Client, port_file: &Path) -> anyhow::Result<u16> {
    for _ in 0..100 {
        match tokio::fs::read_to_string(port_file).await {
            Ok(raw) => {
                let port = raw
                    .trim()
                    .parse::<u16>()
                    .map_err(|err| anyhow::anyhow!("invalid admin port file contents: {err}"))?;
                wait_for_health(http, port).await?;
                return Ok(port);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(err) => return Err(err.into()),
        }
    }
    anyhow::bail!("admin port file did not become ready in time")
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
