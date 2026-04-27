use crate::pg_connection::PgConnection;
use crate::world::DoormanWorld;
use cucumber::{then, when};

#[when(
    regex = r#"^we create session "([^"]+)" to pg_doorman as "([^"]+)" with password "([^"]*)" and database "([^"]+)"$"#
)]
pub async fn create_named_session(
    world: &mut DoormanWorld,
    session_name: String,
    user: String,
    password: String,
    database: String,
) {
    let doorman_port = world.doorman_port.expect("pg_doorman not started");
    let doorman_addr = format!("127.0.0.1:{}", doorman_port);

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

    world.named_sessions.insert(session_name, conn);
}

#[when(
    regex = r#"^we create extended session "([^"]+)" to pg_doorman as "([^"]+)" with password "([^"]*)" and database "([^"]+)"$"#
)]
pub async fn create_extended_named_session(
    world: &mut DoormanWorld,
    session_name: String,
    user: String,
    password: String,
    database: String,
) {
    create_named_session(world, session_name, user, password, database).await;
}

#[when(
    regex = r#"^we create TLS session "([^"]+)" to pg_doorman as "([^"]+)" with password "([^"]*)" and database "([^"]+)"$"#
)]
pub async fn create_tls_named_session(
    world: &mut DoormanWorld,
    session_name: String,
    user: String,
    password: String,
    database: String,
) {
    let doorman_port = world.doorman_port.expect("pg_doorman not started");
    let doorman_addr = format!("127.0.0.1:{}", doorman_port);

    let mut conn = PgConnection::connect(&doorman_addr)
        .await
        .expect("Failed to connect to pg_doorman");
    conn.upgrade_to_tls()
        .await
        .expect("Failed to upgrade to TLS");
    conn.send_startup(&user, &database)
        .await
        .expect("Failed to send startup to pg_doorman");
    conn.authenticate(&user, &password)
        .await
        .expect("Failed to authenticate to pg_doorman");

    world.named_sessions.insert(session_name, conn);
}

#[when(
    regex = r#"^we create session "([^"]+)" to postgres as "([^"]+)" with password "([^"]*)" and database "([^"]+)"$"#
)]
pub async fn create_named_session_to_postgres(
    world: &mut DoormanWorld,
    session_name: String,
    user: String,
    password: String,
    database: String,
) {
    let pg_port = world.pg_port.expect("PostgreSQL not started");
    let pg_addr = format!("127.0.0.1:{}", pg_port);

    // Connect to PostgreSQL directly
    let mut conn = PgConnection::connect(&pg_addr)
        .await
        .expect("Failed to connect to PostgreSQL");
    conn.send_startup(&user, &database)
        .await
        .expect("Failed to send startup to PostgreSQL");
    conn.authenticate(&user, &password)
        .await
        .expect("Failed to authenticate to PostgreSQL");

    world.named_sessions.insert(session_name, conn);
}

#[when(regex = r#"^we send SimpleQuery "([^"]+)" to session "([^"]+)"$"#)]
#[then(regex = r#"^we send SimpleQuery "([^"]+)" to session "([^"]+)"$"#)]
pub async fn send_simple_query_to_session(
    world: &mut DoormanWorld,
    query: String,
    session_name: String,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    conn.send_simple_query(&query)
        .await
        .expect("Failed to send query");

    // Read all messages until ReadyForQuery
    let messages = conn
        .read_all_messages_until_ready()
        .await
        .expect("Failed to read messages");

    world
        .session_messages
        .insert(session_name.clone(), messages);
}

