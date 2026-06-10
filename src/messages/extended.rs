use std::collections::hash_map::DefaultHasher;
use std::ffi::CString;
use std::hash::Hasher;
use std::mem;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use bytes::{Buf, BufMut, BytesMut};
use xxhash_rust::xxh3::Xxh3;
use zerocopy::IntoBytes;

use crate::client::PREPARED_STATEMENT_COUNTER;
use crate::errors::Error;

pub const MAX_PARSE_NAME_BYTES: usize = 1024;
pub const MAX_PARSE_QUERY_BYTES: usize = 64 * 1024;

/// convert `usize` message length to wire `i32`, returning
/// `Err` if the value would not fit. Without this, a `len > i32::MAX`
/// (≈2.1 GiB) silently wraps to a negative integer, the on-wire length
/// prefix is invalid, and the backend desyncs the entire pipelined
/// session. Inputs are bounded by `MAX_MESSAGE_SIZE` on ingest, but
/// re-serialization paths (DISCARD-ALL rewrite, prepared-statement
/// rename) can compound - defensive guard.
#[inline]
fn checked_msg_len_i32(len: usize) -> Result<i32, Error> {
    i32::try_from(len).map_err(|_| {
        Error::ParseBytesError(format!(
            "outbound message length {len} bytes exceeds i32::MAX"
        ))
    })
}

fn find_limited_nul(remaining: &[u8], field: &str, max_len: usize) -> Result<usize, Error> {
    let window = &remaining[..remaining.len().min(max_len + 1)];
    match window.iter().position(|&byte| byte == b'\0') {
        Some(nul_pos) => Ok(nul_pos),
        None if remaining.len() > max_len => Err(Error::ParseBytesError(format!(
            "{field} length exceeds limit {max_len}"
        ))),
        None => Err(Error::ParseBytesError(format!("Unterminated {field}"))),
    }
}

/// Read a length-limited, nul-terminated **name** field (statement or portal
/// name) from an extended-protocol message, rejecting an over-`max_len` name
/// *before* allocating it or scanning the rest of an oversized frame. `field`
/// carries the message-type-qualified context (e.g. `"Parse statement name"`,
/// `"Bind portal name"`).
///
/// every extended-protocol name a client sends is cloned into
/// prepared-statement-cache keys and echoed in ErrorResponse / debug logs,
/// some of it on the cache-miss path *while a backend is checked out*. Without
/// a cap a hostile authenticated client can send a name close to
/// `MAX_MESSAGE_SIZE` (256 MiB), forcing several full-size heap clones per
/// message. PostgreSQL identifiers are <= NAMEDATALEN (63 bytes), so the shared
/// `MAX_PARSE_NAME_BYTES` (1024) cap is far above any legitimate name while
/// bounding the amplification.
fn read_limited_parse_string(
    cursor: &mut std::io::Cursor<&BytesMut>,
    field: &str,
    max_len: usize,
) -> Result<String, Error> {
    let start = usize::try_from(cursor.position())
        .map_err(|_| Error::ParseBytesError(format!("{field} position does not fit usize")))?;
    let buf = cursor.get_ref();
    let Some(remaining) = buf.get(start..) else {
        return Err(Error::ParseBytesError(format!(
            "{field} starts past end of message"
        )));
    };
    let nul_pos = find_limited_nul(remaining, field, max_len)?;
    let end = start + nul_pos;
    let value = std::str::from_utf8(&buf[start..end])
        .map_err(|err| Error::ParseBytesError(format!("{field} invalid utf8: {err}")))?
        .to_string();
    cursor.set_position((end + 1) as u64);
    Ok(value)
}

/// Same bounded scan/limit/UTF-8/terminator semantics as
/// [`read_limited_parse_string`], but builds the result as `Arc<str>` directly
/// from the borrowed buffer slice. This avoids materializing an intermediate
/// `String` and then copying it again into `Arc<str>` (two allocations + two
/// copies of a multi-KB query); `Arc::<str>::from(&str)` performs a single
/// allocation.
fn read_limited_parse_arc_str(
    cursor: &mut std::io::Cursor<&BytesMut>,
    field: &str,
    max_len: usize,
) -> Result<Arc<str>, Error> {
    let start = usize::try_from(cursor.position())
        .map_err(|_| Error::ParseBytesError(format!("{field} position does not fit usize")))?;
    let buf = cursor.get_ref();
    let Some(remaining) = buf.get(start..) else {
        return Err(Error::ParseBytesError(format!(
            "{field} starts past end of message"
        )));
    };
    let nul_pos = find_limited_nul(remaining, field, max_len)?;
    let end = start + nul_pos;
    let value: Arc<str> = Arc::from(
        std::str::from_utf8(&buf[start..end])
            .map_err(|err| Error::ParseBytesError(format!("{field} invalid utf8: {err}")))?,
    );
    cursor.set_position((end + 1) as u64);
    Ok(value)
}

/// Find the nul terminator of an extended-protocol name field in `data`,
/// scanning **at most** `max_len + 1` bytes starting at `from`, and return the
/// absolute index of the terminator.
///
/// the lightweight `Bind::get_name` / `Bind::get_portal_str`
/// scanners run on the prepared-statement-cache-miss path *while a backend is
/// checked out*, and their result is cloned into the cache key and echoed in
/// ErrorResponse / debug logs. Capping the scan window (rather than scanning
/// the whole - possibly 256 MiB - message) rejects an over-length name before
/// the slice/clone, bounding the amplification. `MAX_PARSE_NAME_BYTES` (1024)
/// is far above any legitimate name (PostgreSQL NAMEDATALEN is 63).
fn find_capped_name_nul(
    data: &[u8],
    from: usize,
    field: &str,
    max_len: usize,
) -> Result<usize, Error> {
    let Some(rest) = data.get(from..) else {
        return Err(Error::ParseBytesError(format!(
            "Bind: {field} starts past end of message"
        )));
    };
    let window = &rest[..rest.len().min(max_len + 1)];
    match window.iter().position(|&b| b == 0) {
        Some(off) => Ok(from + off),
        None if rest.len() > max_len => Err(Error::ParseBytesError(format!(
            "Bind {field} length exceeds limit {max_len}"
        ))),
        None => Err(Error::ParseBytesError(format!(
            "Bind: missing {field} null terminator"
        ))),
    }
}

/// Extended protocol data enum for different message types.
pub enum ExtendedProtocolData {
    Parse {
        data: BytesMut,
        metadata: Option<(Arc<Parse>, u64)>,
    },
    Bind {
        data: BytesMut,
        metadata: Option<String>,
    },
    Describe {
        data: BytesMut,
        metadata: Option<String>,
    },
    Execute {
        data: BytesMut,
    },
    Close {
        data: BytesMut,
        close: Close,
    },
}

impl ExtendedProtocolData {
    pub fn create_new_parse(data: BytesMut, metadata: Option<(Arc<Parse>, u64)>) -> Self {
        Self::Parse { data, metadata }
    }

    pub fn create_new_bind(data: BytesMut, metadata: Option<String>) -> Self {
        Self::Bind { data, metadata }
    }

    pub fn create_new_describe(data: BytesMut, metadata: Option<String>) -> Self {
        Self::Describe { data, metadata }
    }

    pub fn create_new_execute(data: BytesMut) -> Self {
        Self::Execute { data }
    }

    pub fn create_new_close(data: BytesMut, close: Close) -> Self {
        Self::Close { data, close }
    }
}

