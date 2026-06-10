//! Cross-site request forgery (CSRF) protection for admin POST endpoints.
//!
//! Threat model: an authenticated operator (Basic credentials or an SSO
//! cookie that grants Admin role) is browsing a malicious page. That page
//! issues a cross-origin POST to `pg_doorman:7777/api/admin/shutdown`.
//! For Basic credentials this is safe (the browser does not auto-attach
//! a Basic header), but **SSO cookies are auto-attached by the browser to
//! cross-origin POSTs**. Without a defence the attacker fires admin
//! mutations from any tab the operator opens.
//!
//! Defence: compare the `Origin` (or `Referer` when Origin is absent) host
//! against the request's `Host` header. Same-origin → allow. Different
//! origin → 403. Missing both headers → reject (browsers always send one
//! on cross-origin POST; missing = scripted request from a non-browser
//! context, which should authenticate via Bearer, not cookies - but the
//! outer dispatch already routes Bearer/Basic admin without going through
//! cookie SSO, so this is fail-closed).

/// Returns `true` when the request's effective origin (Origin / Referer)
/// matches an operator-configured allowlist OR (fallback when allowlist
/// is empty) matches the listener Host header.
///
/// the legacy `Host`-matching path is vulnerable to
/// Host-header injection from non-browser HTTP clients (SSRF reflectors,
/// scripted curl, mis-configured reverse proxies) - attacker controls
/// both `Host` and `Origin` and matches them. The allowlist mode pins
/// the trust anchor to operator configuration, removing the forgery
/// surface entirely. Operators should populate `[web].allowed_admin_origins`
/// on multi-tenant networks.
pub(crate) fn is_same_origin(
    origin: Option<&str>,
    referer: Option<&str>,
    host: Option<&str>,
    allowed_admin_origins: &[String],
) -> bool {
    // allowlist mode (preferred).
    if !allowed_admin_origins.is_empty() {
        let presented = origin.or(referer);
        let Some(presented) = presented else {
            // No Origin/Referer in allowlist mode - reject. Browsers
            // always send one on cross-origin; absence = scripted client.
            return false;
        };
        let candidate_origin = match extract_origin_key(presented) {
            Some(s) => s.to_ascii_lowercase(),
            None => return false,
        };
        return allowed_admin_origins.iter().any(|allowed| {
            extract_origin_key(allowed)
                .map(|allowed_origin| allowed_origin.to_ascii_lowercase() == candidate_origin)
                .unwrap_or(false)
        });
    }

    // Legacy Host-matching fallback (no allowlist configured). Best-effort
    // protection vs accidental cross-origin; documented as bypassable by
    // non-browser clients with forged Host headers.
    let Some(host) = host else {
        return false;
    };
    let normalised_host = host.trim().to_ascii_lowercase();
    if let Some(origin) = origin {
        return origin_host_matches(origin, &normalised_host);
    }
    if let Some(referer) = referer {
        return origin_host_matches(referer, &normalised_host);
    }
    false
}

/// Extract the host:port authority component from a URL-shaped string
/// (Origin, Referer, or an operator-configured allowlist entry like
/// `"https://pgd.example:7777"`). Returns `None` for malformed input
/// or for `null`-origin (sandboxed iframes).
///
/// /// - Runtime Origin/Referer parsing strips userinfo
///   (`user:pass@host` → `host`). Browsers don't send userinfo in
///   Origin/Referer, but a malicious non-browser client could embed it
///   to bypass an allowlist that contained only the bare host.
/// - Operator-configured allowlist entries reject userinfo during config
///   validation, so credentials are not accepted or echoed by config
///   surfaces.
/// - Reject empty authority (`http://`).
/// - Reject embedded whitespace (`pgd.example\n`) - `trim()` only
///   strips edges.
///
/// Public wrapper for config validation. Returns the normalised
/// `scheme://host[:port]` origin key of `url`, or `None` for malformed
/// input (scheme-less, userinfo, empty, whitespace, etc.).
pub fn extract_authority_for_config(url: &str) -> Option<String> {
    if configured_origin_has_userinfo(url) {
        return None;
    }
    if configured_origin_has_path_query_fragment(url) {
        return None;
    }
    extract_origin_key(url)
}

