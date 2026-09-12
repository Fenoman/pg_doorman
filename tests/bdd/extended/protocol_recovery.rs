use crate::pg_connection::PgConnection;
use crate::world::DoormanWorld;
use cucumber::then;
use std::time::Duration;

use super::helpers::ProtocolMessages;

async fn read_message(conn: &mut PgConnection, endpoint: &str) -> (char, Vec<u8>) {
    tokio::time::timeout(Duration::from_secs(3), conn.read_message())
        .await
        .unwrap_or_else(|_| panic!("{endpoint}: timed out waiting for the next protocol message"))
        .unwrap_or_else(|err| panic!("{endpoint}: protocol connection closed: {err}"))
}

async fn read_ready(conn: &mut PgConnection, endpoint: &str) -> ProtocolMessages {
    let mut messages = Vec::new();
    loop {
        let message = read_message(conn, endpoint).await;
        let ready = message.0 == 'Z';
        messages.push(message);
        if ready {
            return messages;
        }
    }
}

fn message_types(messages: &ProtocolMessages) -> String {
    messages.iter().map(|(kind, _)| *kind).collect()
}

fn first_value(messages: &ProtocolMessages) -> Option<String> {
    messages.iter().find_map(|(kind, body)| {
        if *kind == 'D' {
            assert_eq!(&body[..2], &1i16.to_be_bytes());
            let len = i32::from_be_bytes(body[2..6].try_into().unwrap()) as usize;
            Some(String::from_utf8(body[6..6 + len].to_vec()).unwrap())
        } else {
            None
        }
    })
}

async fn simple(conn: &mut PgConnection, endpoint: &str, query: &str) -> ProtocolMessages {
    conn.send_simple_query(query).await.unwrap();
    read_ready(conn, endpoint).await
}

fn sqlstate(messages: &ProtocolMessages) -> Option<String> {
    messages.iter().find_map(|(kind, body)| {
        if *kind != 'E' {
            return None;
        }
        body.split(|&byte| byte == 0).find_map(|field| {
            field
                .strip_prefix(b"C")
                .map(|code| String::from_utf8(code.to_vec()).unwrap())
        })
    })
}

async fn parse(conn: &mut PgConnection, endpoint: &str, name: &str, query: &str) {
    conn.send_parse(name, query).await.unwrap();
    conn.send_sync().await.unwrap();
    assert_eq!(
        message_types(&read_ready(conn, endpoint).await),
        "1Z",
        "{endpoint}: Parse failed"
    );
}

async fn execute(conn: &mut PgConnection, endpoint: &str, name: &str) -> ProtocolMessages {
    conn.send_bind("", name, Vec::new()).await.unwrap();
    conn.send_execute("", 0).await.unwrap();
    conn.send_sync().await.unwrap();
    read_ready(conn, endpoint).await
}

#[then("a cold Bind after an error-skipped batch remains usable after savepoint rollback")]
pub async fn cold_bind_after_error(world: &mut DoormanWorld) {
    for (endpoint, conn) in [
        ("PostgreSQL", world.pg_conn.as_mut().unwrap()),
        ("pg_doorman", world.doorman_conn.as_mut().unwrap()),
    ] {
        simple(conn, endpoint, "BEGIN").await;
        parse(conn, endpoint, "keep", "SELECT 42").await;
        parse(conn, endpoint, "evict", "SELECT 55").await;
        simple(conn, endpoint, "SAVEPOINT recovery_savepoint").await;
        conn.send_parse("bad", "SELECT protocol_recovery_missing_column")
            .await
            .unwrap();
        let failed = execute(conn, endpoint, "keep").await;
        assert_eq!(message_types(&failed), "EZ", "{endpoint}");
        assert_eq!(sqlstate(&failed), Some("42703".to_string()), "{endpoint}");
        assert_eq!(failed.last().unwrap().1, b"E", "{endpoint}");
        simple(conn, endpoint, "ROLLBACK TO recovery_savepoint").await;
        let recovered = execute(conn, endpoint, "keep").await;
        assert_eq!(message_types(&recovered), "2DCZ", "{endpoint}");
        assert_eq!(
            first_value(&recovered),
            Some("42".to_string()),
            "{endpoint}"
        );
        assert_eq!(recovered.last().unwrap().1, b"T", "{endpoint}");
        simple(conn, endpoint, "ROLLBACK").await;
    }
}