/// Parse (F) message.
/// See: <https://www.postgresql.org/docs/current/protocol-message-formats.html>
#[derive(Clone, Debug)]
pub struct Parse {
    code: char,
    #[allow(dead_code)]
    len: i32,
    pub name: String,
    /// Query text stored as Arc<str> for efficient sharing between clients.
    /// Even when Arc<Parse> is evicted from pool cache, the query text remains shared.
    query: Arc<str>,
    num_params: i16,
    param_types: Vec<i32>,
}

impl TryFrom<&BytesMut> for Parse {
    type Error = Error;

    fn try_from(buf: &BytesMut) -> Result<Parse, Error> {
        let mut cursor = std::io::Cursor::new(buf);
        if cursor.remaining() < 5 {
            return Err(Error::ParseBytesError(
                "Parse message too short for header".to_string(),
            ));
        }
        let code = cursor.get_u8() as char;
        let len = cursor.get_i32();
        let name =
            read_limited_parse_string(&mut cursor, "Parse statement name", MAX_PARSE_NAME_BYTES)?;
        // Build the query Arc<str> in one allocation directly from the buffer
        // slice, instead of read_limited_parse_string -> String -> Arc::from
        // (two allocations + two copies of a multi-KB query).
        let query = read_limited_parse_arc_str(&mut cursor, "Parse query", MAX_PARSE_QUERY_BYTES)?;
        if cursor.remaining() < 2 {
            return Err(Error::ParseBytesError(
                "Parse message truncated before num_params".to_string(),
            ));
        }
        let num_params = cursor.get_i16();
        // a hostile authenticated client can send `num_params = i16::MIN`.
        // The cast `num_params as usize` in `TryFrom<Parse> for BytesMut`
        // would sign-extend to a huge value and overflow length arithmetic.
        // Reject negative counts at decode time.
        if num_params < 0 {
            return Err(Error::ParseBytesError(format!(
                "Parse declared negative num_params: {num_params}"
            )));
        }
        // Each param_type is 4 bytes; bound-check before the read loop so
        // `Buf::get_i32` cannot panic on EOF.
        let needed = (num_params as usize) * 4;
        if cursor.remaining() < needed {
            return Err(Error::ParseBytesError(format!(
                "Parse truncated: declared {num_params} param_types but only {} bytes remain",
                cursor.remaining()
            )));
        }
        let mut param_types = Vec::with_capacity(num_params as usize);
        for _ in 0..num_params {
            param_types.push(cursor.get_i32());
        }
        if cursor.remaining() != 0 {
            return Err(Error::ParseBytesError(format!(
                "Parse trailing bytes after param_types: {}",
                cursor.remaining()
            )));
        }

        Ok(Parse {
            code,
            len,
            name,
            query,
            num_params,
            param_types,
        })
    }
}

impl TryFrom<Parse> for BytesMut {
    type Error = Error;

    fn try_from(parse: Parse) -> Result<BytesMut, Error> {
        let name_binding = CString::new(parse.name)?;
        let name = name_binding.as_bytes_with_nul();

        let query_binding = CString::new(&*parse.query)?;
        let query = query_binding.as_bytes_with_nul();

        // Recompute length of the message.
        let len = 4 // self
            + name.len()
            + query.len()
            + 2
            + 4 * parse.num_params as usize;

        // Pre-size to the exact frame length (1 code byte + the
        // self-describing `len`) so the multi-KB query is copied once with
        // no growth reallocations. Wire output is unchanged.
        let mut bytes = BytesMut::with_capacity(1 + len);
        bytes.put_u8(parse.code as u8);
        bytes.put_i32(checked_msg_len_i32(len)?);
        bytes.put_slice(name);
        bytes.put_slice(query);
        bytes.put_i16(parse.num_params);
        for param in parse.param_types {
            bytes.put_i32(param);
        }

        Ok(bytes)
    }
}

impl TryFrom<&Parse> for BytesMut {
    type Error = Error;

    fn try_from(parse: &Parse) -> Result<BytesMut, Error> {
        // Serialize directly against the borrowed &Parse. The previous
        // `parse.clone().try_into()` deep-copied the name String, the
        // Arc<str> query and the param_types Vec only to read them.
        // `to_bytes_with_name(&parse.name)` produces byte-identical output
        // (same code, len, field order) with no clone.
        parse.to_bytes_with_name(&parse.name)
    }
}

impl Parse {
    /// Renames the prepared statement to a new name based on the global counter
    ///
    /// The counter is monotonic and never compared against another atomic for
    /// ordering, so `Relaxed` is sufficient. The name is built with
    /// `String::with_capacity` plus a manual decimal formatter that uses a
    /// stack buffer, so the integer part does not allocate on the heap.
    pub fn rewrite(mut self) -> Self {
        const PREFIX: &str = "DOORMAN_";
        let counter = PREPARED_STATEMENT_COUNTER.fetch_add(1, Ordering::Relaxed);
        // u64::MAX is 20 decimal digits.
        let mut digit_buf = [0u8; 20];
        let mut pos = digit_buf.len();
        let mut n = counter as u64;
        if n == 0 {
            pos -= 1;
            digit_buf[pos] = b'0';
        } else {
            while n > 0 {
                pos -= 1;
                digit_buf[pos] = b'0' + (n % 10) as u8;
                n /= 10;
            }
        }
        let digits = &digit_buf[pos..];
        let mut name = String::with_capacity(PREFIX.len() + digits.len());
        name.push_str(PREFIX);
        // SAFETY: digits are ASCII '0'-'9' produced by the loop above.
        name.push_str(unsafe { std::str::from_utf8_unchecked(digits) });
        self.name = name;
        self
    }

    /// Interns the query string into the matching interner half. `is_anonymous`
    /// routes the text into the anonymous interner (TTL-bounded) or the named
    /// interner (passive `strong_count` GC). Should be called after computing
    /// the hash.
    pub fn intern_query(mut self, hash: u64, is_anonymous: bool) -> Self {
        use crate::server::intern_query;
        self.query = intern_query(&self.query, hash, is_anonymous);
        self
    }

    /// Gets the name of the prepared statement from the buffer
    pub fn get_name(buf: &BytesMut) -> Result<String, Error> {
        let mut cursor = std::io::Cursor::new(buf);
        // Skip the code and length
        cursor.advance(mem::size_of::<u8>() + mem::size_of::<i32>());
        // cap the name before allocation, same as Parse::try_from.
        read_limited_parse_string(&mut cursor, "Parse statement name", MAX_PARSE_NAME_BYTES)
    }

    /// Hashes the parse statement to be used as a key in the global cache
    pub fn get_hash(&self) -> u64 {
        if self.query.len() >= 64 {
            let mut hasher = Xxh3::default();

            hasher.write(self.query.as_bytes());
            hasher.write_i16(self.num_params);
            hasher.write(self.param_types.as_slice().as_bytes());

            hasher.finish()
        } else {
            // in benchmarks default hasher was better on short strings.
            let mut hasher = DefaultHasher::new();

            hasher.write(self.query.as_bytes());
            hasher.write_i16(self.num_params);
            hasher.write(self.param_types.as_slice().as_bytes());

            hasher.finish()
        }
    }

    /// Cache-key variant that includes a precomputed planner-state hash.
    /// Different startup-time planner GUCs must produce different
    /// server-side `DOORMAN_N` names for the same query text.
    ///
    /// `planner_param_hash == 0` collapses to the legacy `get_hash`
    /// result for callers with no planner state to add.
    pub fn get_hash_with_planner_params(&self, planner_param_hash: u64) -> u64 {
        if planner_param_hash == 0 {
            return self.get_hash();
        }
        if self.query.len() >= 64 {
            let mut hasher = Xxh3::default();
            hasher.write(self.query.as_bytes());
            hasher.write_i16(self.num_params);
            hasher.write(self.param_types.as_slice().as_bytes());
            hasher.write_u64(planner_param_hash);
            hasher.finish()
        } else {
            let mut hasher = DefaultHasher::new();
            hasher.write(self.query.as_bytes());
            hasher.write_i16(self.num_params);
            hasher.write(self.param_types.as_slice().as_bytes());
            hasher.write_u64(planner_param_hash);
            hasher.finish()
        }
    }