pub fn configured_origin_has_userinfo(url: &str) -> bool {
    let trimmed = url.trim();
    let Some((_, after_scheme)) = trimmed.split_once("://") else {
        return false;
    };
    let end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    after_scheme[..end].contains('@')
}

pub fn configured_origin_has_path_query_fragment(url: &str) -> bool {
    let trimmed = url.trim();
    let Some((_, after_scheme)) = trimmed.split_once("://") else {
        return false;
    };
    after_scheme.find(['/', '?', '#']).is_some()
}

fn extract_origin_key(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.eq_ignore_ascii_case("null") {
        return None;
    }
    let (scheme, after_scheme) = trimmed.split_once("://")?;
    if scheme.is_empty() || scheme.chars().any(char::is_whitespace) {
        return None;
    }
    let authority = extract_authority_from_after_scheme(after_scheme)?;
    Some(format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    ))
}

fn extract_authority(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.eq_ignore_ascii_case("null") {
        return None;
    }
    let after_scheme = trimmed.split_once("://")?.1;
    extract_authority_from_after_scheme(after_scheme)
}

fn extract_authority_from_after_scheme(after_scheme: &str) -> Option<String> {
    let end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let raw = &after_scheme[..end];
    // Strip userinfo: split on '@' and take the right side.
    let host_port = match raw.split_once('@') {
        Some((_, rest)) => rest,
        None => raw,
    };
    if host_port.is_empty() {
        return None;
    }
    if host_port.chars().any(char::is_whitespace) {
        return None;
    }
    Some(host_port.to_string())
}

