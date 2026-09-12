use crate::pg_connection::PgConnection;
use crate::world::DoormanWorld;
use cucumber::then;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::helpers::ProtocolMessages;

async fn read_until(conn: &mut PgConnection, endpoint: &str, terminal: char) -> ProtocolMessages {
    let mut messages = Vec::new();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(3), conn.read_message())
            .await
            .unwrap_or_else(|_| panic!("{endpoint}: COPY response timed out"))
            .unwrap_or_else(|err| panic!("{endpoint}: COPY connection closed: {err}"));
        let done = message.0 == terminal;
        assert_ne!(message.0, 'E', "{endpoint}: unexpected error: {message:?}");
        messages.push(message);
        if done {
            return messages;
        }
    }
}

async fn simple(conn: &mut PgConnection, endpoint: &str, query: &str) -> ProtocolMessages {
    conn.send_simple_query(query).await.unwrap();
    read_until(conn, endpoint, 'Z').await
}

fn first_value(messages: &ProtocolMessages) -> String {
    let (_, body) = messages.iter().find(|(kind, _)| *kind == 'D').unwrap();
    assert_eq!(&body[..2], &1i16.to_be_bytes());
    let len = i32::from_be_bytes(body[2..6].try_into().unwrap()) as usize;
    String::from_utf8(body[6..6 + len].to_vec()).unwrap()
}

#[then(regex = r#"^COPY FROM via "(simple|extended)" preserves rows across "(Flush|Sync)"$"#)]
pub async fn copy_control_preserves_rows(
    world: &mut DoormanWorld,
    protocol: String,
    control: String,
) {
    for (endpoint, conn) in [
        ("PostgreSQL", world.pg_conn.as_mut().unwrap()),
        ("pg_doorman", world.doorman_conn.as_mut().unwrap()),
    ] {
        simple(
            conn,
            endpoint,
            "BEGIN; CREATE TEMP TABLE copy_control_rows(value integer)",
        )
        .await;
        let pid = first_value(&simple(conn, endpoint, "SELECT pg_backend_pid()").await);
        if protocol == "simple" {
            conn.send_simple_query("COPY copy_control_rows FROM STDIN")
                .await
                .unwrap();
        } else {
            conn.send_parse("copy_input", "COPY copy_control_rows FROM STDIN")
                .await
                .unwrap();
            conn.send_bind("", "copy_input", Vec::new()).await.unwrap();
            conn.send_execute("", 0).await.unwrap();
            conn.send_flush().await.unwrap();
        }
        read_until(conn, endpoint, 'G').await;
        conn.send_copy_data(b"7\n").await.unwrap();
        match control.as_str() {
            "Flush" => conn.send_flush().await.unwrap(),
            "Sync" => conn.send_sync().await.unwrap(),
            _ => unreachable!(),
        }
        conn.send_copy_data(b"11\n").await.unwrap();
        conn.send_copy_done().await.unwrap();
        if protocol == "extended" {
            conn.send_flush().await.unwrap();
        }

        let complete = read_until(conn, endpoint, 'C').await;
        assert_eq!(
            complete,
            vec![('C', b"COPY 2\0".to_vec())],
            "{endpoint}: COPY lost or duplicated buffered rows"
        );
        if protocol == "extended" {
            // A Sync consumed during COPY does not replace the final Sync.
            conn.send_sync().await.unwrap();
        }
        assert_eq!(
            read_until(conn, endpoint, 'Z').await,
            vec![('Z', b"T".to_vec())],
            "{endpoint}: COPY changed the transaction boundary"
        );
        assert_eq!(
            first_value(&simple(conn, endpoint, "SELECT sum(value) FROM copy_control_rows").await),
            "18",
            "{endpoint}: COPY row contents changed"
        );
        assert_eq!(
            first_value(&simple(conn, endpoint, "SELECT pg_backend_pid()").await),
            pid,
            "{endpoint}: COPY evicted the backend"
        );
        simple(conn, endpoint, "ROLLBACK").await;
    }
}

