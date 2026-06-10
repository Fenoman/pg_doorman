use std::path::Path;
use std::{fs, str::FromStr};

use ipnet::IpNet;

use crate::transport::ClientTransport;

/// Authentication method supported by our checker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMethod {
    Trust,
    Md5,
    ScramSha256,
    Reject,
    Other(String), // keep unrecognized for completeness
}

impl AuthMethod {
    fn from_token(tok: &str) -> Self {
        match tok.to_ascii_lowercase().as_str() {
            "trust" => AuthMethod::Trust,
            "md5" => AuthMethod::Md5,
            "scram-sha-256" | "scram_sha_256" | "scramsha256" => AuthMethod::ScramSha256,
            "reject" => AuthMethod::Reject,
            other => AuthMethod::Other(other.to_string()),
        }
    }
}

/// Matcher for database/user fields (supports keyword `all` and comma lists).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameMatcher {
    All,
    Name(String),
    List(Vec<String>),
}

impl NameMatcher {
    fn from_token(tok: &str) -> Self {
        if tok.contains(',') {
            NameMatcher::List(
                tok.split(',')
                    .filter_map(|part| {
                        let part = part.trim();
                        (!part.is_empty()).then(|| part.to_string())
                    })
                    .collect(),
            )
        } else if tok.eq_ignore_ascii_case("all") {
            NameMatcher::All
        } else {
            NameMatcher::Name(tok.to_string())
        }
    }

    fn from_hba_token(tok: &HbaToken) -> Result<Self, String> {
        reject_mixed_quoted_comma_token(tok)?;
        if tok.quoted {
            Ok(NameMatcher::Name(tok.value.clone()))
        } else {
            Ok(Self::from_token(&tok.value))
        }
    }

    fn try_from_token(tok: &str) -> Result<Self, String> {
        validate_name_token(tok)?;
        Ok(Self::from_token(tok))
    }

    fn try_from_hba_token(tok: &HbaToken) -> Result<Self, String> {
        reject_mixed_quoted_comma_token(tok)?;
        if tok.quoted {
            Ok(NameMatcher::Name(tok.value.clone()))
        } else {
            Self::try_from_token(&tok.value)
        }
    }

    pub(crate) fn matches(&self, value: &str) -> bool {
        match self {
            NameMatcher::All => true,
            NameMatcher::Name(ref n) => n == value,
            NameMatcher::List(names) => names
                .iter()
                .any(|n| n.eq_ignore_ascii_case("all") || n == value),
        }
    }
}

fn database_matcher_from_hba_token(tok: &HbaToken) -> Result<NameMatcher, String> {
    reject_mixed_quoted_comma_token(tok)?;
    if tok.quoted {
        return Ok(NameMatcher::Name(tok.value.clone()));
    }

    validate_database_token(&tok.value)?;
    Ok(NameMatcher::from_token(&tok.value))
}

fn reject_mixed_quoted_comma_token(tok: &HbaToken) -> Result<(), String> {
    if tok.quoted && tok.has_unquoted_comma {
        return Err(format!(
            "mixed quoted comma-list pg_hba name token '{}'",
            tok.value
        ));
    }
    Ok(())
}

fn validate_name_token(tok: &str) -> Result<(), String> {
    let parts = tok.split(',');
    for part in parts {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("empty item in pg_hba name list '{tok}'"));
        }
        let lower = part.to_ascii_lowercase();
        if lower == "sameuser"
            || lower == "samerole"
            || lower == "samegroup"
            || part.starts_with('+')
            || part.starts_with('/')
            || part.starts_with('@')
        {
            return Err(format!(
                "unsupported pg_hba database/user token '{part}' in '{tok}'"
            ));
        }
    }
    Ok(())
}

fn validate_database_token(tok: &str) -> Result<(), String> {
    validate_name_token(tok)?;
    for part in tok.split(',') {
        let part = part.trim();
        if part.eq_ignore_ascii_case("replication") {
            return Err(format!(
                "unsupported pg_hba database token '{part}' in '{tok}'"
            ));
        }
    }
    Ok(())
}

/// A single pg_hba.conf rule reduced to what we need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HbaRule {
    pub host_type: HostType,
    pub database: NameMatcher,
    pub user: NameMatcher,
    pub address: Option<IpNet>,
    pub method: AuthMethod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostType {
    Local,
    Host,
    HostSSL,
    HostNoSSL,
}

impl HostType {
    fn from_token(tok: &str) -> Option<Self> {
        match tok.to_ascii_lowercase().as_str() {
            "local" => Some(HostType::Local),
            "host" => Some(HostType::Host),
            "hostssl" => Some(HostType::HostSSL),
            "hostnossl" => Some(HostType::HostNoSSL),
            _ => None,
        }
    }

    fn matches_ssl(&self, ssl: bool) -> bool {
        match self {
            HostType::Local => true,
            HostType::Host => true,
            HostType::HostSSL => ssl,
            HostType::HostNoSSL => !ssl,
        }
    }
}

/// Result of `check_hba` evaluation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckResult {
    /// No HBA rule matched for given connection parameters and auth type
    NotMatched,
    /// Explicitly forbidden by a matching `reject` rule
    Deny,
    /// Matched rule allows given auth type
    Allow,
    /// Matched rule with `trust` method (no password expected)
    Trust,
}