#[when(regex = r#"^we send SimpleQuery "([^"]+)" to session "([^"]+)" and store backend_pid$"#)]
pub async fn send_simple_query_and_store_backend_pid(
    world: &mut DoormanWorld,
    query: String,
    session_name: String,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    conn.send_simple_query(&query)
        .await
        .expect("Failed to send query");

    // Read messages and parse backend_pid
    let mut backend_pid: Option<i32> = None;
    loop {
        let (msg_type, data) = conn.read_message().await.expect("Failed to read message");

        match msg_type {
            'T' => {
                // RowDescription - skip
            }
            'D' => {
                backend_pid = super::helpers::parse_first_datarow_int(&data);
            }
            'Z' => break,
            'E' => {
                eprintln!(
                    "Error received (expected for bad sql): {:?}",
                    String::from_utf8_lossy(&data)
                );
            }
            _ => {}
        }
    }

    if let Some(pid) = backend_pid {
        world.session_backend_pids.insert(session_name, pid);
    }
}

#[when(
    regex = r#"^we send SimpleQuery "([^"]+)" to session "([^"]+)" and store backend_pid as "([^"]+)"$"#
)]
pub async fn send_simple_query_and_store_named_backend_pid(
    world: &mut DoormanWorld,
    query: String,
    session_name: String,
    pid_name: String,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    conn.send_simple_query(&query)
        .await
        .expect("Failed to send query");

    // Read messages and parse backend_pid
    let mut backend_pid: Option<i32> = None;
    loop {
        let (msg_type, data) = conn.read_message().await.expect("Failed to read message");

        match msg_type {
            'T' => {
                // RowDescription - skip
            }
            'D' => {
                backend_pid = super::helpers::parse_first_datarow_int(&data);
            }
            'Z' => break,
            'E' => {
                eprintln!(
                    "Error received (expected for bad sql): {:?}",
                    String::from_utf8_lossy(&data)
                );
            }
            _ => {}
        }
    }

    if let Some(pid) = backend_pid {
        world
            .named_backend_pids
            .insert((session_name, pid_name), pid);
    }
}

#[when(regex = r#"^we sleep (\d+)ms$"#)]
pub async fn sleep_ms(_world: &mut DoormanWorld, ms: String) {
    let duration = ms.parse::<u64>().expect("Invalid sleep duration");
    tokio::time::sleep(tokio::time::Duration::from_millis(duration)).await;
}

#[when(regex = r#"^we sleep for (\d+) milliseconds$"#)]
pub async fn sleep_for_milliseconds(_world: &mut DoormanWorld, ms: String) {
    let duration = ms.parse::<u64>().expect("Invalid sleep duration");
    tokio::time::sleep(tokio::time::Duration::from_millis(duration)).await;
}

#[when(regex = r#"^we send SimpleQuery "([^"]+)" to session "([^"]+)" without waiting$"#)]
pub async fn send_simple_query_to_session_without_waiting(
    world: &mut DoormanWorld,
    query: String,
    session_name: String,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    conn.send_simple_query(&query)
        .await
        .expect("Failed to send query");
    // Don't wait for response - just send the query
}

#[then(regex = r#"^we read SimpleQuery response from session "([^"]+)" within (\d+)ms$"#)]
pub async fn read_simple_query_response_within_timeout(
    world: &mut DoormanWorld,
    session_name: String,
    timeout_ms: u64,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    let duration = std::time::Duration::from_millis(timeout_ms);
    let messages = tokio::time::timeout(duration, conn.read_all_messages_until_ready())
        .await
        .unwrap_or_else(|_| panic!("Response not received within {}ms", timeout_ms))
        .expect("Failed to read messages");

    world.session_messages.insert(session_name, messages);
}

#[when(regex = r#"^we send SimpleQuery "([^"]+)" to session "([^"]+)" and store response$"#)]
#[then(regex = r#"^we send SimpleQuery "([^"]+)" to session "([^"]+)" and store response$"#)]
pub async fn send_simple_query_and_store_response(
    world: &mut DoormanWorld,
    query: String,
    session_name: String,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    conn.send_simple_query(&query)
        .await
        .expect("Failed to send query");

    let messages = conn
        .read_all_messages_until_ready()
        .await
        .expect("Failed to read messages");

    world
        .session_messages
        .insert(session_name.clone(), messages);
}

