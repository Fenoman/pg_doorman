mod api;
mod cluster_manager;
mod config;
mod patroni;
mod port;
mod stream;

use api::start_http_server;
use clap::Parser;
use cluster_manager::{handle_config_changes, ClusterManager};
use config::{ClusterDiff, ConfigDiff, ConfigRepository};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Patroni Proxy: PostgreSQL proxy for Patroni clusters
#[derive(Parser, Debug)]
#[command(name = "patroni_proxy", author, version, about, long_about = None)]
struct Args {
    /// Path to configuration file
    #[arg(default_value = "patroni_proxy.yaml")]
    config_file: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // install the same panic hook as pg_doorman main.
    // Without it, panics in patroni_proxy worker tasks use the default
    // Rust hook (stderr print only) and the process keeps running
    // silently - no count, no operator signal. Matches pg_doorman's
    // Semantics: log + WORKER_PANIC_COUNT inc + delegate to default
    // (task aborts, process survives).
    pg_doorman::app::install_panic_hook();

    // install SIGTERM / SIGHUP handlers AS THE FIRST
    // async action - BEFORE cluster init, HTTP bind, and any slow
    // network work. A SIGTERM landing during a slow bring-up (DNS
    // lag, port already in use, Patroni unreachable) earlier
    // killed the process with the kernel default disposition,
    // skipping `Port::stop()` cleanup and leaving listener sockets
    // bound until kernel close. Tokio buffers the signal in the
    // handler's internal channel until the consumer (the
    // tokio::select! below) polls it, so an early-arriving SIGTERM
    // still produces an orderly exit.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;

    // Parse command line arguments (handles --version and --help automatically)
    let args = Args::parse();
    let config_path = args.config_file;

    info!("Starting patroni-proxy with config: {}", config_path);

    // Load configuration
    let config_repo = Arc::new(ConfigRepository::new(&config_path).map_err(|e| {
        error!("Failed to load configuration: {}", e);
        e
    })?);

    info!("Configuration loaded successfully");

    // Cluster managers
    let cluster_managers: Arc<RwLock<HashMap<String, Arc<ClusterManager>>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Initialize clusters and start ports
    {
        let config = config_repo.get();
        let update_interval = Duration::from_secs(config.cluster_update_interval);
        let mut managers = cluster_managers.write().await;

        for (cluster_name, cluster_config) in &config.clusters {
            info!(
                "Initializing cluster '{}': {} hosts, {} ports",
                cluster_name,
                cluster_config.hosts.len(),
                cluster_config.ports.len()
            );

            let manager = Arc::new(ClusterManager::new(
                cluster_name.clone(),
                cluster_config.hosts.clone(),
                update_interval,
                cluster_config.tls.as_ref(),
            )?);

            // Start ports
            manager.start_ports(&cluster_config.ports).await?;

            // Start update loop
            manager.clone().start_update_loop();

            managers.insert(cluster_name.clone(), manager);
        }
    }

    // Start HTTP server
    {
        let config = config_repo.get();
        start_http_server(config.listen_address.clone(), Arc::clone(&cluster_managers)).await?;
    }

    // Setup SIGHUP handler for configuration reload. `sighup` is built at
    // the top of main so a SIGHUP during slow cluster bring-up is still
    // serviced.
    let config_repo_clone = Arc::clone(&config_repo);
    let cluster_managers_clone = Arc::clone(&cluster_managers);
    tokio::spawn(async move {
        loop {
            sighup.recv().await;
            info!("Received SIGHUP, reloading configuration...");

            match config_repo_clone.prepare_reload() {
                Ok(prepared) => {
                    if prepared.diff.has_changes() {
                        log_config_changes(&prepared.diff);
                        let config = config_repo_clone.get();
                        let update_interval = Duration::from_secs(config.cluster_update_interval);
                        match handle_config_changes(
                            &prepared.diff,
                            &cluster_managers_clone,
                            update_interval,
                        )
                        .await
                        {
                            Ok(()) => {
                                config_repo_clone.publish_reload(prepared);
                                info!("Configuration reloaded successfully");
                            }
                            Err(e) => {
                                error!("Failed to apply reloaded configuration: {}", e);
                                warn!("Keeping previous configuration");
                            }
                        }
                    } else {
                        info!("Configuration unchanged");
                    }
                }
                Err(e) => {
                    error!("Failed to reload configuration: {}", e);
                    warn!("Keeping previous configuration");
                }
            }
        }
    });

    info!(
        "patroni-proxy is running. Send SIGHUP to reload configuration, SIGTERM or Ctrl+C to stop."
    );

    // `sigterm` was built at the top of main
    // so SIGTERM during slow cluster bring-up is still serviced.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, shutting down patroni-proxy...");
        }
        _ = sigterm.recv() => {
            info!("Received SIGTERM, shutting down patroni-proxy...");
        }
    }

    // Stop all clusters
    {
        let managers = cluster_managers.read().await;
        for (cluster_name, manager) in managers.iter() {
            info!("Stopping cluster '{}'", cluster_name);
            manager.stop_update_loop();
            manager.stop_ports().await;
        }
    }

    Ok(())
}

fn log_config_changes(diff: &ConfigDiff) {
    for change in &diff.changes {
        match change {
            ClusterDiff::TopLevelChanged(field, old, new) => {
                info!("Top-level config '{}' changed: {} -> {}", field, old, new);
            }
            ClusterDiff::Added(name, _) => {
                info!("Cluster '{}' added", name);
            }
            ClusterDiff::Removed(name) => {
                info!("Cluster '{}' removed", name);
            }
            ClusterDiff::HostsChanged(name, old, new) => {
                info!("Cluster '{}' hosts changed: {:?} -> {:?}", name, old, new);
            }
            ClusterDiff::PortsChanged(name, _, _) => {
                info!("Cluster '{}' ports configuration changed", name);
            }
            ClusterDiff::TlsChanged(name) => {
                info!("Cluster '{}' TLS configuration changed", name);
            }
        }
    }
}