/// Parsed pg_hba set of rules, in order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PgHba {
    pub rules: Vec<HbaRule>,
}

// Human-readable formatting for pg_hba components
impl std::fmt::Display for NameMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NameMatcher::All => f.write_str("all"),
            NameMatcher::Name(s) => f.write_str(s),
            NameMatcher::List(names) => f.write_str(&names.join(",")),
        }
    }
}

impl std::fmt::Display for HostType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            HostType::Local => "local",
            HostType::Host => "host",
            HostType::HostSSL => "hostssl",
            HostType::HostNoSSL => "hostnossl",
        };
        f.write_str(s)
    }
}

impl std::fmt::Display for AuthMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthMethod::Trust => f.write_str("trust"),
            AuthMethod::Md5 => f.write_str("md5"),
            AuthMethod::ScramSha256 => f.write_str("scram-sha-256"),
            AuthMethod::Reject => f.write_str("reject"),
            AuthMethod::Other(s) => f.write_str(s),
        }
    }
}

impl std::fmt::Display for HbaRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.host_type {
            HostType::Local => {
                write!(
                    f,
                    "{} {} {} {}",
                    self.host_type, self.database, self.user, self.method
                )
            }
            _ => {
                if let Some(addr) = &self.address {
                    write!(
                        f,
                        "{} {} {} {} {}",
                        self.host_type, self.database, self.user, addr, self.method
                    )
                } else {
                    // address missing (unknown format when parsed) — emit without it
                    write!(
                        f,
                        "{} {} {} {}",
                        self.host_type, self.database, self.user, self.method
                    )
                }
            }
        }
    }
}

impl std::fmt::Display for PgHba {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, rule) in self.rules.iter().enumerate() {
            if i > 0 {
                f.write_str("\n")?;
            }
            write!(f, "{rule}")?;
        }
        Ok(())
    }
}

// Serde support: you can define this in TOML as a string (inline content),
// or as a table with either { path = "..." } or { content = "..." }.
// Examples:
//   hba = """
//   host all all 0.0.0.0/0 md5
//   hostssl all all 10.0.0.0/8 scram-sha-256
//   """
//   hba = { path = "./pg_hba.conf" }
//   hba = { content = "host all all 127.0.0.1/32 trust" }
impl<'de> serde::Deserialize<'de> for PgHba {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as DeError, MapAccess, Visitor};
        use std::fmt;

        struct PgHbaVisitor;

        impl<'de> Visitor<'de> for PgHbaVisitor {
            type Value = PgHba;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string with pg_hba content or a map with { path = \"...\" } or { content = \"...\" }")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                PgHba::try_from_content(v).map_err(DeError::custom)
            }

            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                PgHba::try_from_content(&v).map_err(DeError::custom)
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut path: Option<String> = None;
                let mut content: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "path" => {
                            if path.is_some() {
                                return Err(DeError::duplicate_field("path"));
                            }
                            path = Some(map.next_value()?);
                        }
                        "content" => {
                            if content.is_some() {
                                return Err(DeError::duplicate_field("content"));
                            }
                            content = Some(map.next_value()?);
                        }
                        other => {
                            // consume and ignore unknown
                            let _ignored: serde::de::IgnoredAny = map.next_value()?;
                            return Err(DeError::unknown_field(other, &["path", "content"]));
                        }
                    }
                }

                if let Some(c) = content {
                    return PgHba::try_from_content(&c).map_err(DeError::custom);
                }
                if let Some(p) = path {
                    let data = fs::read_to_string(&p).map_err(|e| {
                        DeError::custom(format!("failed to read hba file {p}: {e}"))
                    })?;
                    return PgHba::try_from_content(&data).map_err(DeError::custom);
                }
                Err(DeError::custom(
                    "expected either 'path' or 'content' field for PgHba",
                ))
            }
        }

        deserializer.deserialize_any(PgHbaVisitor)
    }
}

impl PgHba {
    /// Parse from file path (utf-8 text expected)
    pub fn from_path(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let content = fs::read_to_string(path)?;
        Self::try_from_content(&content)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
    }

    /// Parse from string content of a pg_hba.conf
    pub fn from_content(content: &str) -> Self {
        Self::parse_content(content, false).expect("non-strict pg_hba parse must not fail")
    }

    /// Parse from string content and reject pg_hba name constructs that this
    /// reduced checker cannot evaluate safely.
    pub fn try_from_content(content: &str) -> Result<Self, String> {
        Self::parse_content(content, true)
    }

