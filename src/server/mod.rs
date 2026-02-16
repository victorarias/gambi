mod admin;
mod mcp;

use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const ADMIN_DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);
const ADMIN_DAEMON_HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(100);
const ADMIN_DAEMON_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
const ADMIN_DAEMON_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(2);
const ADMIN_DAEMON_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

use crate::auth::AuthManager;
use crate::cli::ServeArgs;
use crate::config::ConfigStore;
use crate::upstream::UpstreamManager;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceTask {
    Admin,
    Mcp,
    AuthRefresh,
    Heartbeat,
}

#[derive(Debug)]
struct TaskOutcome {
    task: ServiceTask,
    result: Result<(), anyhow::Error>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DaemonStateRecord {
    pid: u32,
    host: String,
    port: u16,
}

impl fmt::Display for ServiceTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServiceTask::Admin => write!(f, "admin"),
            ServiceTask::Mcp => write!(f, "mcp"),
            ServiceTask::AuthRefresh => write!(f, "auth-refresh"),
            ServiceTask::Heartbeat => write!(f, "heartbeat"),
        }
    }
}

pub async fn serve(args: ServeArgs, store: ConfigStore) -> Result<()> {
    info!(
        profile = store.runtime_profile().as_str(),
        token_store = store.token_store_mode(),
        "runtime profile and token storage resolved"
    );

    if store.runtime_profile() == crate::config::RuntimeProfile::Production
        && store.token_store_mode() != "keychain"
    {
        warn!(
            "production profile is running with file token storage; keychain is recommended for non-plaintext credential persistence"
        );
    }

    let weak_permissions = store.weak_permission_paths()?;
    for path in weak_permissions {
        warn!(
            path = %path.display(),
            "permissions are weaker than owner-only; expected 0600 files and 0700 dir"
        );
    }

    let exec_enabled = !args.no_exec;
    let admin_host = args.admin_host;
    let admin_port = args.admin_port;
    let admin_port_file = args.admin_port_file.clone();
    if exec_enabled {
        info!("gambi_execute enabled (default)");
    } else {
        info!("gambi_execute disabled with --no-exec");
    }

    let run_mcp_task = !args.admin_only;
    let mut run_admin_task = args.admin_only;
    let mut daemon_base_url = None;

    if args.admin_only {
        info!("running admin-only mode");
    } else if admin_port == 0 {
        run_admin_task = true;
        info!(
            "admin port is dynamic (0); shared daemon bootstrap disabled, running admin in-process"
        );
    } else if ensure_shared_admin_daemon(&args, &store).await? {
        run_admin_task = false;
        daemon_base_url = Some(admin_base_url(admin_host, admin_port));
        info!(
            admin_base_url = %daemon_base_url.as_deref().unwrap_or(""),
            "using shared admin daemon"
        );
    } else {
        run_admin_task = true;
        warn!("shared admin daemon unavailable; running admin in-process");
    }

    let mcp_controls_lifetime = run_mcp_task && !run_admin_task;
    let run_auth_refresh = run_admin_task;

    let shutdown = CancellationToken::new();
    let auth = AuthManager::new(store.clone());
    let upstream = UpstreamManager::new();

    let mut tasks: JoinSet<TaskOutcome> = JoinSet::new();
    if run_admin_task {
        let store = store.clone();
        let auth = auth.clone();
        let upstream = upstream.clone();
        let admin_shutdown = shutdown.child_token();
        let admin_idle_timeout = Some(ADMIN_DAEMON_IDLE_TIMEOUT);
        tasks.spawn(async move {
            let bind = admin::AdminBindOptions {
                host: admin_host,
                port: admin_port,
                port_file: admin_port_file,
            };
            TaskOutcome {
                task: ServiceTask::Admin,
                result: admin::run(
                    bind,
                    store,
                    auth,
                    upstream,
                    exec_enabled,
                    admin_shutdown,
                    admin_idle_timeout,
                )
                .await,
            }
        });
    }
    if run_auth_refresh {
        let auth = auth.clone();
        let auth_shutdown = shutdown.child_token();
        tasks.spawn(async move {
            auth.run_refresh_loop(auth_shutdown).await;
            TaskOutcome {
                task: ServiceTask::AuthRefresh,
                result: Ok(()),
            }
        });
    }
    if run_mcp_task {
        let store = store.clone();
        let upstream = upstream.clone();
        let mcp_shutdown = shutdown.child_token();
        tasks.spawn(async move {
            TaskOutcome {
                task: ServiceTask::Mcp,
                result: mcp::run(store, upstream, exec_enabled, mcp_shutdown).await,
            }
        });
    }
    if let Some(base_url) = daemon_base_url {
        let heartbeat_shutdown = shutdown.child_token();
        tasks.spawn(async move {
            TaskOutcome {
                task: ServiceTask::Heartbeat,
                result: run_daemon_heartbeat(base_url, heartbeat_shutdown).await,
            }
        });
    }

    // Only the admin task exiting is fatal. MCP stdio and auth-refresh can
    // fail without bringing down the process (e.g. stdin closed when running
    // from a terminal, or no MCP client connected).
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received shutdown signal");
                break;
            }
            joined = tasks.join_next() => {
                let Some(result) = joined else { break };
                match result {
                    Ok(outcome) => match outcome.result {
                        Ok(()) if outcome.task == ServiceTask::Mcp && mcp_controls_lifetime => {
                            info!("mcp task exited; shutting down mcp-only process");
                            break;
                        }
                        Ok(()) if outcome.task == ServiceTask::Admin => {
                            info!("admin task exited, shutting down");
                            break;
                        }
                        Ok(()) => {
                            info!(task = %outcome.task, "service task exited");
                        }
                        Err(err) if outcome.task == ServiceTask::Admin => {
                            if is_admin_port_in_use(&err) {
                                warn!(
                                    error = %err,
                                    "admin UI port already in use; assuming another gambi instance is managing admin and continuing MCP-only for this process"
                                );
                            } else {
                                error!(error = %err, "admin task failed");
                                shutdown.cancel();
                                drain_remaining(&mut tasks).await;
                                upstream.shutdown().await;
                                return Err(err);
                            }
                        }
                        Err(err) if outcome.task == ServiceTask::Mcp && mcp_controls_lifetime => {
                            error!(error = %err, "mcp task failed in mcp-only mode");
                            shutdown.cancel();
                            drain_remaining(&mut tasks).await;
                            upstream.shutdown().await;
                            return Err(err);
                        }
                        Err(err) => {
                            warn!(task = %outcome.task, error = %err, "service task failed (non-fatal)");
                        }
                    },
                    Err(err) => {
                        warn!(error = %err, "service task join failed");
                    }
                }
            }
        }
    }

    shutdown.cancel();

    let graceful = async {
        drain_remaining(&mut tasks).await;
        upstream.shutdown().await;
    };

    if tokio::time::timeout(SHUTDOWN_TIMEOUT, graceful)
        .await
        .is_err()
    {
        warn!("graceful shutdown timed out, exiting");
        std::process::exit(0);
    }

    if args.admin_only
        && let Err(err) = clear_daemon_state_if_owner(&store, std::process::id())
    {
        warn!(error = %err, "failed to clear daemon state file on shutdown");
    }

    Ok(())
}

