use crate::pg_connection::PgConnection;
use crate::world::DoormanWorld;
use cucumber::then;
use std::time::Duration;

use super::helpers::ProtocolMessages;

async fn read_until(conn: &mut PgConnection, endpoint: &str, terminal: &str) -> ProtocolMessages {
    let mut messages = Vec::new();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(3), conn.read_message())
            .await
            .unwrap_or_else(|_| panic!("{endpoint}: waiting for {terminal}"))
            .unwrap_or_else(|err| panic!("{endpoint}: protocol connection closed: {err}"));
        let done = terminal.contains(message.0);
        messages.push(message);
        if done {
            return messages;
        }
    }
}

fn kinds(messages: &ProtocolMessages) -> String {
    messages.iter().map(|(kind, _)| *kind).collect()
}

fn values(messages: &ProtocolMessages) -> Vec<String> {
    messages
        .iter()
        .filter(|(kind, _)| *kind == 'D')
        .map(|(_, body)| {
            assert_eq!(&body[..2], &1i16.to_be_bytes());
            let len = i32::from_be_bytes(body[2..6].try_into().unwrap()) as usize;
            String::from_utf8(body[6..6 + len].to_vec()).unwrap()
        })
        .collect()
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

async fn simple(conn: &mut PgConnection, endpoint: &str, query: &str) -> ProtocolMessages {
    conn.send_simple_query(query).await.unwrap();
    read_until(conn, endpoint, "Z").await
}

async fn parse(conn: &mut PgConnection, endpoint: &str, name: &str, query: &str) {
    conn.send_parse(name, query).await.unwrap();
    conn.send_sync().await.unwrap();
    assert_eq!(
        kinds(&read_until(conn, endpoint, "Z").await),
        "1Z",
        "{endpoint}"
    );
}

async fn bind_execute(conn: &mut PgConnection, name: &str) {
    conn.send_bind("", name, Vec::new()).await.unwrap();
    conn.send_execute("", 0).await.unwrap();
}

async fn execute(conn: &mut PgConnection, endpoint: &str, name: &str) -> ProtocolMessages {
    bind_execute(conn, name).await;
    conn.send_sync().await.unwrap();
    read_until(conn, endpoint, "Z").await
}

#[then(
    regex = r#"^a fresh "([^"]+)" Parse observes DDL and rollback while an old Bind keeps its result shape$"#
)]
pub async fn fresh_parse_after_ddl(world: &mut DoormanWorld, name: String) {
    let fresh_name = if name == "anonymous" { "" } else { "fresh" };
    let query = "SELECT * FROM fresh_parse_rows";
    for (endpoint, conn) in [
        ("PostgreSQL", world.pg_conn.as_mut().unwrap()),
        ("pg_doorman", world.doorman_conn.as_mut().unwrap()),
    ] {
        assert_eq!(
            sqlstate(
                &simple(
                    conn,
                    endpoint,
                    "BEGIN; CREATE TEMP TABLE fresh_parse_rows AS SELECT 7 AS value",
                )
                .await
            ),
            None,
            "{endpoint}"
        );
        let pid = values(&simple(conn, endpoint, "SELECT pg_backend_pid()").await);
        parse(conn, endpoint, "old", query).await;
        assert_eq!(values(&execute(conn, endpoint, "old").await), ["7"]);
        simple(conn, endpoint, "SAVEPOINT before_ddl").await;
        assert_eq!(
            sqlstate(
                &simple(
                    conn,
                    endpoint,
                    "ALTER TABLE fresh_parse_rows ADD COLUMN note text DEFAULT 'new'",
                )
                .await
            ),
            None,
            "{endpoint}"
        );
        simple(conn, endpoint, "SAVEPOINT after_ddl").await;
        assert_eq!(
            sqlstate(&execute(conn, endpoint, "old").await),
            Some("0A000".to_string()),
            "{endpoint}: old Bind must retain its original result descriptor"
        );
        simple(conn, endpoint, "ROLLBACK TO after_ddl").await;
        parse(conn, endpoint, fresh_name, query).await;
        let response = execute(conn, endpoint, fresh_name).await;
        assert_eq!(
            sqlstate(&response),
            None,
            "{endpoint}: fresh Parse reused an old plan"
        );
        let rows: Vec<_> = response.iter().filter(|(kind, _)| *kind == 'D').collect();
        assert_eq!(rows.len(), 1, "{endpoint}");
        // Two text fields: integer 7 and the newly added column's default.
        assert_eq!(rows[0].1, b"\0\x02\0\0\0\x017\0\0\0\x03new", "{endpoint}");
        assert_eq!(
            sqlstate(&execute(conn, endpoint, "old").await),
            Some("0A000".to_string()),
            "{endpoint}: fresh Parse must not overwrite the old logical statement"
        );
        simple(conn, endpoint, "ROLLBACK TO before_ddl").await;
        parse(conn, endpoint, "after_rollback", query).await;
        assert_eq!(
            values(&execute(conn, endpoint, "after_rollback").await),
            ["7"]
        );
        assert_eq!(values(&execute(conn, endpoint, "old").await), ["7"]);
        assert_eq!(
            values(&simple(conn, endpoint, "SELECT pg_backend_pid()").await),
            pid
        );
        simple(conn, endpoint, "ROLLBACK").await;
    }
}

