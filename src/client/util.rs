use bytes::BytesMut;
use once_cell::sync::Lazy;
use smallvec::SmallVec;
use std::sync::{atomic::AtomicUsize, Arc};

use crate::server::cleanup::{ResetCleanupCommand, SetCleanupCommand};

/// Incrementally count prepared statements
/// to avoid random conflicts in places where the random number generator is weak.
pub static PREPARED_STATEMENT_COUNTER: Lazy<Arc<AtomicUsize>> =
    Lazy::new(|| Arc::new(AtomicUsize::new(0)));

// Ignore deallocate queries from pgx.
pub(crate) static QUERY_DEALLOCATE: &[u8] = "deallocate ".as_bytes();

/// Checks if the message is a standalone BEGIN query (simple query protocol).
/// Accepts the two exact simple forms drivers commonly emit: `BEGIN` and
/// `BEGIN;`.
///
/// Q message format:
/// - Byte 0: 'Q' (0x51)
/// - Bytes 1-4: length in big-endian
/// - Bytes 5..N-1: query bytes
/// - Last byte: null terminator (0x00)
#[inline]
pub(crate) fn is_standalone_begin(message: &BytesMut) -> bool {
    let query = simple_query_body(message);
    query.eq_ignore_ascii_case(b"begin") || query.eq_ignore_ascii_case(b"begin;")
}

/// Extract the query string body (without the trailing null) from a simple
/// query `Q` message. Returns an empty slice for malformed/truncated messages.
///
/// Message layout: `Q` (1 byte tag) + length (4 bytes big-endian, includes
/// itself but NOT the tag) + query text + `\0` (1 byte).
///
/// This function is now a **security gate** for the DISCARD ALL interception
/// fast path - a truncated, misframed, or tag-spoofed message that slipped
/// through here would synthesise a CommandComplete the client believes came
/// from PostgreSQL, then leak whatever extra bytes the client appended into
/// the next backend exchange. So we strictly enforce:
///
/// * minimum size (header + tag + at least the `\0`),
/// * leading byte is `b'Q'`,
/// * the declared length matches the buffer (`len + 1` accounts for the tag
///   byte the protocol excludes),
/// * the trailing byte is the null terminator.
///
/// Any mismatch -> empty slice -> interception declines and the message goes
/// through the normal forward path where the upstream parser handles it.
#[inline(always)]
pub(crate) fn simple_query_body(message: &BytesMut) -> &[u8] {
    // Minimum well-formed Q: tag(1) + len(4) + null(1) = 6 bytes for an
    // empty query, the smallest legal frame.
    if message.len() < 6 {
        return &[];
    }
    if message[0] != b'Q' {
        return &[];
    }
    let declared_len =
        u32::from_be_bytes([message[1], message[2], message[3], message[4]]) as usize;
    // PG simple-query length field includes the 4 length bytes plus the
    // body plus the null terminator, but NOT the leading tag. So a buffer
    // of length N implies `declared_len == N - 1` (everything except 'Q').
    if declared_len + 1 != message.len() {
        return &[];
    }
    // Must end with the null terminator - without it the body would
    // silently include bytes from a follow-up message.
    if message[message.len() - 1] != 0 {
        return &[];
    }
    &message[5..message.len() - 1]
}

/// True iff `bytes` is a single standalone `DISCARD ALL` statement, modulo
/// surrounding whitespace, SQL comments (`-- line` and `/* block */` -
/// nested block comments per PostgreSQL spec), and trailing semicolons.
///
/// Comments are tolerated because most drivers (psql, pg_bouncer-style
/// probes, ORM fingerprinting) wrap their cleanup statements in
/// `/* app=foo */ DISCARD ALL` or `DISCARD ALL -- bench tag`, and the
/// per-pool operator contract is "no DISCARD ALL ever reaches PostgreSQL
/// in transaction pooling" - letting a commented variant slip through
/// would silently drop the long-lived shared temp-table state the
/// transaction-pool workload depends on.
///
/// **Explicitly NOT in scope** (rejected -> message forwarded to backend):
/// - multi-statement queries (`SELECT 1; DISCARD ALL`, `DISCARD ALL;
///   SELECT 2`) - extracting one statement's effect from a batch is
///   ambiguous and the other statements would still need a real
///   round-trip;
/// - `DISCARD PLANS` / `DISCARD SEQUENCES` / `DISCARD TEMP` - narrower
///   semantics, let the backend's view be authoritative;
/// - comments embedded *between* `DISCARD` and `ALL` keywords are accepted
///   because PostgreSQL treats comments as token separators.
#[inline(always)]
pub(crate) fn contains_discard_all(bytes: &[u8]) -> bool {
    let mut idx = 0;
    let len = bytes.len();
    if !skip_whitespace_and_comments(bytes, &mut idx) {
        return false;
    }
    if !consume_keyword(bytes, &mut idx, b"DISCARD") {
        return false;
    }
    // Mandatory token separator between DISCARD and ALL - no glue.
    // PostgreSQL treats comments as whitespace, so `DISCARD/*x*/ALL`
    // is the same statement as `DISCARD ALL`.
    if !skip_required_whitespace_or_comments(bytes, &mut idx) {
        return false;
    }
    if !consume_keyword(bytes, &mut idx, b"ALL") {
        return false;
    }
    // After "ALL", tolerate trailing whitespace, trailing comments, and
    // trailing semicolons. A non-empty non-comment payload after `;`
    // would be a second statement - we don't intercept then. An
    // unterminated block comment anywhere is fail-closed: the message
    // is forwarded to PostgreSQL so the real syntax error surfaces.
    if !skip_whitespace_and_comments(bytes, &mut idx) {
        return false;
    }
    while idx < len && bytes[idx] == b';' {
        idx += 1;
        if !skip_whitespace_and_comments(bytes, &mut idx) {
            return false;
        }
    }
    idx == len
}

/// True when any SimpleQuery statement is a standalone `DISCARD ALL`.
/// Unlike [`contains_discard_all`], this handles multi-statement SQL bodies
/// while still ignoring semicolons inside literals, identifiers, and comments.
pub(crate) fn contains_discard_all_statement(bytes: &[u8]) -> bool {
    let mut statement_start = 0usize;
    let mut idx = 0usize;

    while idx < bytes.len() {
        match bytes[idx] {
            b'E' | b'e' if is_escape_string_prefix(bytes, idx) => {
                idx = skip_escape_single_quoted_literal(bytes, idx + 1)
            }
            b'\'' => idx = skip_single_quoted_literal(bytes, idx),
            b'"' => idx = skip_double_quoted_identifier(bytes, idx),
            b'$' => {
                if let Some(next_idx) = skip_dollar_quoted_literal(bytes, idx) {
                    idx = next_idx;
                } else {
                    idx += 1;
                }
            }
            b'-' if idx + 1 < bytes.len() && bytes[idx + 1] == b'-' => {
                idx = skip_line_comment(bytes, idx);
            }
            b'/' if idx + 1 < bytes.len() && bytes[idx + 1] == b'*' => {
                let Some(next_idx) = skip_block_comment(bytes, idx) else {
                    break;
                };
                idx = next_idx;
            }
            b';' => {
                if contains_discard_all(&bytes[statement_start..idx]) {
                    return true;
                }
                idx += 1;
                statement_start = idx;
            }
            _ => idx += 1,
        }
    }

    statement_start < bytes.len() && contains_discard_all(&bytes[statement_start..])
}

