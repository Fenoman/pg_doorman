use crate::pg_connection::PgConnection;
use crate::world::DoormanWorld;
use cucumber::{then, when};

#[when(
    regex = r#"^we create session "([^"]+)" to pg_doorman as "([^"]+)" with password "([^"]*)" and database "([^"]+)" and store backend key$"#
)]
pub async fn create_named_session_with_backend_key(
    world: &mut DoormanWorld,
    session_name: String,
    user: String,
    password: String,
    database: String,
) {
    let doorman_port = world.doorman_port.expect("pg_doorman not started");
    let doorman_addr = format!("127.0.0.1:{doorman_port}");

    // Connect to pg_doorman
    let mut conn = PgConnection::connect(&doorman_addr)
        .await
        .expect("Failed to connect to pg_doorman");
    conn.send_startup(&user, &database)
        .await
        .expect("Failed to send startup to pg_doorman");
    conn.authenticate(&user, &password)
        .await
        .expect("Failed to authenticate to pg_doorman");

    // Store backend key data (process_id and secret_key from BackendKeyData)
    if let (Some(process_id), Some(secret_key)) = (conn.get_process_id(), conn.get_secret_key()) {
        world
            .session_backend_pids
            .insert(session_name.clone(), process_id);
        world
            .session_secret_keys
            .insert(session_name.clone(), secret_key);
        eprintln!(
            "Session '{session_name}': stored backend_pid={process_id}, secret_key={secret_key}"
        );
    } else {
        panic!("Session '{session_name}': BackendKeyData not received during authentication");
    }

    world.named_sessions.insert(session_name, conn);
}

#[when(regex = r#"^we send cancel request for session "([^"]+)"$"#)]
pub async fn send_cancel_request_for_session(world: &mut DoormanWorld, session_name: String) {
    let doorman_port = world.doorman_port.expect("pg_doorman not started");
    let doorman_addr = format!("127.0.0.1:{doorman_port}");

    let process_id = world
        .session_backend_pids
        .get(&session_name)
        .unwrap_or_else(|| panic!("No backend_pid stored for session '{session_name}'"));

    let secret_key = world
        .session_secret_keys
        .get(&session_name)
        .unwrap_or_else(|| panic!("No secret_key stored for session '{session_name}'"));

    eprintln!(
        "Sending cancel request for session '{session_name}': process_id={process_id}, secret_key={secret_key}"
    );

    PgConnection::send_cancel_request(&doorman_addr, *process_id, *secret_key)
        .await
        .expect("Failed to send cancel request");

    // Give the server a moment to process the cancel
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
}

#[then(regex = r#"^session "([^"]+)" should receive cancel error containing "([^"]+)"$"#)]
pub async fn session_should_receive_cancel_error(
    world: &mut DoormanWorld,
    session_name: String,
    expected_text: String,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    // Read messages until we get an error or ReadyForQuery
    let mut error_found = false;
    let mut error_message = String::new();

    loop {
        let (msg_type, data) = conn.read_message().await.expect("Failed to read message");

        match msg_type {
            'E' => {
                // Error message - parse it
                error_message = String::from_utf8_lossy(&data).to_string();
                error_found = true;
                eprintln!("Session '{session_name}': received error: {error_message}");
            }
            'Z' => {
                // ReadyForQuery - done
                break;
            }
            _ => {
                // Other messages - continue
            }
        }
    }

    assert!(
        error_found,
        "Session '{session_name}': expected to receive an error, but none was received"
    );

    assert!(
        error_message
            .to_lowercase()
            .contains(&expected_text.to_lowercase()),
        "Session '{session_name}': expected error to contain '{expected_text}', got '{error_message}'"
    );
}

#[when(regex = r#"^we send cancel request with process_id (\d+) and secret_key (\d+)$"#)]
pub async fn send_cancel_request_with_fabricated_credentials(
    world: &mut DoormanWorld,
    process_id: i32,
    secret_key: i32,
) {
    let doorman_port = world.doorman_port.expect("pg_doorman not started");
    let doorman_addr = format!("127.0.0.1:{doorman_port}");

    PgConnection::send_cancel_request(&doorman_addr, process_id, secret_key)
        .await
        .expect("Failed to send cancel request");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
}

#[when(regex = r#"^we send cancel request for session "([^"]+)" with wrong secret_key (\d+)$"#)]
pub async fn send_cancel_request_wrong_secret_key(
    world: &mut DoormanWorld,
    session_name: String,
    wrong_secret_key: i32,
) {
    let doorman_port = world.doorman_port.expect("pg_doorman not started");
    let doorman_addr = format!("127.0.0.1:{doorman_port}");

    let process_id = *world
        .session_backend_pids
        .get(&session_name)
        .unwrap_or_else(|| panic!("No backend_pid stored for session '{session_name}'"));

    PgConnection::send_cancel_request(&doorman_addr, process_id, wrong_secret_key)
        .await
        .expect("Failed to send cancel request");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
}

#[then(regex = r#"^session "([^"]+)" should complete without error$"#)]
pub async fn session_should_complete_without_error(world: &mut DoormanWorld, session_name: String) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    // Read messages until ReadyForQuery, checking for errors
    let mut error_found = false;
    let mut error_message = String::new();

    loop {
        let (msg_type, data) = conn.read_message().await.expect("Failed to read message");

        match msg_type {
            'E' => {
                // Error message - this is unexpected
                error_message = String::from_utf8_lossy(&data).to_string();
                error_found = true;
                eprintln!("Session '{session_name}': received unexpected error: {error_message}");
            }
            'Z' => {
                // ReadyForQuery - done
                eprintln!("Session '{session_name}': query completed successfully");
                break;
            }
            _ => {
                // Other messages - continue (T=RowDescription, D=DataRow, C=CommandComplete, etc.)
            }
        }
    }

    assert!(
        !error_found,
        "Session '{session_name}': expected query to complete without error, but got: {error_message}"
    );
}