    pub fn anonymous(&self) -> bool {
        self.name.is_empty()
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn param_types(&self) -> &[i32] {
        &self.param_types
    }

    /// Replace the Parse's query text with a different SQL string. Preserves
    /// the prepared-statement name, declared parameter count, and parameter
    /// types - caller is responsible for ensuring the substitute query is
    /// shape-compatible with the original (same arity, same parameter types).
    ///
    /// Used by the extended-protocol `DISCARD ALL` interception path
    /// A client `Parse` carrying
    /// `DISCARD ALL` is rewritten to a no-op like `SELECT 1` so the
    /// backend's prepared-statement cache + planner state are preserved
    /// while the client still gets a valid `ParseComplete + BindComplete +
    /// CommandComplete + ReadyForQuery` flow from the real backend. The
    /// caller checks `num_params == 0` before invoking so the
    /// shape-compatibility precondition holds by construction (DISCARD ALL
    /// always declares zero parameters).
    pub fn with_replaced_query(mut self, new_query: &str) -> Self {
        self.query = Arc::from(new_query);
        self
    }

    /// Declared parameter count from the Parse message header. Used by the
    /// `DISCARD ALL` extended-protocol interception path to bail out if the
    /// client somehow declared parameters on a statement that should have
    /// none (shape mismatch with the substitute query).
    #[inline]
    pub fn num_params(&self) -> i16 {
        self.num_params
    }

    /// Construct a Parse from raw parts. Used during client migration
    /// to rebuild cache entries in the new process.
    pub fn from_parts(query: &str, param_types: &[i32]) -> Self {
        Parse {
            code: 'P',
            len: 0, // not used for cache registration
            name: String::new(),
            query: Arc::from(query),
            num_params: param_types.len() as i16,
            param_types: param_types.to_vec(),
        }
    }

    /// Approximate memory usage of the parse statement in bytes
    pub fn memory_usage(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.name.capacity()
            + self.query.len()  // Arc<str> doesn't have capacity(), use len()
            + self.param_types.capacity() * std::mem::size_of::<i32>()
    }

    /// Converts the Parse to bytes using a custom statement name.
    /// This is used for async clients that need unique names on the server.
    pub fn to_bytes_with_name(&self, name: &str) -> Result<BytesMut, Error> {
        let mut bytes = BytesMut::new();

        let name_binding = CString::new(name)?;
        let name_bytes = name_binding.as_bytes_with_nul();

        let query_binding = CString::new(&*self.query)?;
        let query_bytes = query_binding.as_bytes_with_nul();

        // Compute length of the message
        let len = 4 // self
            + name_bytes.len()
            + query_bytes.len()
            + 2
            + 4 * self.num_params as usize;

        bytes.put_u8(self.code as u8);
        bytes.put_i32(checked_msg_len_i32(len)?);
        bytes.put_slice(name_bytes);
        bytes.put_slice(query_bytes);
        bytes.put_i16(self.num_params);
        for param in &self.param_types {
            bytes.put_i32(*param);
        }

        Ok(bytes)
    }
}
/// See: <https://www.postgresql.org/docs/current/protocol-message-formats.html>
#[derive(Clone, Debug)]
pub struct Bind {
    code: char,
    #[allow(dead_code)]
    len: i64,
    portal: String,
    pub prepared_statement: String,
    num_param_format_codes: i16,
    param_format_codes: Vec<i16>,
    num_param_values: i16,
    param_values: Vec<(i32, BytesMut)>,
    num_result_column_format_codes: i16,
    result_columns_format_codes: Vec<i16>,
}

impl TryFrom<&BytesMut> for Bind {
    type Error = Error;

    fn try_from(buf: &BytesMut) -> Result<Bind, Error> {
        // every cursor read is bounds-checked. An authenticated client can
        // freely send malformed Bind messages; earlier `Buf::get_*` panicked
        // on EOF and `BytesMut::with_capacity(param_len as usize)` on negative
        // `param_len` attempted a multi-GB allocation. Validate counts and
        // remaining bytes at every step.
        let mut cursor = std::io::Cursor::new(buf);
        if cursor.remaining() < 5 {
            return Err(Error::ParseBytesError(
                "Bind message too short for header".to_string(),
            ));
        }
        let code = cursor.get_u8() as char;
        let len = cursor.get_i32();
        // cap both name fields before allocation (see
        // read_limited_parse_string).
        let portal =
            read_limited_parse_string(&mut cursor, "Bind portal name", MAX_PARSE_NAME_BYTES)?;
        let prepared_statement =
            read_limited_parse_string(&mut cursor, "Bind statement name", MAX_PARSE_NAME_BYTES)?;
        if cursor.remaining() < 2 {
            return Err(Error::ParseBytesError(
                "Bind truncated before num_param_format_codes".to_string(),
            ));
        }
        let num_param_format_codes = cursor.get_i16();
        if num_param_format_codes < 0 {
            return Err(Error::ParseBytesError(format!(
                "Bind declared negative num_param_format_codes: {num_param_format_codes}"
            )));
        }
        let needed = (num_param_format_codes as usize) * 2;
        if cursor.remaining() < needed {
            return Err(Error::ParseBytesError(format!(
                "Bind truncated: {num_param_format_codes} format codes but only {} bytes",
                cursor.remaining()
            )));
        }
        // cap with_capacity; cursor.remaining() is the
        // upper bound (each format code is 2 bytes).
        let fmt_cap = std::cmp::min(num_param_format_codes as usize, cursor.remaining() / 2);
        let mut param_format_codes = Vec::with_capacity(fmt_cap);
        for _ in 0..num_param_format_codes {
            param_format_codes.push(cursor.get_i16());
        }

        if cursor.remaining() < 2 {
            return Err(Error::ParseBytesError(
                "Bind truncated before num_param_values".to_string(),
            ));
        }
        let num_param_values = cursor.get_i16();
        if num_param_values < 0 {
            return Err(Error::ParseBytesError(format!(
                "Bind declared negative num_param_values: {num_param_values}"
            )));
        }
        // cap `with_capacity` to the maximum
        // realistic count given remaining buffer bytes. Each param
        // costs at least 4 bytes (length prefix); allocating
        // `i16::MAX` slots (32767) for a Bind that only has space
        // for a handful was a per-message ~1MB amplification.
        let cap_hint = std::cmp::min(num_param_values as usize, cursor.remaining() / 4);
        let mut param_values = Vec::with_capacity(cap_hint);

        for _ in 0..num_param_values {
            if cursor.remaining() < 4 {
                return Err(Error::ParseBytesError(
                    "Bind truncated before param length".to_string(),
                ));
            }
            let param_len = cursor.get_i32();
            if param_len == -1 {
                param_values.push((-1, BytesMut::new()));
            } else if param_len < -1 {
                return Err(Error::ParseBytesError(format!(
                    "Bind invalid param length: {param_len}"
                )));
            } else {
                let n = param_len as usize;
                if cursor.remaining() < n {
                    return Err(Error::ParseBytesError(format!(
                        "Bind truncated: param declared {n} bytes but only {} remain",
                        cursor.remaining()
                    )));
                }
                let mut param = BytesMut::with_capacity(n);
                // Zero-copy slice fetch instead of per-byte get_u8 (~40x for large params).
                let pos = cursor.position() as usize;
                param.extend_from_slice(&buf[pos..pos + n]);
                cursor.set_position((pos + n) as u64);
                param_values.push((param_len, param));
            }
        }

        if cursor.remaining() < 2 {
            return Err(Error::ParseBytesError(
                "Bind truncated before num_result_column_format_codes".to_string(),
            ));
        }
        let num_result_column_format_codes = cursor.get_i16();
        if num_result_column_format_codes < 0 {
            return Err(Error::ParseBytesError(format!(
                "Bind declared negative num_result_column_format_codes: {num_result_column_format_codes}"
            )));
        }
        let needed = (num_result_column_format_codes as usize) * 2;
        if cursor.remaining() < needed {
            return Err(Error::ParseBytesError(format!(
                "Bind truncated: {num_result_column_format_codes} result format codes but only {} bytes",
                cursor.remaining()
            )));
        }
        // same cap for result-column format codes.
        let rfmt_cap = std::cmp::min(
            num_result_column_format_codes as usize,
            cursor.remaining() / 2,
        );
        let mut result_columns_format_codes = Vec::with_capacity(rfmt_cap);
        for _ in 0..num_result_column_format_codes {
            result_columns_format_codes.push(cursor.get_i16());
        }

        // symmetry with related extended-protocol guards - Parse, Describe and
        // Close all reject trailing bytes after the last declared field;
        // Bind silently accepted them. The next `Bind::rename` recomputes
        // the body length from declared fields, so any trailing garbage
        // was dropped before reaching the backend - undetectable from
        // either side. A driver in a desynchronised state (asyncpg pre-
        // 0.27 codec bug under cancellation; fuzz harnesses) could land a
        // half-message in pg_doorman without ever surfacing the
        // discrepancy until a much later, unrelated frame failed.
        if cursor.remaining() != 0 {
            return Err(Error::ParseBytesError(format!(
                "Bind trailing bytes after result column format codes: {}",
                cursor.remaining()
            )));
        }

        Ok(Bind {
            code,
            len: len as i64,
            portal,
            prepared_statement,
            num_param_format_codes,
            param_format_codes,
            num_param_values,
            param_values,
            num_result_column_format_codes,
            result_columns_format_codes,
        })
    }
}

impl TryFrom<Bind> for BytesMut {
    type Error = Error;

