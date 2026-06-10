//! Batch handling for PostgreSQL Extended Query Protocol.
//!
//! This module handles the reordering of ParseComplete messages when some Parse
//! operations are skipped due to prepared statement caching.
//!
//! ## Problem
//!
//! When pg_doorman caches prepared statements, it may skip sending Parse messages
//! to the server for statements that are already cached. However, the client expects
//! to receive ParseComplete responses in the same order as the Parse messages it sent.
//!
//! ## Solution
//!
//! This module tracks batch operations and inserts synthetic ParseComplete messages
//! at the correct positions in the response stream.

use bytes::BytesMut;
use log::debug;
use smallvec::SmallVec;

use super::core::{BatchOperation, Client};

/// Type alias for insertion map: stores (index, count) pairs.
/// SmallVec with inline capacity 8 avoids heap allocation for typical batch sizes.
type InsertionMap = SmallVec<[(usize, usize); 8]>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResponseAnchor {
    Parse,
    Bind,
    ParamDesc,
    PortalDesc,
    Execute,
    Close,
}

fn next_response_anchor(
    operations: &[BatchOperation],
    parse_seen: usize,
    bind_seen: usize,
    param_desc_seen: usize,
    portal_desc_seen: usize,
    execute_seen: usize,
    close_seen: usize,
) -> Option<ResponseAnchor> {
    let mut parse_index = 0usize;
    let mut bind_index = 0usize;
    let mut param_desc_index = 0usize;
    let mut portal_desc_index = 0usize;
    let mut execute_index = 0usize;
    let mut close_index = 0usize;

    for op in operations {
        match op {
            BatchOperation::ParseSkipped { .. } => {}
            BatchOperation::ParseSent { .. } => {
                if parse_index == parse_seen {
                    return Some(ResponseAnchor::Parse);
                }
                parse_index += 1;
            }
            BatchOperation::Bind { .. } => {
                if bind_index == bind_seen {
                    return Some(ResponseAnchor::Bind);
                }
                bind_index += 1;
            }
            BatchOperation::Describe { .. } => {
                if param_desc_index == param_desc_seen {
                    return Some(ResponseAnchor::ParamDesc);
                }
                param_desc_index += 1;
            }
            BatchOperation::DescribePortal => {
                if portal_desc_index == portal_desc_seen {
                    return Some(ResponseAnchor::PortalDesc);
                }
                portal_desc_index += 1;
            }
            BatchOperation::Execute => {
                if execute_index == execute_seen {
                    return Some(ResponseAnchor::Execute);
                }
                execute_index += 1;
            }
            BatchOperation::Close => {
                if close_index == close_seen {
                    return Some(ResponseAnchor::Close);
                }
                close_index += 1;
            }
        }
    }
    None
}

/// Helper to add or increment count for an index in InsertionMap
#[inline]
fn insertion_map_add(map: &mut InsertionMap, index: usize, count: usize) {
    if let Some(entry) = map.iter_mut().find(|(idx, _)| *idx == index) {
        entry.1 += count;
    } else {
        map.push((index, count));
    }
}

/// Helper to get count for an index from InsertionMap
#[inline]
fn insertion_map_get(map: &InsertionMap, index: usize) -> Option<usize> {
    map.iter()
        .find(|(idx, _)| *idx == index)
        .map(|(_, count)| *count)
}

/// Helper to sum all counts in InsertionMap
#[inline]
fn insertion_map_sum(map: &InsertionMap) -> usize {
    map.iter().map(|(_, count)| *count).sum()
}

// Static ParseComplete message: '1' (1 byte) + length 4 (4 bytes big-endian)
pub(crate) const PARSE_COMPLETE_MSG: [u8; 5] = [b'1', 0, 0, 0, 4];

