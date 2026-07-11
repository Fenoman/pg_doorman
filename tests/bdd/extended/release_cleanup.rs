//! Steps for the release-query client-RST scenario: wedge pg_doorman into
//! writing a fully-buffered backend response to a client that stopped
//! reading, then verify that a TCP RST at that point does not leak
//! session-local backend state (advisory locks) into the idle pool.

use std::time::{Duration, Instant};

use cucumber::when;

use crate::pg_connection::PgConnection;
use crate::world::DoormanWorld;

/// Reduce the client-side kernel receive buffer so pg_doorman's writes to
/// this session start blocking after a few kilobytes of unread responses.
#[when(regex = r#"^we shrink receive buffer to (\d+) bytes for session "([^"]+)"$"#)]
pub async fn shrink_receive_buffer_for_session(
    world: &mut DoormanWorld,
    bytes: usize,
    session_name: String,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);
    conn.shrink_recv_buffer(bytes)
        .expect("Failed to shrink receive buffer");
}

/// Send `count` extended-protocol batches (Parse with a unique client-side
/// statement name over the same SQL, Bind, Execute, Sync) without reading
/// any responses. Stops early once the sends no longer complete: at that
/// point pg_doorman is blocked writing responses to the non-reading client
/// and has stopped draining the client socket, which is exactly the state
/// the scenario needs.
#[when(regex = r#"^we send (\d+) unread extended query batches "([^"]+)" to session "([^"]+)"$"#)]
pub async fn send_unread_extended_query_batches(
    world: &mut DoormanWorld,
    count: usize,
    query: String,
    session_name: String,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);
    let send_timeout = Duration::from_secs(2);
    for index in 0..count {
        let statement = format!("unread_batch_{index}");
        let sent = async {
            conn.send_parse(&statement, &query).await?;
            conn.send_bind("", &statement, Vec::new()).await?;
            conn.send_execute("", 0).await?;
            conn.send_sync().await
        };
        match tokio::time::timeout(send_timeout, sent).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => panic!("Failed to send batch {index}: {err}"),
            // The client socket is full: pg_doorman is wedged writing to us.
            Err(_) => break,
        }
    }
}

/// Wait until the flood settles into the state the RST must interrupt:
/// PostgreSQL shows exactly one backend that is idle in ClientRead while
/// holding a granted advisory lock, and SHOW SERVERS reports the same
/// backend as an active checkout that pg_doorman is not exchanging bytes
/// with. Three consecutive observations of the same pid are required so a
/// transient between-batches snapshot cannot satisfy the barrier.
#[when(
    regex = r#"^we wait until the advisory-lock backend is parked in ClientRead and reported active by admin session "([^"]+)"$"#
)]
pub async fn wait_for_parked_advisory_lock_backend(
    world: &mut DoormanWorld,
    admin_session: String,
) {
    let pg_port = world.pg_port.expect("PostgreSQL must be running");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut consecutive = 0;
    let mut last_pid: Option<String> = None;

    loop {
        let pid = query_parked_advisory_lock_pid(pg_port);
        let confirmed = match pid {
            Some(ref pid) => {
                let admin = super::helpers::get_session(&mut world.named_sessions, &admin_session);
                show_servers_reports_active_idle(admin, pid).await
            }
            None => false,
        };

        if confirmed && pid == last_pid {
            consecutive += 1;
        } else if confirmed {
            consecutive = 1;
            last_pid = pid;
        } else {
            consecutive = 0;
            last_pid = None;
        }

        if consecutive >= 3 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "backend never settled into idle-in-ClientRead with a granted advisory lock \
             while SHOW SERVERS reports it as an active checkout"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Return the pid of the single PostgreSQL backend that is idle in
/// ClientRead while holding a granted advisory lock, or `None` if there is
/// no such backend or more than one.
fn query_parked_advisory_lock_pid(pg_port: u16) -> Option<String> {
    const PARKED_BACKEND_SQL: &str = "SELECT a.pid FROM pg_stat_activity a \
         WHERE a.state = 'idle' AND a.wait_event = 'ClientRead' \
         AND EXISTS (SELECT 1 FROM pg_locks l WHERE l.pid = a.pid \
         AND l.locktype = 'advisory' AND l.granted)";

    let mut command = if crate::utils::is_root() {
        let mut c = std::process::Command::new("sudo");
        c.arg("-u").arg("postgres").arg("psql");
        c
    } else {
        std::process::Command::new("psql")
    };
    let output = command
        .args([
            "-h",
            "127.0.0.1",
            "-p",
            &pg_port.to_string(),
            "-U",
            "postgres",
            "-d",
            "postgres",
            "-t",
            "-A",
            "-c",
            PARKED_BACKEND_SQL,
        ])
        .env("PGSSLMODE", "disable")
        .output()
        .expect("Failed to run psql against PostgreSQL");
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let pids: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    match pids.as_slice() {
        [pid] => Some((*pid).to_string()),
        _ => None,
    }
}

/// Run SHOW SERVERS on the admin session and check that the row for
/// `backend_pid` reports state "active" (checked out) and wait "idle"
/// (no backend I/O in flight).
async fn show_servers_reports_active_idle(admin: &mut PgConnection, backend_pid: &str) -> bool {
    admin
        .send_simple_query("SHOW SERVERS")
        .await
        .expect("Failed to send SHOW SERVERS");

    let mut matched = false;
    loop {
        let (msg_type, data) = admin
            .read_message()
            .await
            .expect("Failed to read SHOW SERVERS response");
        match msg_type {
            'D' => {
                let fields = super::helpers::parse_datarow_fields(&data);
                // Columns: server_id, server_process_id, database_name,
                // user, application_name, tls, state, wait, ...
                if fields.len() > 7
                    && fields[1] == backend_pid
                    && fields[6] == "active"
                    && fields[7] == "idle"
                {
                    matched = true;
                }
            }
            'E' => panic!(
                "SHOW SERVERS returned an error: {:?}",
                String::from_utf8_lossy(&data)
            ),
            'Z' => break,
            _ => {}
        }
    }
    matched
}