#[when(regex = r#"^we send SimpleQuery "([^"]+)" to session "([^"]+)" expecting error$"#)]
pub async fn send_simple_query_to_session_expecting_error(
    world: &mut DoormanWorld,
    query: String,
    session_name: String,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    conn.send_simple_query(&query)
        .await
        .expect("Failed to send query");

    // Read all messages until ReadyForQuery or error and store them
    let messages = conn
        .read_all_messages_until_ready()
        .await
        .expect("Failed to read messages");

    world
        .session_messages
        .insert(session_name.clone(), messages);
}

#[when(
    regex = r#"^we send SimpleQuery "([^"]+)" to session "([^"]+)" expecting error after ready$"#
)]
pub async fn send_simple_query_to_session_expecting_error_after_ready(
    world: &mut DoormanWorld,
    query: String,
    session_name: String,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    conn.send_simple_query(&query)
        .await
        .expect("Failed to send query");

    // Read all messages until ReadyForQuery AND any additional messages after (like ErrorResponse)
    let messages = conn
        .read_all_messages_until_ready_and_more()
        .await
        .expect("Failed to read messages");

    world
        .session_messages
        .insert(session_name.clone(), messages);
}

#[when(
    regex = r#"^we send SimpleQuery "([^"]+)" to session "([^"]+)" expecting connection close$"#
)]
pub async fn send_simple_query_to_session_expecting_connection_close(
    world: &mut DoormanWorld,
    query: String,
    session_name: String,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    // Try to send query - may fail if connection already closed
    if conn.send_simple_query(&query).await.is_err() {
        // Connection already closed - this is expected
        return;
    }

    // Try to read response - should fail with connection reset or return error
    match conn.read_all_messages_until_ready().await {
        Ok(messages) => {
            // Check if we got an error response (pooler shutdown message)
            let has_error = messages.iter().any(|(msg_type, _)| *msg_type == 'E');
            if has_error {
                // Store messages for potential further inspection
                world
                    .session_messages
                    .insert(session_name.clone(), messages);
                return;
            }
            panic!(
                "Expected connection close or error for session '{}', but got successful response",
                session_name
            );
        }
        Err(_) => {
            // Connection closed - this is expected
        }
    }
}

#[when(regex = r#"^we send Parse "([^"]*)" with query "([^"]+)" to session "([^"]+)"$"#)]
#[then(regex = r#"^we send Parse "([^"]*)" with query "([^"]+)" to session "([^"]+)"$"#)]
pub async fn send_parse_to_session(
    world: &mut DoormanWorld,
    name: String,
    query: String,
    session_name: String,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    conn.send_parse(&name, &query)
        .await
        .expect("Failed to send Parse");
}

#[when(
    regex = r#"^we send Bind "([^"]*)" to "([^"]*)" with params "([^"]*)" to session "([^"]+)"$"#
)]
#[then(
    regex = r#"^we send Bind "([^"]*)" to "([^"]*)" with params "([^"]*)" to session "([^"]+)"$"#
)]
pub async fn send_bind_to_session(
    world: &mut DoormanWorld,
    portal: String,
    statement: String,
    params_str: String,
    session_name: String,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    let params = super::helpers::parse_bind_params(&params_str);

    conn.send_bind(&portal, &statement, params)
        .await
        .expect("Failed to send Bind");
}

#[when(regex = r#"^we send Execute "([^"]*)" to session "([^"]+)"$"#)]
#[then(regex = r#"^we send Execute "([^"]*)" to session "([^"]+)"$"#)]
pub async fn send_execute_to_session(
    world: &mut DoormanWorld,
    portal: String,
    session_name: String,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    conn.send_execute(&portal, 0)
        .await
        .expect("Failed to send Execute");
}

