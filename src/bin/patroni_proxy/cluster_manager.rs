use crate::config::{self, ClusterDiff, ConfigDiff};
use crate::patroni::PatroniClient;
use crate::port::Port;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Cluster manager that handles ports and member updates
pub struct ClusterManager {
    /// Cluster name
    name: String,
    /// Patroni API hosts
    hosts: Arc<RwLock<Vec<String>>>,
    /// Patroni API client
    client: PatroniClient,
    /// Active ports
    ports: Arc<RwLock<HashMap<String, Arc<Port>>>>,
    /// Update interval for polling Patroni API
    update_interval: Duration,
    /// Background Patroni polling task for this cluster.
    update_task: Mutex<Option<JoinHandle<()>>>,
}

struct PreparedPort {
    full_name: String,
    port: Arc<Port>,
    listener: TcpListener,
}

impl ClusterManager {
    pub fn new(
        name: String,
        hosts: Vec<String>,
        update_interval: Duration,
        tls: Option<&config::TlsConfig>,
    ) -> Result<Self, String> {
        let client = PatroniClient::new_with_tls(tls)?;
        Ok(Self {
            name,
            hosts: Arc::new(RwLock::new(hosts)),
            client,
            ports: Arc::new(RwLock::new(HashMap::new())),
            update_interval,
            update_task: Mutex::new(None),
        })
    }

    /// Start all ports for this cluster
    pub async fn start_ports(
        &self,
        port_configs: &HashMap<String, config::PortConfig>,
    ) -> Result<(), String> {
        let prepared_ports = self.prepare_ports(port_configs).await?;
        self.activate_prepared_ports(prepared_ports).await;

        Ok(())
    }

    async fn prepare_ports(
        &self,
        port_configs: &HashMap<String, config::PortConfig>,
    ) -> Result<HashMap<String, PreparedPort>, String> {
        let mut prepared_ports = HashMap::new();

        for (port_name, port_config) in port_configs {
            let prepared = self.prepare_port(port_name, port_config).await?;
            prepared_ports.insert(port_name.clone(), prepared);
        }

        Ok(prepared_ports)
    }

    async fn prepare_port(
        &self,
        port_name: &str,
        port_config: &config::PortConfig,
    ) -> Result<PreparedPort, String> {
        let full_name = format!("{}:{}", self.name, port_name);
        let port = Arc::new(Port::new(full_name.clone(), port_config).map_err(|e| e.to_string())?);
        let listener = port.bind_listener().await.map_err(|e| e.to_string())?;

        Ok(PreparedPort {
            full_name,
            port,
            listener,
        })
    }

    async fn activate_prepared_ports(&self, prepared_ports: HashMap<String, PreparedPort>) {
        let mut to_spawn = Vec::new();
        {
            let mut ports = self.ports.write().await;
            for (port_name, prepared) in prepared_ports {
                ports.insert(port_name, Arc::clone(&prepared.port));
                to_spawn.push(prepared);
            }
        }

        for prepared in to_spawn {
            Self::spawn_prepared_port(prepared);
        }
    }

    fn spawn_prepared_port(prepared: PreparedPort) {
        let PreparedPort {
            full_name,
            port,
            listener,
        } = prepared;
        let listen_addr = listener.local_addr().unwrap_or_else(|_| port.listen_addr());

        info!("Starting port '{}' on {}", full_name, listen_addr);

        let port_clone = Arc::clone(&port);
        tokio::spawn(async move {
            if let Err(e) = port_clone.run_with_listener(listener).await {
                error!("Port '{}' error: {}", full_name, e);
            }
        });
    }

    /// Stop all ports
    pub async fn stop_ports(&self) {
        let ports = self.ports.read().await;
        for (port_name, port) in ports.iter() {
            info!("Stopping port '{}:{}'", self.name, port_name);
            port.stop().await;
        }
    }

    /// Update cluster members from Patroni API
    pub async fn update_members(&self) {
        let hosts = self.hosts.read().await.clone();
        match self.client.fetch_members(&hosts).await {
            Ok(members) => {
                debug!(
                    "Cluster '{}': fetched {} members from Patroni API",
                    self.name,
                    members.len()
                );

                let ports = self.ports.read().await;
                for (port_name, port) in ports.iter() {
                    port.update_members(members.clone()).await;
                    let backend_count = port.backend_count().await;
                    debug!(
                        "Port '{}:{}': {} backends available",
                        self.name, port_name, backend_count
                    );
                }
            }
            Err(e) => {
                warn!("Cluster '{}': failed to fetch members: {}", self.name, e);
            }
        }
    }

