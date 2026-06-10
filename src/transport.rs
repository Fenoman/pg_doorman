//! Transport descriptor used by the client authentication pipeline.
//!
//! A single enum carries transport-specific data through HBA matching,
//! client startup, and log formatting.

use std::net::SocketAddr;

/// How a client reached the pooler.
#[derive(Debug, Clone)]
pub enum ClientTransport {
    /// Classic TCP (optionally over TLS).
    Tcp {
        peer: SocketAddr,
        /// True when the client completed the TLS handshake before sending
        /// its startup packet. Drives hostssl rule matching and the
        /// `ClientStats::is_tls` counter.
        ssl: bool,
    },
    /// Unix domain socket. Peer address is not meaningful for these
    /// connections — the kernel does not expose a remote endpoint and
    /// `SO_PEERCRED` is not currently threaded through.
    Unix,
}

impl ClientTransport {
    /// True when the client is connected over a TLS-upgraded TCP socket.
    pub fn is_tls(&self) -> bool {
        matches!(self, ClientTransport::Tcp { ssl: true, .. })
    }

    /// True when the client is connected over a Unix domain socket.
    pub fn is_unix(&self) -> bool {
        matches!(self, ClientTransport::Unix)
    }

    /// Short display string used in logs and in `ClientStats` / `SHOW
    /// CLIENTS` rows. TCP clients carry their `peer.to_string()`; Unix
    /// clients render as `unix:` so operators can tell them apart from
    /// localhost TCP at a glance.
    pub fn peer_display(&self) -> String {
        match self {
            ClientTransport::Tcp { peer, .. } => peer.to_string(),
            ClientTransport::Unix => "unix:".to_string(),
        }
    }

    /// IP that the HBA matcher should use when checking `host`/`hostssl`
    /// rules. Unix transport has no meaningful IP, so we return a sentinel
    /// loopback value — the matcher ignores the IP for Unix clients
    /// anyway (see `src/auth/hba.rs`).
    ///
    /// On a dual-stack (`host = "::"`) listener an IPv4 peer arrives as a
    /// V4-mapped IPv6 address (`::ffff:a.b.c.d`). Such an address does NOT match
    /// IPv4 CIDR rules, which would silently skip IPv4 `reject`/`allow` HBA
    /// rules. We canonicalize mapped addresses to their IPv4 form (mirroring how
    /// PostgreSQL canonicalizes addresses before HBA matching).
    pub fn hba_ip(&self) -> std::net::IpAddr {
        match self {
            ClientTransport::Tcp { peer, .. } => canonicalize_hba_ip(peer.ip()),
            ClientTransport::Unix => std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        }
    }
}

/// Canonicalizes an IP address for HBA matching.
///
/// V4-mapped IPv6 addresses (`::ffff:a.b.c.d`) are converted to the
/// corresponding IPv4 address (`a.b.c.d`); all other addresses (genuine IPv6
/// and plain IPv4) are returned unchanged. This ensures IPv4 CIDR HBA rules
/// apply to IPv4 peers connecting over a dual-stack listener.
fn canonicalize_hba_ip(ip: std::net::IpAddr) -> std::net::IpAddr {
    match ip {
        std::net::IpAddr::V6(v6) => v6.to_canonical(),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn tcp_is_tls_reflects_ssl_flag() {
        let peer = SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), 5432));
        assert!(!ClientTransport::Tcp { peer, ssl: false }.is_tls());
        assert!(ClientTransport::Tcp { peer, ssl: true }.is_tls());
        assert!(!ClientTransport::Tcp { peer, ssl: true }.is_unix());
    }

    #[test]
    fn unix_is_unix_and_never_tls() {
        assert!(ClientTransport::Unix.is_unix());
        assert!(!ClientTransport::Unix.is_tls());
    }

    #[test]
    fn peer_display_distinguishes_transports() {
        let peer = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 54321));
        assert_eq!(
            ClientTransport::Tcp { peer, ssl: false }.peer_display(),
            "127.0.0.1:54321"
        );
        assert_eq!(ClientTransport::Unix.peer_display(), "unix:");
    }

    #[test]
    fn hba_ip_for_unix_is_loopback_sentinel() {
        // The HBA matcher drops the IP entirely for Unix clients, so the
        // exact value does not matter — but we pin loopback here so a
        // regression is easy to spot.
        assert_eq!(
            ClientTransport::Unix.hba_ip(),
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
    }

    // on a dual-stack (`host = "::"`) listener an IPv4 peer arrives as
    // a V4-mapped IPv6 address (::ffff:a.b.c.d). hba_ip must canonicalize it to
    // the plain IPv4 form so IPv4 CIDR HBA rules (reject/allow) still apply;
    // otherwise an IPv4 reject rule is silently skipped.
    #[test]
    fn hba_ip_canonicalizes_v4_mapped_ipv6_to_ipv4() {
        use ipnet::Ipv4Net;
        use std::net::{IpAddr, Ipv6Addr};
        use std::str::FromStr;

        // ::ffff:10.0.0.5 - an IPv4 peer as seen on a dual-stack listener.
        let mapped = IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x0a00, 0x0005));
        let transport = ClientTransport::Tcp {
            peer: SocketAddr::new(mapped, 54321),
            ssl: false,
        };

        let net = Ipv4Net::from_str("10.0.0.0/8").unwrap();
        match transport.hba_ip() {
            IpAddr::V4(v4) => assert!(
                net.contains(&v4),
                "canonicalized ::ffff:10.0.0.5 must match the IPv4 CIDR 10.0.0.0/8"
            ),
            IpAddr::V6(_) => panic!("hba_ip must canonicalize V4-mapped IPv6 to IPv4"),
        }
    }

    #[test]
    fn hba_ip_leaves_genuine_ipv6_unchanged() {
        use std::net::{IpAddr, Ipv6Addr};

        let real_v6 = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let transport = ClientTransport::Tcp {
            peer: SocketAddr::new(IpAddr::V6(real_v6), 54321),
            ssl: false,
        };
        assert_eq!(transport.hba_ip(), IpAddr::V6(real_v6));
    }

    #[test]
    fn hba_ip_leaves_plain_ipv4_unchanged() {
        let peer = SocketAddr::from((Ipv4Addr::new(10, 0, 0, 5), 54321));
        assert_eq!(
            ClientTransport::Tcp { peer, ssl: false }.hba_ip(),
            std::net::IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))
        );
    }
}
