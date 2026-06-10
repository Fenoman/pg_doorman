use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::time::{Duration, Instant};

// parking_lot::RwLock has no lock poisoning: a panic while a guard is held
// cannot permanently poison the lock and kill the single failover-tracking
// update loop (std::sync::RwLock would panic on every later .read()/.write()
// .unwrap() after a poison). It also returns the guard directly (no Result).
use parking_lot::RwLock;

use crate::config::TlsConfig;
use bytes::{Bytes, BytesMut};

/// HTTP request timeout for Patroni API
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum accepted Patroni /cluster response body size.
const MAX_CLUSTER_RESPONSE_BYTES: usize = 1024 * 1024;

/// Maximum accepted Patroni members in one /cluster response.
const MAX_CLUSTER_MEMBERS: usize = 1024;

/// Duration in seconds for which a host stays in the blacklist (down upstreams)
const BLACKLIST_DURATION_SECS: u64 = 10;

/// Patroni cluster member role
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// Cluster leader (primary)
    Leader,
    /// Synchronous replica
    Sync,
    /// Asynchronous replica
    Async,
}

impl Role {
    /// Convert Patroni role string to Role enum
    pub fn from_patroni_role(role: &str) -> Option<Role> {
        match role {
            "leader" => Some(Role::Leader),
            "sync_standby" => Some(Role::Sync),
            "replica" => Some(Role::Async),
            _ => None,
        }
    }

    /// Convert Role enum to Patroni role string
    #[allow(dead_code)]
    pub fn to_patroni_role(self) -> &'static str {
        match self {
            Role::Leader => "leader",
            Role::Sync => "sync_standby",
            Role::Async => "replica",
        }
    }
}

/// Cluster member tags (optional fields)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberTags {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clonefrom: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noloadbalance: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicatefrom: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nosync: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nofailover: Option<bool>,
    /// Additional user-defined tags
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Patroni cluster member
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    pub role: String,
    pub state: String,
    pub api_url: String,
    pub host: String,
    pub port: u16,
    /// Timeline value in Patroni may have inconsistent typing - stored as generic JSON
    #[serde(default)]
    pub timeline: Value,
    /// Lag field is present on replicas and may be absent - stored as generic JSON
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag: Option<Value>,
    /// Optional tags object; may be absent; fields inside are also optional
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<MemberTags>,
}

impl Member {
    /// Get member role as enum
    pub fn get_role(&self) -> Option<Role> {
        Role::from_patroni_role(&self.role)
    }
}

/// Patroni cluster
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cluster {
    pub members: Vec<Member>,
    pub scope: String,
}

/// Patroni API errors
#[derive(Debug)]
pub enum PatroniError {
    /// HTTP request error
    HttpError(reqwest::Error),
    /// JSON parsing error
    ParseError(serde_json::Error),
    /// All hosts are unavailable
    AllHostsUnavailable,
    /// Request timeout
    Timeout,
    /// Patroni response body exceeds the configured cap
    BodyTooLarge { limit: usize },
    /// Patroni response contains too many members
    TooManyMembers { count: usize, limit: usize },
    /// The endpoint responded with a non-2xx HTTP status (carries the code).
    /// This is transient (e.g. a healthy follower replying 503 during an
    /// election); unlike connect/parse failures it must NOT blacklist the host.
    ServerError(u16),
}

impl std::fmt::Display for PatroniError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PatroniError::HttpError(e) => write!(f, "HTTP error: {e}"),
            PatroniError::ParseError(e) => write!(f, "Parse error: {e}"),
            PatroniError::AllHostsUnavailable => write!(f, "All hosts unavailable"),
            PatroniError::Timeout => write!(f, "Request timeout"),
            PatroniError::BodyTooLarge { limit } => {
                write!(f, "Patroni /cluster response exceeds {limit} bytes")
            }
            PatroniError::TooManyMembers { count, limit } => write!(
                f,
                "Patroni /cluster response contains {count} members; limit is {limit}"
            ),
            PatroniError::ServerError(status) => {
                write!(f, "Patroni endpoint returned HTTP {status}")
            }
        }
    }
}

impl std::error::Error for PatroniError {}

impl From<reqwest::Error> for PatroniError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            PatroniError::Timeout
        } else {
            PatroniError::HttpError(e)
        }
    }
}

impl From<serde_json::Error> for PatroniError {
    fn from(e: serde_json::Error) -> Self {
        PatroniError::ParseError(e)
    }
}

/// Blacklist of unavailable hosts
struct HostBlacklist {
    /// Map of host -> time when added to blacklist
    entries: RwLock<HashMap<String, Instant>>,
}