#[when(regex = r#"^we send Flush to session "([^"]+)"$"#)]
#[then(regex = r#"^we send Flush to session "([^"]+)"$"#)]
pub async fn send_flush_to_session(world: &mut DoormanWorld, session_name: String) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    conn.send_flush().await.expect("Failed to send Flush");
}

#[when(regex = r#"^we send Sync to session "([^"]+)"$"#)]
#[then(regex = r#"^we send Sync to session "([^"]+)"$"#)]
pub async fn send_sync_to_session(world: &mut DoormanWorld, session_name: String) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    conn.send_sync().await.expect("Failed to send Sync");

    // Read all messages until ReadyForQuery and store them
    let messages = conn
        .read_all_messages_until_ready()
        .await
        .expect("Failed to read messages");

    world
        .session_messages
        .insert(session_name.clone(), messages);
}

fn take_next_session_message(world: &mut DoormanWorld, session_name: &str) -> (char, Vec<u8>) {
    let messages = world
        .session_messages
        .get_mut(session_name)
        .unwrap_or_else(|| panic!("No messages stored for session '{}'", session_name));

    if messages.is_empty() {
        panic!("No unread messages remain for session '{}'", session_name);
    }

    messages.remove(0)
}

fn assert_next_message_type(
    world: &mut DoormanWorld,
    session_name: &str,
    expected_type: char,
    expected_name: &str,
) -> Vec<u8> {
    let (actual_type, data) = take_next_session_message(world, session_name);
    assert_eq!(
        actual_type,
        expected_type,
        "Session '{}': expected next message {}, got {}",
        session_name,
        expected_name,
        super::helpers::format_message_details(actual_type, &data)
    );
    data
}

#[then(regex = r#"^session "([^"]+)" should receive ParseComplete$"#)]
pub async fn session_should_receive_parse_complete(world: &mut DoormanWorld, session_name: String) {
    assert_next_message_type(world, &session_name, '1', "ParseComplete");
}

#[then(regex = r#"^session "([^"]+)" should receive BindComplete$"#)]
pub async fn session_should_receive_bind_complete(world: &mut DoormanWorld, session_name: String) {
    assert_next_message_type(world, &session_name, '2', "BindComplete");
}

#[then(regex = r#"^session "([^"]+)" should receive CommandComplete "([^"]+)"$"#)]
pub async fn session_should_receive_command_complete(
    world: &mut DoormanWorld,
    session_name: String,
    expected_tag: String,
) {
    let data = assert_next_message_type(world, &session_name, 'C', "CommandComplete");
    let tag = data
        .split(|b| *b == 0)
        .next()
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    assert_eq!(
        tag, expected_tag,
        "Session '{}': expected CommandComplete tag '{}', got '{}'",
        session_name, expected_tag, tag
    );
}

#[then(regex = r#"^session "([^"]+)" should receive ReadyForQuery "([^"]+)"$"#)]
pub async fn session_should_receive_ready_for_query(
    world: &mut DoormanWorld,
    session_name: String,
    expected_status: String,
) {
    let data = assert_next_message_type(world, &session_name, 'Z', "ReadyForQuery");
    let actual_status = data.first().copied().unwrap_or_default() as char;
    let expected_status = expected_status
        .chars()
        .next()
        .expect("Expected ReadyForQuery status cannot be empty");
    assert_eq!(
        actual_status, expected_status,
        "Session '{}': expected ReadyForQuery '{}', got '{}'",
        session_name, expected_status, actual_status
    );
}

#[then(regex = r#"^session "([^"]+)" should receive RowDescription with (\d+) fields$"#)]
pub async fn session_should_receive_row_description(
    world: &mut DoormanWorld,
    session_name: String,
    expected_fields: u16,
) {
    let data = assert_next_message_type(world, &session_name, 'T', "RowDescription");
    assert!(
        data.len() >= 2,
        "Session '{}': RowDescription payload is too short",
        session_name
    );
    let actual_fields = u16::from_be_bytes([data[0], data[1]]);
    assert_eq!(
        actual_fields, expected_fields,
        "Session '{}': expected {} RowDescription fields, got {}",
        session_name, expected_fields, actual_fields
    );
}

