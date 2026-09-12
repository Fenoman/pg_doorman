use crate::pg_connection::PgConnection;
use crate::world::DoormanWorld;
use cucumber::then;
use std::time::Duration;

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