    fn parse_content(content: &str, strict_names: bool) -> Result<Self, String> {
        let mut rules = Vec::new();
        for (line_no, raw_line) in content.lines().enumerate() {
            let line = strip_comments(raw_line).trim();
            if line.is_empty() {
                continue;
            }

            let tokens = match shell_like_split(line) {
                Ok(tokens) => tokens,
                Err(err) => {
                    if strict_names {
                        return Err(format!("pg_hba line {}: {err}", line_no + 1));
                    }
                    log::warn!(
                        "[pg_hba] dropping malformed rule on line {}: {err}",
                        line_no + 1
                    );
                    continue;
                }
            };
            if tokens.is_empty() {
                continue;
            }

            // connection type
            let Some(ht) = HostType::from_token(&tokens[0].value) else {
                if strict_names {
                    return Err(format!(
                        "pg_hba line {}: unsupported pg_hba record type '{}'",
                        line_no + 1,
                        tokens[0].value
                    ));
                }
                continue;
            };

            // Minimal pg_hba format:
            // type  database  user  address  method
            // For local, address is omitted. Strict config parsing rejects
            // PostgreSQL auth-options (`clientcert=...`, `map=...`, etc.)
            // because this reduced evaluator cannot enforce them safely.

            // Ensure we have enough tokens to read method and optional address.
            // We'll map positions based on host type.
            // Parse database and user (common positions)
            if tokens.len() < 3 {
                if strict_names {
                    return Err(format!(
                        "pg_hba line {}: too few fields for pg_hba record",
                        line_no + 1
                    ));
                }
                continue;
            }
            let database = match database_matcher_from_hba_token(&tokens[1]) {
                Ok(database) => database,
                Err(err) => {
                    if strict_names {
                        return Err(format!("pg_hba line {}: {err}", line_no + 1));
                    }
                    log::warn!(
                        "[pg_hba] dropping rule with unsupported database token on line {}: {err}",
                        line_no + 1
                    );
                    continue;
                }
            };
            let user = match if strict_names {
                NameMatcher::try_from_hba_token(&tokens[2])
            } else {
                NameMatcher::from_hba_token(&tokens[2])
            } {
                Ok(user) => user,
                Err(err) => {
                    if strict_names {
                        return Err(format!("pg_hba line {}: {err}", line_no + 1));
                    }
                    log::warn!(
                        "[pg_hba] dropping rule with ambiguous user token on line {}: {err}",
                        line_no + 1
                    );
                    continue;
                }
            };

            let (method_idx, address_opt) = match ht {
                HostType::Local => {
                    // type database user method [options]
                    if tokens.len() < 4 {
                        if strict_names {
                            return Err(format!(
                                "pg_hba line {}: too few fields for local pg_hba record",
                                line_no + 1
                            ));
                        }
                        continue;
                    }
                    let method_idx = 3;
                    (method_idx, None)
                }
                _ => {
                    // type database user address method [options]
                    if tokens.len() < 5 {
                        if strict_names {
                            return Err(format!(
                                "pg_hba line {}: too few fields for host pg_hba record",
                                line_no + 1
                            ));
                        }
                        continue;
                    }
                    let addr_token = &tokens[3].value;
                    // parse_address returning None used
                    // to fall through as "no address restriction" - the
                    // resulting rule matched every TCP peer. PG semantics:
                    // a malformed address makes the rule invalid; we drop
                    // it and log a warning so operators notice.
                    let address = match parse_address(addr_token) {
                        Some(net) => net,
                        None => {
                            if strict_names {
                                return Err(format!(
                                    "pg_hba line {}: unsupported pg_hba address token \
                                     '{addr_token}' (only CIDR forms like \
                                     192.168.0.0/24 or 2001:db8::/32 are supported)",
                                    line_no + 1
                                ));
                            }
                            log::warn!(
                                "[pg_hba] dropping rule with unsupported \
                                 address token '{addr_token}' (only CIDR \
                                 forms like 192.168.0.0/24 or \
                                 2001:db8::/32 are supported)"
                            );
                            continue;
                        }
                    };
                    let method_idx = 4;
                    (method_idx, Some(address))
                }
            };

            let method_token = tokens[method_idx].value.as_str();
            let method = AuthMethod::from_token(method_token);
            if let AuthMethod::Other(method_name) = &method {
                if strict_names {
                    return Err(format!(
                        "pg_hba line {}: unsupported pg_hba auth method '{}'",
                        line_no + 1,
                        method_name
                    ));
                }
                log::warn!("[pg_hba] dropping rule with unsupported auth method '{method_name}'");
                continue;
            }
            if strict_names && tokens.len() > method_idx + 1 {
                return Err(format!(
                    "pg_hba line {}: unsupported pg_hba auth-options after method '{}'",
                    line_no + 1,
                    tokens[method_idx].value
                ));
            }

            rules.push(HbaRule {
                host_type: ht,
                database,
                user,
                address: address_opt,
                method,
            });
        }
        Ok(PgHba { rules })
    }

