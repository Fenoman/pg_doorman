//! Single source of truth for the database-scoped admin actions. Both the
//! postgres-protocol admin socket (`crate::admin::commands::{pause,resume,
//! reconnect}`) and the REST surface (`POST /api/admin/{pause,resume,
//! reconnect}`) call into the helpers here and translate the typed
//! [`AdminEffect`] into their own response envelopes. That way the two
//! transports cannot diverge: a `db` filter that matches no pool is
//! reported as a `NoMatchingDb` outcome to both, instead of an SQLSTATE
//! error in one path and a silent `affected: 0` in the other.
//!
//! `events::push_event` is emitted from here, so the Web UI's events
//! overlay paints a marker on every successful action regardless of
//! origin.

use log::info;
use std::collections::HashSet;

use crate::config::reload_config;
use crate::errors::Error;
use crate::pool::{
    get_all_pools, get_client_server_map, get_retired_pools, pool_write_lock, ConnectionPool,
    PoolIdentifier,
};

/// Scope filter for `pause` / `resume` / `reconnect`. The REST surface
/// accepts both `?db=<name>` (every user@db pool of one database) and
/// `?pool=<user>@<db>` (one specific pool); the admin protocol path
/// currently only takes a database name, so it always passes
/// [`AdminScope::Database`] or [`AdminScope::AllPools`].
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum AdminScope {
    AllPools,
    Database(String),
    Pool { user: String, db: String },
}

impl AdminScope {
    fn matches(&self, identifier: &PoolIdentifier) -> bool {
        match self {
            AdminScope::AllPools => true,
            AdminScope::Database(name) => identifier.db == *name,
            AdminScope::Pool { user, db } => identifier.user == *user && identifier.db == *db,
        }
    }
}

/// Outcome of an admin action.
///
/// `Applied { affected: [] }` is legitimate when [`AdminScope::AllPools`]
/// is used but the pooler holds no pools yet. `NoMatchingDb` /
/// `NoMatchingPool` are reserved for the case where the caller did pass
/// a scope filter and no pool matches — transports turn these into a
/// 404 / SQLSTATE 3D000 so the operator gets a clear signal that the
/// typo / stale name took no effect.
///
/// `affected` is the list of touched pools, not just a count, so DBAs
/// can see exactly which `user@db` rows the action ran against — useful
/// when the same database has several users.
#[derive(Debug, PartialEq, Eq)]
pub enum AdminEffect {
    NoMatchingDb { db: String },
    NoMatchingPool { user: String, db: String },
    Applied { affected: Vec<PoolIdentifier> },
}

fn reload_event_message(changed: bool) -> &'static str {
    if changed {
        "config reloaded"
    } else {
        "config unchanged"
    }
}

/// Reload the configuration file. Equivalent to `RELOAD` on the admin
/// protocol; emits the same RELOAD event. Returns `true` when the config
/// actually changed and pools were reconciled, `false` when the file
/// re-parsed identically to the live config (a no-op reload).
pub async fn reload_now() -> Result<bool, Error> {
    let csm = get_client_server_map()
        .ok_or_else(|| Error::SocketError("client_server_map not initialised".into()))?;
    info!("Reloading config (via /api/admin/reload)");
    let changed = match reload_config(csm).await {
        Ok(c) => c,
        Err(e) => {
            crate::admin::events::push_event_rate_limited(
                "CONFIG_VALIDATION_ERROR",
                format!("/api/admin/reload rejected: {e}"),
            );
            return Err(e);
        }
    };
    crate::admin::events::push_event("RELOAD", reload_event_message(changed).to_string());
    crate::config::get_config().show();
    Ok(changed)
}

/// Pause every pool the scope selects.
pub fn pause_now(scope: AdminScope) -> AdminEffect {
    apply_per_pool(scope, |identifier, pool| {
        pool.database.pause();
        crate::admin::events::push_event("PAUSE", format!("pool {identifier} paused"));
        info!("PAUSE: paused pool {identifier}");
    })
}

/// Resume — mirror of [`pause_now`].
pub fn resume_now(scope: AdminScope) -> AdminEffect {
    apply_per_pool(scope, |identifier, pool| {
        pool.database.resume();
        crate::admin::events::push_event("RESUME", format!("pool {identifier} resumed"));
        info!("RESUME: resumed pool {identifier}");
    })
}

/// Reconnect — bumps the pool epoch and drains idle connections. Active
/// connections are refused on return.
pub fn reconnect_now(scope: AdminScope) -> AdminEffect {
    let mut drain = Vec::new();
    let effect = apply_per_pool(scope, |identifier, pool| {
        // Mark every serving generation under the publication lock, then
        // close idle sockets after releasing it. Active connections still
        // fail the epoch check on return, as with Pool::reconnect.
        let new_epoch = pool.database.server_pool().bump_epoch();
        drain.push(pool.database.clone());
        crate::admin::events::push_event(
            "RECONNECT",
            format!("pool {identifier} reconnected (epoch={new_epoch})"),
        );
        info!("RECONNECT: reconnected pool {identifier} (new epoch: {new_epoch})");
    });
    for pool in drain {
        pool.retain(|_, _| false);
    }
    effect
}

