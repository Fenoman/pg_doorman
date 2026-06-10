use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Leader,
    Sync,
    Async,
    Any,
}

impl Role {
    pub fn is_valid(role_str: &str) -> bool {
        matches!(
            role_str.to_lowercase().as_str(),
            "leader" | "sync" | "async" | "any"
        )
    }

    pub fn from_str(s: &str) -> Result<Self, ConfigError> {
        match s.to_lowercase().as_str() {
            "leader" => Ok(Role::Leader),
            "sync" => Ok(Role::Sync),
            "async" => Ok(Role::Async),
            "any" => Ok(Role::Any),
            _ => Err(ConfigError::InvalidRole(s.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TlsConfig {
    pub ca_cert: Option<String>,
    pub client_cert: Option<String>,
    pub client_key: Option<String>,
    pub skip_verify: Option<bool>,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            ca_cert: None,
            client_cert: None,
            client_key: None,
            skip_verify: Some(false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortConfig {
    pub listen: String,
    pub roles: Vec<String>,
    pub host_port: u16,
    #[serde(default)]
    pub max_lag_in_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterConfig {
    pub hosts: Vec<String>,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    pub ports: HashMap<String, PortConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Cluster update interval in seconds (default: 3)
    #[serde(default = "default_cluster_update_interval")]
    pub cluster_update_interval: u64,
    /// HTTP listen address for health checks and metrics (default: "127.0.0.1:8009")
    #[serde(default = "default_listen_address")]
    pub listen_address: String,
    pub clusters: HashMap<String, ClusterConfig>,
}

fn default_cluster_update_interval() -> u64 {
    3
}

fn default_listen_address() -> String {
    "127.0.0.1:8009".to_string()
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigError {
    IoError(String),
    ParseError(String),
    InvalidRole(String),
    InvalidHost(String),
    DuplicateHost(String),
    DuplicateListen(String),
    EmptyHosts(String),
    EmptyRoles(String),
    EmptyPorts(String),
    InvalidListenAddress(String),
    InvalidInterval(String),
    UnsupportedHotReload(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::IoError(e) => write!(f, "IO error: {e}"),
            ConfigError::ParseError(e) => write!(f, "Parse error: {e}"),
            ConfigError::InvalidRole(r) => write!(
                f,
                "Invalid role '{r}'. Allowed roles: leader, sync, async, any"
            ),
            ConfigError::InvalidHost(reason) => write!(f, "Invalid host: {reason}"),
            ConfigError::DuplicateHost(h) => write!(f, "Duplicate host: {h}"),
            ConfigError::DuplicateListen(l) => write!(f, "Duplicate listen address: {l}"),
            ConfigError::EmptyHosts(c) => write!(f, "Cluster '{c}' has no hosts defined"),
            ConfigError::EmptyRoles(p) => write!(f, "Port '{p}' has no roles defined"),
            ConfigError::EmptyPorts(c) => write!(f, "Cluster '{c}' has no ports defined"),
            ConfigError::InvalidListenAddress(a) => write!(f, "Invalid listen address: {a}"),
            ConfigError::InvalidInterval(reason) => write!(f, "Invalid interval: {reason}"),
            ConfigError::UnsupportedHotReload(reason) => {
                write!(f, "Unsupported hot reload: {reason}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path).map_err(|e| ConfigError::IoError(e.to_string()))?;
        Self::from_str(&content)
    }

    pub fn from_str(content: &str) -> Result<Self, ConfigError> {
        let config: Config =
            serde_yaml::from_str(content).map_err(|e| ConfigError::ParseError(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut all_listen_addresses: HashSet<String> = HashSet::new();

        if self.cluster_update_interval == 0 {
            return Err(ConfigError::InvalidInterval(
                "cluster_update_interval must be at least 1 second".to_string(),
            ));
        }

        if self.listen_address.parse::<SocketAddr>().is_err() {
            return Err(ConfigError::InvalidListenAddress(
                self.listen_address.clone(),
            ));
        }

        for (cluster_name, cluster) in &self.clusters {
            // Validate hosts are not empty
            if cluster.hosts.is_empty() {
                return Err(ConfigError::EmptyHosts(cluster_name.clone()));
            }

            // Validate hosts: only http/https, no secret-bearing components, no duplicates
            let mut seen_hosts: HashSet<String> = HashSet::new();
            for host in &cluster.hosts {
                let normalized = validate_patroni_host_url(host)?;

                // Check for duplicates within cluster
                if seen_hosts.contains(&normalized) {
                    return Err(ConfigError::DuplicateHost(host.clone()));
                }
                seen_hosts.insert(normalized);
            }

            // Validate ports are not empty
            if cluster.ports.is_empty() {
                return Err(ConfigError::EmptyPorts(cluster_name.clone()));
            }

            // Validate ports
            for (port_name, port_config) in &cluster.ports {
                // Validate roles are not empty
                if port_config.roles.is_empty() {
                    return Err(ConfigError::EmptyRoles(port_name.clone()));
                }

                // Validate each role
                for role in &port_config.roles {
                    if !Role::is_valid(role) {
                        return Err(ConfigError::InvalidRole(role.clone()));
                    }
                }

                // Validate listen address format
                let listen = &port_config.listen;
                if listen.parse::<SocketAddr>().is_err() {
                    return Err(ConfigError::InvalidListenAddress(listen.clone()));
                }

                // Check for duplicate listen addresses across all clusters
                if all_listen_addresses.contains(listen) {
                    return Err(ConfigError::DuplicateListen(listen.clone()));
                }
                all_listen_addresses.insert(listen.clone());
            }
        }

        Ok(())
    }
}

fn validate_patroni_host_url(host: &str) -> Result<String, ConfigError> {
    if host.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
        return Err(ConfigError::InvalidHost(
            "URL must not contain whitespace or control characters".to_string(),
        ));
    }

    let parsed = reqwest::Url::parse(host).map_err(|_| {
        ConfigError::InvalidHost("URL must be a valid absolute http:// or https:// URL".to_string())
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ConfigError::InvalidHost(
            "only http:// and https:// schemes are allowed".to_string(),
        ));
    }
    if parsed.host_str().filter(|host| !host.is_empty()).is_none() {
        return Err(ConfigError::InvalidHost(
            "URL must include a non-empty host".to_string(),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ConfigError::InvalidHost(
            "URL userinfo is not allowed; remove username/password".to_string(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ConfigError::InvalidHost(
            "URL query/fragment is not allowed; configure only scheme, host, port, and path"
                .to_string(),
        ));
    }

    Ok(parsed.as_str().to_ascii_lowercase())
}

// Diff types for detecting configuration changes
#[derive(Debug, Clone, PartialEq)]
pub enum ClusterDiff {
    TopLevelChanged(String, String, String), // field_name, old_value, new_value
    Added(String, ClusterConfig),
    Removed(String),
    HostsChanged(String, Vec<String>, Vec<String>), // cluster_name, old_hosts, new_hosts
    PortsChanged(
        String,
        HashMap<String, PortConfig>,
        HashMap<String, PortConfig>,
    ),
    TlsChanged(String),
}

#[derive(Debug, Clone)]
pub struct ConfigDiff {
    pub changes: Vec<ClusterDiff>,
}

impl ConfigDiff {
    pub fn compute(old: &Config, new: &Config) -> Self {
        let mut changes = Vec::new();

        if old.cluster_update_interval != new.cluster_update_interval {
            changes.push(ClusterDiff::TopLevelChanged(
                "cluster_update_interval".to_string(),
                old.cluster_update_interval.to_string(),
                new.cluster_update_interval.to_string(),
            ));
        }

        if old.listen_address != new.listen_address {
            changes.push(ClusterDiff::TopLevelChanged(
                "listen_address".to_string(),
                old.listen_address.clone(),
                new.listen_address.clone(),
            ));
        }

        // Find removed clusters
        for cluster_name in old.clusters.keys() {
            if !new.clusters.contains_key(cluster_name) {
                changes.push(ClusterDiff::Removed(cluster_name.clone()));
            }
        }

        // Find added or modified clusters
        for (cluster_name, new_cluster) in &new.clusters {
            match old.clusters.get(cluster_name) {
                None => {
                    changes.push(ClusterDiff::Added(
                        cluster_name.clone(),
                        new_cluster.clone(),
                    ));
                }
                Some(old_cluster) => {
                    // Check hosts changes
                    if old_cluster.hosts != new_cluster.hosts {
                        changes.push(ClusterDiff::HostsChanged(
                            cluster_name.clone(),
                            old_cluster.hosts.clone(),
                            new_cluster.hosts.clone(),
                        ));
                    }

                    // Check ports changes
                    if !ports_equal(&old_cluster.ports, &new_cluster.ports) {
                        changes.push(ClusterDiff::PortsChanged(
                            cluster_name.clone(),
                            old_cluster.ports.clone(),
                            new_cluster.ports.clone(),
                        ));
                    }

                    // Check TLS changes
                    if !tls_equal(&old_cluster.tls, &new_cluster.tls) {
                        changes.push(ClusterDiff::TlsChanged(cluster_name.clone()));
                    }
                }
            }
        }

        ConfigDiff { changes }
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn has_changes(&self) -> bool {
        !self.changes.is_empty()
    }
}

fn ports_equal(a: &HashMap<String, PortConfig>, b: &HashMap<String, PortConfig>) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (key, val_a) in a {
        match b.get(key) {
            None => return false,
            Some(val_b) => {
                if val_a.listen != val_b.listen
                    || val_a.roles != val_b.roles
                    || val_a.host_port != val_b.host_port
                    || val_a.max_lag_in_bytes != val_b.max_lag_in_bytes
                {
                    return false;
                }
            }
        }
    }
    true
}

fn tls_equal(a: &Option<TlsConfig>, b: &Option<TlsConfig>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
        (Some(tls_a), Some(tls_b)) => {
            tls_a.ca_cert == tls_b.ca_cert
                && tls_a.client_cert == tls_b.client_cert
                && tls_a.client_key == tls_b.client_key
                && tls_a.skip_verify == tls_b.skip_verify
        }
    }
}

// Repository for managing configuration with hot-reload support
pub struct ConfigRepository {
    config: ArcSwap<Config>,
    config_path: String,
}

pub struct PreparedConfigReload {
    pub diff: ConfigDiff,
    config: Config,
}

impl ConfigRepository {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let path_str = path.as_ref().to_string_lossy().to_string();
        let config = Config::from_file(&path)?;
        Ok(Self {
            config: ArcSwap::from_pointee(config),
            config_path: path_str,
        })
    }

    pub fn get(&self) -> Arc<Config> {
        self.config.load_full()
    }

    #[allow(dead_code)]
    pub fn reload(&self) -> Result<ConfigDiff, ConfigError> {
        let prepared = self.prepare_reload()?;
        Ok(self.publish_reload(prepared))
    }

    pub fn prepare_reload(&self) -> Result<PreparedConfigReload, ConfigError> {
        let new_config = Config::from_file(&self.config_path)?;
        let old_config = self.config.load();
        let diff = ConfigDiff::compute(&old_config, &new_config);

        if diff.has_changes() {
            Self::ensure_supported_hot_reload(&diff)?;
        }

        Ok(PreparedConfigReload {
            diff,
            config: new_config,
        })
    }

    pub fn publish_reload(&self, prepared: PreparedConfigReload) -> ConfigDiff {
        let diff = prepared.diff;
        if diff.has_changes() {
            self.config.store(Arc::new(prepared.config));
        }
        diff
    }

    fn ensure_supported_hot_reload(diff: &ConfigDiff) -> Result<(), ConfigError> {
        if let Some((field, old, new)) = diff.changes.iter().find_map(|change| match change {
            ClusterDiff::TopLevelChanged(field, old, new) => Some((field, old, new)),
            _ => None,
        }) {
            return Err(ConfigError::UnsupportedHotReload(format!(
                "{field} changed from '{old}' to '{new}'; restart patroni-proxy to apply top-level configuration changes"
            )));
        }
        if let Some(cluster_name) = diff.changes.iter().find_map(|change| match change {
            ClusterDiff::TlsChanged(name) => Some(name),
            _ => None,
        }) {
            return Err(ConfigError::UnsupportedHotReload(format!(
                "cluster '{cluster_name}' TLS configuration changed; restart patroni-proxy to apply TLS changes"
            )));
        }
        if let Some((cluster_name, port_name, listen)) =
            diff.changes.iter().find_map(|change| match change {
                ClusterDiff::PortsChanged(cluster_name, old, new) => {
                    old.iter().find_map(|(port_name, old_config)| {
                        new.get(port_name).and_then(|new_config| {
                            (old_config != new_config && old_config.listen == new_config.listen)
                                .then_some((cluster_name, port_name, &old_config.listen))
                        })
                    })
                }
                _ => None,
            })
        {
            return Err(ConfigError::UnsupportedHotReload(format!(
                "cluster '{cluster_name}' port '{port_name}' changed while keeping listen '{listen}'; restart patroni-proxy to apply same-listen port changes"
            )));
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub fn config_path(&self) -> &str {
        &self.config_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_roles() {
        assert!(Role::is_valid("leader"));
        assert!(Role::is_valid("Leader"));
        assert!(Role::is_valid("LEADER"));
        assert!(Role::is_valid("sync"));
        assert!(Role::is_valid("async"));
        assert!(Role::is_valid("any"));
        assert!(!Role::is_valid("master")); // master is not valid, use leader
        assert!(!Role::is_valid("replica"));
        assert!(!Role::is_valid("invalid"));
    }

    #[test]
    fn test_role_from_str() {
        assert_eq!(Role::from_str("leader").unwrap(), Role::Leader);
        assert_eq!(Role::from_str("SYNC").unwrap(), Role::Sync);
        assert_eq!(Role::from_str("Async").unwrap(), Role::Async);
        assert_eq!(Role::from_str("any").unwrap(), Role::Any);
        assert!(Role::from_str("invalid").is_err());
    }

    #[test]
    fn test_valid_config() {
        let yaml = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
      - "https://192.168.0.2:8008"
    tls:
      ca_cert: "/path/to/ca.crt"
      client_cert: "/path/to/client.crt"
      client_key: "/path/to/client.key"
      skip_verify: false
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
      any:
        listen: "127.0.0.1:6432"
        roles: ["any"]
        host_port: 6432
        max_lag_in_bytes: 16777216
"#;
        let config = Config::from_str(yaml);
        assert!(config.is_ok(), "Config should be valid: {:?}", config.err());
    }

    #[test]
    fn test_invalid_role() {
        let yaml = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["invalid_role"]
        host_port: 6432
"#;
        let config = Config::from_str(yaml);
        assert!(matches!(config, Err(ConfigError::InvalidRole(_))));
    }

    #[test]
    fn test_cluster_update_interval_rejects_zero() {
        let yaml = r#"
cluster_update_interval: 0
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
"#;
        let config = Config::from_str(yaml);
        assert!(matches!(config, Err(ConfigError::InvalidInterval(_))));
    }

    #[test]
    fn test_invalid_host_scheme() {
        let yaml = r#"
clusters:
  one:
    hosts:
      - "ftp://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
"#;
        let config = Config::from_str(yaml);
        assert!(matches!(config, Err(ConfigError::InvalidHost(_))));
    }

    #[test]
    fn test_invalid_host_rejects_userinfo_query_fragment_without_echoing_secrets() {
        for host in [
            "https://user:secret@patroni.local:8008",
            "https://patroni.local:8008?token=secret",
            "https://patroni.local:8008#secret",
        ] {
            let yaml = format!(
                r#"
clusters:
  one:
    hosts:
      - "{host}"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
"#
            );
            let err = Config::from_str(&yaml)
                .expect_err("Patroni proxy host URL must reject secret-bearing components");
            let msg = err.to_string();
            assert!(matches!(err, ConfigError::InvalidHost(_)), "{msg}");
            assert!(
                !msg.contains("secret") && !msg.contains("token"),
                "invalid host error must not echo secret URL payload: {msg}"
            );
        }
    }

    #[test]
    fn test_duplicate_hosts() {
        let yaml = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
"#;
        let config = Config::from_str(yaml);
        assert!(matches!(config, Err(ConfigError::DuplicateHost(_))));
    }

    #[test]
    fn test_duplicate_listen_same_cluster() {
        let yaml = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
      replica:
        listen: "127.0.0.1:5432"
        roles: ["sync"]
        host_port: 6432
"#;
        let config = Config::from_str(yaml);
        assert!(matches!(config, Err(ConfigError::DuplicateListen(_))));
    }

    #[test]
    fn test_duplicate_listen_different_clusters() {
        let yaml = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
  two:
    hosts:
      - "http://192.168.0.2:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
"#;
        let config = Config::from_str(yaml);
        assert!(matches!(config, Err(ConfigError::DuplicateListen(_))));
    }

    #[test]
    fn test_empty_hosts() {
        let yaml = r#"
clusters:
  one:
    hosts: []
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
"#;
        let config = Config::from_str(yaml);
        assert!(matches!(config, Err(ConfigError::EmptyHosts(_))));
    }

    #[test]
    fn test_empty_roles() {
        let yaml = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: []
        host_port: 6432
"#;
        let config = Config::from_str(yaml);
        assert!(matches!(config, Err(ConfigError::EmptyRoles(_))));
    }

    #[test]
    fn test_invalid_listen_address() {
        let yaml = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "invalid_address"
        roles: ["leader"]
        host_port: 6432
"#;
        let config = Config::from_str(yaml);
        assert!(matches!(config, Err(ConfigError::InvalidListenAddress(_))));
    }

    #[test]
    fn test_invalid_top_level_listen_address() {
        let yaml = r#"
listen_address: "not_a_socket"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
"#;
        let config = Config::from_str(yaml);
        assert!(matches!(config, Err(ConfigError::InvalidListenAddress(_))));
    }

    #[test]
    fn test_config_diff_no_changes() {
        let yaml = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
"#;
        let config1 = Config::from_str(yaml).unwrap();
        let config2 = Config::from_str(yaml).unwrap();
        let diff = ConfigDiff::compute(&config1, &config2);
        assert!(diff.is_empty());
    }

    #[test]
    fn test_config_diff_cluster_added() {
        let yaml1 = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
"#;
        let yaml2 = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
  two:
    hosts:
      - "http://192.168.0.2:8008"
    ports:
      master:
        listen: "127.0.0.1:5433"
        roles: ["leader"]
        host_port: 6432
"#;
        let config1 = Config::from_str(yaml1).unwrap();
        let config2 = Config::from_str(yaml2).unwrap();
        let diff = ConfigDiff::compute(&config1, &config2);
        assert!(diff.has_changes());
        assert!(diff
            .changes
            .iter()
            .any(|c| matches!(c, ClusterDiff::Added(name, _) if name == "two")));
    }

    #[test]
    fn test_config_diff_cluster_removed() {
        let yaml1 = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
  two:
    hosts:
      - "http://192.168.0.2:8008"
    ports:
      master:
        listen: "127.0.0.1:5433"
        roles: ["leader"]
        host_port: 6432
"#;
        let yaml2 = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
"#;
        let config1 = Config::from_str(yaml1).unwrap();
        let config2 = Config::from_str(yaml2).unwrap();
        let diff = ConfigDiff::compute(&config1, &config2);
        assert!(diff.has_changes());
        assert!(diff
            .changes
            .iter()
            .any(|c| matches!(c, ClusterDiff::Removed(name) if name == "two")));
    }

    #[test]
    fn test_config_diff_hosts_changed() {
        let yaml1 = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
"#;
        let yaml2 = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
      - "http://192.168.0.3:8008"
    ports:
      master:
        listen: "127.0.0.1:5432"
        roles: ["leader"]
        host_port: 6432
"#;
        let config1 = Config::from_str(yaml1).unwrap();
        let config2 = Config::from_str(yaml2).unwrap();
        let diff = ConfigDiff::compute(&config1, &config2);
        assert!(diff.has_changes());
        assert!(diff
            .changes
            .iter()
            .any(|c| matches!(c, ClusterDiff::HostsChanged(name, _, _) if name == "one")));
    }

    #[test]
    fn reload_rejects_tls_change_without_publishing() {
        let initial = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:15432"
        roles: ["leader"]
        host_port: 6432
"#;
        let changed = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    tls:
      skip_verify: true
    ports:
      master:
        listen: "127.0.0.1:15432"
        roles: ["leader"]
        host_port: 6432
"#;

        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), initial).unwrap();
        let repo = ConfigRepository::new(file.path()).unwrap();

        std::fs::write(file.path(), changed).unwrap();
        let reload = repo.reload();

        assert!(
            reload.is_err(),
            "Patroni TLS hot reload must be rejected because existing managers keep old TLS clients"
        );
        assert!(
            repo.get().clusters.get("one").unwrap().tls.is_none(),
            "rejected TLS reload must not publish the new config snapshot"
        );
    }

    #[test]
    fn reload_rejects_top_level_change_without_publishing() {
        let initial = r#"
cluster_update_interval: 3
listen_address: "127.0.0.1:18009"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:15432"
        roles: ["leader"]
        host_port: 6432
"#;
        let changed = r#"
cluster_update_interval: 9
listen_address: "127.0.0.1:18010"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:15432"
        roles: ["leader"]
        host_port: 6432
"#;

        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), initial).unwrap();
        let repo = ConfigRepository::new(file.path()).unwrap();

        std::fs::write(file.path(), changed).unwrap();
        let reload = repo.reload();

        assert!(
            reload.is_err(),
            "top-level patroni-proxy settings require restart because runtime tasks keep the old values"
        );
        let current = repo.get();
        assert_eq!(current.cluster_update_interval, 3);
        assert_eq!(current.listen_address, "127.0.0.1:18009");
    }

    #[test]
    fn prepare_reload_defers_publication_until_publish() {
        let initial = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:15432"
        roles: ["leader"]
        host_port: 6432
"#;
        let changed = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
      - "http://192.168.0.2:8008"
    ports:
      master:
        listen: "127.0.0.1:15432"
        roles: ["leader"]
        host_port: 6432
"#;

        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), initial).unwrap();
        let repo = ConfigRepository::new(file.path()).unwrap();

        std::fs::write(file.path(), changed).unwrap();
        let prepared = repo.prepare_reload().unwrap();

        assert!(prepared.diff.has_changes());
        assert_eq!(
            repo.get().clusters.get("one").unwrap().hosts,
            vec!["http://192.168.0.1:8008".to_string()],
            "prepare_reload must not publish before runtime apply succeeds"
        );

        let diff = repo.publish_reload(prepared);

        assert!(diff.has_changes());
        assert_eq!(
            repo.get().clusters.get("one").unwrap().hosts,
            vec![
                "http://192.168.0.1:8008".to_string(),
                "http://192.168.0.2:8008".to_string()
            ]
        );
    }

    #[test]
    fn prepare_reload_rejects_same_listen_port_change_without_publishing() {
        let initial = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:15432"
        roles: ["any"]
        host_port: 5432
"#;
        let changed = r#"
clusters:
  one:
    hosts:
      - "http://192.168.0.1:8008"
    ports:
      master:
        listen: "127.0.0.1:15432"
        roles: ["leader"]
        host_port: 6432
"#;

        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), initial).unwrap();
        let repo = ConfigRepository::new(file.path()).unwrap();

        std::fs::write(file.path(), changed).unwrap();
        let reload = repo.prepare_reload();

        assert!(
            reload.is_err(),
            "same-listen port config changes cannot be applied without dropping the old listener first"
        );
        let current = repo.get();
        let port = current
            .clusters
            .get("one")
            .unwrap()
            .ports
            .get("master")
            .unwrap();
        assert_eq!(port.roles, vec!["any".to_string()]);
        assert_eq!(port.host_port, 5432);
    }

    #[test]
    fn test_multiple_clusters() {
        let yaml = r#"
clusters:
  production:
    hosts:
      - "https://prod1.example.com:8008"
      - "https://prod2.example.com:8008"
      - "https://prod3.example.com:8008"
    tls:
      ca_cert: "/etc/ssl/ca.crt"
      skip_verify: false
    ports:
      primary:
        listen: "0.0.0.0:5432"
        roles: ["leader"]
        host_port: 6432
      replicas:
        listen: "0.0.0.0:5433"
        roles: ["sync", "async"]
        host_port: 6432
        max_lag_in_bytes: 16777216
  staging:
    hosts:
      - "http://staging1.example.com:8008"
    ports:
      all:
        listen: "0.0.0.0:5434"
        roles: ["any"]
        host_port: 6432
"#;
        let config = Config::from_str(yaml);
        assert!(config.is_ok(), "Config should be valid: {:?}", config.err());
        let config = config.unwrap();
        assert_eq!(config.clusters.len(), 2);
        assert!(config.clusters.contains_key("production"));
        assert!(config.clusters.contains_key("staging"));
    }
}