    /// Evaluate given connection parameters against parsed HBA rules.
    ///
    /// - `transport`: how the client reached the pooler (TCP + SSL state,
    ///   or a Unix socket). Drives `local` vs `host*` rule matching.
    /// - `type_auth`: requested auth method name, e.g. "md5" or "scram-sha-256"
    /// - `username`: database user name
    /// - `database`: target database name
    ///
    /// Returns the decision from the first rule whose transport, database,
    /// user, and address fields match. This mirrors PostgreSQL pg_hba rule
    /// ordering: a later rule must not override an earlier method mismatch.
    pub fn check_hba(
        &self,
        transport: &ClientTransport,
        type_auth: &str,
        username: &str,
        database: &str,
    ) -> CheckResult {
        // match case-insensitively without allocating a
        // lowercased String per connection. Production callers pass the
        // literal "md5" / "scram-sha-256", so the common path now does
        // zero allocation.
        let want = if type_auth.eq_ignore_ascii_case("md5") {
            AuthMethod::Md5
        } else if type_auth.eq_ignore_ascii_case("scram-sha-256")
            || type_auth.eq_ignore_ascii_case("scram_sha_256")
            || type_auth.eq_ignore_ascii_case("scramsha256")
        {
            AuthMethod::ScramSha256
        } else {
            AuthMethod::Other(type_auth.to_string())
        };

        for rule in &self.rules {
            match rule.host_type {
                HostType::Local => {
                    // local rules match only Unix socket connections
                    if !transport.is_unix() {
                        continue;
                    }
                }
                _ => {
                    // host/hostssl/hostnossl rules match only TCP connections
                    if transport.is_unix() {
                        continue;
                    }
                    if !rule.host_type.matches_ssl(transport.is_tls()) {
                        continue;
                    }
                    // defense-in-depth - even if
                    // a future change ever lets `host*` rule reach here
                    // with `address: None`, refuse to match instead of
                    // falling through as "any address".
                    match &rule.address {
                        Some(net) => {
                            if !net.contains(&transport.hba_ip()) {
                                continue;
                            }
                        }
                        None => continue,
                    }
                }
            }
            // Database and user must match as well (supporting keyword `all`).
            if !rule.database.matches(database) || !rule.user.matches(username) {
                continue;
            }

            // First matching rule that applies decides. PostgreSQL's `md5`
            // method can still use SCRAM when the stored verifier is SCRAM, so
            // treat it as compatible with both verifier families here.
            match rule.method {
                AuthMethod::Trust => return CheckResult::Trust,
                AuthMethod::Reject => return CheckResult::Deny,
                AuthMethod::Md5 if matches!(want, AuthMethod::Md5 | AuthMethod::ScramSha256) => {
                    return CheckResult::Allow
                }
                AuthMethod::ScramSha256 if want == AuthMethod::ScramSha256 => {
                    return CheckResult::Allow
                }
                ref m if *m == want => return CheckResult::Allow,
                _ => return CheckResult::Deny,
            }
        }
        CheckResult::NotMatched
    }
}

fn strip_comments(s: &str) -> &str {
    let mut in_quotes = false;
    for (idx, c) in s.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => return &s[..idx],
            _ => {}
        }
    }
    s
}

/// Very small splitter that treats consecutive whitespace as separators and supports
/// double-quoted tokens with spaces (like "db name"). It does not support escapes inside quotes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HbaToken {
    value: String,
    quoted: bool,
    has_unquoted_comma: bool,
}

fn shell_like_split(line: &str) -> Result<Vec<HbaToken>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut quoted = false;
    let mut has_unquoted_comma = false;

    for c in line.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                quoted = true;
            }
            ',' if !in_quotes => {
                has_unquoted_comma = true;
                cur.push(c);
            }
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    out.push(HbaToken {
                        value: std::mem::take(&mut cur),
                        quoted,
                        has_unquoted_comma,
                    });
                    quoted = false;
                    has_unquoted_comma = false;
                }
            }
            _ => cur.push(c),
        }
    }
    if in_quotes {
        return Err("unterminated quoted token".to_string());
    }
    if !cur.is_empty() {
        out.push(HbaToken {
            value: cur,
            quoted,
            has_unquoted_comma,
        });
    }
    Ok(out)
}