#[then(
    regex = r#"^a cold "([^"]+)" after "([^"]+)" preserves the open protocol cycle using "([^"]+)"$"#
)]
pub async fn cold_after_flush(
    world: &mut DoormanWorld,
    operation: String,
    boundary: String,
    terminal: String,
) {
    for (endpoint, conn) in [
        ("PostgreSQL", world.pg_conn.as_mut().unwrap()),
        ("pg_doorman", world.doorman_conn.as_mut().unwrap()),
    ] {
        simple(
            conn,
            endpoint,
            "CREATE TEMP TABLE cold_bind_marker AS SELECT 7 AS value",
        )
        .await;
        let before_pid = values(&simple(conn, endpoint, "SELECT pg_backend_pid()").await);
        simple(conn, endpoint, "BEGIN; SAVEPOINT cold_bind_savepoint").await;
        parse(conn, endpoint, "cold", "SELECT 42").await;
        let active_query = if boundary == "PortalSuspended" {
            "SELECT generate_series(1,3)"
        } else {
            "SELECT 55"
        };
        parse(conn, endpoint, "active", active_query).await;
        match boundary.as_str() {
            "Describe" => conn.send_describe('S', "active").await.unwrap(),
            "PortalSuspended" => {
                conn.send_bind("cursor", "active", Vec::new())
                    .await
                    .unwrap();
                conn.send_execute("cursor", 1).await.unwrap();
            }
            "Execute" => bind_execute(conn, "active").await,
            _ => panic!("Unknown Flush boundary {boundary}"),
        }
        conn.send_flush().await.unwrap();
        let prefix = read_until(conn, endpoint, "CTnsE").await;
        assert_eq!(
            kinds(&prefix),
            match boundary.as_str() {
                "Describe" => "tT",
                "PortalSuspended" => "2Ds",
                _ => "2DC",
            },
            "{endpoint}"
        );
        match operation.as_str() {
            "Bind" => bind_execute(conn, "cold").await,
            "Describe" => conn.send_describe('S', "cold").await.unwrap(),
            _ => panic!("Unknown cold operation {operation}"),
        }
        let mut response = if terminal == "Flush" {
            conn.send_flush().await.unwrap();
            let response = read_until(conn, endpoint, "CTnE").await;
            assert_eq!(
                kinds(&response),
                if operation == "Bind" { "2DC" } else { "tT" },
                "{endpoint}: cold operation did not complete at Flush"
            );
            response
        } else {
            assert_eq!(terminal, "Sync", "Unknown protocol terminal {terminal}");
            Vec::new()
        };
        conn.send_sync().await.unwrap();
        let synced = read_until(conn, endpoint, "Z").await;
        if terminal == "Flush" {
            assert_eq!(
                kinds(&synced),
                "Z",
                "{endpoint}: unexpected response after Flush"
            );
        }
        response.extend(synced);
        assert_eq!(
            kinds(&response),
            if operation == "Bind" { "2DCZ" } else { "tTZ" },
            "{endpoint}"
        );
        assert_eq!(
            response.last().unwrap().1,
            b"T",
            "{endpoint}: internal reprepare ended the transaction"
        );
        if operation == "Bind" {
            assert_eq!(values(&response), ["42"], "{endpoint}");
        }
        if boundary == "PortalSuspended" {
            conn.send_execute("cursor", 1).await.unwrap();
            conn.send_sync().await.unwrap();
            let continued = read_until(conn, endpoint, "Z").await;
            assert_eq!(
                kinds(&continued),
                "DsZ",
                "{endpoint}: suspended portal was lost"
            );
            assert_eq!(values(&continued), ["2"], "{endpoint}");
        }
        simple(conn, endpoint, "ROLLBACK TO cold_bind_savepoint; COMMIT").await;
        assert_eq!(
            values(&simple(conn, endpoint, "SELECT value FROM cold_bind_marker").await),
            ["7"],
            "{endpoint}: warm temp state was lost"
        );
        assert_eq!(
            values(&simple(conn, endpoint, "SELECT pg_backend_pid()").await),
            before_pid,
            "{endpoint}: backend was discarded"
        );
    }
}