// Parse and Close in an error-skipped suffix must not become visible to the
// next batch, while ParseComplete acknowledges the successful prefix.
#[then(regex = r#"^extended error case "([^"]+)" preserves the PostgreSQL statement namespace$"#)]
pub async fn extended_error(world: &mut DoormanWorld, case: String) {
    for (endpoint, conn) in [
        ("PostgreSQL", world.pg_conn.as_mut().unwrap()),
        ("pg_doorman", world.doorman_conn.as_mut().unwrap()),
    ] {
        parse(conn, endpoint, "keep", "SELECT 42").await;
        parse(conn, endpoint, "", "SELECT 42").await;
        if case == "evicted backend" {
            parse(conn, endpoint, "evict", "SELECT 55").await;
        }
        if case == "acknowledged Close" {
            conn.send_close('S', "keep").await.unwrap();
        }
        conn.send_parse("prefix", "SELECT 99").await.unwrap();
        conn.send_parse("bad", "SELECT * FROM protocol_recovery_missing_relation")
            .await
            .unwrap();
        let after_flush = matches!(case.as_str(), "after Flush" | "Simple Query after Flush");
        if after_flush {
            conn.send_flush().await.unwrap();
            assert_eq!(read_message(conn, endpoint).await.0, '1');
            let error = read_message(conn, endpoint).await;
            assert_eq!(sqlstate(&vec![error]), Some("42P01".to_string()));
        }
        match case.as_str() {
            "Close" => conn.send_close('S', "keep").await.unwrap(),
            "cached Parse" | "after Flush" | "evicted backend" => {
                conn.send_parse("later", "SELECT 42").await.unwrap()
            }
            "Simple Query after Flush" => {
                conn.send_simple_query("SELECT 1000").await.unwrap();
                conn.send_simple_query("").await.unwrap();
                conn.send_parse("later", "SELECT 42").await.unwrap();
            }
            "named overwrite" => conn.send_parse("keep", "SELECT 77").await.unwrap(),
            "unnamed overwrite" => conn.send_parse("", "SELECT 77").await.unwrap(),
            "unnamed Close" => conn.send_close('S', "").await.unwrap(),
            "acknowledged Close" => conn.send_parse("keep", "SELECT 42").await.unwrap(),
            "repeated mutations" => {
                conn.send_close('S', "keep").await.unwrap();
                conn.send_parse("keep", "SELECT 77").await.unwrap();
                conn.send_close('S', "keep").await.unwrap();
                conn.send_parse("", "SELECT 77").await.unwrap();
                conn.send_parse("", "SELECT 99").await.unwrap();
                conn.send_close('S', "").await.unwrap();
            }
            _ => panic!("Unknown extended error case {case}"),
        }
        conn.send_sync().await.unwrap();
        let failed = read_ready(conn, endpoint).await;
        if after_flush {
            assert_eq!(
                message_types(&failed),
                "Z",
                "{endpoint}: messages after error must be skipped until Sync"
            );
        } else {
            let expected = if case == "acknowledged Close" {
                "31EZ"
            } else {
                "1EZ"
            };
            assert_eq!(
                message_types(&failed),
                expected,
                "{endpoint}: suffix must not produce acknowledgements"
            );
            assert_eq!(sqlstate(&failed), Some("42P01".to_string()));
        }
        assert_eq!(
            first_value(&execute(conn, endpoint, "prefix").await),
            Some("99".to_string()),
            "{endpoint}: successful prefix was lost"
        );
        let kept = execute(conn, endpoint, "keep").await;
        if case == "acknowledged Close" {
            assert_eq!(
                sqlstate(&kept),
                Some("26000".to_string()),
                "{endpoint}: acknowledged Close was undone"
            );
        } else {
            assert_eq!(
                first_value(&kept),
                Some("42".to_string()),
                "{endpoint}: skipped suffix changed the named statement"
            );
        }
        assert_eq!(
            first_value(&execute(conn, endpoint, "").await),
            Some("42".to_string()),
            "{endpoint}: skipped suffix changed the unnamed statement"
        );
        if matches!(case.as_str(), "cached Parse" | "evicted backend") || after_flush {
            conn.send_describe('S', "later").await.unwrap();
            conn.send_sync().await.unwrap();
            let absent = read_ready(conn, endpoint).await;
            assert_eq!(
                sqlstate(&absent),
                Some("26000".to_string()),
                "{endpoint}: skipped Parse created a statement"
            );
            assert_eq!(message_types(&absent), "EZ");
            assert_eq!(
                first_value(&execute(conn, endpoint, "prefix").await),
                Some("99".to_string()),
                "{endpoint}: Sync did not recover after the missing statement"
            );
        }
    }
}