fn parse_address(token: &str) -> Option<IpNet> {
    // token may be:
    // - a CIDR: 192.168.0.0/24 or 2001:db8::/32
    // - an IP + mask: 192.168.0.0 255.255.255.0 (but this would be two tokens; we don't support here)
    // - a single IP meaning /32 or /128 (not standard for pg_hba, so keep to CIDR)
    IpNet::from_str(token).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    const SAMPLE: &str = r#"
# comment
host all all 10.0.0.0/8 md5
hostssl all all 192.168.0.0/16 scram-sha-256
hostnossl all all 127.0.0.1/32 trust
"#;

    fn tcp(peer: IpAddr, ssl: bool) -> ClientTransport {
        ClientTransport::Tcp {
            peer: SocketAddr::new(peer, 54321),
            ssl,
        }
    }

    fn unix_transport() -> ClientTransport {
        ClientTransport::Unix
    }

    #[test]
    fn parse_and_check() {
        let hba = PgHba::from_content(SAMPLE);
        assert_eq!(hba.rules.len(), 3);

        // md5 allowed for 10.1.2.3 over non-ssl and ssl (host matches both)
        let ip = IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3));
        assert_eq!(
            hba.check_hba(&tcp(ip, false), "md5", "alice", "app"),
            CheckResult::Allow
        );
        assert_eq!(
            hba.check_hba(&tcp(ip, true), "md5", "alice", "app"),
            CheckResult::Allow
        );

        // scram allowed for 192.168.1.10 only with ssl
        let ip2 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10));
        assert_eq!(
            hba.check_hba(&tcp(ip2, true), "scram-sha-256", "alice", "app"),
            CheckResult::Allow
        );
        assert_eq!(
            hba.check_hba(&tcp(ip2, false), "scram-sha-256", "alice", "app"),
            CheckResult::NotMatched
        );

        // trust on localhost without ssl
        let ip3 = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        assert_eq!(
            hba.check_hba(&tcp(ip3, false), "md5", "alice", "app"),
            CheckResult::Trust
        );
    }

    /// `host` rule with IPv6 CIDR is supported by
    /// `IpNet::from_str` and `parse_address`, but no test exercised
    /// the v6 match path. Lock it in: an IPv6 peer inside the CIDR
    /// matches; outside the CIDR rejects; an IPv4 peer never matches
    /// a v6 rule (ipnet returns false for cross-family contains).
    #[test]
    fn host_rule_ipv6_cidr_matches_ipv6_peer_and_rejects_ipv4() {
        let hba = PgHba::from_content("host all all 2001:db8::/32 md5\n");
        let v6_inside: IpAddr = "2001:db8::1".parse().unwrap();
        let v6_outside: IpAddr = "fe80::1".parse().unwrap();
        let v4 = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
        assert_eq!(
            hba.check_hba(&tcp(v6_inside, false), "md5", "u", "d"),
            CheckResult::Allow
        );
        assert_eq!(
            hba.check_hba(&tcp(v6_outside, false), "md5", "u", "d"),
            CheckResult::NotMatched
        );
        assert_eq!(
            hba.check_hba(&tcp(v4, false), "md5", "u", "d"),
            CheckResult::NotMatched
        );
    }

    /// a malformed address token (bare
    /// IP without /N, `samenet`, hostname, IP+mask) used to parse to
    /// `address: None` and silently match every TCP peer. Now we drop
    /// the rule entirely so the next rule (or default deny) wins.
    #[test]
    fn bare_ip_and_unsupported_address_tokens_do_not_become_wildcards() {
        // None of these tokens is a valid CIDR, so all four rules
        // must be dropped at parse - nothing should match.
        let hba = PgHba::from_content(
            "\
host all all 192.168.0.1 md5\n\
host all all 10.0.0.0 255.0.0.0 md5\n\
host all all db.example.com md5\n\
host all all samenet md5\n\
",
        );
        assert_eq!(
            hba.rules.len(),
            0,
            "all rules with non-CIDR address tokens must be rejected"
        );

        // Sanity: a peer that would have matched the wildcard
        // bypass now resolves NotMatched.
        let ip = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
        assert_eq!(
            hba.check_hba(&tcp(ip, false), "md5", "alice", "app"),
            CheckResult::NotMatched
        );
    }

    // ----- Serde tests -----
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Wrapper {
        hba: PgHba,
    }

    #[test]
    fn serde_inline_string() {
        let toml_in = r#"
            hba = """
            host all all 127.0.0.1/32 trust
            host all all 10.0.0.0/8 md5
            """
        "#;
        let cfg: Wrapper = toml::from_str(toml_in).expect("toml parse inline string");
        assert_eq!(cfg.hba.rules.len(), 2);
        // First rule trust for 127.0.0.1
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        assert_eq!(
            cfg.hba.check_hba(&tcp(ip, false), "md5", "alice", "app"),
            CheckResult::Trust
        );
        // Second rule md5 for 10.1.2.3
        let ip2 = IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3));
        assert_eq!(
            cfg.hba.check_hba(&tcp(ip2, false), "md5", "alice", "app"),
            CheckResult::Allow
        );
    }

    #[test]
    fn serde_map_content() {
        let toml_in = r#"
            hba = { content = "host all all 0.0.0.0/0 md5" }
        "#;
        let cfg: Wrapper = toml::from_str(toml_in).expect("toml parse map content");
        let ip = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
        assert_eq!(
            cfg.hba.check_hba(&tcp(ip, false), "md5", "alice", "app"),
            CheckResult::Allow
        );
        assert_eq!(
            cfg.hba.check_hba(&tcp(ip, true), "md5", "alice", "app"),
            CheckResult::Allow
        );
    }

    #[test]
    fn serde_map_path() {
        // Create a temporary file with HBA content
        use std::fs;
        use std::time::{SystemTime, UNIX_EPOCH};
        let mut path = std::env::temp_dir();
        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!("pg_doorman_test_hba_{uniq}.conf"));
        let content = "host all all 192.168.0.0/16 scram-sha-256\n";
        fs::write(&path, content).expect("write temp hba");

        let toml_in = format!(r#"hba = {{ path = "{}" }}"#, path.display());
        let cfg: Wrapper = toml::from_str(&toml_in).expect("toml parse map path");
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2));
        assert_eq!(
            cfg.hba
                .check_hba(&tcp(ip, true), "scram-sha-256", "alice", "app"),
            CheckResult::Allow
        );

        // Best-effort cleanup
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn serde_map_missing_fields_error() {
        // Missing both path and content should error
        let toml_in = r#"hba = {}"#;
        let err = toml::from_str::<Wrapper>(toml_in).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("expected either 'path' or 'content' field"),
            "actual: {msg}"
        );
    }

    #[test]
    fn serde_map_unknown_field_error() {
        let toml_in = r#"hba = { foo = "bar" }"#;
        let err = toml::from_str::<Wrapper>(toml_in).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown field"), "actual: {msg}");
    }

    #[test]
    fn serde_both_path_and_content_prefers_content() {
        // When both are present, our implementation prefers `content`
        // (no error; resolved after visiting all keys)
        let toml_in = r#"
            hba = { path = "/non/existent/should/not/be/read", content = "host all all 0.0.0.0/0 md5" }
        "#;
        let cfg: Wrapper = toml::from_str(toml_in).expect("toml parse both fields");
        let ip = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
        assert_eq!(
            cfg.hba.check_hba(&tcp(ip, false), "md5", "alice", "app"),
            CheckResult::Allow
        );
    }

    #[test]
    fn display_formats_hba() {
        let hba = PgHba::from_content(SAMPLE);
        let s = hba.to_string();
        let expected = "host all all 10.0.0.0/8 md5\nhostssl all all 192.168.0.0/16 scram-sha-256\nhostnossl all all 127.0.0.1/32 trust";
        assert_eq!(s, expected);
    }

    // ---- ClientTransport semantics: local rules match Unix, host* rules match TCP ----

    #[test]
    fn local_trust_matches_unix_client() {
        let hba = PgHba::from_content("local all all trust");
        assert_eq!(
            hba.check_hba(&unix_transport(), "md5", "alice", "app"),
            CheckResult::Trust
        );
    }

    #[test]
    fn local_trust_does_not_match_tcp_client() {
        // Complement to local_trust_matches_unix_client: the same rule must
        // stay invisible to TCP clients so the existing hba_eval_tests remain
        // correct and nobody can shadow host rules by adding a local entry.
        let hba = PgHba::from_content("local all all trust");
        let ip = IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3));
        assert_eq!(
            hba.check_hba(&tcp(ip, false), "md5", "alice", "app"),
            CheckResult::NotMatched
        );
    }

    #[test]
    fn host_rule_does_not_match_unix_client() {
        // host* rules may contain a matching CIDR, but Unix connections must
        // never be authenticated by them — only by `local` entries.
        let hba = PgHba::from_content("host all all 0.0.0.0/0 md5");
        assert_eq!(
            hba.check_hba(&unix_transport(), "md5", "alice", "app"),
            CheckResult::NotMatched
        );
    }

    #[test]
    fn hostssl_rule_ignored_for_unix() {
        // The ClientTransport enum makes it impossible to build a
        // `Unix + ssl=true` state, so the matcher only needs to confirm the
        // hostssl rule itself does not leak through for Unix transport.
        let hba = PgHba::from_content("hostssl all all 0.0.0.0/0 trust");
        assert_eq!(
            hba.check_hba(&unix_transport(), "scram-sha-256", "alice", "app"),
            CheckResult::NotMatched
        );
    }

    #[test]
    fn local_reject_blocks_named_user_via_unix() {
        // Rule order matches PostgreSQL: the first rule that applies wins,
        // so bob is rejected while alice falls through to the trust rule.
        let hba = PgHba::from_content("local all bob reject\nlocal all all trust");
        assert_eq!(
            hba.check_hba(&unix_transport(), "md5", "bob", "app"),
            CheckResult::Deny
        );
        assert_eq!(
            hba.check_hba(&unix_transport(), "md5", "alice", "app"),
            CheckResult::Trust
        );
    }

    #[test]
    fn local_md5_rule_allows_unix_md5_client() {
        let hba = PgHba::from_content("local all all md5");
        assert_eq!(
            hba.check_hba(&unix_transport(), "md5", "alice", "app"),
            CheckResult::Allow
        );
    }

    #[test]
    fn local_scram_rule_denies_md5_request() {
        // A local rule with a stricter auth method must not grant access to a
        // weaker requested method or fall through to a later rule.
        let hba = PgHba::from_content("local all all scram-sha-256");
        assert_eq!(
            hba.check_hba(&unix_transport(), "md5", "alice", "app"),
            CheckResult::Deny
        );
    }

    #[test]
    fn first_matching_scram_rule_blocks_later_trust_for_md5_request() {
        let hba = PgHba::from_content(
            "host all all 127.0.0.1/32 scram-sha-256\nhost all all 127.0.0.1/32 trust",
        );
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        assert_eq!(
            hba.check_hba(&tcp(ip, false), "md5", "alice", "app"),
            CheckResult::Deny
        );
        assert_eq!(
            hba.check_hba(&tcp(ip, false), "scram-sha-256", "alice", "app"),
            CheckResult::Allow
        );
    }

    #[test]
    fn first_matching_md5_rule_does_not_fall_through_to_later_trust_for_scram_request() {
        let hba =
            PgHba::from_content("host all all 127.0.0.1/32 md5\nhost all all 127.0.0.1/32 trust");
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        assert_eq!(
            hba.check_hba(&tcp(ip, false), "scram-sha-256", "alice", "app"),
            CheckResult::Allow
        );
        assert_eq!(
            hba.check_hba(&tcp(ip, false), "md5", "alice", "app"),
            CheckResult::Allow
        );
    }

    #[test]
    fn comma_separated_user_list_is_first_match_decisive() {
        let hba = PgHba::from_content(
            "host all alice,bob 127.0.0.1/32 reject\nhost all all 127.0.0.1/32 trust",
        );
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        assert_eq!(
            hba.check_hba(&tcp(ip, false), "md5", "alice", "app"),
            CheckResult::Deny
        );
        assert_eq!(
            hba.check_hba(&tcp(ip, false), "md5", "bob", "app"),
            CheckResult::Deny
        );
        assert_eq!(
            hba.check_hba(&tcp(ip, false), "md5", "carol", "app"),
            CheckResult::Trust
        );
    }

    #[test]
    fn comma_separated_database_list_is_first_match_decisive() {
        let hba = PgHba::from_content(
            "host app,admin all 127.0.0.1/32 reject\nhost all all 127.0.0.1/32 trust",
        );
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        assert_eq!(
            hba.check_hba(&tcp(ip, false), "md5", "alice", "app"),
            CheckResult::Deny
        );
        assert_eq!(
            hba.check_hba(&tcp(ip, false), "md5", "alice", "admin"),
            CheckResult::Deny
        );
        assert_eq!(
            hba.check_hba(&tcp(ip, false), "md5", "alice", "other"),
            CheckResult::Trust
        );
    }

    #[test]
    fn serde_rejects_unsupported_database_user_tokens() {
        let toml_in = r#"
            hba = """
            host all +admins 127.0.0.1/32 reject
            """
        "#;

        let err = toml::from_str::<Wrapper>(toml_in)
            .expect_err("unsupported pg_hba role tokens must reject config");

        assert!(
            err.to_string().contains("unsupported pg_hba"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn serde_rejects_regex_database_user_tokens() {
        for toml_in in [
            r#"
            hba = """
            host /^tenant_/ all 127.0.0.1/32 reject
            host all all 127.0.0.1/32 trust
            """
            "#,
            r#"
            hba = """
            host all /^tenant_/ 127.0.0.1/32 reject
            host all all 127.0.0.1/32 trust
            """
            "#,
        ] {
            let err = toml::from_str::<Wrapper>(toml_in)
                .expect_err("unsupported pg_hba regex tokens must reject config");

            assert!(
                err.to_string().contains("unsupported pg_hba"),
                "unexpected error: {err}"
            );
        }
    }

    #[test]
    fn serde_rejects_unsupported_address_tokens() {
        let toml_in = r#"
            hba = """
            host all all samehost reject
            host all all 0.0.0.0/0 trust
            """
        "#;

        let err = toml::from_str::<Wrapper>(toml_in)
            .expect_err("unsupported pg_hba address tokens must reject config");

        assert!(
            err.to_string().contains("unsupported pg_hba address token"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn serde_rejects_unsupported_record_types() {
        let toml_in = r#"
            hba = """
            hostgssenc all all 127.0.0.1/32 reject
            host all all 127.0.0.1/32 trust
            """
        "#;

        let err = toml::from_str::<Wrapper>(toml_in)
            .expect_err("unsupported pg_hba record types must reject config");

        assert!(
            err.to_string().contains("unsupported pg_hba record type"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn serde_rejects_too_short_records() {
        let toml_in = r#"
            hba = """
            host all all
            host all all 127.0.0.1/32 trust
            """
        "#;

        let err = toml::from_str::<Wrapper>(toml_in)
            .expect_err("truncated pg_hba records must reject config");

        assert!(
            err.to_string().contains("too few fields"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn serde_rejects_unsupported_auth_options() {
        let toml_in = r#"
            hba = """
            hostssl all all 0.0.0.0/0 trust clientcert=verify-full
            """
        "#;

        let err = toml::from_str::<Wrapper>(toml_in)
            .expect_err("unsupported pg_hba auth-options must reject config");

        assert!(
            err.to_string().contains("unsupported pg_hba auth-options"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn serde_rejects_unsupported_auth_methods() {
        let toml_in = r#"
            hba = """
            host all all 127.0.0.1/32 password
            """
        "#;

        let err = toml::from_str::<Wrapper>(toml_in)
            .expect_err("unsupported pg_hba auth methods must reject config");

        assert!(
            err.to_string().contains("unsupported pg_hba auth method"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn serde_rejects_unquoted_replication_database_token() {
        let toml_in = r#"
            hba = """
            host replication repl 127.0.0.1/32 trust
            """
        "#;

        let err = toml::from_str::<Wrapper>(toml_in)
            .expect_err("unquoted pg_hba replication database token must reject config");

        assert!(
            err.to_string().contains("replication"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn serde_rejects_unterminated_quoted_tokens() {
        for (field, line) in [
            ("method", r#"host app alice 127.0.0.1/32 "trust"#),
            ("database", r#"host "app all 127.0.0.1/32 trust"#),
            ("user", r#"host app "alice 127.0.0.1/32 trust"#),
            ("address", r#"host app alice "127.0.0.1/32 trust"#),
        ] {
            let toml_in = format!(
                r#"
                hba = """
                {line}
                """
                "#
            );

            let err = match toml::from_str::<Wrapper>(&toml_in) {
                Ok(_) => panic!("unterminated quoted {field} token must reject config"),
                Err(err) => err,
            };

            assert!(
                err.to_string().contains("unterminated quoted token"),
                "unexpected error for {field}: {err}"
            );
        }
    }

    #[test]
    fn quoted_hash_in_name_is_not_treated_as_comment() {
        let toml_in = r##"
            hba = """
            host app "bob#prod" 127.0.0.1/32 reject
            host all all 127.0.0.1/32 trust
            """
        "##;

        let cfg: Wrapper =
            toml::from_str(toml_in).expect("quoted # inside pg_hba name must parse as token data");
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        assert_eq!(
            cfg.hba.check_hba(&tcp(ip, false), "md5", "bob#prod", "app"),
            CheckResult::Deny,
            "quoted # user rule must remain decisive before later trust"
        );
    }

    #[test]
    fn unquoted_replication_database_rule_does_not_match_normal_database() {
        let hba = PgHba::from_content(
            "host replication repl 127.0.0.1/32 trust\n\
             host all all 127.0.0.1/32 reject",
        );
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        assert_eq!(
            hba.check_hba(&tcp(ip, false), "md5", "repl", "replication"),
            CheckResult::Deny,
            "unquoted replication is a PostgreSQL replication pseudo-database, \
             not a normal database named replication"
        );
    }

    #[test]
    fn quoted_replication_database_is_literal_name() {
        let toml_in = r#"
            hba = """
            host "replication" repl 127.0.0.1/32 trust
            host all all 127.0.0.1/32 reject
            """
        "#;

        let cfg: Wrapper = toml::from_str(toml_in)
            .expect("quoted replication must parse as a literal database name");
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        assert_eq!(
            cfg.hba
                .check_hba(&tcp(ip, false), "md5", "repl", "replication"),
            CheckResult::Trust
        );
        assert_eq!(
            cfg.hba.check_hba(&tcp(ip, false), "md5", "repl", "app"),
            CheckResult::Deny
        );
    }

    #[test]
    fn quoted_all_database_and_user_are_literals_not_wildcards() {
        let toml_in = r#"
            hba = """
            host "all" "all" 127.0.0.1/32 trust
            host all all 127.0.0.1/32 reject
            """
        "#;

        let cfg: Wrapper = toml::from_str(toml_in)
            .expect("quoted all in pg_hba names must parse as literal token data");
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        assert_eq!(
            cfg.hba.check_hba(&tcp(ip, false), "md5", "alice", "app"),
            CheckResult::Deny,
            "quoted all must not wildcard arbitrary users/databases before a later reject"
        );
        assert_eq!(
            cfg.hba.check_hba(&tcp(ip, false), "md5", "all", "all"),
            CheckResult::Trust,
            "quoted all still matches the literal database and user names"
        );
    }

    #[test]
    fn quoted_comma_user_is_literal_not_a_name_list() {
        let toml_in = r#"
            hba = """
            host all "alice,bob" 127.0.0.1/32 trust
            host all all 127.0.0.1/32 reject
            """
        "#;

        let cfg: Wrapper = toml::from_str(toml_in)
            .expect("quoted comma in pg_hba name must parse as literal token data");
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        assert_eq!(
            cfg.hba.check_hba(&tcp(ip, false), "md5", "alice", "app"),
            CheckResult::Deny,
            "quoted comma names must not expand into a list before a later reject"
        );
        assert_eq!(
            cfg.hba
                .check_hba(&tcp(ip, false), "md5", "alice,bob", "app"),
            CheckResult::Trust,
            "quoted comma names still match the literal user name"
        );
    }

    #[test]
    fn serde_rejects_mixed_quoted_comma_list_name_tokens() {
        for (field, line) in [
            (
                "database",
                r#"host "app,admin",other all 127.0.0.1/32 trust"#,
            ),
            ("user", r#"host all "alice,bob",carol 127.0.0.1/32 trust"#),
        ] {
            let toml_in = format!(
                r#"
                hba = """
                {line}
                """
                "#
            );

            let err = match toml::from_str::<Wrapper>(&toml_in) {
                Ok(_) => panic!("mixed quoted comma-list {field} token must reject config"),
                Err(err) => err,
            };

            assert!(
                err.to_string().contains("mixed quoted comma-list"),
                "unexpected error for {field}: {err}"
            );
        }
    }

    #[test]
    fn local_and_host_rules_are_independent_decisions() {
        // Single file, both transports: unix follows `local`, TCP follows `host`.
        let hba = PgHba::from_content("local all all trust\nhost all all 0.0.0.0/0 md5");
        let ip = IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3));
        assert_eq!(
            hba.check_hba(&unix_transport(), "md5", "alice", "app"),
            CheckResult::Trust
        );
        assert_eq!(
            hba.check_hba(&tcp(ip, false), "md5", "alice", "app"),
            CheckResult::Allow
        );
    }

    #[test]
    fn local_database_filter_narrows_match() {
        // Database scoping is orthogonal to the transport check: a local rule
        // for "admin" must not authenticate a client connecting to "app".
        let hba = PgHba::from_content("local admin all trust\nlocal all all reject");
        assert_eq!(
            hba.check_hba(&unix_transport(), "md5", "alice", "admin"),
            CheckResult::Trust
        );
        assert_eq!(
            hba.check_hba(&unix_transport(), "md5", "alice", "app"),
            CheckResult::Deny
        );
    }

    #[test]
    fn empty_hba_returns_not_matched_for_unix() {
        // A configured-but-empty PgHba must fall through — the upstream
        // caller decides what to do, it should not be silently promoted.
        let hba = PgHba::from_content("");
        assert_eq!(
            hba.check_hba(&unix_transport(), "md5", "alice", "app"),
            CheckResult::NotMatched
        );
    }
}
