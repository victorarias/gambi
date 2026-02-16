use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    service::RunningService,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use tokio::process::Command;

pub fn fixture_server_url() -> String {
    let bin = env!("CARGO_BIN_EXE_gambi");
    format!("stdio:///{bin}?arg=__fixture_progress_server", bin = bin)
}

pub async fn spawn_gambi_with_config<H>(
    handler: H,
    config_json: &str,
    tokens_json: &str,
    no_exec: bool,
    extra_env: &BTreeMap<String, String>,
) -> Result<(RunningService<RoleClient, H>, tempfile::TempDir)>
where
    H: ClientHandler + Send + Sync + 'static,
{
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;

    let gambi_config_dir = temp.path().join("gambi");
    std::fs::create_dir_all(&gambi_config_dir)?;

    write_config(&gambi_config_dir.join("config.json"), config_json)?;
    write_config(&gambi_config_dir.join("tokens.json"), tokens_json)?;

    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.arg("serve");
        cmd.arg("--admin-port");
        cmd.arg("0");
        if no_exec {
            cmd.arg("--no-exec");
        }
        cmd.env("GAMBI_CONFIG_DIR", &gambi_config_dir);
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
    }))?;

    let client = handler.serve(transport).await?;
    Ok((client, temp))
}

pub fn write_config(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