    fn try_from(bind: Bind) -> Result<BytesMut, Error> {
        let mut bytes = BytesMut::new();

        let portal_binding = CString::new(bind.portal)?;
        let portal = portal_binding.as_bytes_with_nul();

        let prepared_statement_binding = CString::new(bind.prepared_statement)?;
        let prepared_statement = prepared_statement_binding.as_bytes_with_nul();

        let mut len = 4 // self
            + portal.len()
            + prepared_statement.len()
            + 2 // num_param_format_codes
            + 2 * bind.num_param_format_codes as usize // num_param_format_codes
            + 2; // num_param_values

        // NULL params carry `param_len == -1` (sentinel
        // from the parser). Casting `-1i32 as usize` earlier
        // wrapped to `usize::MAX` and produced a garbage length
        // prefix that desyncs the next backend frame. Use the
        // body slice length, only counting non-NULL params.
        for (param_len, param) in &bind.param_values {
            let body_len = if *param_len < 0 { 0 } else { param.len() };
            len += 4 + body_len;
        }
        len += 2; // num_result_column_format_codes
        len += 2 * bind.num_result_column_format_codes as usize;

        bytes.put_u8(bind.code as u8);
        bytes.put_i32(checked_msg_len_i32(len)?);
        bytes.put_slice(portal);
        bytes.put_slice(prepared_statement);
        bytes.put_i16(bind.num_param_format_codes);
        for param_format_code in bind.param_format_codes {
            bytes.put_i16(param_format_code);
        }
        bytes.put_i16(bind.num_param_values);
        for (param_len, param) in bind.param_values {
            bytes.put_i32(param_len);
            bytes.put_slice(&param);
        }
        bytes.put_i16(bind.num_result_column_format_codes);
        for result_column_format_code in bind.result_columns_format_codes {
            bytes.put_i16(result_column_format_code);
        }

        Ok(bytes)
    }
}

impl Bind {
    /// Gets the portal name as a borrowed string from the buffer.
    pub fn get_portal_str(buf: &BytesMut) -> Result<&str, Error> {
        let data = &buf[..];
        if data.len() < 5 {
            return Err(Error::ParseBytesError("Bind message too short".to_string()));
        }
        let header_end = 5;
        let portal_end =
            find_capped_name_nul(data, header_end, "portal name", MAX_PARSE_NAME_BYTES)?;
        std::str::from_utf8(&data[header_end..portal_end])
            .map_err(|err| Error::ParseBytesError(format!("Bind portal invalid utf8: {err}")))
    }

    /// Gets the portal name from the buffer.
    pub fn get_portal(buf: &BytesMut) -> Result<String, Error> {
        Self::get_portal_str(buf).map(|name| name.to_string())
    }

    /// Gets the name of the prepared statement from the buffer.
    pub fn get_name(buf: &BytesMut) -> Result<String, Error> {
        let data = &buf[..];
        if data.len() < 5 {
            return Err(Error::ParseBytesError("Bind message too short".to_string()));
        }
        let header_end = 5;
        let portal_end =
            find_capped_name_nul(data, header_end, "portal name", MAX_PARSE_NAME_BYTES)?;
        let stmt_start = portal_end + 1;
        let stmt_end =
            find_capped_name_nul(data, stmt_start, "statement name", MAX_PARSE_NAME_BYTES)?;
        std::str::from_utf8(&data[stmt_start..stmt_end])
            .map(|name| name.to_string())
            .map_err(|err| Error::ParseBytesError(format!("Bind statement invalid utf8: {err}")))
    }

    /// Renames the prepared statement to a new name.
    /// Zero-copy: scans for null terminators in-place, no String/Vec/CString allocations.
    pub fn rename(mut buf: BytesMut, new_name: &str) -> Result<BytesMut, Error> {
        // Bind format: [B][len:i32][portal\0][statement\0][...params...]
        if buf.len() < 5 {
            return Err(Error::ParseBytesError("Bind message too short".to_string()));
        }
        let header_end = 5;

        // Scope the immutable borrow so it ends before the F8 fast path can
        // index `buf` mutably. Everything carried out of the block is a
        // `usize`/`i32` copy, not a borrow into `buf`.
        let (portal_end, stmt_start, stmt_end, old_stmt_len, params_start, current_len) = {
            let data = &buf[..];

            let portal_end = data[header_end..]
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| {
                    Error::ParseBytesError("Bind: missing portal null terminator".to_string())
                })?
                + header_end;

            let stmt_start = portal_end + 1;
            let stmt_end = data[stmt_start..]
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| {
                    Error::ParseBytesError("Bind: missing statement null terminator".to_string())
                })?
                + stmt_start;

            let old_stmt_len = stmt_end - stmt_start;
            let params_start = stmt_end + 1;
            let current_len = i32::from_be_bytes([data[1], data[2], data[3], data[4]]);

            (
                portal_end,
                stmt_start,
                stmt_end,
                old_stmt_len,
                params_start,
                current_len,
            )
        };