#[then(
    regex = r#"^cold reprepare error case "([^"]+)" preserves savepoint recovery and statement names$"#
)]
pub async fn cold_reprepare_error(world: &mut DoormanWorld, case: String) {
    for (endpoint, conn) in [
        ("PostgreSQL", world.pg_conn.as_mut().unwrap()),
        ("pg_doorman", world.doorman_conn.as_mut().unwrap()),
    ] {
        simple(
            conn,
            endpoint,
            "CREATE TEMP TABLE cold_reprepare_rows AS SELECT 7 AS value",
        )
        .await;
        simple(conn, endpoint, "BEGIN; SAVEPOINT cold_error_savepoint").await;
        parse(conn, endpoint, "keep", "SELECT 42").await;
        if case == "acknowledgement order" {
            parse(
                conn,
                endpoint,
                "stale",
                "SELECT value FROM cold_reprepare_rows",
            )
            .await;
            parse(conn, endpoint, "evict", "SELECT 55").await;
            simple(conn, endpoint, "DROP TABLE cold_reprepare_rows").await;
            conn.send_parse("prefix", "SELECT 99").await.unwrap();
            bind_execute(conn, "stale").await;
            conn.send_close('S', "keep").await.unwrap();
        } else {
            parse(
                conn,
                endpoint,
                "recover",
                "ROLLBACK TO cold_error_savepoint",
            )
            .await;
            assert_eq!(
                sqlstate(&simple(conn, endpoint, "SELECT 1/0").await),
                Some("22012".to_string())
            );
            if case == "buffered rollback" {
                bind_execute(conn, "recover").await;
            }
            bind_execute(conn, "keep").await;
        }
        conn.send_sync().await.unwrap();
        let response = read_until(conn, endpoint, "Z").await;
        match case.as_str() {
            "aborted transaction" => {
                assert_eq!(kinds(&response), "EZ", "{endpoint}");
                assert_eq!(sqlstate(&response), Some("25P02".to_string()), "{endpoint}");
            }
            "buffered rollback" => {
                assert_eq!(
                    kinds(&response),
                    "2C2DCZ",
                    "{endpoint}: reprepare overtook ROLLBACK TO"
                );
                assert_eq!(values(&response), ["42"], "{endpoint}");
                assert_eq!(response.last().unwrap().1, b"T");
            }
            "acknowledgement order" => {
                assert_eq!(
                    kinds(&response),
                    "1EZ",
                    "{endpoint}: internal ParseComplete leaked or changed the journal"
                );
                assert_eq!(sqlstate(&response), Some("42P01".to_string()), "{endpoint}");
            }
            _ => panic!("Unknown cold reprepare error case {case}"),
        }
        simple(conn, endpoint, "ROLLBACK TO cold_error_savepoint").await;
        assert_eq!(
            values(&execute(conn, endpoint, "keep").await),
            ["42"],
            "{endpoint}: confirmed logical statement was lost"
        );
        if case == "acknowledgement order" {
            assert_eq!(
                values(&execute(conn, endpoint, "prefix").await),
                ["99"],
                "{endpoint}: acknowledged prefix was rolled back"
            );
            assert_eq!(
                values(&execute(conn, endpoint, "stale").await),
                ["7"],
                "{endpoint}: reprepare error removed the logical name"
            );
        }
        simple(conn, endpoint, "ROLLBACK").await;
        assert_eq!(
            values(&simple(conn, endpoint, "SELECT value FROM cold_reprepare_rows").await),
            ["7"],
            "{endpoint}: backend temp state was lost"
        );
    }
}

#[then(regex = r#"^a hidden reprepare before "([^"]+)" preserves synthetic ParseComplete order$"#)]
pub async fn cold_reprepare_streaming(world: &mut DoormanWorld, case: String) {
    for (endpoint, conn) in [
        ("PostgreSQL", world.pg_conn.as_mut().unwrap()),
        ("pg_doorman", world.doorman_conn.as_mut().unwrap()),
    ] {
        let (query, count) = if case == "large row" {
            ("SELECT repeat('x',2097152)", 1)
        } else {
            ("SELECT generate_series(1,100000)", 100000)
        };
        parse(conn, endpoint, "streamed", query).await;
        parse(conn, endpoint, "seed", "SELECT 42").await;
        conn.send_bind("", "streamed", Vec::new()).await.unwrap();
        conn.send_parse("cached", "SELECT 42").await.unwrap();
        conn.send_execute("", 0).await.unwrap();
        conn.send_sync().await.unwrap();
        let response = read_until(conn, endpoint, "Z").await;
        assert_eq!(
            response.iter().filter(|(kind, _)| *kind == '1').count(),
            1,
            "{endpoint}: hidden reprepare leaked a ParseComplete"
        );
        assert_eq!(
            response.iter().filter(|(kind, _)| *kind == 'D').count(),
            count,
            "{endpoint}"
        );
        assert_eq!(
            response
                .iter()
                .take(3)
                .map(|(kind, _)| *kind)
                .collect::<String>(),
            "21D",
            "{endpoint}: frontend ParseComplete was out of order"
        );
        assert!(sqlstate(&response).is_none(), "{endpoint}");
        if case == "large row" {
            assert_eq!(values(&response), ["x".repeat(2097152)], "{endpoint}");
        }
        assert_eq!(
            values(&execute(conn, endpoint, "cached").await),
            ["42"],
            "{endpoint}: cached Parse was not committed"
        );
    }
}