async fn drain_remaining(tasks: &mut JoinSet<TaskOutcome>) {
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(outcome) => match outcome.result {
                Ok(()) => info!(task = %outcome.task, "service task exited"),
                Err(err) => warn!(
                    task = %outcome.task,
                    error = %err,
                    "service task failed during shutdown"
                ),
            },
            Err(err) => warn!(error = %err, "service task panicked during shutdown"),
        }
    }
}

fn is_admin_port_in_use(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| io_err.kind() == ErrorKind::AddrInUse)
    }) || err.to_string().contains("failed to bind admin UI")
}

async fn ensure_shared_admin_daemon(args: &ServeArgs, store: &ConfigStore) -> Result<bool> {
    let base_url = admin_base_url(args.admin_host, args.admin_port);
    if admin_health_ok(&base_url).await {
        return Ok(true);
    }

    if let Err(err) = spawn_admin_daemon(args, store) {
        warn!(error = %err, "failed to spawn shared admin daemon");
        return Ok(false);
    }

    let started = tokio::time::Instant::now();
    while started.elapsed() < ADMIN_DAEMON_START_TIMEOUT {
        if admin_health_ok(&base_url).await {
            info!(admin_base_url = %base_url, "shared admin daemon is healthy");
            return Ok(true);
        }
        tokio::time::sleep(ADMIN_DAEMON_HEALTH_POLL_INTERVAL).await;
    }

    warn!(
        admin_base_url = %base_url,
        timeout_seconds = ADMIN_DAEMON_START_TIMEOUT.as_secs(),
        "shared admin daemon did not become healthy in time"
    );
    if let Err(err) = clear_daemon_state(store) {
        warn!(error = %err, "failed to clear daemon state after startup timeout");
    }
    Ok(false)
}