// Every ordinary Q message destroys the logical unnamed statement even if
// the query is empty or can be answered before backend checkout.
#[then(regex = r#"^Simple Query case "([^"]+)" invalidates the logical unnamed statement$"#)]
pub async fn simple_invalidates_unnamed(world: &mut DoormanWorld, case: String) {
    let query = match case.as_str() {
        "ordinary" => "SELECT 9",
        "empty" => "",
        "deferred BEGIN" => "BEGIN",
        "intercepted DISCARD" => "DISCARD ALL",
        "health check" => "SELECT 1",
        _ => panic!("Unknown Simple Query case {case}"),
    };
    for (endpoint, conn) in [
        ("PostgreSQL", world.pg_conn.as_mut().unwrap()),
        ("pg_doorman", world.doorman_conn.as_mut().unwrap()),
    ] {
        parse(conn, endpoint, "", "SELECT 42").await;
        assert!(sqlstate(&simple(conn, endpoint, query).await).is_none());
        if case == "ordinary" {
            conn.send_bind("", "", Vec::new()).await.unwrap();
            conn.send_execute("", 0).await.unwrap();
        } else {
            conn.send_describe('S', "").await.unwrap();
        }
        conn.send_sync().await.unwrap();
        let absent = read_ready(conn, endpoint).await;
        assert_eq!(
            sqlstate(&absent),
            Some("26000".to_string()),
            "{endpoint}: Simple Query retained the unnamed statement"
        );
        assert_eq!(message_types(&absent), "EZ");
        simple(conn, endpoint, "ROLLBACK").await;
        parse(conn, endpoint, "", "SELECT 42").await;
        assert_eq!(
            first_value(&execute(conn, endpoint, "").await),
            Some("42".to_string()),
            "{endpoint}: re-Parse after Sync did not recover"
        );
    }
}

#[then("backend cache aliases cannot be addressed as logical client statements")]
pub async fn backend_alias_is_not_a_client_statement(world: &mut DoormanWorld) {
    let endpoint = "pg_doorman";
    let conn = world.doorman_conn.as_mut().unwrap();
    parse(conn, endpoint, "keep", "SELECT 42").await;
    let names = simple(
        conn,
        endpoint,
        "SELECT name FROM pg_prepared_statements WHERE statement = 'SELECT 42'",
    )
    .await;
    let backend_name = first_value(&names).expect("the backend must retain the cached statement");
    assert_ne!(backend_name, "keep");
    conn.send_close('S', &backend_name).await.unwrap();
    conn.send_sync().await.unwrap();
    assert_eq!(message_types(&read_ready(conn, endpoint).await), "3Z");
    assert_eq!(
        first_value(&execute(conn, endpoint, "keep").await),
        Some("42".to_string()),
        "closing an unknown logical name must not deallocate a cached physical alias"
    );
    parse(conn, endpoint, &backend_name, "SELECT 99").await;
    conn.send_close('S', &backend_name).await.unwrap();
    conn.send_sync().await.unwrap();
    assert_eq!(message_types(&read_ready(conn, endpoint).await), "3Z");
    assert_eq!(
        first_value(&execute(conn, endpoint, "keep").await),
        Some("42".to_string()),
        "closing a known logical name must not deallocate another statement's physical alias"
    );
    let absent = execute(conn, endpoint, &backend_name).await;
    assert_eq!(sqlstate(&absent), Some("26000".to_string()));
    assert_eq!(message_types(&absent), "EZ");
    assert_eq!(
        first_value(&execute(conn, endpoint, "keep").await),
        Some("42".to_string())
    );
}