#[then(regex = r#"^session "([^"]+)" should receive DataRow$"#)]
pub async fn session_should_receive_any_datarow(world: &mut DoormanWorld, session_name: String) {
    assert_next_message_type(world, &session_name, 'D', "DataRow");
}

#[when(regex = r#"^we close session "([^"]+)"$"#)]
#[then(regex = r#"^we close session "([^"]+)"$"#)]
#[when(regex = r#"^we disconnect session "([^"]+)"$"#)]
#[then(regex = r#"^we disconnect session "([^"]+)"$"#)]
pub async fn close_session(world: &mut DoormanWorld, session_name: String) {
    world
        .named_sessions
        .remove(&session_name)
        .unwrap_or_else(|| panic!("Session '{}' not found", session_name));
}

#[when(regex = r#"^we abort TCP connection for session "([^"]+)"$"#)]
pub async fn abort_session_tcp_connection(world: &mut DoormanWorld, session_name: String) {
    let conn = world
        .named_sessions
        .remove(&session_name)
        .unwrap_or_else(|| panic!("Session '{}' not found", session_name));

    // Abruptly close the TCP connection
    conn.abort_connection().await;
}

#[when(regex = r#"^we abort TCP connection with RST for session "([^"]+)"$"#)]
pub async fn abort_session_tcp_connection_with_rst(world: &mut DoormanWorld, session_name: String) {
    let conn = world
        .named_sessions
        .remove(&session_name)
        .unwrap_or_else(|| panic!("Session '{}' not found", session_name));

    conn.abort_connection_with_rst().await;
}

#[then(regex = r#"^session "([^"]+)" should receive DataRow with "([^"]+)"$"#)]
pub async fn session_should_receive_datarow(
    world: &mut DoormanWorld,
    session_name: String,
    expected_value: String,
) {
    // Get messages from the stored session messages
    let messages = world
        .session_messages
        .get(&session_name)
        .unwrap_or_else(|| panic!("No messages stored for session '{}'", session_name));

    let mut found_value: Option<String> = None;
    for (msg_type, data) in messages {
        match msg_type {
            'D' => {
                let fields = super::helpers::parse_datarow_fields(data);
                if let Some(first) = fields.into_iter().next() {
                    found_value = Some(first);
                    break;
                }
            }
            'E' => {
                panic!(
                    "Error received from session '{}': {:?}",
                    session_name,
                    String::from_utf8_lossy(data)
                );
            }
            _ => {}
        }
    }

    let actual_value = found_value.unwrap_or_else(|| {
        panic!(
            "No DataRow received from session '{}', expected '{}'",
            session_name, expected_value
        )
    });

    assert_eq!(
        actual_value, expected_value,
        "Session '{}': expected '{}', got '{}'",
        session_name, expected_value, actual_value
    );
}

#[then(regex = r#"^session "([^"]+)" should receive error containing "([^"]+)"$"#)]
pub async fn session_should_receive_error_containing(
    world: &mut DoormanWorld,
    session_name: String,
    expected_text: String,
) {
    // Get messages from the stored session messages
    let messages = world
        .session_messages
        .get(&session_name)
        .unwrap_or_else(|| panic!("No messages stored for session '{}'", session_name));

    // Find ErrorResponse in the messages
    let mut found_error: Option<String> = None;
    for (msg_type, data) in messages {
        if *msg_type == 'E' {
            // ErrorResponse - parse the error message
            let error_str = String::from_utf8_lossy(data).to_string();
            found_error = Some(error_str);
            break;
        }
    }

    let error_msg = found_error.unwrap_or_else(|| {
        panic!(
            "No ErrorResponse received from session '{}', expected error containing '{}'",
            session_name, expected_text
        )
    });

    assert!(
        error_msg
            .to_lowercase()
            .contains(&expected_text.to_lowercase()),
        "Session '{}': expected error containing '{}', got '{}'",
        session_name,
        expected_text,
        error_msg
    );
}