impl<S, T> Client<S, T>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
    T: tokio::io::AsyncWrite + std::marker::Unpin,
{
    /// Insert ParseComplete messages into response based on batch_operations order.
    /// This ensures that ParseComplete for skipped Parse operations appears in the
    /// correct position relative to other responses.
    ///
    /// PostgreSQL processes messages in order and sends responses in order:
    /// - Parse → ParseComplete (immediately)
    /// - Bind -> BindComplete (immediately)
    /// - Execute → DataRow + CommandComplete (immediately)
    /// - Describe → ParameterDescription + RowDescription (immediately)
    ///
    /// So for skipped Parse operations, we need to insert ParseComplete at the
    /// ABSOLUTE position in the response stream where the Parse was in the batch.
    ///
    /// This function handles streaming responses - it tracks how many messages have been
    /// processed across multiple chunks using self.processed_response_counts.
    pub(crate) fn reorder_parse_complete_responses(
        &mut self,
        response: BytesMut,
        append_trailing_pending: bool,
    ) -> BytesMut {
        if self.prepared.batch_operations.is_empty() || self.prepared.skipped_parses.is_empty() {
            return response;
        }

        debug!(
            "[{}@{} #c{}] reordering responses: operations={}, skipped_parses={}",
            self.username,
            self.pool_name,
            self.connection_id,
            self.prepared.batch_operations.len(),
            self.prepared.skipped_parses.len()
        );

        // Track which backend response index needs ParseComplete inserted before it.
        // We can't use absolute positions because Execute returns variable number of messages.
        // Instead, we track the index of the response-producing message where ParseComplete should go.
        //
        // When ParseSkipped happens, we look at the NEXT operation that will produce a response:
        // - If next is ParseSent -> insert before that ParseComplete
        // - If next is Bind -> insert before that BindComplete
        // - If next is Describe -> insert before that ParameterDescription
        // - If next is Execute/DescribePortal -> we need to insert before the NEXT Bind/Describe after that

        // Maps: ParseComplete index -> count of ParseComplete to insert before it
        //       BindComplete index -> count of ParseComplete to insert before it
        //       ParameterDescription index -> count of ParseComplete to insert before it
        // Using SmallVec to avoid heap allocation for typical batch sizes (< 8 operations)
        let mut insert_before_parse: InsertionMap = SmallVec::new();
        let mut insert_before_bind: InsertionMap = SmallVec::new();
        let mut insert_before_param_desc: InsertionMap = SmallVec::new();
        let mut insert_before_portal_desc: InsertionMap = SmallVec::new();

        // Pending ParseComplete insertions waiting for next Bind/Describe
        let mut pending_insertions: usize = 0;

        // Current indices
        let mut parse_index: usize = 0;
        let mut bind_index: usize = 0;
        let mut describe_index: usize = 0;
        let mut portal_desc_index: usize = 0;

        // Also track Execute index for inserting before Execute's first message
        let mut insert_before_execute: InsertionMap = SmallVec::new();
        let mut execute_index: usize = 0;

        // Track Close index for inserting before CloseComplete
        let mut insert_before_close: InsertionMap = SmallVec::new();
        let mut close_index: usize = 0;

        for op in &self.prepared.batch_operations {
            match op {
                BatchOperation::ParseSkipped { .. } => {
                    // Mark that we need to insert ParseComplete
                    pending_insertions += 1;
                }
                BatchOperation::ParseSent { .. } => {
                    // Server sends ParseComplete. If skipped Parse operations
                    // preceded this real Parse, their synthetic ParseComplete
                    // must be emitted before this ParseComplete. Waiting until
                    // Sync/ReadyForQuery would put the later Parse's success or
                    // error before the earlier cached Parse response.
                    if pending_insertions > 0 {
                        insertion_map_add(
                            &mut insert_before_parse,
                            parse_index,
                            pending_insertions,
                        );
                        pending_insertions = 0;
                    }
                    parse_index += 1;
                }
                BatchOperation::Describe { .. } => {
                    // Insert pending ParseComplete before this ParameterDescription
                    if pending_insertions > 0 {
                        insertion_map_add(
                            &mut insert_before_param_desc,
                            describe_index,
                            pending_insertions,
                        );
                        pending_insertions = 0;
                    }
                    describe_index += 1;
                }
                BatchOperation::Bind { .. } => {
                    // Insert pending ParseComplete before this BindComplete
                    if pending_insertions > 0 {
                        insertion_map_add(&mut insert_before_bind, bind_index, pending_insertions);
                        pending_insertions = 0;
                    }
                    bind_index += 1;
                }
                BatchOperation::DescribePortal => {
                    // Portal Describe produces RowDescription or NoData
                    // directly, without a preceding ParameterDescription.
                    if pending_insertions > 0 {
                        insertion_map_add(
                            &mut insert_before_portal_desc,
                            portal_desc_index,
                            pending_insertions,
                        );
                        pending_insertions = 0;
                    }
                    portal_desc_index += 1;
                }
                BatchOperation::Execute => {
                    // Insert pending ParseComplete before this Execute's first message
                    if pending_insertions > 0 {
                        insertion_map_add(
                            &mut insert_before_execute,
                            execute_index,
                            pending_insertions,
                        );
                        pending_insertions = 0;
                    }
                    execute_index += 1;
                }
                BatchOperation::Close => {
                    // Insert pending ParseComplete before this CloseComplete
                    if pending_insertions > 0 {
                        insertion_map_add(
                            &mut insert_before_close,
                            close_index,
                            pending_insertions,
                        );
                        pending_insertions = 0;
                    }
                    close_index += 1;
                }
            }
        }

        // Get offsets from previous chunks
        let parse_offset = self.prepared.processed_response_counts.parse_complete;
        let bind_offset = self.prepared.processed_response_counts.bind_complete;
        let param_desc_offset = self.prepared.processed_response_counts.param_desc;
        let portal_desc_offset = self.prepared.processed_response_counts.portal_desc;
        let execute_offset = self.prepared.processed_response_counts.execute;
        let close_offset = self.prepared.processed_response_counts.close_complete;

        // Adjust indices by offset - filter and transform in place to avoid new allocations
        let relevant_parse: InsertionMap = insert_before_parse
            .iter()
            .filter(|(idx, _)| *idx >= parse_offset)
            .map(|(idx, count)| (idx - parse_offset, *count))
            .collect();
        let relevant_bind: InsertionMap = insert_before_bind
            .iter()
            .filter(|(idx, _)| *idx >= bind_offset)
            .map(|(idx, count)| (idx - bind_offset, *count))
            .collect();
        let relevant_param_desc: InsertionMap = insert_before_param_desc
            .iter()
            .filter(|(idx, _)| *idx >= param_desc_offset)
            .map(|(idx, count)| (idx - param_desc_offset, *count))
            .collect();
        let relevant_portal_desc: InsertionMap = insert_before_portal_desc
            .iter()
            .filter(|(idx, _)| *idx >= portal_desc_offset)
            .map(|(idx, count)| (idx - portal_desc_offset, *count))
            .collect();
        let relevant_execute: InsertionMap = insert_before_execute
            .iter()
            .filter(|(idx, _)| *idx >= execute_offset)
            .map(|(idx, count)| (idx - execute_offset, *count))
            .collect();
        let relevant_close: InsertionMap = insert_before_close
            .iter()
            .filter(|(idx, _)| *idx >= close_offset)
            .map(|(idx, count)| (idx - close_offset, *count))
            .collect();

        let total_insertions: usize = insertion_map_sum(&relevant_parse)
            + insertion_map_sum(&relevant_bind)
            + insertion_map_sum(&relevant_param_desc)
            + insertion_map_sum(&relevant_portal_desc)
            + insertion_map_sum(&relevant_execute)
            + insertion_map_sum(&relevant_close);

        if total_insertions + pending_insertions == 0 {
            // Still need to count messages for offset tracking
            let mut parse_count = 0usize;
            let mut bind_count = 0usize;
            let mut param_desc_count = 0usize;
            let mut portal_desc_count = 0usize;
            let mut execute_count = 0usize;
            let mut close_complete_count = 0usize;
            let mut statement_desc_pending = self
                .prepared
                .processed_response_counts
                .statement_desc_pending;
            let mut pos = 0;
            while pos + 5 <= response.len() {
                let msg_type = response[pos] as char;
                let msg_len = u32::from_be_bytes([
                    response[pos + 1],
                    response[pos + 2],
                    response[pos + 3],
                    response[pos + 4],
                ]) as usize;
                match msg_type {
                    '1' => parse_count += 1,
                    '2' => bind_count += 1,
                    't' => {
                        param_desc_count += 1;
                        statement_desc_pending += 1;
                    }
                    'T' | 'n' => {
                        if statement_desc_pending > 0 {
                            statement_desc_pending -= 1;
                        } else {
                            let anchor = next_response_anchor(
                                &self.prepared.batch_operations,
                                parse_offset + parse_count,
                                bind_offset + bind_count,
                                param_desc_offset + param_desc_count,
                                portal_desc_offset + portal_desc_count,
                                execute_offset + execute_count,
                                close_offset + close_complete_count,
                            );
                            match anchor {
                                Some(ResponseAnchor::PortalDesc) => portal_desc_count += 1,
                                Some(ResponseAnchor::Execute) => execute_count += 1,
                                _ => {}
                            }
                        }
                    }
                    'C' | 'I' | 's' => execute_count += 1,
                    '3' => close_complete_count += 1,
                    _ => {}
                }
                pos += 1 + msg_len;
            }
            self.prepared.processed_response_counts.parse_complete += parse_count;
            self.prepared.processed_response_counts.bind_complete += bind_count;
            self.prepared.processed_response_counts.param_desc += param_desc_count;
            self.prepared.processed_response_counts.portal_desc += portal_desc_count;
            self.prepared
                .processed_response_counts
                .statement_desc_pending = statement_desc_pending;
            self.prepared.processed_response_counts.execute += execute_count;
            self.prepared.processed_response_counts.close_complete += close_complete_count;
            return response;
        }

        // Build new response
        let mut new_response =
            BytesMut::with_capacity(response.len() + (total_insertions + pending_insertions) * 5);
        let mut pos = 0;
        let mut parse_count: usize = 0;
        let mut bind_count: usize = 0;
        let mut param_desc_count: usize = 0;
        let mut portal_desc_count: usize = 0;
        let mut execute_count: usize = 0;
        let mut close_count: usize = 0;
        let mut inserted_count: usize = 0;
        let mut in_execute: bool = false; // Track if we're inside an Execute response
        let mut statement_desc_pending = self
            .prepared
            .processed_response_counts
            .statement_desc_pending;
        let mut saw_error: bool = false;

        while pos < response.len() {
            if pos + 5 > response.len() {
                new_response.extend_from_slice(&response[pos..]);
                break;
            }

            let msg_type = response[pos] as char;
            let msg_len = u32::from_be_bytes([
                response[pos + 1],
                response[pos + 2],
                response[pos + 3],
                response[pos + 4],
            ]) as usize;

            let msg_end = pos + 1 + msg_len;
            if msg_end > response.len() {
                new_response.extend_from_slice(&response[pos..]);
                break;
            }

            // Insert ParseComplete BEFORE this message if needed
            match msg_type {
                '1' => {
                    if let Some(count) = insertion_map_get(&relevant_parse, parse_count) {
                        for _ in 0..count {
                            new_response.extend_from_slice(&PARSE_COMPLETE_MSG);
                            inserted_count += 1;
                        }
                    }
                    parse_count += 1;
                }
                '2' => {
                    if let Some(count) = insertion_map_get(&relevant_bind, bind_count) {
                        for _ in 0..count {
                            new_response.extend_from_slice(&PARSE_COMPLETE_MSG);
                            inserted_count += 1;
                        }
                    }
                    bind_count += 1;
                }
                't' => {
                    if let Some(count) = insertion_map_get(&relevant_param_desc, param_desc_count) {
                        for _ in 0..count {
                            new_response.extend_from_slice(&PARSE_COMPLETE_MSG);
                            inserted_count += 1;
                        }
                    }
                    param_desc_count += 1;
                    statement_desc_pending += 1;
                }
                '3' => {
                    // CloseComplete - insert pending ParseComplete before it
                    if let Some(count) = insertion_map_get(&relevant_close, close_count) {
                        for _ in 0..count {
                            new_response.extend_from_slice(&PARSE_COMPLETE_MSG);
                            inserted_count += 1;
                        }
                    }
                    close_count += 1;
                }
                'C' => {
                    // CommandComplete can be the first and only Execute
                    // response (e.g. UPDATE/INSERT without RETURNING). If a
                    // cached Parse preceded that Execute, its synthetic
                    // ParseComplete belongs before this terminal response.
                    if !in_execute {
                        if let Some(count) = insertion_map_get(&relevant_execute, execute_count) {
                            for _ in 0..count {
                                new_response.extend_from_slice(&PARSE_COMPLETE_MSG);
                                inserted_count += 1;
                            }
                        }
                    }
                    // CommandComplete marks end of Execute.
                    in_execute = false;
                    execute_count += 1;
                }
                'I' | 's' => {
                    // EmptyQueryResponse and PortalSuspended are also
                    // terminal Execute responses and can be the first message
                    // for that Execute operation.
                    if !in_execute {
                        if let Some(count) = insertion_map_get(&relevant_execute, execute_count) {
                            for _ in 0..count {
                                new_response.extend_from_slice(&PARSE_COMPLETE_MSG);
                                inserted_count += 1;
                            }
                        }
                    }
                    in_execute = false;
                    execute_count += 1;
                }
                'T' | 'n' => {
                    if statement_desc_pending > 0 {
                        statement_desc_pending -= 1;
                    } else {
                        let anchor = next_response_anchor(
                            &self.prepared.batch_operations,
                            parse_offset + parse_count,
                            bind_offset + bind_count,
                            param_desc_offset + param_desc_count,
                            portal_desc_offset + portal_desc_count,
                            execute_offset + execute_count,
                            close_offset + close_count,
                        );
                        if anchor == Some(ResponseAnchor::PortalDesc) {
                            if let Some(count) =
                                insertion_map_get(&relevant_portal_desc, portal_desc_count)
                            {
                                for _ in 0..count {
                                    new_response.extend_from_slice(&PARSE_COMPLETE_MSG);
                                    inserted_count += 1;
                                }
                            }
                            portal_desc_count += 1;
                        } else if !in_execute {
                            if let Some(count) = insertion_map_get(&relevant_execute, execute_count)
                            {
                                for _ in 0..count {
                                    new_response.extend_from_slice(&PARSE_COMPLETE_MSG);
                                    inserted_count += 1;
                                }
                            }
                            in_execute = true;
                        }
                    }
                }
                // 'G' = CopyInResponse, 'H' = CopyOutResponse,
                // 'W' = CopyBothResponse. A prepared `COPY ... TO STDOUT` /
                // `COPY ... FROM STDIN` Execute starts with one of these
                // bytes rather than 'D'/'n'/'T'. Without the broader match
                // the synthetic ParseComplete queue was held until 'Z'
                // (Sync) - which in Flush-only async mode arrives much
                // later or never, causing the driver to read CopyResponse
                // where it expected ParseComplete first and abort with
                // protocol-violation.
                'D' | 'G' | 'H' | 'W' => {
                    // DataRow or any Copy*Response
                    // can be first message of Execute. Insert ParseComplete
                    // before first message of Execute if needed.
                    if !in_execute {
                        if let Some(count) = insertion_map_get(&relevant_execute, execute_count) {
                            for _ in 0..count {
                                new_response.extend_from_slice(&PARSE_COMPLETE_MSG);
                                inserted_count += 1;
                            }
                        }
                        in_execute = true;
                    }
                }
                'E' => {
                    // ErrorResponse replaces the response for the single
                    // backend operation that failed. Only synthetic
                    // ParseComplete entries anchored before that operation
                    // are still visible to the client; PostgreSQL skips
                    // subsequent frontend messages until Sync.
                    let anchor = next_response_anchor(
                        &self.prepared.batch_operations,
                        parse_offset + parse_count,
                        bind_offset + bind_count,
                        param_desc_offset + param_desc_count,
                        portal_desc_offset + portal_desc_count,
                        execute_offset + execute_count,
                        close_offset + close_count,
                    );
                    let count = match anchor {
                        Some(ResponseAnchor::Parse) => {
                            insertion_map_get(&relevant_parse, parse_count).unwrap_or(0)
                        }
                        Some(ResponseAnchor::Bind) => {
                            insertion_map_get(&relevant_bind, bind_count).unwrap_or(0)
                        }
                        Some(ResponseAnchor::ParamDesc) => {
                            insertion_map_get(&relevant_param_desc, param_desc_count).unwrap_or(0)
                        }
                        Some(ResponseAnchor::PortalDesc) => {
                            insertion_map_get(&relevant_portal_desc, portal_desc_count).unwrap_or(0)
                        }
                        Some(ResponseAnchor::Execute) => {
                            insertion_map_get(&relevant_execute, execute_count).unwrap_or(0)
                        }
                        Some(ResponseAnchor::Close) => {
                            insertion_map_get(&relevant_close, close_count).unwrap_or(0)
                        }
                        None => 0,
                    };
                    if count > 0 {
                        for _ in 0..count {
                            new_response.extend_from_slice(&PARSE_COMPLETE_MSG);
                            inserted_count += 1;
                        }
                    }
                    saw_error = true;
                }
                'Z' => {
                    // ReadyForQuery - insert any remaining pending ParseComplete before it.
                    // This includes both pending_insertions at the end of batch AND
                    // any insertions that were skipped due to an error before them.
                    if !saw_error {
                        let remaining = (total_insertions + pending_insertions) - inserted_count;
                        if remaining > 0 {
                            for _ in 0..remaining {
                                new_response.extend_from_slice(&PARSE_COMPLETE_MSG);
                                inserted_count += 1;
                            }
                        }
                    }
                }
                _ => {}
            }

            new_response.extend_from_slice(&response[pos..msg_end]);
            pos = msg_end;
        }

        // Update processed counts
        self.prepared.processed_response_counts.parse_complete += parse_count;
        self.prepared.processed_response_counts.bind_complete += bind_count;
        self.prepared.processed_response_counts.param_desc += param_desc_count;
        self.prepared.processed_response_counts.portal_desc += portal_desc_count;
        self.prepared
            .processed_response_counts
            .statement_desc_pending = statement_desc_pending;
        self.prepared.processed_response_counts.execute += execute_count;
        self.prepared.processed_response_counts.close_complete += close_count;

        if append_trailing_pending && !saw_error {
            let remaining = (total_insertions + pending_insertions) - inserted_count;
            if remaining > 0 {
                for _ in 0..remaining {
                    new_response.extend_from_slice(&PARSE_COMPLETE_MSG);
                }
            }
        }

        new_response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::buffer_pool::PooledBuffer;
    use crate::client::core::{ParseCompleteTarget, PreparedStatementState, SkippedParse};
    use crate::pool::PoolIdentifier;
    use crate::server::ServerParameters;
    use crate::stats::ClientStats;
    use bytes::BytesMut;
    use dashmap::DashMap;
    use std::sync::Arc;
    use tokio::io::{empty, sink, BufReader, Empty, Sink};

    fn message(code: u8, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 4 + body.len());
        out.push(code);
        out.extend_from_slice(&((4 + body.len()) as i32).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    fn parse_complete() -> Vec<u8> {
        message(b'1', &[])
    }

    fn error_response() -> Vec<u8> {
        message(b'E', &[0])
    }

    fn ready_for_query() -> Vec<u8> {
        message(b'Z', b"I")
    }

    fn command_complete(tag: &[u8]) -> Vec<u8> {
        let mut body = Vec::with_capacity(tag.len() + 1);
        body.extend_from_slice(tag);
        body.push(0);
        message(b'C', &body)
    }

    fn empty_query_response() -> Vec<u8> {
        message(b'I', &[])
    }

    fn portal_suspended() -> Vec<u8> {
        message(b's', &[])
    }

    fn row_description() -> Vec<u8> {
        message(b'T', &[])
    }

    fn no_data() -> Vec<u8> {
        message(b'n', &[])
    }

    fn test_client() -> Client<Empty, Sink> {
        let addr = "127.0.0.1:6543".parse().unwrap();
        Client {
            read: BufReader::new(empty()),
            write: sink(),
            buffer: PooledBuffer::new(),
            addr,
            addr_str: addr.to_string(),
            read_buf: BytesMut::new(),
            connection_id: 1,
            cancel_mode: false,
            transaction_mode: false,
            sql_prepare_session_pinned: false,
            secret_key: 0,
            client_server_map: Arc::new(DashMap::new()),
            stats: Arc::new(ClientStats::default()),
            admin: false,
            last_server_stats: None,
            connected_to_server: false,
            session_xact_start: None,
            pool_name: "db".to_string(),
            username: "user".to_string(),
            cached_pool_id: PoolIdentifier::new("db", "user"),
            migration_pool: None,
            migration_pool_is_dynamic: false,
            server_parameters: ServerParameters::default(),
            prepared: PreparedStatementState::new(true, 0),
            max_memory_usage: u64::MAX,
            client_last_messages_in_tx: PooledBuffer::new(),
            client_pending_begin: None,
            pending_app_name_set: None,
            #[cfg(unix)]
            raw_fd: None,
            #[cfg(all(unix, feature = "tls-migration"))]
            ssl_ptr: None,
        }
    }

    fn queue_skipped_parse(client: &mut Client<Empty, Sink>, name: &str) {
        client.prepared.skipped_parses.push(SkippedParse {
            statement_name: name.into(),
            target: ParseCompleteTarget::BindComplete,
            insert_at_beginning: false,
            has_bind: false,
        });
        client
            .prepared
            .batch_operations
            .push(BatchOperation::ParseSkipped {
                statement_name: name.into(),
            });
    }

    #[test]
    fn skipped_parse_before_sent_parse_is_emitted_before_later_parse_error() {
        let mut client = test_client();
        queue_skipped_parse(&mut client, "cached");
        client
            .prepared
            .batch_operations
            .push(BatchOperation::ParseSent {
                statement_name: "new_stmt".into(),
            });

        let response = [error_response(), ready_for_query()].concat();
        let reordered =
            client.reorder_parse_complete_responses(BytesMut::from(&response[..]), false);
        let expected = [parse_complete(), error_response(), ready_for_query()].concat();

        assert_eq!(
            reordered.as_ref(),
            &expected[..],
            "cached ParseComplete must precede the ErrorResponse for the later ParseSent"
        );
    }

    #[test]
    fn skipped_parse_after_failing_backend_op_is_not_reported_successful() {
        let mut client = test_client();
        client
            .prepared
            .batch_operations
            .push(BatchOperation::ParseSent {
                statement_name: "bad_stmt".into(),
            });
        queue_skipped_parse(&mut client, "cached_after_error");
        client.prepared.batch_operations.push(BatchOperation::Bind {
            statement_name: "cached_after_error".into(),
        });

        let response = [error_response(), ready_for_query()].concat();
        let reordered =
            client.reorder_parse_complete_responses(BytesMut::from(&response[..]), false);

        assert_eq!(
            reordered.as_ref(),
            &response[..],
            "cached Parse after a failed backend operation is skipped by PostgreSQL until Sync"
        );
    }

    #[test]
    fn flush_error_does_not_append_skipped_parse_after_failed_backend_op() {
        let mut client = test_client();
        client
            .prepared
            .batch_operations
            .push(BatchOperation::ParseSent {
                statement_name: "bad_stmt".into(),
            });
        queue_skipped_parse(&mut client, "cached_after_error");
        client.prepared.batch_operations.push(BatchOperation::Bind {
            statement_name: "cached_after_error".into(),
        });

        let response = error_response();
        let reordered =
            client.reorder_parse_complete_responses(BytesMut::from(&response[..]), true);

        assert_eq!(
            reordered.as_ref(),
            &response[..],
            "Flush mode must not append ParseComplete for work PostgreSQL skipped after ErrorResponse"
        );
    }

    #[test]
    fn flush_tail_skipped_parse_is_emitted_after_prior_backend_response() {
        let mut client = test_client();
        client
            .prepared
            .batch_operations
            .push(BatchOperation::ParseSent {
                statement_name: "new_stmt".into(),
            });
        queue_skipped_parse(&mut client, "cached");

        let response = parse_complete();
        let reordered =
            client.reorder_parse_complete_responses(BytesMut::from(&response[..]), true);
        let expected = [parse_complete(), parse_complete()].concat();

        assert_eq!(
            reordered.as_ref(),
            &expected[..],
            "Flush without ReadyForQuery must still emit trailing cached ParseComplete \
             at the batch position"
        );
    }

    fn assert_skipped_parse_before_execute_terminal_response_is_emitted_before(
        terminal: Vec<u8>,
        label: &str,
    ) {
        let mut client = test_client();
        queue_skipped_parse(&mut client, "cached");
        client
            .prepared
            .batch_operations
            .push(BatchOperation::Execute);

        let response = [terminal.clone(), ready_for_query()].concat();
        let reordered =
            client.reorder_parse_complete_responses(BytesMut::from(&response[..]), false);
        let expected = [parse_complete(), terminal, ready_for_query()].concat();

        assert_eq!(
            reordered.as_ref(),
            &expected[..],
            "cached ParseComplete must precede Execute terminal response {label}"
        );
    }

    #[test]
    fn skipped_parse_before_execute_command_complete_is_emitted_before_command_complete() {
        assert_skipped_parse_before_execute_terminal_response_is_emitted_before(
            command_complete(b"UPDATE 0"),
            "CommandComplete",
        );
    }

    #[test]
    fn skipped_parse_before_execute_empty_query_is_emitted_before_empty_query() {
        assert_skipped_parse_before_execute_terminal_response_is_emitted_before(
            empty_query_response(),
            "EmptyQueryResponse",
        );
    }

    #[test]
    fn skipped_parse_before_execute_portal_suspended_is_emitted_before_portal_suspended() {
        assert_skipped_parse_before_execute_terminal_response_is_emitted_before(
            portal_suspended(),
            "PortalSuspended",
        );
    }

    fn assert_skipped_parse_before_describe_portal_response_is_emitted_before(
        description: Vec<u8>,
        label: &str,
    ) {
        let mut client = test_client();
        queue_skipped_parse(&mut client, "cached");
        client
            .prepared
            .batch_operations
            .push(BatchOperation::DescribePortal);

        let response = [description.clone(), ready_for_query()].concat();
        let reordered =
            client.reorder_parse_complete_responses(BytesMut::from(&response[..]), false);
        let expected = [parse_complete(), description, ready_for_query()].concat();

        assert_eq!(
            reordered.as_ref(),
            &expected[..],
            "cached ParseComplete must precede Describe Portal {label}"
        );
    }

    #[test]
    fn skipped_parse_before_describe_portal_row_description_is_emitted_before_row_description() {
        assert_skipped_parse_before_describe_portal_response_is_emitted_before(
            row_description(),
            "RowDescription",
        );
    }

    #[test]
    fn skipped_parse_before_describe_portal_no_data_is_emitted_before_no_data() {
        assert_skipped_parse_before_describe_portal_response_is_emitted_before(no_data(), "NoData");
    }
}