/// True when `candidate` (an Origin- or Referer-style URL) shares the
/// host:port authority of `host_value`. `host_value` is already
/// lowercase-trimmed.
fn origin_host_matches(candidate: &str, host_value: &str) -> bool {
    // Origin format: `scheme://host[:port]` (no path).
    // Referer format: `scheme://host[:port]/path?query`.
    extract_authority(candidate)
        .map(|candidate_authority| candidate_authority.to_ascii_lowercase() == host_value)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_origin_matches_origin_header() {
        assert!(is_same_origin(
            Some("http://pgd.example:7777"),
            None,
            Some("pgd.example:7777"),
            &[]
        ));
    }

    #[test]
    fn cross_origin_rejected_by_origin_header() {
        assert!(!is_same_origin(
            Some("https://attacker.example"),
            None,
            Some("pgd.example:7777"),
            &[]
        ));
    }

    #[test]
    fn referer_fallback_matches_authority() {
        assert!(is_same_origin(
            None,
            Some("http://pgd.example:7777/admin/dashboard"),
            Some("pgd.example:7777"),
            &[]
        ));
    }

    #[test]
    fn null_origin_rejected() {
        assert!(!is_same_origin(
            Some("null"),
            None,
            Some("pgd.example:7777"),
            &[]
        ));
    }

    #[test]
    fn missing_origin_and_referer_rejected() {
        assert!(!is_same_origin(None, None, Some("pgd.example:7777"), &[]));
    }

    #[test]
    fn missing_host_rejected() {
        assert!(!is_same_origin(
            Some("http://pgd.example:7777"),
            None,
            None,
            &[]
        ));
    }

    #[test]
    fn case_insensitive_host_match() {
        assert!(is_same_origin(
            Some("http://PGD.Example:7777"),
            None,
            Some("pgd.example:7777"),
            &[]
        ));
    }

    #[test]
    fn port_mismatch_rejected() {
        assert!(!is_same_origin(
            Some("http://pgd.example:8888"),
            None,
            Some("pgd.example:7777"),
            &[]
        ));
    }

    #[test]
    fn origin_with_path_rejected_as_malformed_browser_input() {
        // Origin headers don't include paths per RFC 6454; we accept up to
        // first slash anyway for defensive parsing.
        assert!(is_same_origin(
            Some("http://pgd.example:7777/extra"),
            None,
            Some("pgd.example:7777"),
            &[]
        ));
    }

    // allowlist mode coverage. Previously the entire allowlist
    // branch was untested.
    fn allowlist() -> Vec<String> {
        vec![
            "https://pgd.example:7777".into(),
            "https://admin.local".into(),
        ]
    }

    #[test]
    fn allowlist_match_origin() {
        assert!(is_same_origin(
            Some("https://pgd.example:7777"),
            None,
            Some("anything-untrusted"),
            &allowlist()
        ));
    }

    #[test]
    fn allowlist_mismatch_rejected() {
        assert!(!is_same_origin(
            Some("https://attacker.example"),
            None,
            Some("pgd.example:7777"),
            &allowlist()
        ));
    }

    #[test]
    fn allowlist_case_insensitive_host() {
        assert!(is_same_origin(
            Some("https://PGD.Example:7777"),
            None,
            None,
            &allowlist()
        ));
    }

    #[test]
    fn allowlist_referer_fallback() {
        assert!(is_same_origin(
            None,
            Some("https://admin.local/dashboard?x=1"),
            None,
            &allowlist()
        ));
    }

    #[test]
    fn allowlist_missing_origin_and_referer_rejected() {
        assert!(!is_same_origin(
            None,
            None,
            Some("pgd.example:7777"),
            &allowlist()
        ));
    }

    #[test]
    fn allowlist_port_mismatch_rejected() {
        assert!(!is_same_origin(
            Some("https://pgd.example:8888"),
            None,
            None,
            &allowlist()
        ));
    }

    #[test]
    fn allowlist_scheme_mismatch_rejected() {
        assert!(!is_same_origin(
            Some("http://admin.local"),
            None,
            None,
            &allowlist()
        ));
    }

    #[test]
    fn allowlist_userinfo_stripped() {
        // Attacker embeds matching authority via userinfo:
        // `user@allowed.example:7777` would NOT match `allowed.example:7777`
        // if we naively kept the userinfo prefix. Origin normalization strips it.
        // The userinfo part still has to be after '@', and host:port after
        // matches the allowlist entry - we want THIS to MATCH because
        // the actual authority IS the allowlist target.
        assert!(is_same_origin(
            Some("https://user:pass@pgd.example:7777"),
            None,
            None,
            &allowlist()
        ));
    }

    #[test]
    fn allowlist_scheme_less_entry_rejects_all() {
        // Operator wrote `pgd.example:7777` without scheme - extract_authority
        // returns None, so NO Origin can match this entry. Better to fail
        // closed than silently match anything.
        let bad = vec!["pgd.example:7777".to_string()];
        assert!(!is_same_origin(
            Some("https://pgd.example:7777"),
            None,
            None,
            &bad
        ));
    }

    #[test]
    fn allowlist_embedded_whitespace_rejected() {
        // Origin with embedded whitespace should be rejected regardless of
        // allowlist content.
        let bad = vec!["https://pgd.example".to_string()];
        assert!(!is_same_origin(
            Some("https://pgd.example\n.attacker"),
            None,
            None,
            &bad
        ));
    }

    #[test]
    fn configured_allowlist_origin_rejects_path_query_fragment() {
        assert!(configured_origin_has_path_query_fragment(
            "https://pgd.example:7777/path"
        ));
        assert!(configured_origin_has_path_query_fragment(
            "https://pgd.example:7777?token=secret"
        ));
        assert!(configured_origin_has_path_query_fragment(
            "https://pgd.example:7777#secret"
        ));
        assert!(!configured_origin_has_path_query_fragment(
            "https://pgd.example:7777"
        ));
        assert!(extract_authority_for_config("https://pgd.example:7777/path").is_none());
    }
}
