use std::path::Path;
use std::time::Duration;

use rmcp::{
    ServiceExt,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde::Deserialize;
use tokio::process::Command;

#[derive(Debug, Deserialize)]
struct AuthStatusesResponse {
    statuses: Vec<AuthStatus>,
}

#[derive(Debug, Deserialize)]
struct AuthStatus {
    server: String,
    oauth_configured: bool,
    has_token: bool,
    expires_at_epoch_seconds: Option<u64>,
    degraded: bool,
    last_error: Option<String>,
    next_retry_after_epoch_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AuthStartResponse {
    server: String,
    auth_url: String,
    state: String,
}

#[tokio::test]
async fn admin_exposes_oauth_status_and_start_endpoints() -> anyhow::Result<()> {
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;
    let gambi_config_dir = temp.path().join("gambi");
    std::fs::create_dir_all(&gambi_config_dir)?;

    write_config(
        &gambi_config_dir.join("config.json"),
        r#"{
  "servers": [
    {
      "name": "port",
      "url": "https://example.com/mcp",
      "oauth": {
        "authorize_url": "https://example.com/oauth/authorize",
        "token_url": "https://example.com/oauth/token",
        "client_id": "client-id",
        "scopes": ["read"]
      }
    }
  ]
}
"#,
    )?;
    write_config(&gambi_config_dir.join("tokens.json"), "{}\n")?;

    let admin_port_file = temp.path().join("admin-port.txt");
    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.arg("serve");
        cmd.arg("--admin-port");
        cmd.arg("0");
        cmd.arg("--admin-port-file");
        cmd.arg(&admin_port_file);
        cmd.arg("--no-exec");
        cmd.env("GAMBI_CONFIG_DIR", &gambi_config_dir);
    }))?;

    let client = ().serve(transport).await?;
    let http = reqwest::Client::new();
    let admin_port = wait_for_admin_port(&http, &admin_port_file).await?;

    let statuses = http
        .get(format!("http://127.0.0.1:{admin_port}/auth/status"))
        .send()
        .await?
        .error_for_status()?
        .json::<AuthStatusesResponse>()
        .await?;
    assert_eq!(statuses.statuses.len(), 1);
    assert_eq!(statuses.statuses[0].server, "port");
    assert!(statuses.statuses[0].oauth_configured);
    assert!(!statuses.statuses[0].has_token);
    assert!(statuses.statuses[0].expires_at_epoch_seconds.is_none());
    assert!(!statuses.statuses[0].degraded);
    assert!(statuses.statuses[0].last_error.is_none());
    assert!(
        statuses.statuses[0]
            .next_retry_after_epoch_seconds
            .is_none()
    );

    let started = http
        .post(format!("http://127.0.0.1:{admin_port}/auth/start"))
        .json(&serde_json::json!({ "server": "port" }))
        .send()
        .await?
        .error_for_status()?
        .json::<AuthStartResponse>()
        .await?;
    assert_eq!(started.server, "port");
    assert!(!started.state.is_empty());
    assert!(started.auth_url.contains("code_challenge="));
    assert!(started.auth_url.contains("code_challenge_method=S256"));

    let failed_refresh = http
        .post(format!("http://127.0.0.1:{admin_port}/auth/refresh"))
        .json(&serde_json::json!({ "server": "port" }))
        .send()
        .await?;
    assert_eq!(failed_refresh.status(), reqwest::StatusCode::BAD_REQUEST);

    let statuses_after_failure = http
        .get(format!("http://127.0.0.1:{admin_port}/auth/status"))
        .send()
        .await?
        .error_for_status()?
        .json::<AuthStatusesResponse>()
        .await?;
    assert_eq!(statuses_after_failure.statuses.len(), 1);
    assert!(statuses_after_failure.statuses[0].degraded);
    assert!(
        statuses_after_failure.statuses[0]
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("no oauth token found")
    );

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