#[then(
    regex = r#"^a cached Parse before streaming Execute with "([^"]+)" is acknowledged exactly once$"#
)]
pub async fn cached_parse_before_streaming_execute(world: &mut DoormanWorld, case: String) {
    for (endpoint, conn) in [
        ("PostgreSQL", world.pg_conn.as_mut().unwrap()),
        ("pg_doorman", world.doorman_conn.as_mut().unwrap()),
    ] {
        parse(conn, endpoint, "seed", "SELECT 42").await;
        let (query, expected_rows) = match case.as_str() {
            "many rows" => ("SELECT generate_series(1,100000)", 100000),
            "large row" => ("SELECT repeat('x',2097152)", 1),
            _ => panic!("Unknown streaming case {case}"),
        };
        conn.send_parse("run", query).await.unwrap();
        conn.send_bind("", "run", Vec::new()).await.unwrap();
        conn.send_parse("cached", "SELECT 42").await.unwrap();
        conn.send_execute("", 0).await.unwrap();
        conn.send_sync().await.unwrap();
        let messages = read_ready(conn, endpoint).await;
        assert_eq!(
            messages.iter().filter(|(kind, _)| *kind == '1').count(),
            2,
            "{endpoint}: ParseComplete was repeated across streamed chunks"
        );
        assert_eq!(
            messages.iter().filter(|(kind, _)| *kind == 'D').count(),
            expected_rows
        );
        assert_eq!(messages.iter().filter(|(kind, _)| *kind == 'C').count(), 1);
        assert!(sqlstate(&messages).is_none());
        assert_eq!(
            messages
                .iter()
                .take(4)
                .map(|(kind, _)| *kind)
                .collect::<String>(),
            "121D",
            "{endpoint}: cached ParseComplete must precede the first streamed row"
        );
        if case == "large row" {
            assert_eq!(first_value(&messages).unwrap(), "x".repeat(2097152));
        }
    }
}

#[then(regex = r#"^extended COPY "([^"]+)" completes before a later Sync$"#)]
pub async fn extended_copy_completion(world: &mut DoormanWorld, completion: String) {
    for (endpoint, conn) in [
        ("PostgreSQL", world.pg_conn.as_mut().unwrap()),
        ("pg_doorman", world.doorman_conn.as_mut().unwrap()),
    ] {
        simple(
            conn,
            endpoint,
            "CREATE TEMP TABLE recovery_copy(value integer)",
        )
        .await;
        conn.send_parse("copy", "COPY recovery_copy FROM STDIN")
            .await
            .unwrap();
        conn.send_bind("", "copy", Vec::new()).await.unwrap();
        conn.send_execute("", 0).await.unwrap();
        conn.send_flush().await.unwrap();
        for kind in ['1', '2', 'G'] {
            assert_eq!(read_message(conn, endpoint).await.0, kind);
        }
        if completion == "done" {
            conn.send_copy_data(b"7\n").await.unwrap();
            conn.send_copy_done().await.unwrap();
            conn.send_flush().await.unwrap();
            assert_eq!(
                read_message(conn, endpoint).await.0,
                'C',
                "{endpoint}: COPY completion must not wait for Sync"
            );
        } else {
            conn.send_copy_fail("cancel this COPY").await.unwrap();
            conn.send_flush().await.unwrap();
            assert_eq!(read_message(conn, endpoint).await.0, 'E');
        }
        conn.send_sync().await.unwrap();
        assert_eq!(message_types(&read_ready(conn, endpoint).await), "Z");
        let count = simple(conn, endpoint, "SELECT count(*) FROM recovery_copy").await;
        let expected = if completion == "done" { "1" } else { "0" };
        assert_eq!(
            first_value(&count),
            Some(expected.to_string()),
            "{endpoint}: post-COPY query failed: {count:?}"
        );
    }
}

