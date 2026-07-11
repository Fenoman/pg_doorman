use ahash::{AHashMap, AHashSet};
use bytes::{BufMut, BytesMut};
use once_cell::sync::Lazy;
use std::collections::HashMap;

use crate::config::VERSION;

// every `set_param` (called on every backend
// ParameterStatus frame plus every checkout-sync SET) hashed the GUC
// name with stdlib SipHash-1-3. Profile attributed
// `core::hash::sip::Hasher::write` 0.30 % S2 + `prepared_statements::has`
// 0.30 % to this lookup family. SipHash is the std `RandomState` default
// because it's a security-grade HashDoS mitigation - overkill for
// in-process maps whose keys are GUC identifiers, not adversary-
// controlled hash inputs. AHash is ~2.5-8× faster on short keys and is
// already a project dependency.
static TRACKED_PARAMETERS: Lazy<AHashSet<String>> = Lazy::new(|| {
    let mut set = AHashSet::new();
    set.insert("client_encoding".to_string());
    set.insert("DateStyle".to_string());
    set.insert("IntervalStyle".to_string());
    set.insert("TimeZone".to_string());
    set.insert("standard_conforming_strings".to_string());
    set.insert("application_name".to_string());
    set
});

/// Names that must never be emitted as `SET` or `RESET` during checkout
/// sync. PostgreSQL either owns them or rejects changes to them, and a
/// rejected sync query makes the pooled backend unusable for that checkout.
static SET_FORBIDDEN_PARAMETERS: Lazy<AHashSet<&'static str>> = Lazy::new(|| {
    let mut s = AHashSet::new();
    // Read-only or server-supplied GUCs.
    s.insert("is_superuser");
    s.insert("session_authorization");
    s.insert("server_version");
    s.insert("server_version_num");
    s.insert("server_encoding");
    s.insert("integer_datetimes");
    s.insert("in_hot_standby");
    s.insert("session_user");
    s.insert("current_user");
    s.insert("block_size");
    s.insert("wal_block_size");
    s.insert("wal_segment_size");
    s.insert("max_index_keys");
    s.insert("max_identifier_length");
    s.insert("max_function_args");
    s.insert("data_checksums");
    s.insert("data_directory_mode");
    // Database-level GUCs cannot vary per session.
    s.insert("lc_collate");
    s.insert("lc_ctype");
    // Transaction state cannot be set by checkout sync, which runs
    // before the client transaction starts.
    s.insert("transaction_isolation");
    s.insert("transaction_read_only");
    s.insert("transaction_deferrable");
    // StartupMessage protocol fields, not GUC names.
    s.insert("user");
    s.insert("database");
    s.insert("replication");
    s.insert("options");
    s
});

/// Startup-time GUCs that affect prepared-statement planning and are safe
/// to replay at session level. They become part of the prepared-cache key.
/// Names are stored in canonical form.
static PLANNER_KEYS: Lazy<AHashSet<&'static str>> = Lazy::new(|| {
    let mut s = AHashSet::new();
    // lc_collate and lc_ctype affect planning too, but PostgreSQL will not
    // let a session change them, so they stay in SET_FORBIDDEN_PARAMETERS.
    s.insert("search_path");
    s.insert("default_transaction_isolation");
    s.insert("default_transaction_read_only");
    s.insert("default_text_search_config");
    s.insert("role");
    s
});

/// True when `name` participates in the prepared-statement planner hash.
pub fn is_planner_key(name: &str) -> bool {
    PLANNER_KEYS.contains(name)
}

/// True when checkout sync must not emit SET/RESET for this name.
pub fn is_set_forbidden(name: &str) -> bool {
    SET_FORBIDDEN_PARAMETERS.contains(name)
}

/// True when checkout sync MAY emit SET/RESET for this name. A curated,
/// fork-agnostic allowlist: the tracked session GUCs plus planner keys.
/// Backend `ParameterStatus` also stores read-only reported GUCs
/// (`server_version`, and fork-specific ones such as PostgresPro/Tantor
/// `pgpro_build` / `pgpro_edition` / `pgpro_version`); those are PGC_INTERNAL,
/// so SETting them fails with SQLSTATE 55P02 and poisons the backend. An
/// allowlist stays correct for every fork, where a read-only denylist cannot.
pub fn is_syncable_param(name: &str) -> bool {
    (TRACKED_PARAMETERS.contains(name) || is_planner_key(name)) && !is_set_forbidden(name)
}