impl HostBlacklist {
    fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Check if host is in the blacklist
    fn is_blacklisted(&self, host: &str) -> bool {
        let entries = self.entries.read();
        if let Some(added_at) = entries.get(host) {
            added_at.elapsed().as_secs() < BLACKLIST_DURATION_SECS
        } else {
            false
        }
    }

    /// Add host to the blacklist
    fn add(&self, host: &str) {
        let mut entries = self.entries.write();
        entries.insert(host.to_string(), Instant::now());
    }

    /// Remove host from the blacklist (on successful connection)
    fn remove(&self, host: &str) {
        let mut entries = self.entries.write();
        entries.remove(host);
    }

    /// Clean up expired entries
    fn cleanup(&self) {
        let mut entries = self.entries.write();
        entries.retain(|_, added_at| added_at.elapsed().as_secs() < BLACKLIST_DURATION_SECS);
    }
}

/// Client for Patroni API
pub struct PatroniClient {
    /// HTTP client
    client: reqwest::Client,
    /// Blacklist of unavailable hosts
    blacklist: HostBlacklist,
}

impl PatroniClient {
    /// Create a new Patroni API client
    pub fn new() -> Result<Self, String> {
        Self::new_with_tls(None)
    }

    /// Create a new Patroni API client with optional TLS settings from config.
    pub fn new_with_tls(tls: Option<&TlsConfig>) -> Result<Self, String> {
        let mut builder = reqwest::Client::builder().timeout(HTTP_TIMEOUT);

        if let Some(tls) = tls {
            if tls.skip_verify.unwrap_or(false) {
                builder = builder.danger_accept_invalid_certs(true);
            }

            if let Some(ca_cert) = &tls.ca_cert {
                let pem = fs::read(ca_cert)
                    .map_err(|e| format!("failed to read Patroni TLS ca_cert {ca_cert}: {e}"))?;
                let cert = reqwest::Certificate::from_pem(&pem)
                    .map_err(|e| format!("invalid Patroni TLS ca_cert {ca_cert}: {e}"))?;
                builder = builder.add_root_certificate(cert);
            }

            match (&tls.client_cert, &tls.client_key) {
                (Some(_), Some(_)) => {
                    return Err(
                        "Patroni TLS client_cert/client_key are not supported by this build"
                            .to_string(),
                    );
                }
                (None, None) => {}
                _ => {
                    return Err(
                        "Patroni TLS client_cert and client_key must be configured together"
                            .to_string(),
                    );
                }
            }
        }

        let client = builder.build().map_err(|e| e.to_string())?;

        Ok(Self {
            client,
            blacklist: HostBlacklist::new(),
        })
    }

    /// Fetch cluster information from the specified URL
    ///
    /// # Arguments
    /// * `url` - base Patroni API URL (e.g., http://192.168.0.1:8008)
    pub async fn fetch_cluster(&self, url: &str) -> Result<Cluster, PatroniError> {
        let cluster_url = format!("{}/cluster", url.trim_end_matches('/'));
        let response = self.client.get(&cluster_url).send().await?;
        // Check the HTTP status before parsing. A healthy follower can reply
        // 503 during an election; that body is not JSON, and feeding it to
        // serde_json would yield a ParseError that fetch_members blacklists for
        // the full duration. Treat non-2xx as a transient ServerError instead
        // (mirrors src/patroni/client.rs status().is_success() handling).
        if !response.status().is_success() {
            return Err(PatroniError::ServerError(response.status().as_u16()));
        }
        let body = read_limited_body(response).await?;
        parse_cluster_body(&body)
    }