// A DataRow before CopyInResponse must hand control back to the client; a
// streaming result after CopyDone must reach its real ReadyForQuery boundary.
#[then(regex = r#"^mixed COPY case "([^"]+)" completes identically on both connections$"#)]
pub async fn mixed_copy(world: &mut DoormanWorld, case: String) {
    let (query, expected_rows, expected_copies, expected_commands) = match case.as_str() {
        "prefix" => ("SELECT 1; COPY recovery_copy FROM STDIN", 1, 1, 2),
        "small suffix" => ("COPY recovery_copy FROM STDIN; SELECT generate_series(1,10)", 10, 1, 2),
        "large suffix" => ("COPY recovery_copy FROM STDIN; SELECT generate_series(1,100000)", 100000, 1, 2),
        "multiple" => ("SELECT 1; COPY recovery_copy FROM STDIN; SELECT generate_series(1,100000); COPY recovery_copy FROM STDIN; SELECT 2", 100002, 2, 5),
        "copy out suffix" => ("COPY recovery_copy FROM STDIN; COPY (SELECT generate_series(1,100000)) TO STDOUT", 0, 1, 2),
        _ => panic!("Unknown mixed COPY case {case}"),
    };
    for (endpoint, conn) in [
        ("PostgreSQL", world.pg_conn.as_mut().unwrap()),
        ("pg_doorman", world.doorman_conn.as_mut().unwrap()),
    ] {
        let setup = simple(
            conn,
            endpoint,
            "CREATE TEMP TABLE recovery_copy(value integer)",
        )
        .await;
        assert_eq!(message_types(&setup), "CZ", "{endpoint}: setup failed");
        let pid_before = first_value(&simple(conn, endpoint, "SELECT pg_backend_pid()").await);
        conn.send_simple_query(query).await.unwrap();
        let (mut rows, mut copies, mut commands, mut copy_rows) = (0, 0, 0, 0);
        loop {
            let (kind, body) = read_message(conn, endpoint).await;
            match kind {
                'G' => {
                    copies += 1;
                    conn.send_copy_data(b"7\n").await.unwrap();
                    conn.send_copy_done().await.unwrap();
                }
                'D' => rows += 1,
                'd' => copy_rows += body.iter().filter(|&&byte| byte == b'\n').count(),
                'C' => commands += 1,
                'E' => panic!(
                    "{endpoint}: COPY failed: {}",
                    String::from_utf8_lossy(&body)
                ),
                'Z' => {
                    assert_eq!(
                        body, b"I",
                        "{endpoint}: COPY must finish outside a transaction"
                    );
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(rows, expected_rows, "{endpoint}: truncated result");
        assert_eq!(
            copies, expected_copies,
            "{endpoint}: missing CopyInResponse"
        );
        assert_eq!(
            commands, expected_commands,
            "{endpoint}: missing CommandComplete"
        );
        if case == "copy out suffix" {
            assert_eq!(copy_rows, 100000, "{endpoint}: truncated COPY OUT");
        }
        let values = simple(conn, endpoint, "SELECT sum(value) FROM recovery_copy").await;
        assert_eq!(
            first_value(&values),
            Some((7 * expected_copies).to_string()),
            "{endpoint}: warm temp state was lost"
        );
        let pid_after = first_value(&simple(conn, endpoint, "SELECT pg_backend_pid()").await);
        assert_eq!(pid_after, pid_before, "{endpoint}: backend was evicted");
    }
}