fn spawn_admin_daemon(args: &ServeArgs, store: &ConfigStore) -> Result<()> {
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("serve")
        .arg("--admin-host")
        .arg(args.admin_host.to_string())
        .arg("--admin-port")
        .arg(args.admin_port.to_string())
        .arg("--admin-only")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if args.no_exec {
        command.arg("--no-exec");
    }
    if let Some(port_file) = &args.admin_port_file {
        command.arg("--admin-port-file").arg(port_file);
    }

    let child = command
        .spawn()
        .context("failed to spawn shared admin daemon")?;
    let pid = child.id();
    write_daemon_state(
        store,
        &DaemonStateRecord {
            pid,
            host: args.admin_host.to_string(),
            port: args.admin_port,
        },
    )?;
    Ok(())
}

fn admin_base_url(host: std::net::IpAddr, port: u16) -> String {
    format!("http://{}", SocketAddr::from((host, port)))
}

async fn admin_health_ok(base_url: &str) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(ADMIN_DAEMON_HEARTBEAT_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            warn!(error = %err, "failed to build HTTP client for daemon health probe");
            return false;
        }
    };

    let url = format!("{base_url}/health");
    match client.get(url).send().await {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    }
}

async fn run_daemon_heartbeat(base_url: String, shutdown: CancellationToken) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(ADMIN_DAEMON_HEARTBEAT_TIMEOUT)
        .build()
        .context("failed to build daemon heartbeat client")?;
    let url = format!("{base_url}/heartbeat");

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = async {
                match client.post(&url).send().await {
                    Ok(response) if response.status().is_success() => {}
                    Ok(response) => {
                        warn!(
                            status = %response.status(),
                            url = %url,
                            "daemon heartbeat returned non-success status"
                        );
                    }
                    Err(err) => {
                        warn!(error = %err, url = %url, "daemon heartbeat request failed");
                    }
                }
            } => {}
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(ADMIN_DAEMON_HEARTBEAT_INTERVAL) => {}
        }
    }

    Ok(())
}

fn read_daemon_state(store: &ConfigStore) -> Result<Option<DaemonStateRecord>> {
    let path = store.daemon_state_file();
    let raw = match fs::read_to_string(&path) {
        Ok(value) => value,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let parsed: DaemonStateRecord = serde_json::from_str(&raw)
        .with_context(|| format!("invalid daemon state in {}", path.display()))?;
    Ok(Some(parsed))
}

fn write_daemon_state(store: &ConfigStore, state: &DaemonStateRecord) -> Result<()> {
    let path = store.daemon_state_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let payload = format!(
        "{}\n",
        serde_json::to_string(state).context("failed to serialize daemon state")?
    );
    fs::write(&path, payload).with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

fn clear_daemon_state(store: &ConfigStore) -> Result<()> {
    let path = store.daemon_state_file();
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn clear_daemon_state_if_owner(store: &ConfigStore, pid: u32) -> Result<()> {
    let Some(state) = read_daemon_state(store)? else {
        return Ok(());
    };
    if state.pid == pid {
        clear_daemon_state(store)?;
    }
    Ok(())
}