/// True when any SimpleQuery statement starts with an opaque procedural command.
/// These commands can mutate session state inside procedural bodies or called
/// routines while reporting generic CommandComplete tags such as `DO`/`CALL`,
/// so validation cannot mirror their side effects safely.
pub(crate) fn contains_opaque_procedural_statement(bytes: &[u8]) -> bool {
    let mut statement_start = 0usize;
    let mut idx = 0usize;

    while idx < bytes.len() {
        match bytes[idx] {
            b'E' | b'e' if is_escape_string_prefix(bytes, idx) => {
                idx = skip_escape_single_quoted_literal(bytes, idx + 1)
            }
            b'\'' => idx = skip_single_quoted_literal(bytes, idx),
            b'"' => idx = skip_double_quoted_identifier(bytes, idx),
            b'$' => {
                if let Some(next_idx) = skip_dollar_quoted_literal(bytes, idx) {
                    idx = next_idx;
                } else {
                    idx += 1;
                }
            }
            b'-' if idx + 1 < bytes.len() && bytes[idx + 1] == b'-' => {
                idx = skip_line_comment(bytes, idx);
            }
            b'/' if idx + 1 < bytes.len() && bytes[idx + 1] == b'*' => {
                let Some(next_idx) = skip_block_comment(bytes, idx) else {
                    break;
                };
                idx = next_idx;
            }
            b';' => {
                if statement_is_opaque_procedural(&bytes[statement_start..idx]) {
                    return true;
                }
                idx += 1;
                statement_start = idx;
            }
            _ => idx += 1,
        }
    }

    statement_start < bytes.len() && statement_is_opaque_procedural(&bytes[statement_start..])
}

pub(crate) fn contains_session_control_statement(bytes: &[u8]) -> bool {
    let mut statement_start = 0usize;
    let mut idx = 0usize;

    while idx < bytes.len() {
        match bytes[idx] {
            b'E' | b'e' if is_escape_string_prefix(bytes, idx) => {
                idx = skip_escape_single_quoted_literal(bytes, idx + 1)
            }
            b'\'' => idx = skip_single_quoted_literal(bytes, idx),
            b'"' => idx = skip_double_quoted_identifier(bytes, idx),
            b'$' => {
                if let Some(next_idx) = skip_dollar_quoted_literal(bytes, idx) {
                    idx = next_idx;
                } else {
                    idx += 1;
                }
            }
            b'-' if idx + 1 < bytes.len() && bytes[idx + 1] == b'-' => {
                idx = skip_line_comment(bytes, idx);
            }
            b'/' if idx + 1 < bytes.len() && bytes[idx + 1] == b'*' => {
                let Some(next_idx) = skip_block_comment(bytes, idx) else {
                    break;
                };
                idx = next_idx;
            }
            b';' => {
                if statement_is_session_control(&bytes[statement_start..idx]) {
                    return true;
                }
                idx += 1;
                statement_start = idx;
            }
            _ => idx += 1,
        }
    }

    statement_start < bytes.len() && statement_is_session_control(&bytes[statement_start..])
}

pub(crate) fn contains_untrusted_function_call(bytes: &[u8]) -> bool {
    let mut idx = 0usize;

    while idx < bytes.len() {
        match bytes[idx] {
            b'E' | b'e' if is_escape_string_prefix(bytes, idx) => {
                idx = skip_escape_single_quoted_literal(bytes, idx + 1)
            }
            b'\'' => idx = skip_single_quoted_literal(bytes, idx),
            b'"' if idx == 0 || !is_sql_identifier_continuation(bytes[idx.saturating_sub(1)]) => {
                let (lookahead, quoted) = qualified_identifier_call_open(bytes, idx);
                if let Some(open_paren_idx) = lookahead {
                    if quoted {
                        return true;
                    }
                    idx = open_paren_idx + 1;
                } else {
                    idx = skip_double_quoted_identifier(bytes, idx);
                }
            }
            b'"' => idx = skip_double_quoted_identifier(bytes, idx),
            b'$' => {
                if let Some(next_idx) = skip_dollar_quoted_literal(bytes, idx) {
                    idx = next_idx;
                } else {
                    idx += 1;
                }
            }
            b'-' if idx + 1 < bytes.len() && bytes[idx + 1] == b'-' => {
                idx = skip_line_comment(bytes, idx);
            }
            b'/' if idx + 1 < bytes.len() && bytes[idx + 1] == b'*' => {
                let Some(next_idx) = skip_block_comment(bytes, idx) else {
                    return true;
                };
                idx = next_idx;
            }
            b if is_sql_identifier_start(b)
                && (idx == 0 || !is_sql_identifier_continuation(bytes[idx - 1])) =>
            {
                match unquoted_function_call_at(bytes, idx) {
                    FunctionCallScan::Trusted { next_idx } => idx = next_idx,
                    FunctionCallScan::Untrusted => return true,
                    FunctionCallScan::NoCall { next_idx } => idx = next_idx,
                }
            }
            _ => idx += 1,
        }
    }

    false
}

/// ASCII case-insensitive substring test. Used as a cheap necessary-condition
/// gate before the full quote- and comment-aware cleanup scanners: a body with
/// no `set`/`reset` keyword cannot contain a SET/RESET cleanup statement.
fn contains_ascii_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|w| w.iter().zip(needle).all(|(a, b)| a.eq_ignore_ascii_case(b)))
}

/// Return the reset-cleanup-relevant `RESET` statements from a SimpleQuery body
/// in statement order. PostgreSQL emits the same `CommandComplete("RESET")` tag
/// for `RESET ALL` and narrower `RESET foo` statements, so the server response
/// path needs this client-side attribution to safely disarm SET cleanup only for
/// proven `RESET ALL` statements.
pub(crate) fn extract_reset_cleanup_commands(bytes: &[u8]) -> SmallVec<[ResetCleanupCommand; 2]> {
    // A body with no `reset` keyword has no RESET statement to attribute.
    if !contains_ascii_ci(bytes, b"reset") {
        return SmallVec::new();
    }
    let mut commands = SmallVec::new();
    let mut statement_start = 0usize;
    let mut idx = 0usize;

    while idx < bytes.len() {
        match bytes[idx] {
            b'E' | b'e' if is_escape_string_prefix(bytes, idx) => {
                idx = skip_escape_single_quoted_literal(bytes, idx + 1)
            }
            b'\'' => idx = skip_single_quoted_literal(bytes, idx),
            b'"' => idx = skip_double_quoted_identifier(bytes, idx),
            b'$' => {
                if let Some(next_idx) = skip_dollar_quoted_literal(bytes, idx) {
                    idx = next_idx;
                } else {
                    idx += 1;
                }
            }
            b'-' if idx + 1 < bytes.len() && bytes[idx + 1] == b'-' => {
                idx = skip_line_comment(bytes, idx);
            }
            b'/' if idx + 1 < bytes.len() && bytes[idx + 1] == b'*' => {
                let Some(next_idx) = skip_block_comment(bytes, idx) else {
                    break;
                };
                idx = next_idx;
            }
            b';' => {
                if let Some(command) = parse_reset_cleanup_command(&bytes[statement_start..idx]) {
                    commands.push(command);
                }
                idx += 1;
                statement_start = idx;
            }
            _ => idx += 1,
        }
    }

    if statement_start < bytes.len() {
        if let Some(command) = parse_reset_cleanup_command(&bytes[statement_start..]) {
            commands.push(command);
        }
    }

    commands
}

/// Return the session-cleanup-relevant `SET` statements from a SimpleQuery body
/// in statement order. PostgreSQL emits the same `CommandComplete("SET")` tag
/// for ordinary GUC assignment, `SET ROLE`, and `SET SESSION AUTHORIZATION`, so
/// the response path needs this attribution to avoid treating `RESET ALL` as
/// proof that role/session identity was restored.
pub(crate) fn extract_set_cleanup_commands(bytes: &[u8]) -> SmallVec<[SetCleanupCommand; 2]> {
    // A body with no `set` keyword has no SET statement to attribute.
    if !contains_ascii_ci(bytes, b"set") {
        return SmallVec::new();
    }
    let mut commands = SmallVec::new();
    let mut statement_start = 0usize;
    let mut idx = 0usize;

    while idx < bytes.len() {
        match bytes[idx] {
            b'E' | b'e' if is_escape_string_prefix(bytes, idx) => {
                idx = skip_escape_single_quoted_literal(bytes, idx + 1)
            }
            b'\'' => idx = skip_single_quoted_literal(bytes, idx),
            b'"' => idx = skip_double_quoted_identifier(bytes, idx),
            b'$' => {
                if let Some(next_idx) = skip_dollar_quoted_literal(bytes, idx) {
                    idx = next_idx;
                } else {
                    idx += 1;
                }
            }
            b'-' if idx + 1 < bytes.len() && bytes[idx + 1] == b'-' => {
                idx = skip_line_comment(bytes, idx);
            }
            b'/' if idx + 1 < bytes.len() && bytes[idx + 1] == b'*' => {
                let Some(next_idx) = skip_block_comment(bytes, idx) else {
                    break;
                };
                idx = next_idx;
            }
            b';' => {
                if let Some(command) = parse_set_cleanup_command(&bytes[statement_start..idx]) {
                    commands.push(command);
                }
                idx += 1;
                statement_start = idx;
            }
            _ => idx += 1,
        }
    }

    if statement_start < bytes.len() {
        if let Some(command) = parse_set_cleanup_command(&bytes[statement_start..]) {
            commands.push(command);
        }
    }

    commands
}