#[then(
    regex = r#"^cancellation during idle COPY via "(simple|extended_sync|extended_flush)" preserves the client session$"#
)]
pub async fn idle_copy_cancel_preserves_session(world: &mut DoormanWorld, protocol: String) {
    let pg_addr = format!("127.0.0.1:{}", world.pg_port.unwrap());
    let doorman_addr = format!("127.0.0.1:{}", world.doorman_port.unwrap());
    for (endpoint, addr, conn) in [
        ("PostgreSQL", pg_addr, world.pg_conn.as_mut().unwrap()),
        (
            "pg_doorman",
            doorman_addr,
            world.doorman_conn.as_mut().unwrap(),
        ),
    ] {
        simple(
            conn,
            endpoint,
            "BEGIN; CREATE TEMP TABLE idle_copy_rows(value integer); SAVEPOINT before_copy",
        )
        .await;
        let pid = first_value(&simple(conn, endpoint, "SELECT pg_backend_pid()").await);
        if protocol == "simple" {
            conn.send_simple_query("COPY idle_copy_rows FROM STDIN")
                .await
                .unwrap();
        } else {
            conn.send_parse("idle_copy", "COPY idle_copy_rows FROM STDIN")
                .await
                .unwrap();
            conn.send_bind("", "idle_copy", Vec::new()).await.unwrap();
            conn.send_execute("", 0).await.unwrap();
            if protocol == "extended_sync" {
                conn.send_sync().await.unwrap();
            } else {
                conn.send_flush().await.unwrap();
            }
        }
        read_until(conn, endpoint, 'G').await;
        // Enter the idle backend monitor before the cancellation arrives.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut cancel = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut request = Vec::with_capacity(16);
        for value in [
            16i32,
            80877102,
            conn.get_process_id().unwrap(),
            conn.get_secret_key().unwrap(),
        ] {
            request.extend_from_slice(&value.to_be_bytes());
        }
        cancel.write_all(&request).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), cancel.read(&mut [0]))
                .await
                .expect("CancelRequest connection did not close")
                .unwrap(),
            0
        );
        // Advance COPY's input so PostgreSQL can process the pending cancel.
        // The application then waits for the error without sending CopyDone.
        conn.send_copy_data(&b"7\n".repeat(5000)).await.unwrap();
        let (kind, error) = tokio::time::timeout(Duration::from_secs(3), conn.read_message())
            .await
            .unwrap_or_else(|_| panic!("{endpoint}: COPY cancellation response timed out"))
            .unwrap_or_else(|err| panic!("{endpoint}: COPY cancellation disconnected: {err}"));
        assert_eq!(kind, 'E', "{endpoint}: expected COPY cancellation error");
        assert!(
            error
                .split(|&byte| byte == 0)
                .any(|field| field == b"C57014"),
            "{endpoint}: expected SQLSTATE 57014, got {error:?}"
        );
        // COPY input may already be in flight when the error arrives. Both
        // protocols discard it; extended also ignores other messages until Sync.
        conn.send_copy_data(b"11\n").await.unwrap();
        conn.send_copy_done().await.unwrap();
        conn.send_copy_fail("COPY already cancelled").await.unwrap();
        if protocol != "simple" {
            conn.send_parse("ignored_after_copy", "SELECT 99")
                .await
                .unwrap();
            conn.send_flush().await.unwrap();
            // An earlier Sync consumed by COPY does not synchronize its error.
            conn.send_sync().await.unwrap();
        }
        assert_eq!(
            read_until(conn, endpoint, 'Z').await,
            vec![('Z', b"E".to_vec())],
            "{endpoint}: cancellation lost the explicit transaction"
        );
        simple(conn, endpoint, "ROLLBACK TO before_copy").await;
        assert_eq!(
            first_value(
                &simple(
                    conn,
                    endpoint,
                    "SELECT pg_backend_pid()::text || ':' || count(*)::text FROM idle_copy_rows",
                )
                .await
            ),
            format!("{pid}:0"),
            "{endpoint}: cancellation lost the backend or temp state"
        );
        // Switching back to simple COPY must not inherit the failed extended
        // Execute's completion mode or rows buffered before its cancellation.
        conn.send_simple_query("COPY idle_copy_rows FROM STDIN")
            .await
            .unwrap();
        read_until(conn, endpoint, 'G').await;
        conn.send_copy_data(b"23\n").await.unwrap();
        conn.send_copy_done().await.unwrap();
        assert_eq!(
            read_until(conn, endpoint, 'Z').await,
            vec![('C', b"COPY 1\0".to_vec()), ('Z', b"T".to_vec())],
            "{endpoint}: next simple COPY did not finish normally"
        );
        assert_eq!(
            first_value(&simple(conn, endpoint, "SELECT sum(value) FROM idle_copy_rows").await),
            "23",
            "{endpoint}: next COPY inherited cancelled rows"
        );
        simple(conn, endpoint, "ROLLBACK").await;
        // Delayed COPY input is also harmless after the backend lease ends.
        conn.send_copy_data(b"31\n").await.unwrap();
        conn.send_copy_done().await.unwrap();
        conn.send_copy_fail("COPY already cancelled").await.unwrap();
        assert_eq!(
            first_value(&simple(conn, endpoint, "SELECT 42").await),
            "42",
            "{endpoint}: client could not start its next transaction"
        );
    }
}
