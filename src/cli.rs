use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde::Deserialize;

use crate::auth::{TokenState, prune_token_state_for_server_names};
use crate::config::{
    AppConfig, ConfigStore, ExposureMode, ServerConfig, ToolPolicyMode, TransportMode,
};
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
    Stop(StopArgs),
    Add(AddArgs),
    Exposure(ExposureArgs),
    Policy(PolicyArgs),
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
    /// Run only the admin/auth services without MCP stdio.
    #[arg(long, hide = true, default_value_t = false)]
    pub admin_only: bool,
}

#[derive(Debug, Args)]
struct AddArgs {
    name: String,
    url: String,
    /// Transport mode: auto (detect from URL), sse, or streamable-http.
    #[arg(long, default_value = "auto")]
    transport: String,
    /// Exposure mode for gambi_help: passthrough|compact|names-only|server-only.
    #[arg(long, default_value = "passthrough")]
    exposure: String,
    /// Tool policy mode: heuristic|all-safe|all-escalated|custom.
    #[arg(long, default_value = "heuristic")]
    policy: String,
}

#[derive(Debug, Args)]
struct ExposureArgs {
    /// Configured server name.
    name: String,
    /// Exposure mode: passthrough|compact|names-only|server-only.
    exposure: String,
}

#[derive(Debug, Args)]
struct PolicyArgs {
    /// Configured server name.
    name: String,
    /// Policy mode: heuristic|all-safe|all-escalated|custom.
    policy: String,
}

#[derive(Debug, Args)]
struct StopArgs {
    /// Loopback address for the shared admin daemon.
    #[arg(long, default_value = "127.0.0.1")]
    admin_host: IpAddr,
    /// Admin daemon port.
    #[arg(long, default_value_t = 3333)]
    admin_port: u16,
    /// Force kill if graceful shutdown does not stop the daemon.
    #[arg(long, default_value_t = false)]
    force: bool,
}