    /// Start periodic member updates
    pub fn start_update_loop(self: Arc<Self>) {
        let update_interval = self.update_interval;
        let manager = Arc::clone(&self);
        let handle = tokio::spawn(async move {
            loop {
                manager.update_members().await;
                tokio::time::sleep(update_interval).await;
            }
        });

        let mut update_task = self.update_task.lock().expect("update task lock poisoned");
        if let Some(old_handle) = update_task.replace(handle) {
            old_handle.abort();
        }
    }

    /// Stop periodic member updates for this cluster.
    pub fn stop_update_loop(&self) {
        if let Some(handle) = self
            .update_task
            .lock()
            .expect("update task lock poisoned")
            .take()
        {
            handle.abort();
        }
    }
}

#[derive(Default)]
struct PreparedConfigChanges {
    added_clusters: HashMap<String, PreparedCluster>,
    ports: HashMap<(String, String), PreparedPort>,
}

struct PreparedCluster {
    manager: Arc<ClusterManager>,
    ports: HashMap<String, PreparedPort>,
}

async fn cleanup_prepared_config_changes(prepared: &PreparedConfigChanges) {
    for cluster in prepared.added_clusters.values() {
        for port in cluster.ports.values() {
            port.port.stop().await;
        }
    }
    for port in prepared.ports.values() {
        port.port.stop().await;
    }
}

