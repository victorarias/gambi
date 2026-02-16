use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use rmcp::{
    ServiceExt,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use tokio::process::Command;

#[tokio::test]
async fn second_instance_continues_when_admin_port_is_in_use() -> anyhow::Result<()> {
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;
    let gambi_config_dir = temp.path().join("gambi");
    std::fs::create_dir_all(&gambi_config_dir)?;

    write_config(
        &gambi_config_dir.join("config.json"),
        "{\n  \"servers\": []\n}\n",
    )?;
    write_config(&gambi_config_dir.join("tokens.json"), "{}\n")?;

    let admin_port = reserve_loopback_port()?;
    let mut first = Command::new(bin)
        .arg("serve")
        .arg("--admin-port")
        .arg(admin_port.to_string())
        .env("GAMBI_CONFIG_DIR", &gambi_config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    wait_for_admin_health(admin_port).await?;

    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.arg("serve");
        cmd.arg("--admin-port");
        cmd.arg(admin_port.to_string());
        cmd.env("GAMBI_CONFIG_DIR", &gambi_config_dir);
    }))?;
    let client = ().serve(transport).await?;

    let tools = client.list_all_tools().await?;
    assert!(
        tools
            .iter()
            .any(|tool| tool.name.as_ref() == "gambi_list_servers"),
        "expected second instance to remain MCP-functional even when admin bind conflicts"
    );

    let _ = client.cancel().await;
    first.kill().await?;
    Ok(())
}

fn reserve_loopback_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

async fn wait_for_admin_health(admin_port: u16) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    for _ in 0..100 {
        let response = client
            .get(format!("http://127.0.0.1:{admin_port}/health"))
            .timeout(Duration::from_millis(300))
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

fn write_config(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