    /// Fetch cluster members by iterating through hosts
    ///
    /// Optimizations:
    /// - Hosts that don't respond are added to blacklist for 10 seconds
    /// - If all hosts are blacklisted, try all hosts from the blacklist
    ///
    /// # Arguments
    /// * `hosts` - list of base Patroni API URLs
    pub async fn fetch_members(&self, hosts: &[String]) -> Result<Vec<Member>, PatroniError> {
        // Clean up expired blacklist entries
        self.blacklist.cleanup();

        // Split hosts into available and blacklisted
        let mut available: Vec<&String> = Vec::new();
        let mut blacklisted: Vec<&String> = Vec::new();

        for host in hosts {
            if self.blacklist.is_blacklisted(host) {
                blacklisted.push(host);
            } else {
                available.push(host);
            }
        }

        // Determine iteration order: available first, then blacklisted
        // If all are blacklisted - try all
        let all_in_blacklist = available.is_empty();

        // First try available hosts (or all if all are blacklisted)
        let first_batch = if all_in_blacklist {
            &blacklisted
        } else {
            &available
        };

        for host in first_batch {
            match self.fetch_cluster(host).await {
                Ok(cluster) => {
                    // Successfully got response - remove from blacklist
                    self.blacklist.remove(host);
                    return Ok(cluster.members);
                }
                // Transient non-2xx (e.g. a follower's 503 during an election):
                // the endpoint is reachable but not currently serving the
                // cluster view. Log and move on WITHOUT blacklisting, so we do
                // not drop a healthy host for the full blacklist duration.
                Err(PatroniError::ServerError(status)) => {
                    tracing::warn!(
                        "Patroni endpoint {} returned transient HTTP {}; not blacklisting",
                        host,
                        status
                    );
                }
                Err(e) => {
                    // Add to blacklist
                    tracing::warn!("Failed to fetch cluster from {}: {}", host, e);
                    self.blacklist.add(host);
                }
            }
        }

        // If not all were blacklisted, try hosts from the blacklist
        if !all_in_blacklist {
            for host in &blacklisted {
                match self.fetch_cluster(host).await {
                    Ok(cluster) => {
                        self.blacklist.remove(host);
                        return Ok(cluster.members);
                    }
                    // Transient non-2xx: do not extend/refresh the blacklist.
                    Err(PatroniError::ServerError(status)) => {
                        tracing::warn!(
                            "Patroni endpoint {} returned transient HTTP {}; not blacklisting",
                            host,
                            status
                        );
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch cluster from {}: {}", host, e);
                        self.blacklist.add(host);
                    }
                }
            }
        }

        Err(PatroniError::AllHostsUnavailable)
    }
}

async fn read_limited_body(mut response: reqwest::Response) -> Result<Bytes, PatroniError> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_CLUSTER_RESPONSE_BYTES as u64)
    {
        return Err(PatroniError::BodyTooLarge {
            limit: MAX_CLUSTER_RESPONSE_BYTES,
        });
    }

    let mut body = BytesMut::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_CLUSTER_RESPONSE_BYTES {
            return Err(PatroniError::BodyTooLarge {
                limit: MAX_CLUSTER_RESPONSE_BYTES,
            });
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body.freeze())
}

fn parse_cluster_body(body: &[u8]) -> Result<Cluster, PatroniError> {
    if body.len() > MAX_CLUSTER_RESPONSE_BYTES {
        return Err(PatroniError::BodyTooLarge {
            limit: MAX_CLUSTER_RESPONSE_BYTES,
        });
    }

    let cluster: Cluster = serde_json::from_slice(body)?;
    if cluster.members.len() > MAX_CLUSTER_MEMBERS {
        return Err(PatroniError::TooManyMembers {
            count: cluster.members.len(),
            limit: MAX_CLUSTER_MEMBERS,
        });
    }

    Ok(cluster)
}

impl Default for PatroniClient {
    fn default() -> Self {
        Self::new().expect("Failed to create PatroniClient")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_from_patroni_role() {
        assert_eq!(Role::from_patroni_role("leader"), Some(Role::Leader));
        assert_eq!(Role::from_patroni_role("sync_standby"), Some(Role::Sync));
        assert_eq!(Role::from_patroni_role("replica"), Some(Role::Async));
        assert_eq!(Role::from_patroni_role("unknown"), None);
    }

    #[test]
    fn test_role_to_patroni_role() {
        assert_eq!(Role::Leader.to_patroni_role(), "leader");
        assert_eq!(Role::Sync.to_patroni_role(), "sync_standby");
        assert_eq!(Role::Async.to_patroni_role(), "replica");
    }

    #[test]
    fn test_member_deserialization() {
        let json = r#"{
            "name": "node1",
            "role": "leader",
            "state": "running",
            "api_url": "http://192.168.0.1:8008/patroni",
            "host": "192.168.0.1",
            "port": 5432,
            "timeline": 1
        }"#;

        let member: Member = serde_json::from_str(json).unwrap();
        assert_eq!(member.name, "node1");
        assert_eq!(member.role, "leader");
        assert_eq!(member.get_role(), Some(Role::Leader));
        assert_eq!(member.host, "192.168.0.1");
        assert_eq!(member.port, 5432);
    }

    #[test]
    fn test_member_with_tags() {
        let json = r#"{
            "name": "node2",
            "role": "replica",
            "state": "running",
            "api_url": "http://192.168.0.2:8008/patroni",
            "host": "192.168.0.2",
            "port": 5432,
            "timeline": 1,
            "lag": 0,
            "tags": {
                "noloadbalance": false,
                "clonefrom": true
            }
        }"#;

