//! Process-wide snapshot of `general.pooler_check_query` and its pre-encoded
//! SimpleQuery wire bytes.
//!
//! Lives outside `general.rs` because the snapshot is runtime cache state
//! rather than a serializable config field. Updated on every config
//! `parse()` so that the byte-match path and the per-pool response cache
//! key always read the same pair, even across a `RELOAD` that races with
//! an in-flight probe.

use arc_swap::ArcSwap;
use bytes::{BufMut, Bytes, BytesMut};
use once_cell::sync::Lazy;
use std::mem;
use std::sync::Arc;

use crate::errors::Error;
use crate::messages::MAX_MESSAGE_SIZE;

const SIMPLE_QUERY_LENGTH_OVERHEAD: usize = mem::size_of::<i32>() + 1;

/// Atomic snapshot of `general.pooler_check_query` and its pre-encoded
/// SimpleQuery wire bytes. Initialized with the default `;` value.
pub static POOLER_CHECK_QUERY_SNAPSHOT: Lazy<ArcSwap<PoolerCheckQuerySnapshot>> =
    Lazy::new(|| ArcSwap::from_pointee(PoolerCheckQuerySnapshot::new(";")));

#[derive(Debug)]
pub struct PoolerCheckQuerySnapshot {
    pub query: String,
    pub request_bytes: Bytes,
}

impl PoolerCheckQuerySnapshot {
    pub fn new(query: &str) -> Self {
        let wire_len = pooler_check_query_wire_len(query.len())
            .expect("pooler_check_query length must be validated before snapshot publish");
        debug_assert!(
            validate_pooler_check_query(query).is_ok(),
            "pooler_check_query must be validated before snapshot publish"
        );

        let mut buf = BytesMut::with_capacity(query.len() + 6);
        buf.put_u8(b'Q');
        buf.put_i32(wire_len);
        buf.put_slice(query.as_bytes());
        buf.put_u8(b'\0');
        Self {
            query: query.to_string(),
            request_bytes: buf.freeze(),
        }
    }
}

fn pooler_check_query_wire_len(query_len: usize) -> Result<i32, Error> {
    let wire_len = query_len
        .checked_add(SIMPLE_QUERY_LENGTH_OVERHEAD)
        .ok_or_else(|| Error::BadConfig("general.pooler_check_query is too large".to_string()))?;

    if wire_len >= MAX_MESSAGE_SIZE as usize {
        return Err(Error::BadConfig(format!(
            "general.pooler_check_query encoded SimpleQuery length must be < {MAX_MESSAGE_SIZE} bytes"
        )));
    }

    Ok(wire_len as i32)
}

pub(crate) fn validate_pooler_check_query(query: &str) -> Result<(), Error> {
    if query.as_bytes().contains(&0) {
        return Err(Error::BadConfig(
            "general.pooler_check_query must not contain NUL bytes".to_string(),
        ));
    }

    pooler_check_query_wire_len(query.len()).map(|_| ())
}

/// Atomically replace the global snapshot. Called from config `parse()`
/// after the new `Config` has been swapped into `CONFIG`.
pub fn update_pooler_check_query_snapshot(query: &str) {
    POOLER_CHECK_QUERY_SNAPSHOT.store(Arc::new(PoolerCheckQuerySnapshot::new(query)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_encodes_simple_query_wire_format() {
        let snap = PoolerCheckQuerySnapshot::new(";");
        assert_eq!(snap.query, ";");
        // 'Q' + i32(6) + ';' + '\0' = 7 bytes total; length field counts itself + body + null
        assert_eq!(snap.request_bytes.as_ref(), &[b'Q', 0, 0, 0, 6, b';', 0]);
    }

    #[test]
    fn snapshot_encodes_select_one() {
        let snap = PoolerCheckQuerySnapshot::new("select 1");
        // 'Q' + i32(13) + "select 1" + '\0'
        let expected = {
            let mut v = vec![b'Q', 0, 0, 0, 13];
            v.extend_from_slice(b"select 1");
            v.push(0);
            v
        };
        assert_eq!(snap.request_bytes.as_ref(), &expected[..]);
    }

    #[test]
    fn validation_rejects_embedded_nul() {
        let err = validate_pooler_check_query("select\0 1").expect_err("NUL must be rejected");
        assert!(err.to_string().contains("NUL"));
    }

    #[test]
    fn validation_rejects_protocol_message_cap_without_allocating() {
        let err =
            pooler_check_query_wire_len(MAX_MESSAGE_SIZE as usize - SIMPLE_QUERY_LENGTH_OVERHEAD)
                .expect_err("length exactly at protocol cap must be rejected");
        assert!(
            err.to_string().contains("encoded SimpleQuery length"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn update_swaps_global_snapshot() {
        update_pooler_check_query_snapshot("select 42");
        let live = POOLER_CHECK_QUERY_SNAPSHOT.load();
        assert_eq!(live.query, "select 42");
        update_pooler_check_query_snapshot(";");
    }
}