async fn prepare_config_changes(
    diff: &ConfigDiff,
    cluster_managers: &Arc<RwLock<HashMap<String, Arc<ClusterManager>>>>,
    update_interval: Duration,
) -> Result<PreparedConfigChanges, String> {
    let mut prepared = PreparedConfigChanges::default();

    for change in &diff.changes {
        match change {
            ClusterDiff::TopLevelChanged(field, old, new) => {
                cleanup_prepared_config_changes(&prepared).await;
                return Err(format!(
                    "top-level patroni-proxy configuration '{field}' changed from '{old}' to '{new}'; restart required"
                ));
            }
            ClusterDiff::TlsChanged(name) => {
                cleanup_prepared_config_changes(&prepared).await;
                return Err(format!(
                    "cluster '{name}' TLS configuration changed; restart required"
                ));
            }
            ClusterDiff::Added(name, cluster_config) => {
                if cluster_managers.read().await.contains_key(name) {
                    cleanup_prepared_config_changes(&prepared).await;
                    return Err(format!("cluster '{name}' already exists for add reload"));
                }

                let manager = match ClusterManager::new(
                    name.clone(),
                    cluster_config.hosts.clone(),
                    update_interval,
                    cluster_config.tls.as_ref(),
                ) {
                    Ok(manager) => manager,
                    Err(e) => {
                        cleanup_prepared_config_changes(&prepared).await;
                        return Err(format!(
                            "failed to create cluster manager for '{name}': {e}"
                        ));
                    }
                };

                let manager = Arc::new(manager);
                let ports = match manager.prepare_ports(&cluster_config.ports).await {
                    Ok(ports) => ports,
                    Err(e) => {
                        cleanup_prepared_config_changes(&prepared).await;
                        return Err(format!("failed to start ports for cluster '{name}': {e}"));
                    }
                };

                prepared
                    .added_clusters
                    .insert(name.clone(), PreparedCluster { manager, ports });
            }
            ClusterDiff::Removed(_) => {}
            ClusterDiff::HostsChanged(name, _, _) => {
                if !cluster_managers.read().await.contains_key(name) {
                    cleanup_prepared_config_changes(&prepared).await;
                    return Err(format!(
                        "cluster '{name}' manager not found for hosts reload"
                    ));
                }
            }
            ClusterDiff::PortsChanged(name, old, new) => {
                let manager = {
                    let managers = cluster_managers.read().await;
                    managers.get(name).cloned()
                };
                let Some(manager) = manager else {
                    cleanup_prepared_config_changes(&prepared).await;
                    return Err(format!(
                        "cluster '{name}' manager not found for ports reload"
                    ));
                };

                for (port_name, new_config) in new {
                    let should_add = match old.get(port_name) {
                        None => true,
                        Some(old_config) => old_config != new_config,
                    };

                    if should_add {
                        let old_config = old.get(port_name);
                        let same_listen = old_config
                            .map(|old_config| old_config.listen == new_config.listen)
                            .unwrap_or(false);

                        if old_config.is_some() && same_listen {
                            cleanup_prepared_config_changes(&prepared).await;
                            return Err(format!(
                                "same listen port change for '{name}:{port_name}' requires restart"
                            ));
                        }

                        match manager.prepare_port(port_name, new_config).await {
                            Ok(port) => {
                                prepared
                                    .ports
                                    .insert((name.clone(), port_name.clone()), port);
                            }
                            Err(e) => {
                                cleanup_prepared_config_changes(&prepared).await;
                                return Err(format!(
                                    "failed to start port '{name}:{port_name}': {e}"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(prepared)
}

pub async fn handle_config_changes(
    diff: &ConfigDiff,
    cluster_managers: &Arc<RwLock<HashMap<String, Arc<ClusterManager>>>>,
    update_interval: Duration,
) -> Result<(), String> {
    let mut prepared = prepare_config_changes(diff, cluster_managers, update_interval).await?;

    for change in &diff.changes {
        match change {
            ClusterDiff::TopLevelChanged(field, old, new) => {
                return Err(format!(
                    "top-level patroni-proxy configuration '{field}' changed from '{old}' to '{new}'; restart required"
                ));
            }
            ClusterDiff::TlsChanged(name) => {
                return Err(format!(
                    "cluster '{name}' TLS configuration changed; restart required"
                ));
            }
            ClusterDiff::Added(name, _cluster_config) => {
                let prepared_cluster = prepared
                    .added_clusters
                    .remove(name)
                    .ok_or_else(|| format!("cluster '{name}' was not prepared for add"))?;
                let manager = prepared_cluster.manager;
                manager
                    .activate_prepared_ports(prepared_cluster.ports)
                    .await;
                manager.clone().start_update_loop();

                let mut managers = cluster_managers.write().await;
                managers.insert(name.clone(), manager);

                info!("Cluster '{}' started successfully", name);
            }
            ClusterDiff::Removed(name) => {
                let mut managers = cluster_managers.write().await;
                if let Some(manager) = managers.remove(name) {
                    manager.stop_update_loop();
                    manager.stop_ports().await;
                    info!("Cluster '{}' stopped and removed", name);
                }
            }
            ClusterDiff::HostsChanged(name, _old, new) => {
                let managers = cluster_managers.read().await;
                let manager = managers.get(name).ok_or_else(|| {
                    format!("cluster '{name}' manager not found for hosts reload")
                })?;
                *manager.hosts.write().await = new.clone();
                info!("Cluster '{}' hosts updated to {:?}", name, new);
            }
            ClusterDiff::PortsChanged(name, old, new) => {
                let managers = cluster_managers.read().await;
                let manager = managers.get(name).ok_or_else(|| {
                    format!("cluster '{name}' manager not found for ports reload")
                })?;
                info!(
                    "Cluster '{}' ports changed, applying incremental update",
                    name
                );

                let mut ports = manager.ports.write().await;

                // Find ports to remove (in old but not in new, or config changed)
                let mut to_remove = Vec::new();
                for (port_name, old_config) in old {
                    match new.get(port_name) {
                        None => {
                            // Port removed
                            to_remove.push(port_name.clone());
                        }
                        Some(new_config) => {
                            // Check if config changed
                            if old_config != new_config {
                                to_remove.push(port_name.clone());
                            }
                        }
                    }
                }

                // Stop and remove deleted ports. Changed ports are handled
                // below so a failed replacement bind can keep or restore
                // the old listener.
                for port_name in &to_remove {
                    if new.contains_key(port_name) {
                        continue;
                    }
                    if let Some(port) = ports.remove(port_name) {
                        info!("Stopping port '{}:{}' (removed)", name, port_name);
                        port.stop().await;
                    }
                }

                // Add new ports (not in old, or config changed)
                for (port_name, new_config) in new {
                    let should_add = match old.get(port_name) {
                        None => true,                                 // New port
                        Some(old_config) => old_config != new_config, // Config changed
                    };

                    if should_add {
                        let old_config = old.get(port_name);
                        let same_listen = old_config
                            .map(|old_config| old_config.listen == new_config.listen)
                            .unwrap_or(false);

                        if old_config.is_some() && same_listen {
                            return Err(format!(
                                "same listen port change for '{name}:{port_name}' requires restart"
                            ));
                        } else {
                            let prepared_port = prepared
                                .ports
                                .remove(&(name.clone(), port_name.clone()))
                                .ok_or_else(|| {
                                    format!("port '{name}:{port_name}' was not prepared for reload")
                                })?;
                            if let Some(old_port) = ports.remove(port_name) {
                                info!(
                                    "Stopping port '{}:{}' after replacement listener bound",
                                    name, port_name
                                );
                                old_port.stop().await;
                            }
                            let port = Arc::clone(&prepared_port.port);
                            ports.insert(port_name.clone(), port);
                            ClusterManager::spawn_prepared_port(prepared_port);
                        }
                    }
                }

                info!("Cluster '{}' ports updated successfully", name);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_partial_tls_client_identity() {
        let tls = config::TlsConfig {
            ca_cert: None,
            client_cert: Some("/tmp/client.crt".to_string()),
            client_key: None,
            skip_verify: Some(false),
        };

        let result = ClusterManager::new(
            "cluster".to_string(),
            vec!["https://patroni.local:8008".to_string()],
            Duration::from_secs(3),
            Some(&tls),
        );
        let err = match result {
            Ok(_) => panic!("partial TLS client identity must not be silently ignored"),
            Err(err) => err,
        };

        assert!(
            err.contains("client_cert") && err.contains("client_key"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn new_rejects_missing_tls_ca_cert() {
        let tls = config::TlsConfig {
            ca_cert: Some("/definitely/missing/patroni-ca.pem".to_string()),
            client_cert: None,
            client_key: None,
            skip_verify: Some(false),
        };

        let result = ClusterManager::new(
            "cluster".to_string(),
            vec!["https://patroni.local:8008".to_string()],
            Duration::from_secs(3),
            Some(&tls),
        );
        let err = match result {
            Ok(_) => panic!("missing TLS ca_cert must not be silently ignored"),
            Err(err) => err,
        };

        assert!(err.contains("ca_cert"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn hosts_changed_updates_existing_manager_hosts() {
        let manager = Arc::new(
            ClusterManager::new(
                "cluster".to_string(),
                vec!["http://old-patroni:8008".to_string()],
                Duration::from_secs(3),
                None,
            )
            .expect("manager should be created"),
        );
        let managers = Arc::new(RwLock::new(HashMap::new()));
        managers
            .write()
            .await
            .insert("cluster".to_string(), Arc::clone(&manager));

        let diff = ConfigDiff {
            changes: vec![ClusterDiff::HostsChanged(
                "cluster".to_string(),
                vec!["http://old-patroni:8008".to_string()],
                vec!["http://new-patroni:8008".to_string()],
            )],
        };

        handle_config_changes(&diff, &managers, Duration::from_secs(3))
            .await
            .expect("hosts reload should apply");

        assert_eq!(
            *manager.hosts.read().await,
            vec!["http://new-patroni:8008".to_string()]
        );
    }

    #[tokio::test]
    async fn removed_cluster_stops_update_loop_task() {
        let manager = Arc::new(
            ClusterManager::new(
                "cluster".to_string(),
                Vec::new(),
                Duration::from_secs(60),
                None,
            )
            .expect("manager should be created"),
        );
        let weak = Arc::downgrade(&manager);
        let managers = Arc::new(RwLock::new(HashMap::new()));
        managers
            .write()
            .await
            .insert("cluster".to_string(), Arc::clone(&manager));
        manager.clone().start_update_loop();

        let diff = ConfigDiff {
            changes: vec![ClusterDiff::Removed("cluster".to_string())],
        };

        handle_config_changes(&diff, &managers, Duration::from_secs(60))
            .await
            .expect("removed cluster reload should apply");
        drop(manager);

        for _ in 0..10 {
            if weak.upgrade().is_none() {
                return;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(
            weak.upgrade().is_none(),
            "removed cluster update loop must not retain the old manager"
        );
    }

    #[tokio::test]
    async fn added_cluster_bind_failure_returns_error_without_manager() {
        let managers = Arc::new(RwLock::new(HashMap::new()));
        let occupied_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let occupied_listen = occupied_listener.local_addr().unwrap();

        let diff = ConfigDiff {
            changes: vec![ClusterDiff::Added(
                "new_cluster".to_string(),
                config::ClusterConfig {
                    hosts: vec!["http://patroni:8008".to_string()],
                    tls: None,
                    ports: HashMap::from([(
                        "rw".to_string(),
                        config::PortConfig {
                            listen: occupied_listen.to_string(),
                            roles: vec!["any".to_string()],
                            host_port: 5432,
                            max_lag_in_bytes: None,
                        },
                    )]),
                },
            )],
        };

        let err = handle_config_changes(&diff, &managers, Duration::from_secs(3))
            .await
            .expect_err("failed added cluster bind must abort reload apply");

        assert!(
            err.contains("new_cluster"),
            "error should identify failed cluster: {err}"
        );
        assert!(
            !managers.read().await.contains_key("new_cluster"),
            "failed added cluster must not be published to runtime managers"
        );
    }

    #[tokio::test]
    async fn ports_changed_keeps_old_port_when_new_bind_fails() {
        let manager = Arc::new(
            ClusterManager::new(
                "cluster".to_string(),
                vec!["http://patroni:8008".to_string()],
                Duration::from_secs(3),
                None,
            )
            .expect("manager should be created"),
        );
        let managers = Arc::new(RwLock::new(HashMap::new()));
        managers
            .write()
            .await
            .insert("cluster".to_string(), Arc::clone(&manager));

        let old_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let old_listen = old_listener.local_addr().unwrap();
        drop(old_listener);

        let occupied_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let occupied_listen = occupied_listener.local_addr().unwrap();

        let old_config = config::PortConfig {
            listen: old_listen.to_string(),
            roles: vec!["any".to_string()],
            host_port: 5432,
            max_lag_in_bytes: None,
        };
        let new_config = config::PortConfig {
            listen: occupied_listen.to_string(),
            roles: vec!["leader".to_string()],
            host_port: 6432,
            max_lag_in_bytes: None,
        };

        manager.ports.write().await.insert(
            "rw".to_string(),
            Arc::new(Port::new("cluster:rw".to_string(), &old_config).unwrap()),
        );

        let diff = ConfigDiff {
            changes: vec![ClusterDiff::PortsChanged(
                "cluster".to_string(),
                HashMap::from([("rw".to_string(), old_config.clone())]),
                HashMap::from([("rw".to_string(), new_config)]),
            )],
        };

        let err = handle_config_changes(&diff, &managers, Duration::from_secs(3))
            .await
            .expect_err("failed replacement bind must abort reload apply");
        assert!(
            err.contains("cluster:rw"),
            "error should identify failed port: {err}"
        );

        let ports = manager.ports.read().await;
        let rw = ports.get("rw").expect("old port should remain published");
        assert_eq!(
            rw.listen_addr(),
            old_listen,
            "failed replacement bind must not publish the new listen address"
        );
    }

    #[tokio::test]
    async fn config_changes_do_not_publish_hosts_before_later_port_bind_failure() {
        let manager = Arc::new(
            ClusterManager::new(
                "cluster".to_string(),
                vec!["http://old-patroni:8008".to_string()],
                Duration::from_secs(3),
                None,
            )
            .expect("manager should be created"),
        );
        let managers = Arc::new(RwLock::new(HashMap::new()));
        managers
            .write()
            .await
            .insert("cluster".to_string(), Arc::clone(&manager));

        let old_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let old_listen = old_listener.local_addr().unwrap();
        drop(old_listener);

        let occupied_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let occupied_listen = occupied_listener.local_addr().unwrap();

        let old_config = config::PortConfig {
            listen: old_listen.to_string(),
            roles: vec!["any".to_string()],
            host_port: 5432,
            max_lag_in_bytes: None,
        };
        let new_config = config::PortConfig {
            listen: occupied_listen.to_string(),
            roles: vec!["leader".to_string()],
            host_port: 6432,
            max_lag_in_bytes: None,
        };

        manager.ports.write().await.insert(
            "rw".to_string(),
            Arc::new(Port::new("cluster:rw".to_string(), &old_config).unwrap()),
        );

        let diff = ConfigDiff {
            changes: vec![
                ClusterDiff::HostsChanged(
                    "cluster".to_string(),
                    vec!["http://old-patroni:8008".to_string()],
                    vec!["http://new-patroni:8008".to_string()],
                ),
                ClusterDiff::PortsChanged(
                    "cluster".to_string(),
                    HashMap::from([("rw".to_string(), old_config)]),
                    HashMap::from([("rw".to_string(), new_config)]),
                ),
            ],
        };

        let err = handle_config_changes(&diff, &managers, Duration::from_secs(3))
            .await
            .expect_err("later port bind failure must abort the whole runtime apply");
        assert!(
            err.contains("cluster:rw"),
            "error should identify failed port: {err}"
        );

        assert_eq!(
            *manager.hosts.read().await,
            vec!["http://old-patroni:8008".to_string()],
            "runtime hosts must not be published when a later change fails"
        );
    }

    #[tokio::test]
    async fn prepared_port_is_not_accepted_before_reload_commit() {
        use tokio::io::AsyncReadExt;

        let manager = Arc::new(
            ClusterManager::new(
                "cluster".to_string(),
                vec!["http://patroni:8008".to_string()],
                Duration::from_secs(3),
                None,
            )
            .expect("manager should be created"),
        );
        let managers = Arc::new(RwLock::new(HashMap::new()));
        managers
            .write()
            .await
            .insert("cluster".to_string(), Arc::clone(&manager));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        drop(listener);

        let new_config = config::PortConfig {
            listen: listen_addr.to_string(),
            roles: vec!["any".to_string()],
            host_port: 5432,
            max_lag_in_bytes: None,
        };
        let diff = ConfigDiff {
            changes: vec![ClusterDiff::PortsChanged(
                "cluster".to_string(),
                HashMap::new(),
                HashMap::from([("rw".to_string(), new_config)]),
            )],
        };

        let prepared = prepare_config_changes(&diff, &managers, Duration::from_secs(3))
            .await
            .expect("preflight should bind the replacement listener");

        let mut client = tokio::net::TcpStream::connect(listen_addr)
            .await
            .expect("preflight listener should reserve the address");
        let mut buf = [0_u8; 1];
        let read = tokio::time::timeout(Duration::from_millis(100), client.read(&mut buf)).await;

        cleanup_prepared_config_changes(&prepared).await;

        assert!(
            read.is_err(),
            "preflight must not run an accept loop before reload commit"
        );
    }

    #[tokio::test]
    async fn same_listen_port_change_is_rejected_without_stopping_old_port() {
        let manager = Arc::new(
            ClusterManager::new(
                "cluster".to_string(),
                vec!["http://patroni:8008".to_string()],
                Duration::from_secs(3),
                None,
            )
            .expect("manager should be created"),
        );
        let managers = Arc::new(RwLock::new(HashMap::new()));
        managers
            .write()
            .await
            .insert("cluster".to_string(), Arc::clone(&manager));

        let old_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let old_listen = old_listener.local_addr().unwrap();
        drop(old_listener);

        let old_config = config::PortConfig {
            listen: old_listen.to_string(),
            roles: vec!["any".to_string()],
            host_port: 5432,
            max_lag_in_bytes: None,
        };
        let new_config = config::PortConfig {
            listen: old_listen.to_string(),
            roles: vec!["leader".to_string()],
            host_port: 6432,
            max_lag_in_bytes: None,
        };

        manager
            .start_ports(&HashMap::from([("rw".to_string(), old_config.clone())]))
            .await
            .expect("old port should start");

        let original_port = manager
            .ports
            .read()
            .await
            .get("rw")
            .expect("old port should be published")
            .clone();

        let diff = ConfigDiff {
            changes: vec![ClusterDiff::PortsChanged(
                "cluster".to_string(),
                HashMap::from([("rw".to_string(), old_config.clone())]),
                HashMap::from([("rw".to_string(), new_config)]),
            )],
        };

        let err = handle_config_changes(&diff, &managers, Duration::from_secs(3))
            .await
            .expect_err("same-listen port changes must require restart");
        assert!(
            err.contains("same listen"),
            "error should explain same-listen rejection: {err}"
        );

        let ports = manager.ports.read().await;
        let rw = ports.get("rw").expect("old port should remain published");
        assert!(
            Arc::ptr_eq(rw, &original_port),
            "same-listen rejection must not replace the old port"
        );
        assert_eq!(rw.listen_addr(), old_listen);
    }
}
