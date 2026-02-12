mod admin;
mod mcp;

use std::fmt;

use anyhow::{Result, anyhow};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::auth::AuthManager;
use crate::cli::ServeArgs;
use crate::config::ConfigStore;
use crate::upstream::UpstreamManager;

#[derive(Debug, Clone, Copy)]
enum ServiceTask {
    Admin,
    Mcp,
    AuthRefresh,
}

impl fmt::Display for ServiceTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServiceTask::Admin => write!(f, "admin"),
            ServiceTask::Mcp => write!(f, "mcp"),
            ServiceTask::AuthRefresh => write!(f, "auth-refresh"),
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

    let shutdown = CancellationToken::new();
    let auth = AuthManager::new(store.clone());
    let upstream = UpstreamManager::new();

    let mut tasks = JoinSet::new();
    {
        let store = store.clone();
        let auth = auth.clone();
        let upstream = upstream.clone();
        let admin_shutdown = shutdown.child_token();
        tasks.spawn(async move {
            let bind = admin::AdminBindOptions {
                host: admin_host,
                port: admin_port,
                port_file: admin_port_file,
            };
            admin::run(bind, store, auth, upstream, exec_enabled, admin_shutdown)
                .await
                .map(|_| ServiceTask::Admin)
        });
    }
    {
        let auth = auth.clone();
        let auth_shutdown = shutdown.child_token();
        tasks.spawn(async move {
            auth.run_refresh_loop(auth_shutdown).await;
            Ok(ServiceTask::AuthRefresh)
        });
    }
    {
        let store = store.clone();
        let upstream = upstream.clone();
        let mcp_shutdown = shutdown.child_token();
        tasks.spawn(async move {
            mcp::run(store, upstream, exec_enabled, mcp_shutdown)
                .await
                .map(|_| ServiceTask::Mcp)
        });
    }

    let mut first_error: Option<anyhow::Error> = None;

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("received shutdown signal");
        }
        joined = tasks.join_next() => {
            if let Some(result) = joined {
                record_task_result(result, &mut first_error);
            }
        }
    }

    shutdown.cancel();

    while let Some(result) = tasks.join_next().await {
        record_task_result(result, &mut first_error);
    }
    upstream.shutdown().await;

    if let Some(err) = first_error {
        Err(err)
    } else {
        Ok(())
    }
}

fn record_task_result(
    result: Result<Result<ServiceTask, anyhow::Error>, tokio::task::JoinError>,
    first_error: &mut Option<anyhow::Error>,
) {
    match result {
        Ok(Ok(task)) => info!(task = %task, "service task exited"),
        Ok(Err(err)) => {
            error!(error = %err, "service task failed");
            if first_error.is_none() {
                *first_error = Some(err);
            }
        }
        Err(err) => {
            error!(error = %err, "service task panicked or was cancelled");
            if first_error.is_none() {
                *first_error = Some(anyhow!(err));
            }
        }
    }
}
