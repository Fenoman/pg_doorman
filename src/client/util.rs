use bytes::{Buf, BytesMut};
use once_cell::sync::Lazy;
use std::io::Cursor;
use std::sync::{atomic::AtomicUsize, Arc};

use crate::messages::BytesMutReader;

/// Incrementally count prepared statements
/// to avoid random conflicts in places where the random number generator is weak.
pub static PREPARED_STATEMENT_COUNTER: Lazy<Arc<AtomicUsize>> =
    Lazy::new(|| Arc::new(AtomicUsize::new(0)));

// Ignore deallocate queries from pgx.
pub(crate) static QUERY_DEALLOCATE: &[u8] = "deallocate ".as_bytes();

/// Size of Q message containing "begin;" or "BEGIN;"
/// Format: [Q:1][length:4][query:6][null:1] = 12 bytes
const BEGIN_MSG_LEN: usize = 12;

/// Checks if the message is a standalone BEGIN query (simple query protocol).
/// Micro-optimization: first checks message size (12 bytes), then content.
///
/// Q message format:
/// - Byte 0: 'Q' (0x51)
/// - Bytes 1-4: length in big-endian (11 = 4 + 6 + 1)
/// - Bytes 5-10: "begin;" or "BEGIN;"
/// - Byte 11: null terminator (0x00)
#[inline]
pub(crate) fn is_standalone_begin(message: &BytesMut) -> bool {
    // Fast path: check size first
    if message.len() != BEGIN_MSG_LEN || message[0] != b'Q' {
        return false;
    }

    // Bytes 5-10 contain "begin;" (without null terminator)
    let query = &message[5..11];
    query.eq_ignore_ascii_case(b"begin;")
}

const DISCARD_QUERY_MAX_LEN: usize = 64;

#[inline(always)]
pub(crate) fn contains_discard_all(bytes: &[u8]) -> bool {
    let len = bytes.len().min(DISCARD_QUERY_MAX_LEN);
    let mut idx = 0;
    while idx < len {
        while idx < len && (bytes[idx].is_ascii_whitespace() || bytes[idx] == b';') {
            idx += 1;
        }
        if idx >= len {
            break;
        }

        let start = idx;
        while idx < len && bytes[idx] != b';' {
            idx += 1;
        }
        let end = idx.min(len);

        if is_discard_all_statement(&bytes[start..end]) {
            return true;
        }

        if idx < len && bytes[idx] == b';' {
            idx += 1;
        }
    }

    false
}

#[inline(always)]
fn is_discard_all_statement(stmt: &[u8]) -> bool {
    let mut idx = 0;
    skip_ascii_whitespace(stmt, &mut idx);
    if !consume_keyword(stmt, &mut idx, b"DISCARD") {
        return false;
    }

    skip_ascii_whitespace(stmt, &mut idx);
    if !consume_keyword(stmt, &mut idx, b"ALL") {
        return false;
    }

    skip_ascii_whitespace(stmt, &mut idx);
    while idx < stmt.len() {
        let ch = stmt[idx];
        if ch.is_ascii_whitespace() {
            idx += 1;
            continue;
        }
        if ch == b'-' && idx + 1 < stmt.len() && stmt[idx + 1] == b'-' {
            return true;
        }
        if ch == b'/' && idx + 1 < stmt.len() && stmt[idx + 1] == b'*' {
            return true;
        }
        return false;
    }

    true
}

#[inline(always)]
fn skip_ascii_whitespace(bytes: &[u8], idx: &mut usize) {
    while *idx < bytes.len() && bytes[*idx].is_ascii_whitespace() {
        *idx += 1;
    }
}

#[inline(always)]
fn consume_keyword(bytes: &[u8], idx: &mut usize, keyword: &[u8]) -> bool {
    for expected in keyword {
        if *idx >= bytes.len() {
            return false;
        }
        if bytes[*idx].to_ascii_uppercase() != *expected {
            return false;
        }
        *idx += 1;
    }
    true
}

#[inline(always)]
pub(crate) fn simple_query_body(message: &BytesMut) -> &[u8] {
    if message.len() <= 6 {
        return &[];
    }
    let end = message.len().saturating_sub(1);
    &message[5..end]
}

#[inline(always)]
pub(crate) fn parse_execute_portal(message: &BytesMut) -> Option<String> {
    if message.len() < 6 {
        return None;
    }
    let mut cursor = Cursor::new(message);
    if cursor.get_u8() as char != 'E' {
        return None;
    }
    let _len = cursor.get_i32();
    cursor.read_string().ok()
}