/// Validate a client StartupMessage key before it can become
/// `SET <key> TO ...` in checkout sync. The key must look like a GUC name,
/// must be settable, and must not use PostgreSQL's `_pq_.` protocol prefix.
pub fn is_safe_client_startup_key(key: &str) -> bool {
    if !crate::config::startup_parameters::is_valid_guc_name(key) {
        return false;
    }
    // Canonicalisation lowercases untracked names before SET, so check
    // the reserved prefix case-insensitively.
    const PQ_PREFIX: &[u8] = b"_pq_.";
    if key.len() >= PQ_PREFIX.len()
        && key.as_bytes()[..PQ_PREFIX.len()].eq_ignore_ascii_case(PQ_PREFIX)
    {
        return false;
    }
    let canonical = canonicalize_param_name(key.to_string());
    !is_set_forbidden(&canonical)
}

/// Canonicalise a PostgreSQL session parameter name. Tracked
/// ParameterStatus names keep PostgreSQL's spelling; other names are
/// folded to ASCII lower case for stable comparisons and config merges.
pub fn canonicalize_param_name(key: String) -> String {
    // cheap length prefilter before the case-insensitive
    // compare. `eq_ignore_ascii_case` walks both strings; comparing
    // `len()` first (a single field read) skips that walk for every
    // tracked name whose length differs from `key`, which is the common
    // case for untracked GUCs. Behavior-identical: ASCII case folding
    // never changes byte length, so a length mismatch cannot be a
    // case-insensitive match.
    let key_len = key.len();
    for tracked in TRACKED_PARAMETERS.iter() {
        if tracked.len() == key_len && key.eq_ignore_ascii_case(tracked) {
            return tracked.clone();
        }
    }
    if key.bytes().any(|b| b.is_ascii_uppercase()) {
        key.to_ascii_lowercase()
    } else {
        key
    }
}

/// One action produced by `compare_params` for checkout sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamAction {
    /// `SET key TO 'value'`, value differs or backend has no record.
    SetTo(String),
    /// `RESET key`, backend has a value but the new client does not.
    Reset,
}

#[derive(Debug)]
pub struct ServerParameters {
    // Kept `pub(crate)` to preserve current internal usage patterns during refactor.
    //
    // `AHashMap` (ahash::RandomState) replaces stdlib
    // `HashMap` so the per-`set_param` lookup hot path no longer pays
    // SipHash-1-3. External callers that need owned `HashMap<String,
    // String>` (e.g. `server_parameters_as_hashmap`) materialise it on
    // demand from the iterator.
    pub(crate) parameters: AHashMap<String, String>,

    /// Lazy hash of planner-relevant parameters. Atomic keeps the
    /// containing async types `Send + Sync` while allowing cache updates.
    planner_hash_cache: std::sync::atomic::AtomicU64,
}

impl Clone for ServerParameters {
    fn clone(&self) -> Self {
        // Clones recompute the cache lazily instead of copying a stale
        // atomic value.
        ServerParameters {
            parameters: self.parameters.clone(),
            planner_hash_cache: std::sync::atomic::AtomicU64::new(PLANNER_HASH_UNSET),
        }
    }
}

/// Sentinel for "planner hash not computed yet".
const PLANNER_HASH_UNSET: u64 = u64::MAX;