        let member: Member = serde_json::from_str(json).unwrap();
        assert_eq!(member.name, "node2");
        assert_eq!(member.get_role(), Some(Role::Async));
        assert!(member.tags.is_some());
        let tags = member.tags.unwrap();
        assert_eq!(tags.noloadbalance, Some(false));
        assert_eq!(tags.clonefrom, Some(true));
    }

    #[test]
    fn test_cluster_deserialization() {
        let json = r#"{
            "scope": "my_cluster",
            "members": [
                {
                    "name": "node1",
                    "role": "leader",
                    "state": "running",
                    "api_url": "http://192.168.0.1:8008/patroni",
                    "host": "192.168.0.1",
                    "port": 5432,
                    "timeline": 1
                },
                {
                    "name": "node2",
                    "role": "sync_standby",
                    "state": "running",
                    "api_url": "http://192.168.0.2:8008/patroni",
                    "host": "192.168.0.2",
                    "port": 5432,
                    "timeline": 1,
                    "lag": 0
                }
            ]
        }"#;

        let cluster: Cluster = serde_json::from_str(json).unwrap();
        assert_eq!(cluster.scope, "my_cluster");
        assert_eq!(cluster.members.len(), 2);
        assert_eq!(cluster.members[0].get_role(), Some(Role::Leader));
        assert_eq!(cluster.members[1].get_role(), Some(Role::Sync));
    }

    #[test]
    fn parse_cluster_body_rejects_oversized_body_before_json() {
        let body = vec![b' '; MAX_CLUSTER_RESPONSE_BYTES + 1];
        let err = parse_cluster_body(&body).expect_err("oversized body must be rejected");

        assert!(matches!(err, PatroniError::BodyTooLarge { .. }));
    }

    #[test]
    fn parse_cluster_body_rejects_too_many_members() {
        let member = r#"{
            "name": "node",
            "role": "replica",
            "state": "running",
            "api_url": "http://192.168.0.2:8008/patroni",
            "host": "192.168.0.2",
            "port": 5432,
            "timeline": 1
        }"#;
        let members = vec![member; MAX_CLUSTER_MEMBERS + 1].join(",");
        let body = format!(r#"{{"scope":"my_cluster","members":[{members}]}}"#);

        let err = parse_cluster_body(body.as_bytes()).expect_err("oversized member list must fail");

        assert!(matches!(err, PatroniError::TooManyMembers { .. }));
    }

    #[test]
    fn test_blacklist() {
        let blacklist = HostBlacklist::new();

        assert!(!blacklist.is_blacklisted("host1"));

        blacklist.add("host1");
        assert!(blacklist.is_blacklisted("host1"));

        blacklist.remove("host1");
        assert!(!blacklist.is_blacklisted("host1"));
    }

    // guards HostBlacklist add/contains/remove/cleanup behavior across
    // the std::sync::RwLock -> parking_lot::RwLock swap (parking_lot has no lock
    // poisoning, so a panic-while-locked can no longer permanently kill the
    // single failover-tracking update loop).
    #[test]
    fn host_blacklist_add_contains_remove_cleanup() {
        let bl = HostBlacklist::new();

        assert!(!bl.is_blacklisted("http://h1"));
        bl.add("http://h1");
        assert!(bl.is_blacklisted("http://h1"));

        // A second host is tracked independently.
        bl.add("http://h2");
        assert!(bl.is_blacklisted("http://h2"));

        // remove() clears only the named host.
        bl.remove("http://h1");
        assert!(!bl.is_blacklisted("http://h1"));
        assert!(bl.is_blacklisted("http://h2"));

        // cleanup() keeps still-fresh entries (well within BLACKLIST_DURATION_SECS).
        bl.cleanup();
        assert!(bl.is_blacklisted("http://h2"));
    }

    // a healthy follower returning 503 must surface as a transient
    // ServerError, NOT a ParseError. Previously fetch_cluster never checked the
    // HTTP status, so the non-JSON 503 body flowed into serde_json -> ParseError,
    // which fetch_members blacklists for the full duration -- dropping a
    // perfectly reachable endpoint.
    #[tokio::test]
    async fn fetch_cluster_returns_server_error_on_503() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let body = "503 Service Unavailable";
            let resp = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        });

        let client = PatroniClient::new().unwrap();
        let endpoint = format!("http://{addr}");
        let err = client.fetch_cluster(&endpoint).await.unwrap_err();

        assert!(
            matches!(err, PatroniError::ServerError(503)),
            "503 must be a transient ServerError(503), got {err:?}"
        );
        assert!(
            !matches!(err, PatroniError::ParseError(_)),
            "503 must not be misclassified as a parse failure"
        );

        let _ = server.await;
    }

    // A transient ServerError from one host must NOT add it to the blacklist.
    #[tokio::test]
    async fn fetch_members_does_not_blacklist_on_server_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let body = "503";
            let resp = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        });

        let client = PatroniClient::new().unwrap();
        let host = format!("http://{addr}");
        let _ = client.fetch_members(std::slice::from_ref(&host)).await;

        assert!(
            !client.blacklist.is_blacklisted(&host),
            "a transient 503 must not blacklist the endpoint"
        );

        let _ = server.await;
    }
}