#[derive(Debug, Deserialize)]
struct DaemonStateRecord {
    pid: u32,
    host: String,
    port: u16,
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
    fn parses_stop_defaults() {
        let cli = Cli::parse_from(["gambi", "stop"]);
        let debug = format!("{cli:?}");
        assert!(debug.contains("admin_host: 127.0.0.1"));
        assert!(debug.contains("admin_port: 3333"));
        assert!(debug.contains("force: false"));
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

    #[test]
    fn parses_add_with_exposure_mode() {
        let cli = Cli::parse_from([
            "gambi",
            "add",
            "atlassian",
            "https://mcp.atlassian.com/v1/sse",
            "--exposure",
            "names-only",
        ]);
        let debug = format!("{cli:?}");
        assert!(debug.contains("exposure: \"names-only\""));
    }

    #[test]
    fn parses_add_with_policy_mode() {
        let cli = Cli::parse_from([
            "gambi",
            "add",
            "atlassian",
            "https://mcp.atlassian.com/v1/sse",
            "--policy",
            "all-escalated",
        ]);
        let debug = format!("{cli:?}");
        assert!(debug.contains("policy: \"all-escalated\""));
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
                    transport: Default::default(),
                    exposure_mode: Default::default(),
                }],
                server_tool_policy_modes: std::collections::BTreeMap::new(),
                tool_description_overrides: std::collections::BTreeMap::new(),
                tool_policy_overrides: std::collections::BTreeMap::new(),
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
                tokens.registered_clients.insert(
                    "port".to_string(),
                    crate::auth::RegisteredClient {
                        client_id: "dynamic-client".to_string(),
                        client_secret: None,
                        registration_client_uri: None,
                        registration_access_token: None,
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
        assert!(tokens.registered_clients.is_empty());
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
        Commands::Stop(args) => stop_daemon(args, store).await,
        Commands::Add(args) => {
            let transport = parse_transport_flag(&args.transport)?;
            let exposure_mode = parse_exposure_flag(&args.exposure)?;
            let policy_mode = parse_policy_flag(&args.policy)?;
            let server_name = args.name.clone();
            store.update(|cfg| {
                cfg.add_server(ServerConfig {
                    name: args.name,
                    url: args.url,
                    oauth: None,
                    transport,
                    exposure_mode,
                })?;
                cfg.set_server_tool_policy_mode(&server_name, policy_mode)
            })?;
            println!("server added");
            Ok(())
        }
        Commands::Exposure(args) => {
            let exposure_mode = parse_exposure_flag(&args.exposure)?;
            let server_name = args.name.clone();
            store.update(move |cfg| cfg.set_server_exposure_mode(&server_name, exposure_mode))?;
            println!("server exposure updated");
            Ok(())
        }
        Commands::Policy(args) => {
            let policy_mode = parse_policy_flag(&args.policy)?;
            let server_name = args.name.clone();
            store.update(move |cfg| cfg.set_server_tool_policy_mode(&server_name, policy_mode))?;
            println!("server policy updated");
            Ok(())
        }
        Commands::List => {
            let cfg = store.load()?;
            if cfg.servers.is_empty() {
                println!("no servers configured");
                return Ok(());
            }

            for server in &cfg.servers {
                let policy = cfg.server_tool_policy_mode_for(&server.name);
                println!(
                    "{}\t{}\t{}\t{}",
                    server.name,
                    server.url,
                    server.exposure_mode.as_str(),
                    policy.as_str()
                );
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

async fn stop_daemon(args: StopArgs, store: ConfigStore) -> Result<()> {
    if !args.admin_host.is_loopback() {
        bail!("admin host must be loopback-only (127.0.0.1 or ::1)");
    }

    let base_url = format!(
        "http://{}",
        SocketAddr::from((args.admin_host, args.admin_port))
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .context("failed to build HTTP client for daemon stop")?;

    let shutdown_url = format!("{base_url}/shutdown");
    if let Ok(response) = client.post(&shutdown_url).send().await
        && response.status().is_success()
    {
        for _ in 0..30 {
            if !admin_health_ok(&client, &base_url).await {
                let _ = clear_daemon_state_file(&store);
                println!("daemon stopped");
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    let Some(state) = read_daemon_state_file(&store)? else {
        println!("daemon not running");
        return Ok(());
    };
    if state.host != args.admin_host.to_string() || state.port != args.admin_port {
        println!("daemon not running");
        return Ok(());
    }

    if !process_exists(state.pid)? {
        let _ = clear_daemon_state_file(&store);
        println!("daemon not running");
        return Ok(());
    }

    terminate_process(state.pid, "TERM")?;
    for _ in 0..30 {
        if !process_exists(state.pid)? {
            let _ = clear_daemon_state_file(&store);
            println!("daemon stopped");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if args.force {
        terminate_process(state.pid, "KILL")?;
        for _ in 0..20 {
            if !process_exists(state.pid)? {
                let _ = clear_daemon_state_file(&store);
                println!("daemon force-stopped");
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    bail!("daemon is still running; rerun with --force")
}

async fn admin_health_ok(client: &reqwest::Client, base_url: &str) -> bool {
    let health_url = format!("{base_url}/health");
    match client.get(&health_url).send().await {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    }
}

fn daemon_state_file(store: &ConfigStore) -> PathBuf {
    store.daemon_state_file()
}

fn read_daemon_state_file(store: &ConfigStore) -> Result<Option<DaemonStateRecord>> {
    let path = daemon_state_file(store);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    let parsed: DaemonStateRecord = serde_json::from_str(&raw)
        .with_context(|| format!("invalid daemon state in {}", path.display()))?;
    Ok(Some(parsed))
}

fn clear_daemon_state_file(store: &ConfigStore) -> Result<()> {
    let path = daemon_state_file(store);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn process_exists(pid: u32) -> Result<bool> {
    let status = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .context("failed to execute 'kill -0'")?;
    Ok(status.success())
}

fn terminate_process(pid: u32, signal: &str) -> Result<()> {
    let status = std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .with_context(|| format!("failed to execute 'kill -{signal}'"))?;
    if !status.success() {
        bail!("kill -{signal} failed for pid {pid}");
    }
    Ok(())
}

fn parse_transport_flag(raw: &str) -> Result<TransportMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(TransportMode::Auto),
        "sse" => Ok(TransportMode::Sse),
        "streamable-http" => Ok(TransportMode::StreamableHttp),
        other => bail!("invalid transport '{other}', expected auto|sse|streamable-http"),
    }
}

fn parse_exposure_flag(raw: &str) -> Result<ExposureMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "passthrough" | "full" => Ok(ExposureMode::Passthrough),
        "compact" | "truncated" => Ok(ExposureMode::Compact),
        "names-only" | "names" => Ok(ExposureMode::NamesOnly),
        "server-only" | "server" => Ok(ExposureMode::ServerOnly),
        other => {
            bail!("invalid exposure '{other}', expected passthrough|compact|names-only|server-only")
        }
    }
}

fn parse_policy_flag(raw: &str) -> Result<ToolPolicyMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "heuristic" => Ok(ToolPolicyMode::Heuristic),
        "all-safe" | "safe" => Ok(ToolPolicyMode::AllSafe),
        "all-escalated" | "escalated" => Ok(ToolPolicyMode::AllEscalated),
        "custom" => Ok(ToolPolicyMode::Custom),
        other => {
            bail!("invalid policy '{other}', expected heuristic|all-safe|all-escalated|custom")
        }
    }
}