#[then(
    regex = r#"^session "([^"]+)" should receive error containing "([^"]+)" with code "([^"]+)"$"#
)]
pub async fn session_should_receive_error_containing_with_code(
    world: &mut DoormanWorld,
    session_name: String,
    expected_text: String,
    expected_code: String,
) {
    // Get messages from the stored session messages
    let messages = world
        .session_messages
        .get(&session_name)
        .unwrap_or_else(|| panic!("No messages stored for session '{}'", session_name));

    // Find ErrorResponse in the messages
    let mut found_error: Option<(String, String)> = None;
    for (msg_type, data) in messages {
        if *msg_type == 'E' {
            // ErrorResponse - parse the error message and code
            // Format: S<severity>\0 V<severity>\0 C<code>\0 M<message>\0 ... \0
            let mut code = String::new();
            let mut message = String::new();
            let mut i = 0;
            while i < data.len() {
                let field_type = data[i] as char;
                if field_type == '\0' {
                    break;
                }
                i += 1;
                let start = i;
                while i < data.len() && data[i] != 0 {
                    i += 1;
                }
                let value = String::from_utf8_lossy(&data[start..i]).to_string();
                i += 1; // skip null terminator
                match field_type {
                    'C' => code = value,
                    'M' => message = value,
                    _ => {}
                }
            }
            found_error = Some((message, code));
            break;
        }
    }

    let (error_msg, error_code) = found_error.unwrap_or_else(|| {
        panic!(
            "No ErrorResponse received from session '{}', expected error containing '{}' with code '{}'",
            session_name, expected_text, expected_code
        )
    });

    assert!(
        error_msg
            .to_lowercase()
            .contains(&expected_text.to_lowercase()),
        "Session '{}': expected error containing '{}', got '{}'",
        session_name,
        expected_text,
        error_msg
    );

    assert_eq!(
        error_code, expected_code,
        "Session '{}': expected error code '{}', got '{}'",
        session_name, expected_code, error_code
    );
}

#[when(
    regex = r#"^we send CopyFromStdin "([^"]+)" with data "([^"]*)" to session "([^"]+)" expecting error$"#
)]
pub async fn send_copy_from_stdin_to_session_expecting_error(
    world: &mut DoormanWorld,
    query: String,
    data: String,
    session_name: String,
) {
    let conn = super::helpers::get_session(&mut world.named_sessions, &session_name);

    // Unescape the data string (handle \t and \n)
    let unescaped_data = data.replace("\\t", "\t").replace("\\n", "\n");

    // Send the COPY command via simple query
    conn.send_simple_query(&query)
        .await
        .expect("Failed to send COPY query");

    // Read initial response (should be CopyInResponse 'G' or ErrorResponse 'E')
    let (msg_type, msg_data) = conn
        .read_message()
        .await
        .expect("Failed to read COPY response");

    let mut messages: Vec<(char, Vec<u8>)> = Vec::new();

    // If we got CopyInResponse ('G'), send the data and CopyDone
    if msg_type == 'G' {
        // Send copy data
        if !unescaped_data.is_empty() {
            conn.send_copy_data(unescaped_data.as_bytes())
                .await
                .expect("Failed to send CopyData");
        }
        conn.send_copy_done()
            .await
            .expect("Failed to send CopyDone");
    } else {
        // Error response - store it
        messages.push((msg_type, msg_data));
    }

    // Read remaining messages until ReadyForQuery
    let remaining = conn
        .read_all_messages_until_ready()
        .await
        .expect("Failed to read messages");
    messages.extend(remaining);

    world.session_messages.insert(session_name, messages);
}
