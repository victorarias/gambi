use std::collections::HashSet;
use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::auth::{TokenState, prune_token_state_for_server_names};
use crate::config::{AppConfig, ConfigStore, ServerConfig};
use crate::server;

#[derive(Debug, Parser)]
#[command(name = "gambi", version, about = "Local MCP aggregator")]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Serve(ServeArgs),
    Add(AddArgs),
    List,
    Remove(RemoveArgs),
    Export(ExportArgs),
    Import(ImportArgs),
    #[command(name = "__fixture_progress_server", hide = true)]
    FixtureProgressServer,
}

#[derive(Debug, Clone, Args)]
pub struct ServeArgs {
    /// Loopback address for the admin UI.
    #[arg(long, default_value = "127.0.0.1")]
    pub admin_host: IpAddr,
    /// Admin UI port.
    #[arg(long, default_value_t = 3333)]
    pub admin_port: u16,
    /// Optional file path to write the resolved admin listener address.
    #[arg(long, hide = true)]
    pub admin_port_file: Option<PathBuf>,
    /// Disable the `gambi_execute` MCP tool.
    #[arg(long, default_value_t = false)]
    pub no_exec: bool,
}

#[derive(Debug, Args)]
struct AddArgs {
    name: String,
    url: String,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, run};
    use crate::auth::{OAuthServerHealth, OAuthToken, TokenState};
    use crate::config::{AppConfig, ConfigStore, ServerConfig};

    #[test]
    fn parses_serve_defaults() {
        let cli = Cli::parse_from(["gambi", "serve"]);
        let debug = format!("{cli:?}");
        assert!(debug.contains("admin_host: 127.0.0.1"));
        assert!(debug.contains("admin_port: 3333"));
        assert!(debug.contains("no_exec: false"));
    }

    #[test]
    fn parses_serve_no_exec_flag() {
        let cli = Cli::parse_from(["gambi", "serve", "--no-exec"]);
        let debug = format!("{cli:?}");
        assert!(debug.contains("no_exec: true"));
    }

    #[test]
    fn parses_export_and_import_commands() {
        let export = Cli::parse_from(["gambi", "export", "--out", "/tmp/config.json"]);
        let export_debug = format!("{export:?}");
        assert!(export_debug.contains("Export"));
        assert!(export_debug.contains("/tmp/config.json"));

        let import = Cli::parse_from(["gambi", "import", "/tmp/config.json"]);
        let import_debug = format!("{import:?}");
        assert!(import_debug.contains("Import"));
        assert!(import_debug.contains("/tmp/config.json"));
    }

    #[tokio::test]
    async fn rejects_non_loopback_admin_host() {
        let cli = Cli::parse_from(["gambi", "serve", "--admin-host", "0.0.0.0"]);
        let store =
            ConfigStore::with_base_dir(tempfile::tempdir().expect("tempdir").path().join("gambi"));

        let err = run(cli, store)
            .await
            .expect_err("non-loopback host must be rejected");
        assert!(err.to_string().contains("loopback-only"));
    }

    #[tokio::test]
    async fn remove_prunes_orphaned_oauth_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ConfigStore::with_base_dir(temp.path().join("gambi"));
        store
            .replace(AppConfig {
                servers: vec![ServerConfig {
                    name: "port".to_string(),
                    url: "https://example.com/mcp".to_string(),
                    oauth: None,
                }],
                tool_description_overrides: std::collections::BTreeMap::new(),
            })
            .expect("config should persist");

        store
            .update_tokens::<TokenState, _, _>(|tokens| {
                tokens.oauth_tokens.insert(
                    "port".to_string(),
                    OAuthToken {
                        access_token: "token".to_string(),
                        refresh_token: None,
                        token_type: Some("Bearer".to_string()),
                        expires_at_epoch_seconds: None,
                        scopes: vec![],
                    },
                );
                tokens.oauth_health.insert(
                    "port".to_string(),
                    OAuthServerHealth {
                        degraded: true,
                        last_error: Some("boom".to_string()),
                        updated_at_epoch_seconds: None,
                        next_retry_after_epoch_seconds: None,
                    },
                );
                Ok(())
            })
            .expect("token state should persist");

        let cli = Cli::parse_from(["gambi", "remove", "port"]);
        run(cli, store.clone())
            .await
            .expect("remove command should succeed");

        let tokens: TokenState = store.load_tokens().expect("token state should load");
        assert!(tokens.oauth_tokens.is_empty());
        assert!(tokens.oauth_health.is_empty());
    }
}

#[derive(Debug, Args)]
struct RemoveArgs {
    name: String,
}

#[derive(Debug, Args)]
struct ExportArgs {
    /// Output file path. Defaults to stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ImportArgs {
    /// Path to JSON config file to import.
    path: PathBuf,
}

pub async fn run(cli: Cli, store: ConfigStore) -> Result<()> {
    match cli.command {
        Commands::Serve(args) => {
            if !args.admin_host.is_loopback() {
                bail!("admin host must be loopback-only (127.0.0.1 or ::1)");
            }
            server::serve(args, store).await
        }
        Commands::Add(args) => {
            store.update(|cfg| {
                cfg.add_server(ServerConfig {
                    name: args.name,
                    url: args.url,
                    oauth: None,
                })
            })?;
            println!("server added");
            Ok(())
        }
        Commands::List => {
            let cfg = store.load()?;
            if cfg.servers.is_empty() {
                println!("no servers configured");
                return Ok(());
            }

            for server in cfg.servers {
                println!("{}\t{}", server.name, server.url);
            }
            Ok(())
        }
        Commands::Remove(args) => {
            let removed = store.update(|cfg| Ok(cfg.remove_server(&args.name)))?;
            let current_cfg = store.load()?;
            let allowed_servers = current_cfg
                .servers
                .iter()
                .map(|server| server.name.clone())
                .collect::<HashSet<_>>();
            let _ = store.update_tokens::<TokenState, _, _>(move |tokens| {
                Ok(prune_token_state_for_server_names(tokens, &allowed_servers))
            })?;
            if removed {
                println!("server removed");
            } else {
                println!("server not found");
            }
            Ok(())
        }
        Commands::Export(args) => {
            let cfg = store.load()?;
            let serialized =
                serde_json::to_string_pretty(&cfg).context("failed to serialize config")?;
            let payload = format!("{serialized}\n");

            if let Some(path) = args.out {
                std::fs::write(&path, payload).with_context(|| {
                    format!("failed to write exported config to {}", path.display())
                })?;
                println!("config exported");
            } else {
                print!("{payload}");
            }
            Ok(())
        }
        Commands::Import(args) => {
            let raw = std::fs::read_to_string(&args.path)
                .with_context(|| format!("failed to read {}", args.path.display()))?;
            let cfg: AppConfig = serde_json::from_str(&raw)
                .with_context(|| format!("invalid JSON in {}", args.path.display()))?;
            store.replace(cfg)?;
            let current_cfg = store.load()?;
            let allowed_servers = current_cfg
                .servers
                .iter()
                .map(|server| server.name.clone())
                .collect::<HashSet<_>>();
            let _ = store.update_tokens::<TokenState, _, _>(move |tokens| {
                Ok(prune_token_state_for_server_names(tokens, &allowed_servers))
            })?;
            println!("config imported");
            Ok(())
        }
        Commands::FixtureProgressServer => crate::fixture_progress::run().await,
    }
}