/// Return true when a query contains a `set_config(...)` call whose scope is
/// not proven to be LOCAL (`set_config(..., true)`). Used by config validation
/// for prewarm SQL, which runs once when a backend is created; session-scoped
/// set_config would seed state before the backend enters the pool.
pub(crate) fn contains_session_set_config(bytes: &[u8]) -> bool {
    let mut idx = 0usize;

    while idx < bytes.len() {
        match bytes[idx] {
            b'E' | b'e' if is_escape_string_prefix(bytes, idx) => {
                idx = skip_escape_single_quoted_literal(bytes, idx + 1)
            }
            b'U' | b'u' if is_unicode_quoted_identifier_prefix(bytes, idx) => return true,
            b'\'' => idx = skip_single_quoted_literal(bytes, idx),
            b'"' if idx == 0 || !is_sql_identifier_continuation(bytes[idx.saturating_sub(1)]) => {
                if let Some(ident_end) = quoted_identifier_matches(bytes, idx, b"set_config") {
                    let mut lookahead = ident_end;
                    if !skip_whitespace_and_comments(bytes, &mut lookahead) {
                        return true;
                    }
                    if lookahead < bytes.len() && bytes[lookahead] == b'(' {
                        if set_config_call_is_session_scoped(bytes, lookahead) {
                            return true;
                        }
                        idx = lookahead + 1;
                    } else {
                        idx = ident_end;
                    }
                } else {
                    idx = skip_double_quoted_identifier(bytes, idx);
                }
            }
            b'"' => idx = skip_double_quoted_identifier(bytes, idx),
            b'$' => {
                if let Some(next_idx) = skip_dollar_quoted_literal(bytes, idx) {
                    idx = next_idx;
                } else {
                    idx += 1;
                }
            }
            b'-' if idx + 1 < bytes.len() && bytes[idx + 1] == b'-' => {
                idx = skip_line_comment(bytes, idx);
            }
            b'/' if idx + 1 < bytes.len() && bytes[idx + 1] == b'*' => {
                let Some(next_idx) = skip_block_comment(bytes, idx) else {
                    return true;
                };
                idx = next_idx;
            }
            b'S' | b's'
                if idx == 0 || !is_sql_identifier_continuation(bytes[idx.saturating_sub(1)]) =>
            {
                let mut lookahead = idx;
                if consume_keyword(bytes, &mut lookahead, b"SET_CONFIG") {
                    if !skip_whitespace_and_comments(bytes, &mut lookahead) {
                        return true;
                    }
                    if lookahead < bytes.len() && bytes[lookahead] == b'(' {
                        if set_config_call_is_session_scoped(bytes, lookahead) {
                            return true;
                        }
                        idx = lookahead + 1;
                    } else {
                        idx += 1;
                    }
                } else {
                    idx += 1;
                }
            }
            _ => idx += 1,
        }
    }

    false
}

fn set_config_call_is_session_scoped(bytes: &[u8], open_paren_idx: usize) -> bool {
    let mut idx = open_paren_idx + 1;
    let mut arg_start = idx;
    let mut completed_args = 0usize;
    let mut nested_parens = 0usize;

    while idx < bytes.len() {
        match bytes[idx] {
            b'E' | b'e' if is_escape_string_prefix(bytes, idx) => {
                idx = skip_escape_single_quoted_literal(bytes, idx + 1)
            }
            b'\'' => idx = skip_single_quoted_literal(bytes, idx),
            b'"' => idx = skip_double_quoted_identifier(bytes, idx),
            b'$' => {
                if let Some(next_idx) = skip_dollar_quoted_literal(bytes, idx) {
                    idx = next_idx;
                } else {
                    idx += 1;
                }
            }
            b'-' if idx + 1 < bytes.len() && bytes[idx + 1] == b'-' => {
                idx = skip_line_comment(bytes, idx);
            }
            b'/' if idx + 1 < bytes.len() && bytes[idx + 1] == b'*' => {
                let Some(next_idx) = skip_block_comment(bytes, idx) else {
                    return true;
                };
                idx = next_idx;
            }
            b'(' => {
                nested_parens += 1;
                idx += 1;
            }
            b')' if nested_parens == 0 => {
                let arg_number = completed_args + 1;
                if arg_number == 3 {
                    return !sql_arg_is_literal_true(&bytes[arg_start..idx]);
                }
                return true;
            }
            b')' => {
                nested_parens -= 1;
                idx += 1;
            }
            b',' if nested_parens == 0 => {
                let arg_number = completed_args + 1;
                if arg_number >= 3 {
                    return true;
                }
                completed_args += 1;
                idx += 1;
                arg_start = idx;
            }
            _ => idx += 1,
        }
    }

    true
}