impl Default for ServerParameters {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerParameters {
    pub fn new() -> Self {
        ServerParameters {
            parameters: AHashMap::new(),
            planner_hash_cache: std::sync::atomic::AtomicU64::new(PLANNER_HASH_UNSET),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.parameters.is_empty()
    }

    pub fn admin() -> Self {
        let mut server_parameters = ServerParameters {
            parameters: AHashMap::new(),
            planner_hash_cache: std::sync::atomic::AtomicU64::new(PLANNER_HASH_UNSET),
        };

        server_parameters.set_param("client_encoding", "UTF8", false);
        server_parameters.set_param("DateStyle", "ISO, MDY", false);
        server_parameters.set_param("TimeZone", "Etc/UTC", false);
        server_parameters.set_param("server_version", VERSION, true);
        server_parameters.set_param("server_encoding", "UTF-8", true);
        server_parameters.set_param("standard_conforming_strings", "on", false);
        // (64 bit = on) as of PostgreSQL 10, this is always on.
        server_parameters.set_param("integer_datetimes", "on", false);
        server_parameters.set_param("application_name", "pg_doorman", false);

        server_parameters
    }

    /// If `startup` is false, only ParameterStatus-tracked names are kept.
    /// Returns true when a planner key changed and the cached planner hash
    /// was invalidated.
    pub fn set_param(
        &mut self,
        key: impl Into<String>,
        value: impl Into<String>,
        startup: bool,
    ) -> bool {
        let key = canonicalize_param_name(key.into());

        // backend-supplied `ParameterStatus` frames
        // and per-statement `SET` echoes flow into this same map via
        // `startup=true` / `startup=false` calls. Those bytes are
        // attacker-controlled when `server_tls_mode != "verify-full"`
        // (MITM on the pg_doorman->backend link). The downstream
        // `sync_parameters` path interpolates `{key}` raw into
        // `SET {key} TO …;` / `RESET {key};` SQL; a tampered key
        // shaped like `"application_name;DROP DATABASE x--"` becomes
        // an injection executed with the backend's privileges on the
        // next client checkout. `canonicalize_param_name` only lowers
        // case - it does NOT enforce identifier shape. Reject anything
        // that is not a valid GUC name BEFORE it enters the map.
        if !crate::config::startup_parameters::is_valid_guc_name(&key) {
            log::warn!(
                "[parameters] refused to set unsafe GUC key (shape violates \
                 [A-Za-z_][A-Za-z0-9_.]*): {key:?} - possible MITM-injected \
                 ParameterStatus frame; tighten server_tls_mode"
            );
            return false;
        }

        let value = value.into();

        if TRACKED_PARAMETERS.contains(&key) || startup {
            let planner_relevant = is_planner_key(&key);
            let changed = match self.parameters.get(&key) {
                Some(existing) => existing != &value,
                None => true,
            };
            self.parameters.insert(key, value);
            if planner_relevant && changed {
                self.planner_hash_cache
                    .store(PLANNER_HASH_UNSET, std::sync::atomic::Ordering::Relaxed);
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Bulk variant of `set_param`.
    pub fn set_from_hashmap(
        &mut self,
        parameters: &HashMap<String, String>,
        startup: bool,
    ) -> bool {
        let mut planner_changed = false;
        for (key, value) in parameters {
            if self.set_param(key, value, startup) {
                planner_changed = true;
            }
        }
        planner_changed
    }

    /// Drop one entry from the snapshot after a successful `RESET key`.
    /// Planner keys invalidate the cached planner hash.
    pub fn remove_param(&mut self, key: &str) {
        let canonical = canonicalize_param_name(key.to_string());
        // Defense in depth: refuse to remove keys that should never have
        // entered the map per set_param's shape check.
        if !crate::config::startup_parameters::is_valid_guc_name(&canonical) {
            return;
        }
        if self.parameters.remove(&canonical).is_some() && is_planner_key(&canonical) {
            self.planner_hash_cache
                .store(PLANNER_HASH_UNSET, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Drop session GUCs that PostgreSQL resets without sending
    /// ParameterStatus, after confirmed `RESET ALL` / `DISCARD ALL`.
    pub(crate) fn remove_startup_only_params_after_session_reset(&mut self) {
        let mut planner_changed = false;
        self.parameters.retain(|key, _| {
            let keep = TRACKED_PARAMETERS.contains(key) || is_set_forbidden(key);
            if !keep && is_planner_key(key) {
                planner_changed = true;
            }
            keep
        });
        if planner_changed {
            self.planner_hash_cache
                .store(PLANNER_HASH_UNSET, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Diff the backend snapshot (`self`) against the client's desired
    /// state and return the SET/RESET actions needed for checkout sync.
    /// Only syncable params (tracked session GUCs + planner keys) are
    /// considered; read-only reported GUCs (including fork-specific ones like
    /// PostgresPro `pgpro_build`) are never SET/RESET — see `is_syncable_param`.
    #[inline(always)]
    pub(crate) fn compare_params(
        &self,
        incoming_parameters: &ServerParameters,
    ) -> HashMap<String, ParamAction> {
        let mut diff = HashMap::new();

        for (key, client_value) in &incoming_parameters.parameters {
            if !is_syncable_param(key) {
                continue;
            }
            match self.parameters.get(key) {
                Some(backend_value) if backend_value == client_value => {}
                _ => {
                    diff.insert(key.clone(), ParamAction::SetTo(client_value.clone()));
                }
            }
        }

        for key in self.parameters.keys() {
            if !is_syncable_param(key) {
                continue;
            }
            if !incoming_parameters.parameters.contains_key(key) {
                diff.insert(key.clone(), ParamAction::Reset);
            }
        }

        diff
    }

    pub fn get_application_name(&self) -> &str {
        // `ServerParameters::new()` does not set application_name
        // (only `::admin()` does) and a client RESET can remove it, so a
        // bare `.unwrap()` here was reachable unsoundness. Fall back to an
        // empty name instead of panicking; the contract stays `&str` so
        // the (stats-recording) callers are unchanged.
        self.parameters
            .get("application_name")
            .map(String::as_str)
            .unwrap_or("")
    }

    pub fn as_hashmap(&self) -> HashMap<String, String> {
        // external API stays std `HashMap` so callers
        // (e.g. `client/migration.rs` snapshot serialiser) don't have
        // to switch to `AHashMap` upstream. The conversion is cheap
        // (per-entry clone) and runs off the hot per-query path.
        self.parameters
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Digest of the planner-relevant parameter set. Returns `0` when
    /// there is no planner state to add to the legacy Parse hash.
    /// Entries are sorted so insertion order cannot change the digest.
    pub fn planner_param_hash(&self) -> u64 {
        let cached = self
            .planner_hash_cache
            .load(std::sync::atomic::Ordering::Relaxed);
        if cached != PLANNER_HASH_UNSET {
            return cached;
        }
        use std::hash::Hasher;
        let mut entries: Vec<(&String, &String)> = self
            .parameters
            .iter()
            .filter(|(k, _)| is_planner_key(k.as_str()))
            .collect();
        if entries.is_empty() {
            self.planner_hash_cache
                .store(0, std::sync::atomic::Ordering::Relaxed);
            return 0;
        }
        entries.sort_by(|a, b| a.0.cmp(b.0));
        let mut hasher = xxhash_rust::xxh3::Xxh3::default();
        let mut count = 0u32;
        for (k, v) in &entries {
            hasher.write(k.as_bytes());
            hasher.write_u8(0);
            hasher.write(v.as_bytes());
            hasher.write_u8(0);
            count += 1;
        }
        hasher.write_u32(count);
        let h = hasher.finish();
        // Keep 0 for "no planner params" and PLANNER_HASH_UNSET for the
        // cache sentinel, even in the unlikely event of a real collision.
        let h = if h == 0 {
            1
        } else if h == PLANNER_HASH_UNSET {
            PLANNER_HASH_UNSET - 1
        } else {
            h
        };
        self.planner_hash_cache
            .store(h, std::sync::atomic::Ordering::Relaxed);
        h
    }

    fn add_parameter_message(key: &str, value: &str, buffer: &mut BytesMut) {
        buffer.put_u8(b'S');

        // 4 is len of i32, plus null terminators.
        let len = 4 + key.len() + 1 + value.len() + 1;

        buffer.put_i32(len as i32);
        buffer.put_slice(key.as_bytes());
        buffer.put_u8(0);
        buffer.put_slice(value.as_bytes());
        buffer.put_u8(0);
    }
}

impl From<&ServerParameters> for BytesMut {
    fn from(server_parameters: &ServerParameters) -> Self {
        let mut bytes = BytesMut::new();

        // Do not echo StartupMessage protocol fields as ParameterStatus.
        // They are not PostgreSQL-reported session parameters.
        for (key, value) in &server_parameters.parameters {
            if PARAMETER_STATUS_SUPPRESSED.contains(key.as_str()) {
                continue;
            }
            ServerParameters::add_parameter_message(key, value, &mut bytes);
        }

        bytes
    }
}

/// StartupMessage protocol fields that must not be serialized as
/// ParameterStatus.
static PARAMETER_STATUS_SUPPRESSED: Lazy<AHashSet<&'static str>> = Lazy::new(|| {
    let mut s = AHashSet::new();
    s.insert("user");
    s.insert("database");
    s.insert("replication");
    s.insert("options");
    s
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_timezone_matches_any_case() {
        assert_eq!(canonicalize_param_name("timezone".to_string()), "TimeZone");
        assert_eq!(canonicalize_param_name("TIMEZONE".to_string()), "TimeZone");
        assert_eq!(canonicalize_param_name("TimeZone".to_string()), "TimeZone");
        assert_eq!(canonicalize_param_name("TimezONE".to_string()), "TimeZone");
    }

    #[test]
    fn canonicalize_datestyle_matches_any_case() {
        assert_eq!(
            canonicalize_param_name("datestyle".to_string()),
            "DateStyle"
        );
        assert_eq!(
            canonicalize_param_name("DATESTYLE".to_string()),
            "DateStyle"
        );
        assert_eq!(
            canonicalize_param_name("DateStyle".to_string()),
            "DateStyle"
        );
    }

    #[test]
    fn canonicalize_intervalstyle_matches_any_case() {
        assert_eq!(
            canonicalize_param_name("intervalstyle".to_string()),
            "IntervalStyle"
        );
        assert_eq!(
            canonicalize_param_name("INTERVALSTYLE".to_string()),
            "IntervalStyle"
        );
    }

    #[test]
    fn canonicalize_lowercases_non_tracked_keys() {
        // Untracked names must collapse to one form for config merges.
        assert_eq!(canonicalize_param_name("work_mem".to_string()), "work_mem");
        assert_eq!(canonicalize_param_name("Work_Mem".to_string()), "work_mem");
        assert_eq!(canonicalize_param_name("WORK_MEM".to_string()), "work_mem");
        assert_eq!(
            canonicalize_param_name("statement_timeout".to_string()),
            "statement_timeout"
        );
        assert_eq!(
            canonicalize_param_name("Statement_Timeout".to_string()),
            "statement_timeout"
        );
    }

    #[test]
    fn is_set_forbidden_covers_read_only_and_reserved() {
        // Read-only GUCs PostgreSQL rejects.
        assert!(is_set_forbidden("is_superuser"));
        assert!(is_set_forbidden("server_version"));
        assert!(is_set_forbidden("lc_collate"));
        assert!(is_set_forbidden("lc_ctype"));
        // StartupMessage protocol fields are not GUCs.
        assert!(is_set_forbidden("user"));
        assert!(is_set_forbidden("database"));
        // Transaction state cannot be pushed during checkout.
        assert!(is_set_forbidden("transaction_isolation"));
        // search_path is mutable and must be replayable.
        assert!(!is_set_forbidden("search_path"));
        // ParameterStatus-tracked GUCs stay settable.
        assert!(!is_set_forbidden("application_name"));
    }

    #[test]
    fn is_planner_key_targets_only_session_mutable_plan_inputs() {
        assert!(is_planner_key("search_path"));
        assert!(is_planner_key("default_transaction_isolation"));
        assert!(is_planner_key("role"));
        // lc_collate affects planning but cannot be changed per session.
        assert!(!is_planner_key("lc_collate"));
        assert!(!is_planner_key("application_name"));
    }

    #[test]
    fn planner_param_hash_empty_returns_zero() {
        // Zero means no planner state is mixed into the Parse hash.
        let sp = ServerParameters::new();
        assert_eq!(sp.planner_param_hash(), 0);
    }

    #[test]
    fn planner_param_hash_distinguishes_different_values() {
        let mut a = ServerParameters::new();
        a.set_param("search_path", "schema_a", true);
        let mut b = ServerParameters::new();
        b.set_param("search_path", "schema_b", true);
        assert_ne!(a.planner_param_hash(), b.planner_param_hash());
    }

    #[test]
    fn planner_param_hash_stable_for_identical_set() {
        // Insertion order must not affect the digest.
        let mut a = ServerParameters::new();
        a.set_param("search_path", "schema_a", true);
        a.set_param("role", "reader", true);
        let mut b = ServerParameters::new();
        b.set_param("role", "reader", true);
        b.set_param("search_path", "schema_a", true);
        assert_eq!(a.planner_param_hash(), b.planner_param_hash());
    }

    #[test]
    fn planner_param_hash_ignores_non_planner_keys() {
        // application_name is not planner state.
        let mut a = ServerParameters::new();
        a.set_param("application_name", "client-A", true);
        let mut b = ServerParameters::new();
        b.set_param("application_name", "client-B", true);
        assert_eq!(a.planner_param_hash(), b.planner_param_hash());
        // Both still have no planner state.
        assert_eq!(a.planner_param_hash(), 0);
    }

    #[test]
    fn planner_param_hash_cache_invalidated_on_set() {
        // Changing planner state must invalidate the cached digest.
        let mut sp = ServerParameters::new();
        sp.set_param("search_path", "schema_a", true);
        let h1 = sp.planner_param_hash();
        sp.set_param("search_path", "schema_b", true);
        let h2 = sp.planner_param_hash();
        assert_ne!(h1, h2);
    }

    #[test]
    fn planner_param_hash_cache_survives_non_planner_set() {
        // Non-planner changes must not invalidate the cached digest.
        let mut sp = ServerParameters::new();
        sp.set_param("search_path", "schema_a", true);
        let h1 = sp.planner_param_hash();
        sp.set_param("application_name", "client-A", true);
        let h2 = sp.planner_param_hash();
        assert_eq!(h1, h2);
    }

    #[test]
    fn compare_params_emits_set_when_client_only() {
        let backend = ServerParameters::new();
        let mut client = ServerParameters::new();
        client.set_param("search_path", "schema_a", true);
        let diff = backend.compare_params(&client);
        match diff.get("search_path") {
            Some(ParamAction::SetTo(v)) => assert_eq!(v, "schema_a"),
            other => panic!("expected SetTo(schema_a), got {other:?}"),
        }
    }

    #[test]
    fn compare_params_emits_reset_when_backend_only() {
        // Backend-only planner state must be reset for the next client.
        let mut backend = ServerParameters::new();
        backend.set_param("search_path", "schema_a", true);
        let client = ServerParameters::new();
        let diff = backend.compare_params(&client);
        assert!(matches!(diff.get("search_path"), Some(ParamAction::Reset)));
    }

    #[test]
    fn compare_params_skips_forbidden_names() {
        // Forbidden client keys must not become checkout SET commands.
        let backend = ServerParameters::new();
        let mut client = ServerParameters::new();
        client.set_param("is_superuser", "on", true);
        client.set_param("user", "rogue", true);
        let diff = backend.compare_params(&client);
        assert!(!diff.contains_key("is_superuser"));
        assert!(!diff.contains_key("user"));
    }

    #[test]
    fn compare_params_skips_backend_reported_fork_readonly_gucs() {
        // PostgresPro/Tantor report PGC_INTERNAL GUCs (pgpro_build, pgpro_edition,
        // pgpro_version) via ParameterStatus. They enter the map through the
        // startup path but must NEVER be SET/RESET by checkout sync: the backend
        // rejects them with SQLSTATE 55P02 and gets marked bad. An allowlist
        // (tracked + planner keys) excludes any fork's read-only GUC without an
        // ever-growing per-fork denylist.
        let mut backend = ServerParameters::new();
        let mut client = ServerParameters::new();
        // Reported fork GUCs, differing between client and backend.
        client.set_param("pgpro_build", "build-A", true);
        backend.set_param("pgpro_build", "build-B", true);
        client.set_param("pgpro_edition", "standard", true);
        client.set_param("pgpro_version", "16.4", true);
        // A legitimately syncable param must still flow through.
        client.set_param("application_name", "svc:42", false);

        let diff = backend.compare_params(&client);

        assert!(
            !diff.contains_key("pgpro_build"),
            "read-only pgpro_build must not be SET"
        );
        assert!(!diff.contains_key("pgpro_edition"));
        assert!(!diff.contains_key("pgpro_version"));
        assert!(
            matches!(diff.get("application_name"), Some(ParamAction::SetTo(v)) if v == "svc:42"),
            "tracked application_name must still sync: {diff:?}"
        );
    }

    #[test]
    fn planner_param_hash_distinguishes_non_search_path_planner_key() {
        // `role` also changes planner state and must affect the digest.
        let mut a = ServerParameters::new();
        a.set_param("role", "reader", true);
        let mut b = ServerParameters::new();
        b.set_param("role", "writer", true);
        assert_ne!(a.planner_param_hash(), b.planner_param_hash());
    }

    #[test]
    fn parameter_status_serialization_drops_startup_reserved_names() {
        // Protocol fields from StartupMessage must not be echoed as
        // ParameterStatus. Real server parameters still pass through.
        let mut sp = ServerParameters::new();
        sp.set_param("user", "rogue", true);
        sp.set_param("database", "elsewhere", true);
        sp.set_param("search_path", "schema_a", true);
        sp.set_param("server_version", "99", true);
        let bytes: bytes::BytesMut = (&sp).into();
        let blob = String::from_utf8_lossy(&bytes);
        // Match NUL-delimited key segments, not substrings.
        assert!(!blob.contains("\0user\0"));
        assert!(!blob.contains("\0database\0"));
        // Non-suppressed planner key passes through.
        assert!(blob.contains("search_path"));
        // Real server ParameterStatus values pass through.
        assert!(blob.contains("server_version"));
    }

    #[test]
    fn remove_param_drops_entry_and_invalidates_planner_cache_for_planner_keys() {
        // RESET removes planner state and invalidates the cached digest.
        let mut sp = ServerParameters::new();
        sp.set_param("search_path", "schema_a", true);
        let h_before = sp.planner_param_hash();
        assert_ne!(h_before, 0);
        sp.remove_param("search_path");
        assert!(!sp.parameters.contains_key("search_path"));
        assert_eq!(sp.planner_param_hash(), 0);
    }

    #[test]
    fn remove_param_keeps_planner_cache_for_non_planner_keys() {
        let mut sp = ServerParameters::new();
        sp.set_param("search_path", "schema_a", true);
        sp.set_param("application_name", "client-A", true);
        let h_before = sp.planner_param_hash();
        sp.remove_param("application_name");
        // Removing non-planner state must not change the planner digest.
        assert_eq!(sp.planner_param_hash(), h_before);
    }

    #[test]
    fn is_safe_client_startup_key_accepts_session_mutable_names() {
        // Common mutable session GUCs pass the client key filter.
        assert!(is_safe_client_startup_key("search_path"));
        assert!(is_safe_client_startup_key("application_name"));
        assert!(is_safe_client_startup_key("work_mem"));
        // Extension GUC names may contain dots.
        assert!(is_safe_client_startup_key("auto_explain.log_min_duration"));
    }

    #[test]
    fn is_safe_client_startup_key_rejects_set_forbidden_names() {
        // Even well-formed forbidden names must not pass the client filter.
        assert!(!is_safe_client_startup_key("is_superuser"));
        assert!(!is_safe_client_startup_key("server_version"));
        assert!(!is_safe_client_startup_key("lc_collate"));
        assert!(!is_safe_client_startup_key("user"));
        assert!(!is_safe_client_startup_key("database"));
        assert!(!is_safe_client_startup_key("transaction_isolation"));
    }

    #[test]
    fn is_safe_client_startup_key_rejects_injection_shaped_names() {
        // The key is used as raw SQL identifier text in checkout SET.
        assert!(!is_safe_client_startup_key(""));
        assert!(!is_safe_client_startup_key("1foo")); // leading digit
        assert!(!is_safe_client_startup_key("foo bar"));
        assert!(!is_safe_client_startup_key("foo;bar"));
        assert!(!is_safe_client_startup_key("foo'bar"));
        assert!(!is_safe_client_startup_key("foo\"bar"));
        assert!(!is_safe_client_startup_key("foo--"));
        // This payload would turn into
        //   SET app TO 'x'; DEALLOCATE ALL; -- TO '...'.
        assert!(!is_safe_client_startup_key(
            "app TO 'x'; DEALLOCATE ALL; --"
        ));
    }

    #[test]
    fn is_safe_client_startup_key_rejects_protocol_reserved_prefix() {
        // PostgreSQL reserves `_pq_.` for protocol extension negotiation.
        assert!(!is_safe_client_startup_key("_pq_.foo"));
        assert!(!is_safe_client_startup_key("_PQ_.foo"));
    }

    /// a key shaped like a SQL
    /// injection payload reaches `set_param` only from
    /// backend-controlled bytes (ParameterStatus / SET-echo) when
    /// `server_tls_mode != "verify-full"`. The shape gate must reject
    /// it BEFORE it enters the map so the downstream
    /// `sync_parameters` interpolation can never produce a multi-
    /// statement SET.
    #[test]
    fn set_param_rejects_backend_supplied_injection_shaped_key() {
        let mut sp = ServerParameters::new();
        // Inject via the same code path the wire-level
        // ParameterStatus handler uses.
        let changed = sp.set_param("application_name;DROP DATABASE production--", "x", true);
        assert!(!changed, "injection-shaped key must not be set");
        // The map must not now hold the malicious key.
        assert!(
            !sp.parameters
                .keys()
                .any(|k| k.contains(';') || k.contains("--")),
            "ServerParameters map contains injection-shaped key after set_param: {:?}",
            sp.parameters
        );
        // Several variants must all be rejected.
        for bad in [
            "foo bar",
            "foo'bar",
            "foo\"bar",
            "1foo",
            "",
            "x; DEALLOCATE ALL; --",
            "x/*comment*/",
        ] {
            let changed = sp.set_param(bad, "y", true);
            assert!(!changed, "key {bad:?} must be rejected");
            assert!(
                !sp.parameters.contains_key(bad),
                "key {bad:?} must not be in map"
            );
        }
    }

    #[test]
    fn clone_resets_planner_hash_cache() {
        // A clone starts with an empty planner-hash cache.
        let mut sp = ServerParameters::new();
        sp.set_param("search_path", "schema_a", true);
        let _ = sp.planner_param_hash(); // populate the cache
        let cloned = sp.clone();
        // Inspect the raw cache sentinel directly.
        let raw = cloned
            .planner_hash_cache
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(raw, PLANNER_HASH_UNSET);
    }

    // Note: get_application_name must not panic when
    // application_name is absent. ServerParameters::new() does NOT set
    // it (only ::admin() does), and a client RESET can remove it, so the
    // old `.unwrap()` was reachable unsoundness. The fix keeps the `&str`
    // contract (callers pass it straight into stats recorders) but falls
    // back to "" instead of panicking.
    #[test]
    fn get_application_name_empty_when_absent() {
        let sp = ServerParameters::new();
        // Would have panicked before the fix.
        assert_eq!(sp.get_application_name(), "");
    }

    #[test]
    fn get_application_name_empty_after_reset() {
        let mut sp = ServerParameters::admin();
        assert_eq!(sp.get_application_name(), "pg_doorman");
        sp.remove_param("application_name");
        assert_eq!(sp.get_application_name(), "");
    }

    #[test]
    fn get_application_name_returns_value_when_set() {
        let sp = ServerParameters::admin();
        assert_eq!(sp.get_application_name(), "pg_doorman");
    }

    #[test]
    fn compare_params_detects_application_name_change() {
        // A differing application_name must surface in the diff returned by
        // compare_params - this is the branch that drives the SET/RESET
        // round-trip counted by pg_doorman_sync_params_applied_total.
        let mut current = ServerParameters::new();
        current.set_param("application_name", "svc-old", false);
        let mut incoming = ServerParameters::new();
        incoming.set_param("application_name", "svc-new", false);
        let diff = current.compare_params(&incoming);
        assert!(
            diff.contains_key("application_name"),
            "application_name change must appear in the diff (drives sync_applied)"
        );
        assert!(matches!(
            diff.get("application_name"),
            Some(ParamAction::SetTo(value)) if value == "svc-new"
        ));
    }
}