        // Fast path for the common DOORMAN_<n> reuse where the
        // new server-side name has the same byte length as the old one. The
        // i32 length prefix is unchanged (new_len = current_len +
        // new_name.len() - old_stmt_len = current_len when lengths are
        // equal), and the portal prefix, the trailing NUL and the params
        // tail all keep their byte offsets. So we overwrite just the
        // statement-name bytes in the existing buffer and return it - no
        // realloc, no portal/tail copy. Byte-identical to the rebuild path
        // below (verified by bind_rename_byte_identical_*).
        if new_name.len() == old_stmt_len {
            buf[stmt_start..stmt_end].copy_from_slice(new_name.as_bytes());
            return Ok(buf);
        }

        let new_len = current_len + new_name.len() as i32 - old_stmt_len as i32;

        let data = &buf[..];
        let mut out = BytesMut::with_capacity(
            1 + 4
                + (portal_end - header_end + 1)
                + new_name.len()
                + 1
                + (data.len() - params_start),
        );
        out.put_u8(data[0]);
        out.put_i32(new_len);
        out.put_slice(&data[header_end..=portal_end]);
        out.put_slice(new_name.as_bytes());
        out.put_u8(0);
        out.put_slice(&data[params_start..]);

        Ok(out)
    }

    pub fn anonymous(&self) -> bool {
        self.prepared_statement.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct Describe {
    code: char,

    #[allow(dead_code)]
    len: i32,
    pub target: char,
    pub statement_name: String,
}

impl TryFrom<&BytesMut> for Describe {
    type Error = Error;

    fn try_from(bytes: &BytesMut) -> Result<Describe, Error> {
        // bounds-check sweep missed this site in C2.
        // Authenticated client can send `D 00 00 00 04` (5-byte
        // Describe claiming len=4, no body) which passes
        // `read_message_reuse` (len >= 4 gate) and reaches here;
        // unguarded `get_u8` / `get_i32` / `get_u8` panic on EOF.
        // Same shape as the Parse/Bind fixes - require header bytes
        // before each read.
        let mut cursor = std::io::Cursor::new(bytes);
        if cursor.remaining() < 6 {
            return Err(Error::ParseBytesError(
                "Describe message too short for header".to_string(),
            ));
        }
        let code = cursor.get_u8() as char;
        let len = cursor.get_i32();
        let target = cursor.get_u8() as char;
        // cap the name before allocation (see read_limited_parse_string).
        let statement_name = read_limited_parse_string(
            &mut cursor,
            "Describe statement name",
            MAX_PARSE_NAME_BYTES,
        )?;
        if cursor.remaining() != 0 {
            return Err(Error::ParseBytesError(format!(
                "Describe trailing bytes after name: {}",
                cursor.remaining()
            )));
        }

        Ok(Describe {
            code,
            len,
            target,
            statement_name,
        })
    }
}

impl TryFrom<Describe> for BytesMut {
    type Error = Error;

    fn try_from(describe: Describe) -> Result<BytesMut, Error> {
        let mut bytes = BytesMut::new();
        let statement_name_binding = CString::new(describe.statement_name)?;
        let statement_name = statement_name_binding.as_bytes_with_nul();
        let len = 4 + 1 + statement_name.len();

        bytes.put_u8(describe.code as u8);
        bytes.put_i32(checked_msg_len_i32(len)?);
        bytes.put_u8(describe.target as u8);
        bytes.put_slice(statement_name);

        Ok(bytes)
    }
}

impl Describe {
    pub fn empty_new() -> Describe {
        Describe {
            code: 'D',
            len: 4 + 1 + 1,
            target: 'S',
            statement_name: "".to_string(),
        }
    }

    pub fn rename(mut self, name: &str) -> Self {
        self.statement_name = name.to_string();
        self
    }

    pub fn anonymous(&self) -> bool {
        self.statement_name.is_empty()
    }
}

/// Close (F) message.
/// See: <https://www.postgresql.org/docs/current/protocol-message-formats.html>
#[derive(Clone, Debug)]
pub struct Close {
    code: char,
    #[allow(dead_code)]
    len: i32,
    close_type: char,
    pub name: String,
}

impl TryFrom<&BytesMut> for Close {
    type Error = Error;

    fn try_from(bytes: &BytesMut) -> Result<Close, Error> {
        // Same shape as Describe: guard header bytes before reading fields.
        let mut cursor = std::io::Cursor::new(bytes);
        if cursor.remaining() < 6 {
            return Err(Error::ParseBytesError(
                "Close message too short for header".to_string(),
            ));
        }
        let code = cursor.get_u8() as char;
        let len = cursor.get_i32();
        let close_type = cursor.get_u8() as char;
        // cap the name before allocation (see read_limited_parse_string).
        let name =
            read_limited_parse_string(&mut cursor, "Close statement name", MAX_PARSE_NAME_BYTES)?;
        if cursor.remaining() != 0 {
            return Err(Error::ParseBytesError(format!(
                "Close trailing bytes after name: {}",
                cursor.remaining()
            )));
        }

        Ok(Close {
            code,
            len,
            close_type,
            name,
        })
    }
}

impl TryFrom<Close> for BytesMut {
    type Error = Error;

    fn try_from(close: Close) -> Result<BytesMut, Error> {
        let mut bytes = BytesMut::new();
        let name_binding = CString::new(close.name)?;
        let name = name_binding.as_bytes_with_nul();
        let len = 4 + 1 + name.len();

        bytes.put_u8(close.code as u8);
        bytes.put_i32(checked_msg_len_i32(len)?);
        bytes.put_u8(close.close_type as u8);
        bytes.put_slice(name);

        Ok(bytes)
    }
}

impl Close {
    pub fn new(name: &str) -> Close {
        let name = name.to_string();

        Close {
            code: 'C',
            len: 4 + 1 + name.len() as i32 + 1, // will be recalculated
            close_type: 'S',
            name,
        }
    }

    pub fn is_prepared_statement(&self) -> bool {
        self.close_type == 'S'
    }

    pub fn is_portal(&self) -> bool {
        self.close_type == 'P'
    }

    pub fn anonymous(&self) -> bool {
        self.name.is_empty()
    }

    /// Rewrite a Close message's statement-name field on the wire to
    /// `new_name`, preserving the `'C'` tag, length prefix, and `close_type`
    /// byte. Returns the rewritten message.
    ///
    /// pg_doorman renames prepared statements at Parse time (the client-
    /// given name `stmt1` maps to a unique backend name like `DOORMAN_5` /
    /// `DOORMAN_async_42`). A client-issued `Close S "stmt1"` forwarded
    /// verbatim hits the backend as a Close for a name that does not exist,
    /// which PostgreSQL silently no-ops - the renamed prepared statement
    /// therefore stays cached on the backend until the per-server LRU
    /// evicts it. Re-writing the name here makes the backend actually
    /// drop its cached entry and keeps pg_doorman's per-server view in
    /// sync with reality.
    ///
    /// Wire format: `C` (1) + len (4) + close_type (1) + name (n) + `\0` (1).
    pub fn rename(buf: BytesMut, new_name: &str) -> Result<BytesMut, Error> {
        if buf.len() < 7 {
            return Err(Error::ParseBytesError(
                "Close message too short to rename".to_string(),
            ));
        }
        if buf[0] != b'C' {
            return Err(Error::ParseBytesError(format!(
                "Close::rename called on wrong message type: 0x{:02x}",
                buf[0]
            )));
        }
        // Locate the NUL that terminates the original name, starting after
        // header (1) + len (4) + close_type (1).
        let name_start = 6;
        let original_nul = buf[name_start..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| {
                Error::ParseBytesError("Close name missing nul terminator".to_string())
            })?
            + name_start;

        let close_type = buf[5];
        let new_name_bytes = new_name.as_bytes();
        let body_len = 4 // length prefix self
            + 1            // close_type
            + new_name_bytes.len()
            + 1; // nul terminator
        let total = 1 + body_len;
        let mut out = BytesMut::with_capacity(total);
        out.put_u8(b'C');
        out.put_i32(body_len as i32);
        out.put_u8(close_type);
        out.put_slice(new_name_bytes);
        out.put_u8(0);

        // If the input buffer carries trailing bytes after the Close
        // frame (rare but legal in pipelined batches), preserve them.
        let original_frame_end = original_nul + 1;
        if buf.len() > original_frame_end {
            out.extend_from_slice(&buf[original_frame_end..]);
        }
        Ok(out)
    }
}

/// Create a CloseComplete message.
pub fn close_complete() -> BytesMut {
    let mut bytes = BytesMut::new();
    bytes.put_u8(b'3');
    bytes.put_i32(4);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bind(portal: &str, statement: &str) -> BytesMut {
        let body_len = portal.len() + 1 + statement.len() + 1 + 2 + 2 + 2;
        let mut buf = BytesMut::with_capacity(1 + 4 + body_len);
        buf.put_u8(b'B');
        buf.put_i32((4 + body_len) as i32);
        buf.put_slice(portal.as_bytes());
        buf.put_u8(0);
        buf.put_slice(statement.as_bytes());
        buf.put_u8(0);
        buf.put_i16(0);
        buf.put_i16(0);
        buf.put_i16(0);
        buf
    }

    fn make_bind_raw(portal: &[u8], statement: &[u8]) -> BytesMut {
        let body_len = portal.len() + 1 + statement.len() + 1 + 2 + 2 + 2;
        let mut buf = BytesMut::with_capacity(1 + 4 + body_len);
        buf.put_u8(b'B');
        buf.put_i32((4 + body_len) as i32);
        buf.put_slice(portal);
        buf.put_u8(0);
        buf.put_slice(statement);
        buf.put_u8(0);
        buf.put_i16(0);
        buf.put_i16(0);
        buf.put_i16(0);
        buf
    }

    fn make_describe_raw(target: u8, name: &[u8]) -> BytesMut {
        let body_len = 1 + name.len() + 1;
        let mut buf = BytesMut::with_capacity(1 + 4 + body_len);
        buf.put_u8(b'D');
        buf.put_i32((4 + body_len) as i32);
        buf.put_u8(target);
        buf.put_slice(name);
        buf.put_u8(0);
        buf
    }

    fn make_close_raw(close_type: u8, name: &[u8]) -> BytesMut {
        let body_len = 1 + name.len() + 1;
        let mut buf = BytesMut::with_capacity(1 + 4 + body_len);
        buf.put_u8(b'C');
        buf.put_i32((4 + body_len) as i32);
        buf.put_u8(close_type);
        buf.put_slice(name);
        buf.put_u8(0);
        buf
    }

    fn make_bind_with_params(portal: &str, statement: &str, params: &[&[u8]]) -> BytesMut {
        let params_size: usize = params.iter().map(|p| 4 + p.len()).sum();
        let body_len = portal.len() + 1 + statement.len() + 1 + 2 + 2 + params_size + 2;
        let mut buf = BytesMut::with_capacity(1 + 4 + body_len);
        buf.put_u8(b'B');
        buf.put_i32((4 + body_len) as i32);
        buf.put_slice(portal.as_bytes());
        buf.put_u8(0);
        buf.put_slice(statement.as_bytes());
        buf.put_u8(0);
        buf.put_i16(0);
        buf.put_i16(params.len() as i16);
        for p in params {
            buf.put_i32(p.len() as i32);
            buf.put_slice(p);
        }
        buf.put_i16(0);
        buf
    }

    #[test]
    fn test_bind_get_name_named() {
        let buf = make_bind("", "my_stmt");
        assert_eq!(Bind::get_name(&buf).unwrap(), "my_stmt");
    }

    #[test]
    fn test_bind_get_name_anonymous() {
        let buf = make_bind("", "");
        assert_eq!(Bind::get_name(&buf).unwrap(), "");
    }

    #[test]
    fn test_bind_get_name_with_portal() {
        let buf = make_bind("portal1", "stmt1");
        assert_eq!(Bind::get_name(&buf).unwrap(), "stmt1");
    }

    #[test]
    fn bind_get_name_rejects_invalid_utf8_statement_name() {
        let buf = make_bind_raw(b"", b"stmt_\xff");
        assert!(Bind::get_name(&buf).is_err());
    }

    // extended-protocol statement/portal names are capped at
    // MAX_PARSE_NAME_BYTES *before* allocation so a hostile authenticated client
    // cannot force full-size heap clones (cache key + ErrorResponse/debug echo)
    // on the prepared-statement-cache-miss path while a backend is checked out.
    #[test]
    fn bind_get_name_rejects_oversized_statement_name() {
        let big = vec![b'x'; MAX_PARSE_NAME_BYTES + 1];
        let buf = make_bind_raw(b"", &big);
        let err = Bind::get_name(&buf).unwrap_err();
        assert!(
            err.to_string()
                .contains("statement name length exceeds limit"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bind_get_name_rejects_oversized_portal_name() {
        let big = vec![b'x'; MAX_PARSE_NAME_BYTES + 1];
        let buf = make_bind_raw(&big, b"stmt");
        let err = Bind::get_name(&buf).unwrap_err();
        assert!(
            err.to_string().contains("portal name length exceeds limit"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bind_get_portal_str_rejects_oversized_portal_name() {
        let big = vec![b'x'; MAX_PARSE_NAME_BYTES + 1];
        let buf = make_bind_raw(&big, b"stmt");
        assert!(Bind::get_portal_str(&buf).is_err());
    }

    #[test]
    fn bind_get_name_accepts_limit_sized_names() {
        let portal = vec![b'p'; MAX_PARSE_NAME_BYTES];
        let stmt = vec![b's'; MAX_PARSE_NAME_BYTES];
        let buf = make_bind_raw(&portal, &stmt);
        assert_eq!(Bind::get_name(&buf).unwrap().len(), MAX_PARSE_NAME_BYTES);
        assert_eq!(
            Bind::get_portal_str(&buf).unwrap().len(),
            MAX_PARSE_NAME_BYTES
        );
    }

    #[test]
    fn bind_try_from_rejects_oversized_portal_name() {
        let big = vec![b'x'; MAX_PARSE_NAME_BYTES + 1];
        let buf = make_bind_raw(&big, b"stmt");
        let result: Result<Bind, _> = (&buf).try_into();
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Bind portal name length"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn describe_rejects_oversized_statement_name() {
        let big = vec![b'x'; MAX_PARSE_NAME_BYTES + 1];
        let buf = make_describe_raw(b'S', &big);
        let err = Describe::try_from(&buf).unwrap_err();
        assert!(
            err.to_string().contains("Describe statement name length"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn describe_accepts_limit_sized_statement_name() {
        let name = vec![b's'; MAX_PARSE_NAME_BYTES];
        let buf = make_describe_raw(b'S', &name);
        let describe = Describe::try_from(&buf).unwrap();
        assert_eq!(describe.statement_name.len(), MAX_PARSE_NAME_BYTES);
    }

    #[test]
    fn close_rejects_oversized_statement_name() {
        let big = vec![b'x'; MAX_PARSE_NAME_BYTES + 1];
        let buf = make_close_raw(b'S', &big);
        let err = Close::try_from(&buf).unwrap_err();
        assert!(
            err.to_string().contains("Close statement name length"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bind_try_from_rejects_invalid_utf8_portal_name() {
        let buf = make_bind_raw(b"portal_\xff", b"stmt");
        let result: Result<Bind, _> = (&buf).try_into();
        assert!(result.is_err());
    }

    #[test]
    fn bind_try_from_rejects_invalid_utf8_statement_name() {
        let buf = make_bind_raw(b"", b"stmt_\xff");
        let result: Result<Bind, _> = (&buf).try_into();
        assert!(result.is_err());
    }

    #[test]
    fn describe_rejects_invalid_utf8_statement_name() {
        let buf = make_describe_raw(b'S', b"stmt_\xff");
        let result: Result<Describe, _> = (&buf).try_into();
        assert!(result.is_err());
    }

    #[test]
    fn describe_rejects_trailing_bytes_after_name() {
        let mut buf = make_describe_raw(b'S', b"stmt");
        buf.put_u8(0xAA);

        let err = Describe::try_from(&buf).unwrap_err();
        assert!(
            err.to_string().contains("Describe trailing bytes"),
            "unexpected error for malformed Describe with trailing bytes: {err}"
        );
    }

    #[test]
    fn close_rejects_invalid_utf8_statement_name() {
        let buf = make_close_raw(b'S', b"stmt_\xff");
        let result: Result<Close, _> = (&buf).try_into();
        assert!(result.is_err());
    }

    #[test]
    fn close_rejects_trailing_bytes_after_name() {
        let mut buf = make_close_raw(b'S', b"stmt");
        buf.put_u8(0xAA);

        let err = Close::try_from(&buf).unwrap_err();
        assert!(
            err.to_string().contains("Close trailing bytes"),
            "unexpected error for malformed Close with trailing bytes: {err}"
        );
    }

    /// symmetry with the related Close guard Close check above.
    #[test]
    fn bind_rejects_trailing_bytes_after_result_format_codes() {
        let mut buf = make_bind("", "stmt");
        buf.put_u8(0xAA);

        let err = Bind::try_from(&buf).unwrap_err();
        assert!(
            err.to_string().contains("Bind trailing bytes"),
            "unexpected error for malformed Bind with trailing bytes: {err}"
        );
    }

    #[test]
    fn test_bind_rename_same_length() {
        let buf = make_bind("", "ABCD");
        let renamed = Bind::rename(buf, "WXYZ").unwrap();
        assert_eq!(Bind::get_name(&renamed).unwrap(), "WXYZ");
    }

    #[test]
    fn test_bind_rename_shorter_to_longer() {
        let buf = make_bind("", "a");
        let renamed = Bind::rename(buf, "DOORMAN_123").unwrap();
        assert_eq!(Bind::get_name(&renamed).unwrap(), "DOORMAN_123");
    }

    #[test]
    fn test_bind_rename_longer_to_shorter() {
        let buf = make_bind("", "very_long_statement_name");
        let renamed = Bind::rename(buf, "D0").unwrap();
        assert_eq!(Bind::get_name(&renamed).unwrap(), "D0");
    }

    #[test]
    fn test_bind_rename_anonymous_to_named() {
        let buf = make_bind("", "");
        let renamed = Bind::rename(buf, "DOORMAN_0").unwrap();
        assert_eq!(Bind::get_name(&renamed).unwrap(), "DOORMAN_0");
    }

    #[test]
    fn test_bind_rename_preserves_portal() {
        let buf = make_bind("my_portal", "old_stmt");
        let renamed = Bind::rename(buf, "new_stmt").unwrap();
        assert_eq!(Bind::get_name(&renamed).unwrap(), "new_stmt");
        let data = &renamed[5..];
        let portal_end = data.iter().position(|&b| b == 0).unwrap();
        assert_eq!(&data[..portal_end], b"my_portal");
    }

    #[test]
    fn test_bind_rename_preserves_params() {
        let params: &[&[u8]] = &[b"hello", b"world"];
        let buf = make_bind_with_params("", "old", params);
        let original_len = buf.len();
        let renamed = Bind::rename(buf, "DOORMAN_0").unwrap();
        assert_eq!(Bind::get_name(&renamed).unwrap(), "DOORMAN_0");
        let expected_len = original_len + "DOORMAN_0".len() - "old".len();
        assert_eq!(renamed.len(), expected_len);
    }

    #[test]
    fn test_bind_rename_message_length_field() {
        let buf = make_bind("", "abc");
        let old_len = i32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
        let renamed = Bind::rename(buf, "DOORMAN_XYZ").unwrap();
        let new_len = i32::from_be_bytes([renamed[1], renamed[2], renamed[3], renamed[4]]);
        assert_eq!(
            new_len - old_len,
            "DOORMAN_XYZ".len() as i32 - "abc".len() as i32
        );
    }

    #[test]
    fn test_bind_rename_roundtrip_via_tryfrom() {
        let params: &[&[u8]] = &[b"val1", b"val2", b"val3"];
        let buf = make_bind_with_params("p", "original_stmt", params);
        let renamed = Bind::rename(buf, "DOORMAN_42").unwrap();
        let bind: Bind = (&renamed).try_into().unwrap();
        assert_eq!(bind.prepared_statement, "DOORMAN_42");
        assert_eq!(bind.portal, "p");
        assert_eq!(bind.num_param_values, 3);
    }

    #[test]
    fn test_bind_get_name_too_short() {
        let buf = BytesMut::from(&[0u8; 3][..]);
        assert!(Bind::get_name(&buf).is_err());
    }

    #[test]
    fn test_bind_rename_too_short() {
        let buf = BytesMut::from(&[0u8; 3][..]);
        assert!(Bind::rename(buf, "test").is_err());
    }

    #[test]
    fn test_parse_from_parts_basic() {
        let p = Parse::from_parts("SELECT $1::int", &[23]);
        assert_eq!(p.query(), "SELECT $1::int");
        assert_eq!(p.param_types(), &[23]);
        assert!(p.anonymous()); // name is empty
    }

    #[test]
    fn test_parse_from_parts_no_params() {
        let p = Parse::from_parts("SELECT 1", &[]);
        assert_eq!(p.query(), "SELECT 1");
        assert_eq!(p.param_types(), &[] as &[i32]);
    }

    #[test]
    fn test_parse_from_parts_hash_deterministic() {
        let p1 = Parse::from_parts("SELECT $1", &[23]);
        let p2 = Parse::from_parts("SELECT $1", &[23]);
        assert_eq!(p1.get_hash(), p2.get_hash());

        let p3 = Parse::from_parts("SELECT $1", &[25]); // different param type
        assert_ne!(p1.get_hash(), p3.get_hash());
    }

    /// No planner state keeps the legacy prepared-cache key.
    #[test]
    fn get_hash_with_planner_params_zero_matches_legacy() {
        let p = Parse::from_parts("SELECT 1", &[23]);
        assert_eq!(p.get_hash(), p.get_hash_with_planner_params(0));
    }

    /// Non-zero planner state must change the prepared-cache key.
    #[test]
    fn get_hash_with_planner_params_nonzero_changes_digest() {
        let p = Parse::from_parts("SELECT 1", &[23]);
        let base = p.get_hash();
        assert_ne!(base, p.get_hash_with_planner_params(0x1234_5678_9ABC_DEF0));
    }

    /// Different planner states must not collide on the mixed key.
    #[test]
    fn get_hash_with_planner_params_distinct_for_distinct_planner_hashes() {
        let p = Parse::from_parts("SELECT 1", &[23]);
        let h_a = p.get_hash_with_planner_params(0x1111_1111_1111_1111);
        let h_b = p.get_hash_with_planner_params(0x2222_2222_2222_2222);
        assert_ne!(h_a, h_b);
    }

    /// Both hash implementations must include planner state.
    #[test]
    fn get_hash_with_planner_params_works_on_both_hasher_branches() {
        let short = Parse::from_parts("SELECT 1", &[]);
        let long_query = format!("SELECT {} FROM t", "a".repeat(80));
        let long = Parse::from_parts(&long_query, &[]);
        assert_ne!(
            short.get_hash(),
            short.get_hash_with_planner_params(0xDEAD_BEEF)
        );
        assert_ne!(
            long.get_hash(),
            long.get_hash_with_planner_params(0xDEAD_BEEF)
        );
    }

    /// Construct a Parse `BytesMut` with an explicit statement name. Used by
    /// the `with_replaced_query` round-trip tests below - `Parse::from_parts`
    /// only produces anonymous statements (empty name), but the
    /// extended-protocol DISCARD ALL interception must also work for named
    /// prepared statements (a client that does `Parse("ds", "DISCARD ALL")`
    /// then `Bind("ds")` later).
    fn make_parse(name: &str, query: &str, param_types: &[i32]) -> BytesMut {
        make_parse_raw(name.as_bytes(), query.as_bytes(), param_types)
    }

    fn make_parse_raw(name: &[u8], query: &[u8], param_types: &[i32]) -> BytesMut {
        let body_len = 4 // length field itself
            + name.len() + 1 // name + NUL
            + query.len() + 1 // query + NUL
            + 2 // num_params (i16)
            + 4 * param_types.len(); // param OIDs (i32 each)
        let mut buf = BytesMut::with_capacity(1 + body_len);
        buf.put_u8(b'P');
        buf.put_i32(body_len as i32);
        buf.put_slice(name);
        buf.put_u8(0);
        buf.put_slice(query);
        buf.put_u8(0);
        buf.put_i16(param_types.len() as i16);
        for pt in param_types {
            buf.put_i32(*pt);
        }
        buf
    }

    #[test]
    fn parse_rejects_invalid_utf8_statement_name_before_rewrite() {
        let buf = make_parse_raw(b"bad\xffname", b"SELECT 1", &[]);
        let err = Parse::try_from(&buf).unwrap_err();
        assert!(err.to_string().contains("statement name invalid utf8"));
    }

    #[test]
    fn parse_rejects_invalid_utf8_query_before_rewrite() {
        let buf = make_parse_raw(b"stmt", b"SELECT '\xff'", &[]);
        let err = Parse::try_from(&buf).unwrap_err();
        assert!(err.to_string().contains("query invalid utf8"));
    }

    #[test]
    fn parse_rejects_oversized_statement_name_before_cache() {
        let name = "n".repeat(MAX_PARSE_NAME_BYTES + 1);
        let buf = make_parse(&name, "SELECT 1", &[]);
        let err = Parse::try_from(&buf).unwrap_err();
        assert!(err.to_string().contains("statement name length"));
    }

    #[test]
    fn parse_rejects_oversized_query_before_cache() {
        let query = "x".repeat(MAX_PARSE_QUERY_BYTES + 1);
        let buf = make_parse("stmt", &query, &[]);
        let err = Parse::try_from(&buf).unwrap_err();
        assert!(err.to_string().contains("query length"));
    }

    #[test]
    fn parse_rejects_trailing_bytes_after_param_types() {
        let mut buf = make_parse("stmt", "SELECT $1::int", &[23]);
        buf.put_u8(0xAA);

        let err = Parse::try_from(&buf).unwrap_err();
        assert!(
            err.to_string().contains("trailing bytes"),
            "unexpected error for malformed Parse with trailing bytes: {err}"
        );
    }

    #[test]
    fn parse_accepts_limit_sized_name_and_query() {
        let name = "n".repeat(MAX_PARSE_NAME_BYTES);
        let query = "x".repeat(MAX_PARSE_QUERY_BYTES);
        let buf = make_parse(&name, &query, &[]);
        let parse = Parse::try_from(&buf).unwrap();
        assert_eq!(parse.name.len(), MAX_PARSE_NAME_BYTES);
        assert_eq!(parse.query().len(), MAX_PARSE_QUERY_BYTES);
    }

    #[test]
    fn with_replaced_query_preserves_anonymous_metadata() {
        let original = Parse::from_parts("DISCARD ALL", &[]);
        let rewritten = original.clone().with_replaced_query("SELECT 1");
        assert_eq!(rewritten.query(), "SELECT 1");
        assert_eq!(rewritten.name, original.name); // both empty
        assert_eq!(rewritten.num_params(), 0);
        assert_eq!(rewritten.param_types(), original.param_types());
    }

    #[test]
    fn with_replaced_query_preserves_named_statement_via_message_roundtrip() {
        // Named: client did Parse("ds", "DISCARD ALL", []) - extended-protocol
        // form a long-lived asyncpg/jdbc-style cleanup hook would take.
        let buf = make_parse("ds", "DISCARD ALL", &[]);
        let parse: Parse = (&buf).try_into().unwrap();
        assert_eq!(parse.name, "ds");
        assert_eq!(parse.query(), "DISCARD ALL");

        let rewritten = parse.with_replaced_query("SELECT 1");
        assert_eq!(rewritten.name, "ds", "name must survive");
        assert_eq!(rewritten.query(), "SELECT 1");
        assert_eq!(rewritten.num_params(), 0);

        // Round-trip the rewritten Parse back through bytes to confirm the
        // wire format is well-formed and re-parses identically - this is
        // the form that actually reaches the backend after pg_doorman
        // forwards it.
        let bytes: BytesMut = rewritten.clone().try_into().unwrap();
        let reparsed: Parse = (&bytes).try_into().unwrap();
        assert_eq!(reparsed.name, "ds");
        assert_eq!(reparsed.query(), "SELECT 1");
        assert_eq!(reparsed.num_params(), 0);
        assert_eq!(reparsed.param_types(), &[] as &[i32]);
    }

    #[test]
    fn with_replaced_query_changes_hash() {
        // Pool prepared-statement cache is keyed by query-text hash. After
        // the rewrite the cache entry must be under the NEW hash so the
        // pool doesn't collide a `DOORMAN_N` slot between the original
        // DISCARD ALL request and a real `SELECT 1` from elsewhere.
        let original = Parse::from_parts("DISCARD ALL", &[]);
        let rewritten = original.clone().with_replaced_query("SELECT 1");
        assert_ne!(
            original.get_hash(),
            rewritten.get_hash(),
            "hash must change so pool cache disambiguates entries"
        );
        // And it must be equal to a fresh SELECT 1 with the same param
        // shape - i.e. the rewrite is observationally identical to a
        // direct SELECT 1 Parse for cache purposes.
        let fresh = Parse::from_parts("SELECT 1", &[]);
        assert_eq!(fresh.get_hash(), rewritten.get_hash());
    }

    #[test]
    fn num_params_exposes_declared_count() {
        // The DISCARD ALL extended-protocol intercept gates on
        // `parse.num_params() == 0` to refuse rewriting any Parse that
        // declared non-zero parameters (shape mismatch with the
        // substitute query). Lock the accessor's contract here so a
        // future refactor doesn't silently break the gate.
        let zero = Parse::from_parts("SELECT 1", &[]);
        assert_eq!(zero.num_params(), 0);
        let one = Parse::from_parts("SELECT $1", &[23]);
        assert_eq!(one.num_params(), 1);
        let three = Parse::from_parts("SELECT $1, $2, $3", &[23, 25, 1043]);
        assert_eq!(three.num_params(), 3);
    }
}