fn sql_arg_is_literal_true(arg: &[u8]) -> bool {
    let mut idx = 0usize;
    if !skip_whitespace_and_comments(arg, &mut idx) {
        return false;
    }
    if !consume_keyword(arg, &mut idx, b"TRUE") {
        return false;
    }
    skip_whitespace_and_comments(arg, &mut idx) && idx == arg.len()
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SetScope {
    Local,
    Session,
}

fn parse_set_cleanup_command(statement: &[u8]) -> Option<SetCleanupCommand> {
    let mut idx = 0usize;
    if !skip_whitespace_and_comments(statement, &mut idx) {
        return None;
    }
    if !consume_keyword(statement, &mut idx, b"SET") {
        return None;
    }
    if idx >= statement.len() || !is_sql_whitespace_or_comment_start(statement, idx) {
        return None;
    }
    if !skip_whitespace_and_comments(statement, &mut idx) {
        return None;
    }
    if idx >= statement.len() {
        return None;
    }

    let constraints_start = idx;
    if consume_keyword(statement, &mut idx, b"CONSTRAINTS") {
        return None;
    }
    idx = constraints_start;

    let scope = consume_optional_set_scope(statement, &mut idx);
    let is_local_scope = matches!(scope, Some(SetScope::Local));

    let keyword_start = idx;
    if consume_keyword(statement, &mut idx, b"SESSION") {
        if idx >= statement.len() || !is_sql_whitespace_or_comment_start(statement, idx) {
            return Some(SetCleanupCommand::GenericSet);
        }
        if !skip_whitespace_and_comments(statement, &mut idx)
            || !consume_keyword(statement, &mut idx, b"AUTHORIZATION")
        {
            return Some(SetCleanupCommand::GenericSet);
        }
        if idx >= statement.len() || !is_sql_whitespace_or_comment_start(statement, idx) {
            return Some(SetCleanupCommand::SetSessionAuthorization);
        }
        if !skip_whitespace_and_comments(statement, &mut idx) {
            return Some(SetCleanupCommand::SetSessionAuthorization);
        }
        if consume_keyword(statement, &mut idx, b"DEFAULT")
            && skip_whitespace_and_comments(statement, &mut idx)
            && idx == statement.len()
        {
            if is_local_scope {
                return Some(SetCleanupCommand::SetSessionAuthorization);
            }
            return Some(SetCleanupCommand::SetSessionAuthorizationDefault);
        }
        return Some(SetCleanupCommand::SetSessionAuthorization);
    }
    idx = keyword_start;

    if consume_keyword(statement, &mut idx, b"ROLE") {
        if idx >= statement.len() || !is_sql_whitespace_or_comment_start(statement, idx) {
            return Some(SetCleanupCommand::SetRole);
        }
        if !skip_whitespace_and_comments(statement, &mut idx) {
            return Some(SetCleanupCommand::SetRole);
        }
        let target_start = idx;
        if (consume_keyword(statement, &mut idx, b"DEFAULT") || {
            idx = target_start;
            consume_keyword(statement, &mut idx, b"NONE")
        }) && skip_whitespace_and_comments(statement, &mut idx)
            && idx == statement.len()
        {
            if is_local_scope {
                return Some(SetCleanupCommand::SetRole);
            }
            return Some(SetCleanupCommand::SetRoleDefault);
        }
        return Some(SetCleanupCommand::SetRole);
    }

    Some(SetCleanupCommand::GenericSet)
}

fn statement_is_opaque_procedural(statement: &[u8]) -> bool {
    let mut idx = 0usize;
    if !skip_whitespace_and_comments(statement, &mut idx) {
        return true;
    }
    let keyword_start = idx;
    if consume_keyword(statement, &mut idx, b"DO")
        && (idx == statement.len() || is_sql_whitespace_or_comment_start(statement, idx))
    {
        return true;
    }
    idx = keyword_start;
    consume_keyword(statement, &mut idx, b"CALL")
        && (idx == statement.len() || is_sql_whitespace_or_comment_start(statement, idx))
}

fn statement_is_session_control(statement: &[u8]) -> bool {
    let mut idx = 0usize;
    if !skip_whitespace_and_comments(statement, &mut idx) {
        return true;
    }
    for keyword in [
        b"BEGIN".as_slice(),
        b"START".as_slice(),
        b"COMMIT".as_slice(),
        b"END".as_slice(),
        b"ROLLBACK".as_slice(),
        b"ABORT".as_slice(),
        b"SAVEPOINT".as_slice(),
        b"RELEASE".as_slice(),
        b"LISTEN".as_slice(),
    ] {
        let mut lookahead = idx;
        if consume_keyword(statement, &mut lookahead, keyword)
            && (lookahead == statement.len()
                || is_sql_whitespace_or_comment_start(statement, lookahead))
        {
            return true;
        }
    }
    false
}

enum FunctionCallScan {
    Trusted { next_idx: usize },
    Untrusted,
    NoCall { next_idx: usize },
}

fn unquoted_function_call_at(bytes: &[u8], idx: usize) -> FunctionCallScan {
    let Some((first_ident, mut lookahead)) = parse_unquoted_identifier(bytes, idx) else {
        return FunctionCallScan::NoCall { next_idx: idx + 1 };
    };
    let mut path = vec![first_ident];

    loop {
        let mut after_ident = lookahead;
        if !skip_whitespace_and_comments(bytes, &mut after_ident) {
            return FunctionCallScan::Untrusted;
        }
        if after_ident >= bytes.len() || bytes[after_ident] != b'.' {
            lookahead = after_ident;
            break;
        }
        after_ident += 1;
        if !skip_whitespace_and_comments(bytes, &mut after_ident) {
            return FunctionCallScan::Untrusted;
        }
        if let Some((ident, next_idx)) = parse_unquoted_identifier(bytes, after_ident) {
            path.push(ident);
            lookahead = next_idx;
            continue;
        }
        if bytes.get(after_ident) == Some(&b'"') {
            if let (Some(open_paren_idx), _) = qualified_identifier_call_open(bytes, after_ident) {
                return if path.len() == 1 && sql_keyword_can_precede_paren(&path[0]) {
                    FunctionCallScan::Trusted {
                        next_idx: open_paren_idx + 1,
                    }
                } else {
                    FunctionCallScan::Untrusted
                };
            }
        }
        return FunctionCallScan::NoCall {
            next_idx: lookahead,
        };
    }

    if lookahead < bytes.len() && bytes[lookahead] == b'(' {
        if (path.len() == 1 && sql_keyword_can_precede_paren(&path[0]))
            || function_call_is_trusted(&path, bytes, lookahead)
        {
            FunctionCallScan::Trusted {
                next_idx: lookahead + 1,
            }
        } else {
            FunctionCallScan::Untrusted
        }
    } else {
        FunctionCallScan::NoCall {
            next_idx: lookahead,
        }
    }
}

fn qualified_identifier_call_open(bytes: &[u8], idx: usize) -> (Option<usize>, bool) {
    let mut lookahead = skip_double_quoted_identifier(bytes, idx);
    let quoted = true;
    loop {
        if !skip_whitespace_and_comments(bytes, &mut lookahead) {
            return (Some(bytes.len()), quoted);
        }
        if lookahead < bytes.len() && bytes[lookahead] == b'(' {
            return (Some(lookahead), quoted);
        }
        if lookahead >= bytes.len() || bytes[lookahead] != b'.' {
            return (None, quoted);
        }
        lookahead += 1;
        if !skip_whitespace_and_comments(bytes, &mut lookahead) {
            return (Some(bytes.len()), quoted);
        }
        if lookahead < bytes.len() && bytes[lookahead] == b'"' {
            lookahead = skip_double_quoted_identifier(bytes, lookahead);
        } else if let Some((_, next_idx)) = parse_unquoted_identifier(bytes, lookahead) {
            lookahead = next_idx;
        } else {
            return (None, quoted);
        }
    }
}

fn parse_unquoted_identifier(bytes: &[u8], mut idx: usize) -> Option<(String, usize)> {
    if idx >= bytes.len() || !is_sql_identifier_start(bytes[idx]) {
        return None;
    }
    let mut ident = String::new();
    while idx < bytes.len() && is_sql_identifier_continuation(bytes[idx]) {
        ident.push(bytes[idx].to_ascii_lowercase() as char);
        idx += 1;
    }
    Some((ident, idx))
}

fn sql_keyword_can_precede_paren(ident: &str) -> bool {
    matches!(
        ident,
        "select"
            | "with"
            | "as"
            | "values"
            | "from"
            | "where"
            | "and"
            | "or"
            | "not"
            | "case"
            | "when"
            | "then"
            | "else"
            | "end"
            | "on"
            | "join"
            | "left"
            | "right"
            | "inner"
            | "outer"
            | "full"
            | "cross"
            | "group"
            | "order"
            | "by"
            | "limit"
            | "offset"
            | "distinct"
            | "union"
            | "all"
    )
}

fn function_path_is_pg_catalog(path: &[String], function_name: &str) -> bool {
    matches!(path, [name] if name == function_name)
        || matches!(path, [schema, name] if schema == "pg_catalog" && name == function_name)
}

fn function_path_is_public_pgv_free(path: &[String]) -> bool {
    matches!(path, [schema, name] if schema == "public" && name == "pgv_free")
}

fn function_call_has_no_arguments(bytes: &[u8], open_paren_idx: usize) -> bool {
    let mut idx = open_paren_idx + 1;
    skip_whitespace_and_comments(bytes, &mut idx) && idx < bytes.len() && bytes[idx] == b')'
}

fn function_call_is_trusted(path: &[String], bytes: &[u8], open_paren_idx: usize) -> bool {
    let Some(function_name) = path.last().map(String::as_str) else {
        return false;
    };
    match function_name {
        "set_config" => {
            function_path_is_pg_catalog(path, "set_config")
                && !set_config_call_is_session_scoped(bytes, open_paren_idx)
        }
        "pg_advisory_unlock_all" => {
            function_path_is_pg_catalog(path, "pg_advisory_unlock_all")
                && function_call_has_no_arguments(bytes, open_paren_idx)
        }
        "pgv_free" => {
            function_path_is_public_pgv_free(path)
                && function_call_has_no_arguments(bytes, open_paren_idx)
        }
        _ => false,
    }
}

fn consume_optional_set_scope(statement: &[u8], idx: &mut usize) -> Option<SetScope> {
    let scope_start = *idx;
    if consume_keyword(statement, idx, b"LOCAL") {
        if skip_required_whitespace_or_comments(statement, idx) {
            return Some(SetScope::Local);
        }
        *idx = scope_start;
        return None;
    }
    *idx = scope_start;

    if consume_keyword(statement, idx, b"SESSION") {
        let mut lookahead = *idx;
        if !skip_required_whitespace_or_comments(statement, &mut lookahead) {
            *idx = scope_start;
            return None;
        }
        let mut auth_idx = lookahead;
        if consume_keyword(statement, &mut auth_idx, b"AUTHORIZATION") {
            *idx = scope_start;
            return None;
        }
        *idx = lookahead;
        return Some(SetScope::Session);
    }
    *idx = scope_start;
    None
}

fn parse_reset_cleanup_command(statement: &[u8]) -> Option<ResetCleanupCommand> {
    let mut idx = 0usize;
    if !skip_whitespace_and_comments(statement, &mut idx) {
        return None;
    }
    if !consume_keyword(statement, &mut idx, b"RESET") {
        return None;
    }
    if idx >= statement.len() || !is_sql_whitespace_or_comment_start(statement, idx) {
        return None;
    }
    if !skip_whitespace_and_comments(statement, &mut idx) {
        return None;
    }
    if idx >= statement.len() {
        return None;
    }

    if consume_keyword(statement, &mut idx, b"ALL") {
        if skip_whitespace_and_comments(statement, &mut idx) && idx == statement.len() {
            return Some(ResetCleanupCommand::ResetAll);
        }
        return None;
    }
    if consume_keyword(statement, &mut idx, b"ROLE") {
        if skip_whitespace_and_comments(statement, &mut idx) && idx == statement.len() {
            return Some(ResetCleanupCommand::ResetRole);
        }
        return None;
    }
    if consume_keyword(statement, &mut idx, b"SESSION") {
        if idx >= statement.len() || !is_sql_whitespace_or_comment_start(statement, idx) {
            return None;
        }
        if skip_whitespace_and_comments(statement, &mut idx)
            && consume_keyword(statement, &mut idx, b"AUTHORIZATION")
            && skip_whitespace_and_comments(statement, &mut idx)
            && idx == statement.len()
        {
            return Some(ResetCleanupCommand::ResetSessionAuthorization);
        }
        return None;
    }

    Some(ResetCleanupCommand::PerGucReset)
}

#[inline]
fn is_sql_whitespace_or_comment_start(bytes: &[u8], idx: usize) -> bool {
    bytes[idx].is_ascii_whitespace()
        || (idx + 1 < bytes.len()
            && ((bytes[idx] == b'-' && bytes[idx + 1] == b'-')
                || (bytes[idx] == b'/' && bytes[idx + 1] == b'*')))
}

fn is_sql_identifier_continuation(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn is_sql_identifier_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_escape_string_prefix(bytes: &[u8], idx: usize) -> bool {
    idx + 1 < bytes.len()
        && bytes[idx + 1] == b'\''
        && (idx == 0 || !is_sql_identifier_continuation(bytes[idx - 1]))
}

fn is_unicode_quoted_identifier_prefix(bytes: &[u8], idx: usize) -> bool {
    idx + 2 < bytes.len()
        && bytes[idx + 1] == b'&'
        && bytes[idx + 2] == b'"'
        && (idx == 0 || !is_sql_identifier_continuation(bytes[idx - 1]))
}

fn skip_single_quoted_literal(bytes: &[u8], mut idx: usize) -> usize {
    idx += 1;
    while idx < bytes.len() {
        match bytes[idx] {
            b'\'' if idx + 1 < bytes.len() && bytes[idx + 1] == b'\'' => idx += 2,
            b'\'' => return idx + 1,
            _ => idx += 1,
        }
    }
    bytes.len()
}

fn skip_escape_single_quoted_literal(bytes: &[u8], mut idx: usize) -> usize {
    idx += 1;
    while idx < bytes.len() {
        match bytes[idx] {
            b'\'' if idx + 1 < bytes.len() && bytes[idx + 1] == b'\'' => idx += 2,
            b'\'' => return idx + 1,
            b'\\' if idx + 1 < bytes.len() => idx += 2,
            _ => idx += 1,
        }
    }
    bytes.len()
}

fn skip_double_quoted_identifier(bytes: &[u8], mut idx: usize) -> usize {
    idx += 1;
    while idx < bytes.len() {
        match bytes[idx] {
            b'"' if idx + 1 < bytes.len() && bytes[idx + 1] == b'"' => idx += 2,
            b'"' => return idx + 1,
            _ => idx += 1,
        }
    }
    bytes.len()
}

fn quoted_identifier_matches(bytes: &[u8], idx: usize, expected: &[u8]) -> Option<usize> {
    if bytes.get(idx) != Some(&b'"') {
        return None;
    }

    let mut pos = idx + 1;
    let mut matched = 0usize;
    while pos < bytes.len() {
        match bytes[pos] {
            b'"' if pos + 1 < bytes.len() && bytes[pos + 1] == b'"' => {
                if matched >= expected.len() || expected[matched] != b'"' {
                    return None;
                }
                matched += 1;
                pos += 2;
            }
            b'"' => {
                return (matched == expected.len()).then_some(pos + 1);
            }
            byte => {
                if matched >= expected.len() || expected[matched] != byte {
                    return None;
                }
                matched += 1;
                pos += 1;
            }
        }
    }
    None
}

fn skip_line_comment(bytes: &[u8], mut idx: usize) -> usize {
    idx += 2;
    while idx < bytes.len() && bytes[idx] != b'\n' {
        idx += 1;
    }
    idx
}

fn skip_block_comment(bytes: &[u8], mut idx: usize) -> Option<usize> {
    idx += 2;
    let mut depth = 1usize;
    while idx + 1 < bytes.len() {
        if bytes[idx] == b'/' && bytes[idx + 1] == b'*' {
            depth += 1;
            idx += 2;
        } else if bytes[idx] == b'*' && bytes[idx + 1] == b'/' {
            depth -= 1;
            idx += 2;
            if depth == 0 {
                return Some(idx);
            }
        } else {
            idx += 1;
        }
    }
    None
}

fn skip_dollar_quoted_literal(bytes: &[u8], idx: usize) -> Option<usize> {
    let tag_end = dollar_quote_tag_end(bytes, idx)?;
    let tag = &bytes[idx..tag_end];
    let mut search = tag_end;
    while search + tag.len() <= bytes.len() {
        if &bytes[search..search + tag.len()] == tag {
            return Some(search + tag.len());
        }
        search += 1;
    }
    Some(bytes.len())
}

fn dollar_quote_tag_end(bytes: &[u8], idx: usize) -> Option<usize> {
    if bytes.get(idx) != Some(&b'$') {
        return None;
    }
    let mut end = idx + 1;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    if end < bytes.len() && bytes[end] == b'$' {
        Some(end + 1)
    } else {
        None
    }
}

/// Advance `idx` past any combination of ASCII whitespace, `-- line`
/// comments (terminate at the next newline OR end of buffer), and
/// `/* block */` comments (PostgreSQL allows nesting - `/* a /* b */ c */`
/// must be matched in full).
///
/// Returns `false` if an unterminated `/* ... */` block was hit so the
/// caller can reject the whole message - otherwise a trailing
/// `DISCARD ALL /*` would silently get past `contains_discard_all` and
/// the synthetic-response path would acknowledge SQL PostgreSQL itself
/// would have rejected with a syntax error. The leading-case (`/* ...`
/// before `DISCARD`) is already caught by `consume_keyword` failing at
/// `idx == len`; the trailing case is asymmetric and needed an explicit
/// signal.
#[inline]
fn skip_whitespace_and_comments(bytes: &[u8], idx: &mut usize) -> bool {
    loop {
        while *idx < bytes.len() && bytes[*idx].is_ascii_whitespace() {
            *idx += 1;
        }
        if *idx + 1 >= bytes.len() {
            return true;
        }
        if bytes[*idx] == b'-' && bytes[*idx + 1] == b'-' {
            *idx += 2;
            while *idx < bytes.len() && bytes[*idx] != b'\n' {
                *idx += 1;
            }
            continue;
        }
        if bytes[*idx] == b'/' && bytes[*idx + 1] == b'*' {
            *idx += 2;
            let mut depth: usize = 1;
            while *idx + 1 < bytes.len() && depth > 0 {
                if bytes[*idx] == b'/' && bytes[*idx + 1] == b'*' {
                    depth += 1;
                    *idx += 2;
                } else if bytes[*idx] == b'*' && bytes[*idx + 1] == b'/' {
                    depth -= 1;
                    *idx += 2;
                } else {
                    *idx += 1;
                }
            }
            if depth != 0 {
                // Unterminated block comment - fail-closed. Signal to the
                // caller so it rejects the message and forwards to
                // PostgreSQL, where the syntax error will surface
                // honestly. Without this, a value like
                // `b"DISCARD ALL /* unterminated"` would advance idx to
                // len, `idx == len` would succeed, and the fast path
                // would synthesise a CommandComplete for invalid SQL.
                *idx = bytes.len();
                return false;
            }
            continue;
        }
        return true;
    }
}

#[inline]
fn skip_required_whitespace_or_comments(bytes: &[u8], idx: &mut usize) -> bool {
    let before = *idx;
    skip_whitespace_and_comments(bytes, idx) && *idx > before
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
    // Reject prefixes of longer identifiers (e.g. "DISCARDED", "ALLOWED").
    if *idx < bytes.len() {
        let next = bytes[*idx];
        if is_sql_identifier_continuation(next) {
            return false;
        }
    }
    true
}

/// parse the target of a `DEALLOCATE` simple-query body,
/// tolerating leading whitespace and SQL comments (line `--` and
/// block `/* */`). Returns:
/// - `Some(DeallocateTarget::All)` for `DEALLOCATE ALL` / `DEALLOCATE PREPARE ALL`
/// - `Some(DeallocateTarget::Named(name))` for `DEALLOCATE [PREPARE] <name>`
/// - `None` if the body is not a recognizable DEALLOCATE.
///
/// The previous prefix-only scan (`query_bytes[..QUERY_DEALLOCATE.len()]
/// .eq_ignore_ascii_case("deallocate ")`) silently missed bodies prefixed
/// with whitespace or comments - common with sqlcommenter / Datadog APM /
/// pgbench script preambles. The cache invalidation step was skipped while
/// the backend ran DEALLOCATE, so the next Bind for that name failed
/// with SQLSTATE 26000.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeallocateTarget {
    All,
    Named(String),
}

#[inline]
pub(crate) fn extract_deallocate_target(bytes: &[u8]) -> Option<DeallocateTarget> {
    let mut idx = 0;
    let len = bytes.len();
    if !skip_whitespace_and_comments(bytes, &mut idx) {
        return None;
    }
    if !consume_keyword(bytes, &mut idx, b"DEALLOCATE") {
        return None;
    }
    // Mandatory SQL whitespace after DEALLOCATE.
    if !skip_required_whitespace_or_comments(bytes, &mut idx) {
        return None;
    }
    // Optional `PREPARE` keyword.
    let prepare_start = idx;
    if consume_keyword(bytes, &mut idx, b"PREPARE") {
        // Mandatory SQL whitespace after PREPARE.
        if !skip_required_whitespace_or_comments(bytes, &mut idx) {
            return None;
        }
    } else {
        // Reset - `PREPARE` would have advanced idx on a partial match.
        idx = prepare_start;
    }
    // Now try `ALL` (case-insensitive identifier).
    let target_start = idx;
    if consume_keyword(bytes, &mut idx, b"ALL") {
        // Allow trailing whitespace, comments, and optional `;`.
        if !skip_whitespace_and_comments(bytes, &mut idx) {
            return None;
        }
        if idx < len && bytes[idx] == b';' {
            idx += 1;
            if !skip_whitespace_and_comments(bytes, &mut idx) {
                return None;
            }
        }
        if idx != len {
            return None;
        }
        return Some(DeallocateTarget::All);
    }
    // Otherwise capture and normalize the prepared-statement name the
    // same way PostgreSQL's SQL lexer does for DEALLOCATE: unquoted
    // identifiers fold to lower-case; quoted identifiers lose their
    // surrounding quotes and collapse doubled `""` into `"`.
    //
    // Quoted statement names may contain spaces and escaped quotes.
    // Keep the client-side prepared cache aligned with the backend
    // result so a later Bind does not reference a stale name.
    let name_limit = crate::messages::extended::MAX_PARSE_NAME_BYTES;
    let (name, name_end) = if target_start < len && bytes[target_start] == b'"' {
        let mut out = Vec::new();
        let mut idx = target_start + 1;
        let mut closed = false;
        while idx < len {
            if bytes[idx] == b'"' {
                if idx + 1 < len && bytes[idx + 1] == b'"' {
                    if out.len() == name_limit {
                        return None;
                    }
                    out.push(b'"');
                    idx += 2;
                    continue;
                }
                idx += 1;
                closed = true;
                break;
            }
            if out.len() == name_limit {
                return None;
            }
            out.push(bytes[idx]);
            idx += 1;
        }
        if !closed {
            return None;
        }
        (String::from_utf8(out).ok()?, idx)
    } else {
        let mut name_end = target_start;
        while name_end < len {
            let b = bytes[name_end];
            if b.is_ascii_alphanumeric() || b == b'_' || (b == b'$' && name_end > target_start) {
                if name_end - target_start == name_limit {
                    return None;
                }
                name_end += 1;
            } else {
                break;
            }
        }
        if name_end == target_start {
            return None;
        }
        let name = std::str::from_utf8(&bytes[target_start..name_end])
            .ok()?
            .to_ascii_lowercase();
        (name, name_end)
    };
    if name.is_empty() {
        return None;
    }
    let mut idx = name_end;
    if !skip_whitespace_and_comments(bytes, &mut idx) {
        return None;
    }
    if idx < len && bytes[idx] == b';' {
        idx += 1;
        if !skip_whitespace_and_comments(bytes, &mut idx) {
            return None;
        }
    }
    if idx != len {
        return None;
    }
    Some(DeallocateTarget::Named(name))
}

#[inline]
pub(crate) fn simple_query_starts_with_prepare(bytes: &[u8]) -> bool {
    let mut idx = 0;
    if !skip_whitespace_and_comments(bytes, &mut idx) {
        return false;
    }
    consume_keyword(bytes, &mut idx, b"PREPARE")
        && skip_required_whitespace_or_comments(bytes, &mut idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_query_message(query: &[u8]) -> BytesMut {
        let mut msg = BytesMut::new();
        msg.extend_from_slice(b"Q");
        msg.extend_from_slice(&((query.len() + 5) as u32).to_be_bytes());
        msg.extend_from_slice(query);
        msg.extend_from_slice(b"\0");
        msg
    }

    fn named(name: &str) -> DeallocateTarget {
        DeallocateTarget::Named(name.to_string())
    }

    #[test]
    fn standalone_begin_matches_without_trailing_semicolon() {
        let msg = simple_query_message(b"BEGIN");

        assert!(is_standalone_begin(&msg));
    }

    /// quoted-identifier names with spaces
    /// must parse cleanly - without this fix the  family of
    /// DEALLOCATE cache-invalidation bugs continued to fire for any
    /// client using `"stmt with spaces"` shaped names.
    #[test]
    fn deallocate_quoted_identifier_handles_spaces_and_embedded_quotes() {
        // Plain quoted name with a space.
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE \"my stmt\""),
            Some(named("my stmt"))
        );
        // Trailing `;`.
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE \"my stmt\";"),
            Some(named("my stmt"))
        );
        // Embedded `""` (PG escape for `"`).
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE \"a\"\"b\""),
            Some(named("a\"b"))
        );
        // PREPARE form.
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE PREPARE \"my stmt\""),
            Some(named("my stmt"))
        );
        // Comment prefix + quoted name.
        assert_eq!(
            extract_deallocate_target(b"/* trace */ DEALLOCATE \"my stmt\""),
            Some(named("my stmt"))
        );
    }

    #[test]
    fn deallocate_named_handles_whitespace_and_comments() {
        // Plain.
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE foo"),
            Some(named("foo"))
        );
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE foo;"),
            Some(named("foo"))
        );
        // Leading whitespace.
        assert_eq!(
            extract_deallocate_target(b"   DEALLOCATE foo"),
            Some(named("foo"))
        );
        // Block comment prefix (sqlcommenter, Datadog APM).
        assert_eq!(
            extract_deallocate_target(b"/* trace=1 */ DEALLOCATE foo"),
            Some(named("foo"))
        );
        // Line comment prefix.
        assert_eq!(
            extract_deallocate_target(b"-- comment\nDEALLOCATE foo"),
            Some(named("foo"))
        );
        // Optional PREPARE keyword.
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE PREPARE foo"),
            Some(named("foo"))
        );
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE foo$bar"),
            Some(named("foo$bar"))
        );
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE PREPARE foo$bar"),
            Some(named("foo$bar"))
        );
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE prepare$foo"),
            Some(named("prepare$foo"))
        );
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE all$foo"),
            Some(named("all$foo"))
        );
        // Comments between grammar tokens are SQL whitespace too.
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE /* trace=1 */ foo"),
            Some(named("foo"))
        );
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE PREPARE /* trace=1 */ foo"),
            Some(named("foo"))
        );
        // Mixed case.
        assert_eq!(
            extract_deallocate_target(b"deallocate FOO ; "),
            Some(named("foo"))
        );
    }

    #[test]
    fn deallocate_named_normalizes_like_postgresql_identifiers() {
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE FOO"),
            Some(named("foo"))
        );
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE FOO$BAR"),
            Some(named("foo$bar"))
        );
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE \"my stmt\""),
            Some(named("my stmt"))
        );
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE \"a\"\"b\""),
            Some(named("a\"b"))
        );
    }

    #[test]
    fn deallocate_named_rejects_over_cap_targets_before_allocation() {
        let quoted = format!(
            "DEALLOCATE \"{}\"",
            "a".repeat(crate::messages::extended::MAX_PARSE_NAME_BYTES + 1)
        );
        assert_eq!(extract_deallocate_target(quoted.as_bytes()), None);

        let unquoted = format!(
            "DEALLOCATE {}",
            "a".repeat(crate::messages::extended::MAX_PARSE_NAME_BYTES + 1)
        );
        assert_eq!(extract_deallocate_target(unquoted.as_bytes()), None);
    }

    #[test]
    fn deallocate_all_handles_whitespace_and_comments() {
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE ALL"),
            Some(DeallocateTarget::All)
        );
        assert_eq!(
            extract_deallocate_target(b"  /* hint */ DEALLOCATE ALL ; "),
            Some(DeallocateTarget::All)
        );
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE PREPARE ALL"),
            Some(DeallocateTarget::All)
        );
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE /* hint */ ALL"),
            Some(DeallocateTarget::All)
        );
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE PREPARE /* hint */ ALL"),
            Some(DeallocateTarget::All)
        );
    }

    #[test]
    fn deallocate_rejects_non_deallocate_and_multi_statement() {
        assert_eq!(extract_deallocate_target(b"SELECT 1"), None);
        assert_eq!(extract_deallocate_target(b"DEALLOCATED foo"), None);
        assert_eq!(extract_deallocate_target(b"DEALLOCATE foo; SELECT 1"), None);
        assert_eq!(extract_deallocate_target(b"DEALLOCATE"), None);
        assert_eq!(extract_deallocate_target(b"DEALLOCATE $foo"), None);
        assert_eq!(
            extract_deallocate_target(b"DEALLOCATE /* unterminated"),
            None
        );
    }

    #[test]
    fn simple_query_prepare_detector_handles_comments_and_boundaries() {
        assert!(simple_query_starts_with_prepare(b"PREPARE s AS SELECT 1"));
        assert!(simple_query_starts_with_prepare(
            b"/* trace */ PREPARE \"s\" AS SELECT 1"
        ));
        assert!(!simple_query_starts_with_prepare(b"PREPARED s AS SELECT 1"));
        assert!(!simple_query_starts_with_prepare(b"PREPARE"));
        assert!(!simple_query_starts_with_prepare(
            b"/* unterminated PREPARE s"
        ));
    }

    #[test]
    fn discard_all_plain_matches() {
        assert!(contains_discard_all(b"DISCARD ALL"));
        assert!(contains_discard_all(b"discard all"));
        assert!(contains_discard_all(b"  DISCARD   ALL  "));
        assert!(contains_discard_all(b"DISCARD ALL;"));
        assert!(contains_discard_all(b"DISCARD ALL ; "));
    }

    #[test]
    fn discard_all_multi_statement_rejected() {
        // A second statement past the synthetic response would silently lose
        // work - must NOT match.
        assert!(!contains_discard_all(b"DISCARD ALL; DISCARD ALL"));
        assert!(!contains_discard_all(b"DISCARD ALL;DISCARD ALL;"));
        assert!(!contains_discard_all(b"SELECT 1; DISCARD ALL"));
        assert!(!contains_discard_all(b"DISCARD ALL; SELECT 1"));
        assert!(!contains_discard_all(b"BEGIN; DISCARD ALL; COMMIT"));
    }

    #[test]
    fn discard_all_with_surrounding_comments_matches() {
        // Common driver patterns: app fingerprints / bench tags / ORM
        // markers wrap the cleanup in a comment. The operator-facing
        // contract is "no DISCARD ALL ever reaches PostgreSQL in
        // transaction pooling", so commented variants MUST be
        // intercepted too.
        assert!(contains_discard_all(b"-- app tag\nDISCARD ALL"));
        assert!(contains_discard_all(b"/* app tag */ DISCARD ALL"));
        assert!(contains_discard_all(b"DISCARD ALL -- trailing"));
        assert!(contains_discard_all(b"DISCARD ALL /* trailing */"));
        assert!(contains_discard_all(b"/* a */ /* b */ DISCARD ALL"));
        assert!(contains_discard_all(b"DISCARD ALL;-- trailing"));
        assert!(contains_discard_all(b"DISCARD ALL ; /* end */"));
        // PostgreSQL allows nested block comments.
        assert!(contains_discard_all(
            b"/* outer /* inner */ outer */ DISCARD ALL"
        ));
        // Unterminated block comment -> bail out (forward to PG).
        assert!(!contains_discard_all(b"/* unterminated DISCARD ALL"));
        // Regression: trailing-unterminated must also bail. Without
        // the bool return from `skip_whitespace_and_comments` the
        // helper would advance idx to len, `idx == len` would succeed,
        // and we'd synthesise `CommandComplete` for SQL PostgreSQL
        // itself would reject with SQLSTATE 42601.
        assert!(!contains_discard_all(b"DISCARD ALL /* unterminated"));
        assert!(!contains_discard_all(b"DISCARD ALL/*"));
        assert!(!contains_discard_all(b"DISCARD ALL; /* still hanging"));
        assert!(!contains_discard_all(
            b"DISCARD ALL /* nested /* inner */ never closes"
        ));
    }

    #[test]
    fn discard_all_comment_between_keywords_matches() {
        // PostgreSQL treats comments as token separators, so the pooler
        // contract ("no DISCARD ALL reaches PostgreSQL in transaction mode")
        // must cover comment-separated spellings too.
        assert!(contains_discard_all(b"DISCARD /* comment */ ALL"));
        assert!(contains_discard_all(b"DISCARD -- comment\nALL"));
    }

    #[test]
    fn discard_variants_other_than_all_rejected() {
        // PLANS / TEMP / SEQUENCES have narrower semantics - let them reach
        // PostgreSQL so the backend's view is authoritative.
        assert!(!contains_discard_all(b"DISCARD PLANS"));
        assert!(!contains_discard_all(b"DISCARD TEMP"));
        assert!(!contains_discard_all(b"DISCARD SEQUENCES"));
        assert!(!contains_discard_all(b"DISCARD"));
        assert!(!contains_discard_all(b"DISCARDED ALL"));
        assert!(!contains_discard_all(b"DISCARD ALLOWED"));
        assert!(!contains_discard_all(b""));
        assert!(!contains_discard_all(b"SELECT 1"));
    }

    #[test]
    fn discard_all_with_long_padding_still_matches() {
        let long = format!("{:>300}DISCARD ALL", "");
        assert!(contains_discard_all(long.as_bytes()));
    }

    #[test]
    fn simple_query_body_extracts_text() {
        // Q(1) + len(4, big-endian) = total bytes after the tag.
        // For "DISCARD ALL\0" the body is 11 + 1 = 12 bytes, plus the 4-byte
        // length field itself -> declared_len = 16 (0x10).
        let mut msg = BytesMut::new();
        msg.extend_from_slice(b"Q\0\0\0\x10DISCARD ALL\0");
        assert_eq!(simple_query_body(&msg), b"DISCARD ALL");
    }

    #[test]
    fn simple_query_body_empty_for_short_message() {
        let mut msg = BytesMut::new();
        msg.extend_from_slice(b"Q\0\0\0");
        assert_eq!(simple_query_body(&msg), b"");
    }

    #[test]
    fn simple_query_body_rejects_wrong_tag() {
        // A 'P' (Parse) frame must NOT be parsed as a simple query body, even
        // if its layout happens to look length-valid.
        let mut msg = BytesMut::new();
        msg.extend_from_slice(b"P\0\0\0\x10DISCARD ALL\0");
        assert_eq!(simple_query_body(&msg), b"");
    }

    #[test]
    fn simple_query_body_rejects_length_mismatch() {
        // Declared length says 0x20 (32) bytes follow, but the buffer is only
        // 17 long. A confused frame would otherwise lead the interceptor to
        // synthesize a response over a partial message.
        let mut msg = BytesMut::new();
        msg.extend_from_slice(b"Q\0\0\0\x20DISCARD ALL\0");
        assert_eq!(simple_query_body(&msg), b"");
    }

    #[test]
    fn simple_query_body_rejects_missing_null_terminator() {
        // Properly-sized buffer but with the trailing byte stomped - without
        // the trailing null PG would still receive the next message's first
        // byte as the end of this query, so we must decline interception.
        let mut msg = BytesMut::new();
        // 12 = 4 (len) + 7 ("FOO BAR") + 1 (null we will overwrite)
        msg.extend_from_slice(b"Q\0\0\0\x0cFOO BAR!");
        assert_eq!(simple_query_body(&msg), b"");
    }

    #[test]
    fn simple_query_body_accepts_empty_query() {
        // Minimum legal frame: Q + length(5) + just the null terminator.
        // declared_len 5 = the 4 length bytes + the null.
        let mut msg = BytesMut::new();
        msg.extend_from_slice(b"Q\0\0\0\x05\0");
        assert_eq!(simple_query_body(&msg), b"");
    }

    #[test]
    fn reset_cleanup_commands_preserve_statement_order() {
        use crate::server::cleanup::ResetCleanupCommand;

        let commands = extract_reset_cleanup_commands(
            b"SET client.app_user = 'u'; RESET application_name; SELECT bad(); RESET ALL",
        );

        assert_eq!(
            commands.as_slice(),
            &[
                ResetCleanupCommand::PerGucReset,
                ResetCleanupCommand::ResetAll,
            ],
            "the first RESET CommandComplete must be attributed to the per-GUC RESET, \
             not to the later RESET ALL that may never execute after an error",
        );
    }

    #[test]
    fn reset_cleanup_commands_ignore_literals_comments_and_identifiers() {
        use crate::server::cleanup::ResetCleanupCommand;

        let commands = extract_reset_cleanup_commands(
            br#"
                SELECT 'RESET ALL; RESET application_name';
                SELECT $$RESET ALL$$;
                -- RESET ALL
                /* RESET ALL */
                RESET "application_name";
                RESET ALL;
                RESET_ROLE;
            "#,
        );

        assert_eq!(
            commands.as_slice(),
            &[
                ResetCleanupCommand::PerGucReset,
                ResetCleanupCommand::ResetAll,
            ],
        );
    }

    #[test]
    fn session_auth_cleanup_commands_distinguish_role_and_session_authorization() {
        use crate::server::cleanup::{ResetCleanupCommand, SetCleanupCommand};

        let set_commands = extract_set_cleanup_commands(
            b"
                SET statement_timeout = 1000;
                SET ROLE audit_reader;
                SET SESSION AUTHORIZATION app_user;
                SET SESSION SESSION AUTHORIZATION service_user;
                SET LOCAL SESSION AUTHORIZATION local_user;
                SET SESSION ROLE session_role;
                SET LOCAL ROLE local_role;
                SET ROLE NONE;
                SET SESSION AUTHORIZATION DEFAULT;
                SET SESSION SESSION AUTHORIZATION DEFAULT;
            ",
        );
        assert_eq!(
            set_commands.as_slice(),
            &[
                SetCleanupCommand::GenericSet,
                SetCleanupCommand::SetRole,
                SetCleanupCommand::SetSessionAuthorization,
                SetCleanupCommand::SetSessionAuthorization,
                SetCleanupCommand::SetSessionAuthorization,
                SetCleanupCommand::SetRole,
                SetCleanupCommand::SetRole,
                SetCleanupCommand::SetRoleDefault,
                SetCleanupCommand::SetSessionAuthorizationDefault,
                SetCleanupCommand::SetSessionAuthorizationDefault,
            ],
        );

        let reset_commands = extract_reset_cleanup_commands(
            b"
                RESET ROLE;
                RESET SESSION AUTHORIZATION;
                RESET ALL;
                RESET statement_timeout;
            ",
        );
        assert_eq!(
            reset_commands.as_slice(),
            &[
                ResetCleanupCommand::ResetRole,
                ResetCleanupCommand::ResetSessionAuthorization,
                ResetCleanupCommand::ResetAll,
                ResetCleanupCommand::PerGucReset,
            ],
        );
    }

    #[test]
    fn set_constraints_does_not_enqueue_set_cleanup_attribution() {
        use crate::server::cleanup::SetCleanupCommand;

        let commands = extract_set_cleanup_commands(
            b"
                SET CONSTRAINTS ALL DEFERRED;
                SET SESSION AUTHORIZATION app_user;
            ",
        );

        assert_eq!(
            commands.as_slice(),
            &[SetCleanupCommand::SetSessionAuthorization],
            "SET CONSTRAINTS has CommandComplete(\"SET CONSTRAINTS\"), not SET, so it must not shift later SET attribution"
        );
    }

    #[test]
    fn local_default_role_and_session_authorization_do_not_disarm_cleanup() {
        use crate::server::cleanup::SetCleanupCommand;

        let commands = extract_set_cleanup_commands(
            b"
                SET LOCAL ROLE DEFAULT;
                SET LOCAL ROLE NONE;
                SET LOCAL SESSION AUTHORIZATION DEFAULT;
            ",
        );

        assert_eq!(
            commands.as_slice(),
            &[
                SetCleanupCommand::SetRole,
                SetCleanupCommand::SetRole,
                SetCleanupCommand::SetSessionAuthorization,
            ],
            "LOCAL DEFAULT/NONE is transaction-local, so it must not disarm session cleanup"
        );
    }

    #[test]
    fn cleanup_keyword_boundaries_treat_dollar_as_identifier_continuation() {
        use crate::server::cleanup::{ResetCleanupCommand, SetCleanupCommand};

        let set_commands =
            extract_set_cleanup_commands(b"SET role$tenant.foo = 'x'; SET role = tenant");
        assert_eq!(
            set_commands.as_slice(),
            &[SetCleanupCommand::GenericSet, SetCleanupCommand::SetRole],
            "`role$tenant` is a GUC identifier prefix, not the SET ROLE keyword"
        );

        let reset_commands =
            extract_reset_cleanup_commands(b"RESET all$tenant.foo; RESET role$tenant.foo");
        assert_eq!(
            reset_commands.as_slice(),
            &[
                ResetCleanupCommand::PerGucReset,
                ResetCleanupCommand::PerGucReset,
            ],
            "`all$tenant` and `role$tenant` are GUC identifiers, not RESET ALL/ROLE"
        );
    }

    #[test]
    fn set_cleanup_commands_treat_standard_strings_like_postgresql() {
        use crate::server::cleanup::SetCleanupCommand;

        let commands =
            extract_set_cleanup_commands(br#"SELECT '\'; SET ROLE tenant; -- still SQL"#);

        assert_eq!(
            commands.as_slice(),
            &[SetCleanupCommand::SetRole],
            "standard SQL strings do not treat backslash as escaping the quote"
        );
    }

    #[test]
    fn set_cleanup_commands_keep_backslash_escapes_in_e_strings() {
        use crate::server::cleanup::SetCleanupCommand;

        let commands =
            extract_set_cleanup_commands(br#"SELECT E'\'; SET ROLE not_sql'; SET ROLE tenant"#);

        assert_eq!(
            commands.as_slice(),
            &[SetCleanupCommand::SetRole],
            "E'' strings must still treat backslash as escaping quotes"
        );
    }
}