/// Iterate the pool table once: skip pools that do not match the scope,
/// return `NoMatchingDb` / `NoMatchingPool` if the scope's filter
/// matched nothing, otherwise return the list of touched pool ids.
fn apply_per_pool<F>(scope: AdminScope, mut act: F) -> AdminEffect
where
    F: FnMut(&PoolIdentifier, &ConnectionPool),
{
    // Serialize state changes with replacement publication. The reload
    // inherits PAUSE and registers retired generations under this same
    // lock, so each action sees either side of a transition in full.
    let _guard = pool_write_lock();
    let pools = get_all_pools();
    let retired = get_retired_pools();
    let mut affected = Vec::new();
    let mut seen_ids = HashSet::new();
    for (identifier, pool) in
        pools
            .iter()
            .map(|(id, pool)| (id.clone(), pool))
            .chain(retired.iter().map(|pool| {
                (
                    PoolIdentifier::new(&pool.address.pool_name, &pool.address.username),
                    pool.as_ref(),
                )
            }))
    {
        if !scope.matches(&identifier) {
            continue;
        }
        act(&identifier, pool);
        if seen_ids.insert(identifier.clone()) {
            affected.push(identifier);
        }
    }
    if affected.is_empty() {
        return match scope {
            AdminScope::AllPools => AdminEffect::Applied {
                affected: Vec::new(),
            },
            AdminScope::Database(db) => AdminEffect::NoMatchingDb { db },
            AdminScope::Pool { user, db } => AdminEffect::NoMatchingPool { user, db },
        };
    }
    AdminEffect::Applied { affected }
}

#[cfg(test)]
mod tests {
    use super::reload_event_message;

    #[tokio::test]
    #[serial_test::serial(retired_pools)]
    async fn admin_actions_reach_retired_generations_once_per_logical_pool() {
        use super::{pause_now, reconnect_now, resume_now, AdminEffect, AdminScope};
        use crate::pool::{
            clear_retired_pools_for_test, pool_write_lock, retire_pool_generations, ConnectionPool,
            PoolIdentifier, POOLS,
        };
        use std::sync::Arc;
        use std::time::Duration;

        let id = PoolIdentifier::new("admin_generation_db", "generation_user");
        let other_id = PoolIdentifier::new("admin_other_db", "generation_user");
        let make_pool = |id: &PoolIdentifier| {
            let mut pool = ConnectionPool::test_for_protocol();
            pool.address.pool_name = id.db.clone();
            pool.address.username = id.user.clone();
            pool
        };
        let first = make_pool(&id);
        let second = make_pool(&id);
        let live = make_pool(&id);
        let other = make_pool(&other_id);
        clear_retired_pools_for_test();
        retire_pool_generations(vec![first.clone(), second.clone()]);
        {
            let _guard = pool_write_lock();
            let mut pools = (**POOLS.load()).clone();
            pools.insert(id.clone(), live.clone());
            pools.insert(other_id.clone(), other.clone());
            POOLS.store(Arc::new(pools));
        }
        scopeguard::defer! {
            clear_retired_pools_for_test();
            let _guard = pool_write_lock();
            let mut pools = (**POOLS.load()).clone();
            pools.remove(&id);
            pools.remove(&other_id);
            POOLS.store(Arc::new(pools));
        }

        let expected = AdminEffect::Applied {
            affected: vec![id.clone()],
        };
        assert_eq!(pause_now(AdminScope::Database(id.db.clone())), expected);
        for pool in [&first, &second, &live] {
            assert!(
                pool.database.is_paused(),
                "every serving generation must pause"
            );
        }
        assert!(!other.database.is_paused());

        // A waiter already registered on the oldest generation must wake.
        let resumed = first.database.server_pool().resume_notified();
        tokio::pin!(resumed);
        resumed.as_mut().enable();
        assert_eq!(
            resume_now(AdminScope::Pool {
                user: id.user.clone(),
                db: id.db.clone(),
            }),
            expected
        );
        tokio::time::timeout(Duration::from_millis(100), resumed)
            .await
            .expect("RESUME must notify a retired generation's existing waiter");
        for pool in [&first, &second, &live] {
            assert!(!pool.database.is_paused());
        }

        assert_eq!(reconnect_now(AdminScope::Database(id.db.clone())), expected);
        for pool in [&first, &second, &live] {
            assert_eq!(pool.database.reconnect_epoch(), 1);
            assert!(
                !pool.database.is_closed(),
                "active sessions keep their pool"
            );
        }
        assert_eq!(other.database.reconnect_epoch(), 0);
    }

    #[test]
    fn reload_event_message_distinguishes_changed_and_unchanged() {
        assert_eq!(reload_event_message(true), "config reloaded");
        assert_eq!(reload_event_message(false), "config unchanged");
    }

    #[test]
    fn rest_reload_event_uses_changed_flag() {
        let src = include_str!("operations.rs");
        let start = src
            .find("pub async fn reload_now")
            .expect("reload_now exists");
        let end = src[start..]
            .find("/// Pause every pool")
            .map(|offset| start + offset)
            .expect("pause docs follow reload_now");
        let body = &src[start..end];

        assert!(
            body.contains("reload_event_message(changed)"),
            "REST reload event must distinguish changed and unchanged reloads"
        );
        assert!(
            !body.contains("push_event(\"RELOAD\", \"config reloaded\".to_string());"),
            "REST reload must not report no-op reloads as config reloaded"
        );
    }
}
