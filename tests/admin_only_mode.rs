use std::path::Path;
use std::time::Duration;

/// When stdin is closed (no MCP client connected), the MCP stdio task fails
/// but the admin UI keeps running. This happens when running `gambi serve`
/// from a terminal or when stdin is piped from /dev/null.
#[tokio::test]
async fn admin_stays_alive_when_stdin_closes() -> anyhow::Result<()> {
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;
    let gambi_config_dir = temp.path().join("gambi");
    std::fs::create_dir_all(&gambi_config_dir)?;

    write_config(
        &gambi_config_dir.join("config.json"),
        r#"{ "servers": [] }"#,
    )?;
    write_config(&gambi_config_dir.join("tokens.json"), "{}\n")?;

    let port_file = temp.path().join("admin-port");
    let mut child = tokio::process::Command::new(bin)
        .arg("serve")
        .arg("--admin-port")
        .arg("0")
        .arg("--admin-port-file")
        .arg(&port_file)
        .env("GAMBI_CONFIG_DIR", &gambi_config_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    // Wait for admin port file to appear
    let admin_port: u16 = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(&port_file).await
                && let Ok(port) = contents.trim().parse::<u16>()
            {
                return port;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    let admin_addr = format!("127.0.0.1:{admin_port}");

    // Give time for the MCP task to fail (stdin is /dev/null → immediate EOF)
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Admin UI should still respond despite MCP task having failed
    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://{admin_addr}/health"))
        .timeout(Duration::from_secs(3))
        .send()
        .await?;
    assert_eq!(response.status(), 200);

    child.kill().await?;
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
