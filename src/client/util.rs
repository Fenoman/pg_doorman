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

/// Returns true only if the query body is standalone "DISCARD ALL"
/// (with optional surrounding whitespace and trailing semicolons).
/// Does NOT match multi-statement queries or DISCARD ALL inside other text.
#[inline(always)]
pub(crate) fn contains_discard_all(bytes: &[u8]) -> bool {
    let mut idx = 0;
    let len = bytes.len();
    // Skip leading whitespace
    while idx < len && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    if !consume_keyword(bytes, &mut idx, b"DISCARD") {
        return false;
    }
    // Skip whitespace between words
    if idx >= len || !bytes[idx].is_ascii_whitespace() {
        return false;
    }
    while idx < len && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    if !consume_keyword(bytes, &mut idx, b"ALL") {
        return false;
    }
    // After "ALL", only whitespace and semicolons allowed
    while idx < len {
        let ch = bytes[idx];
        if ch.is_ascii_whitespace() || ch == b';' {
            idx += 1;
            continue;
        }
        return false;
    }
    true
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

#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_discard_all() {
        assert!(contains_discard_all(b"DISCARD ALL"));
        assert!(contains_discard_all(b"discard all"));
        assert!(contains_discard_all(b"  DISCARD   ALL  "));
        assert!(contains_discard_all(b"DISCARD ALL;"));
        assert!(contains_discard_all(b"DISCARD ALL ; "));
    }

    #[test]
    fn test_multi_statement_not_matched() {
        // Multi-statement queries are NOT intercepted
        assert!(!contains_discard_all(b"DISCARD ALL; DISCARD ALL"));
        assert!(!contains_discard_all(b"DISCARD ALL;DISCARD ALL;"));
        assert!(!contains_discard_all(b"SELECT 1; DISCARD ALL"));
        assert!(!contains_discard_all(b"DISCARD ALL; SELECT 1"));
        assert!(!contains_discard_all(b"BEGIN; DISCARD ALL; COMMIT"));
    }

    #[test]
    fn test_comments_not_matched() {
        // Comments are NOT supported — our drivers send plain DISCARD ALL
        assert!(!contains_discard_all(b"-- app tag\nDISCARD ALL"));
        assert!(!contains_discard_all(b"/* app tag */ DISCARD ALL"));
        assert!(!contains_discard_all(b"DISCARD ALL -- trailing"));
        assert!(!contains_discard_all(b"DISCARD ALL /* trailing */"));
    }

    #[test]
    fn test_not_discard() {
        assert!(!contains_discard_all(b"SELECT 1"));
        assert!(!contains_discard_all(b"DISCARD PLANS"));
        assert!(!contains_discard_all(b"DISCARD SEQUENCES"));
        assert!(!contains_discard_all(b"DISCARD"));
        assert!(!contains_discard_all(b""));
    }

    #[test]
    fn test_long_query_still_matched() {
        // Long queries with only whitespace padding are still matched
        let long = format!("{:>300}DISCARD ALL", "");
        assert!(contains_discard_all(long.as_bytes()));
    }
}
