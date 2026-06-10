use crate::utils::is_root;
use crate::world::DoormanWorld;
use cucumber::{gherkin::Step, given, then, when};
use portpicker::pick_unused_port;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;

/// Fallback definition of `public.pgv_free()` for PostgreSQL instances that
/// do not have the `pg_variables` extension available (vanilla pgcr installs,
/// CI images, etc.). The iServ-compatible `release_query`
/// (`pg_advisory_unlock_all() + pgv_free()`) panics on a missing function and
/// would mark every test backend bad - making the entire `release-query.feature`
/// suite red on machines without `pg_variables`. The no-op shim keeps the SQL
/// well-formed without changing the contract.
const PGV_FREE_FALLBACK_SQL: &str = r#"
DO $pg_doorman$
BEGIN
    IF to_regprocedure('public.pgv_free()') IS NULL THEN
        CREATE FUNCTION public.pgv_free()
        RETURNS void
        LANGUAGE plpgsql
        AS $pgv$
        BEGIN
        END;
        $pgv$;
    END IF;
END
$pg_doorman$;
"#;

/// Build a PostgreSQL command, running as postgres user if we are root
fn pg_command_builder(cmd: &str, args: &[&str]) -> Command {
    if is_root() {
        let mut command = Command::new("sudo");
        command.arg("-u").arg("postgres").arg(cmd).args(args);
        command
    } else {
        let mut command = Command::new(cmd);
        command.args(args);
        command
    }
}

/// Shortcut for `psql -h 127.0.0.1 -p <port> -U postgres -d <database>` with the
/// caller-supplied extra args. Captures stdout/stderr so the iServ helpers below
/// can pretty-print failures.
fn run_psql(port: u16, database: &str, args: &[&str]) -> std::process::Output {
    let port = port.to_string();
    let mut command = pg_command_builder("psql", &[]);
    command
        .args([
            "-h",
            "127.0.0.1",
            "-p",
            &port,
            "-U",
            "postgres",
            "-d",
            database,
        ])
        .args(args)
        .output()
        .expect("Failed to run psql")
}

/// True if `database` exists on the running test PostgreSQL instance.
fn database_exists(port: u16, database: &str) -> bool {
    let output = run_psql(
        port,
        "postgres",
        &[
            "-XAtq",
            "-c",
            &format!("SELECT 1 FROM pg_database WHERE datname = '{database}'"),
        ],
    );
    output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "1"
}

/// Make `public.pgv_free()` resolvable on `database`. If the `pg_variables`
/// extension is available, install it (real function); otherwise install the
/// `PGV_FREE_FALLBACK_SQL` no-op shim. Panics on SQL failure to fail tests fast
/// instead of letting the rest of the scenario time out with cryptic errors.
fn ensure_pgv_free_available(port: u16, database: &str) {
    let available = run_psql(
        port,
        database,
        &[
            "-XAtq",
            "-c",
            "SELECT 1 FROM pg_available_extensions WHERE name = 'pg_variables'",
        ],
    );
    if !available.status.success() {
        eprintln!(
            "pg_available_extensions check failed for database {database}:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&available.stdout),
            String::from_utf8_lossy(&available.stderr)
        );
        panic!("Failed to check pg_variables availability");
    }

    let sql = if String::from_utf8_lossy(&available.stdout).trim() == "1" {
        "CREATE EXTENSION IF NOT EXISTS pg_variables"
    } else {
        PGV_FREE_FALLBACK_SQL
    };
    let create = run_psql(port, database, &["-v", "ON_ERROR_STOP=1", "-c", sql]);
    if !create.status.success() {
        eprintln!(
            "pgv_free setup failed for database {database}:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&create.stdout),
            String::from_utf8_lossy(&create.stderr)
        );
        panic!("Failed to make public.pgv_free() available");
    }

    let verify = run_psql(
        port,
        database,
        &[
            "-XAtq",
            "-c",
            "SELECT to_regprocedure('public.pgv_free()') IS NOT NULL",
        ],
    );
    let verified = verify.status.success() && String::from_utf8_lossy(&verify.stdout).trim() == "t";
    if !verified {
        eprintln!(
            "pgv_free verification failed for database {database}:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&verify.stdout),
            String::from_utf8_lossy(&verify.stderr)
        );
        panic!("public.pgv_free() is not available");
    }
}

/// Stream log file to stdout in a background thread
/// Returns a stop flag that can be used to terminate the streaming thread
fn stream_log_file(log_path: PathBuf) -> Arc<AtomicBool> {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_clone = stop_flag.clone();

    std::thread::spawn(move || {
        use std::fs::File;

        // Wait for log file to be created
        let file = loop {
            if stop_flag_clone.load(Ordering::Relaxed) {
                return;
            }

            match File::open(&log_path) {
                Ok(f) => break f,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
        };

        let mut reader = BufReader::new(file);
        let mut line = String::new();

        loop {
            if stop_flag_clone.load(Ordering::Relaxed) {
                break;
            }

            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    // EOF reached, wait a bit and try again (tail -f behavior)
                    std::thread::sleep(Duration::from_millis(100));

                    // Try to reopen file in case it was rotated
                    if let Ok(new_file) = File::open(&log_path) {
                        // Check if file size decreased (rotation)
                        if let (Ok(old_meta), Ok(new_meta)) =
                            (reader.get_ref().metadata(), new_file.metadata())
                        {
                            if new_meta.len() < old_meta.len() {
                                reader = BufReader::new(new_file);
                                continue;
                            }
                        }
                    }
                }
                Ok(_) => {
                    eprint!("[PG_LOG] {line}");
                }
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });

    stop_flag
}

#[given("a temporary PostgreSQL database started")]
pub async fn start_postgres(world: &mut DoormanWorld) {
    let tmp_dir = TempDir::new().expect("Failed to create temp dir");
    let db_path = tmp_dir.path().join("db");
    let port = pick_unused_port().expect("No free ports");

    if is_root() {
        // If we are root, we need to make sure the postgres user can access the temp dir
        Command::new("chown")
            .arg("-R")
            .arg("postgres:postgres")
            .arg(tmp_dir.path())
            .status()
            .expect("Failed to chown temp dir");
        // Also ensure it has proper permissions
        std::fs::set_permissions(tmp_dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("Failed to set permissions on temp dir");
    }

    // initdb (suppress output, show only on error)
    let output = pg_command_builder("initdb", &["-D", db_path.to_str().unwrap(), "--no-sync"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("Failed to run initdb");
    if !output.status.success() {
        eprintln!(
            "initdb stdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        eprintln!(
            "initdb stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        panic!("initdb failed");
    }

    // Create socket directory inside temp dir (avoids /tmp permission issues in containers)
    let socket_dir = tmp_dir.path().to_str().unwrap();

    // Start and stop to initialize properly (suppress output)
    let _ = pg_command_builder(
        "pg_ctl",
        &[
            "-D",
            db_path.to_str().unwrap(),
            "-o",
            &format!("-p {port} -F -k {socket_dir}"),
            "start",
        ],
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status();
    let _ = pg_command_builder(
        "pg_ctl",
        &["-D", db_path.to_str().unwrap(), "stop", "-m", "immediate"],
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status();

    let log_path = tmp_dir.path().join("pg.log");
    // pg_ctl start (suppress output, logs go to pg.log)
    let _ = pg_command_builder(
        "pg_ctl",
        &[
            "-D",
            db_path.to_str().unwrap(),
            "-l",
            log_path.to_str().unwrap(),
            "-o",
            &format!("-p {port} -F -k {socket_dir}"),
            "start",
        ],
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .expect("Failed to start pg_ctl");

    // Start streaming pg.log to stdout if DEBUG is enabled
    let _log_stream_stop = if std::env::var("DEBUG").is_ok() {
        Some(stream_log_file(log_path.clone()))
    } else {
        None
    };

    // Wait for PG to be ready
    let mut success = false;
    for _ in 0..20 {
        let check = pg_command_builder(
            "pg_isready",
            &["-p", &port.to_string(), "-h", "127.0.0.1", "-t", "1"],
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
        if let Ok(s) = check {
            if s.success() {
                success = true;
                break;
            }
        }
        sleep(Duration::from_millis(500)).await;
    }

    if !success {
        if let Ok(log_content) = std::fs::read_to_string(&log_path) {
            eprintln!("Postgres log:\n{log_content}");
        }
        // Try one more time with psql just to be sure
        let check_psql = pg_command_builder(
            "psql",
            &[
                "-p",
                &port.to_string(),
                "-h",
                "127.0.0.1",
                "-c",
                "SELECT 1",
                "postgres",
            ],
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
        if let Ok(s) = check_psql {
            if s.success() {
                success = true;
            }
        }
    }
    assert!(success, "Postgres failed to start");

    // Make `public.pgv_free()` resolvable so the iServ-compatible release_query
    // default works on machines without `pg_variables`.
    ensure_pgv_free_available(port, "postgres");

    world.pg_tmp_dir = Some(tmp_dir);
    world.pg_port = Some(port);
    world.pg_db_path = Some(db_path);
}

#[given(expr = "fixtures from {string} applied")]
pub async fn apply_fixtures(world: &mut DoormanWorld, file_path: String) {
    let port = world.pg_port.expect("PG not started");
    let output = pg_command_builder(
        "psql",
        &[
            "-h",
            "127.0.0.1",
            "-p",
            &port.to_string(),
            "-U",
            "postgres",
            "-d",
            "postgres",
            "-f",
            &file_path,
        ],
    )
    .output()
    .expect("Failed to run psql");
    if !output.status.success() {
        eprintln!("psql stdout:\n{}", String::from_utf8_lossy(&output.stdout));
        eprintln!("psql stderr:\n{}", String::from_utf8_lossy(&output.stderr));
        panic!("Failed to apply fixtures from {file_path}");
    }

    // Re-install the pgv_free shim on both the default database and any
    // `example_db` that fixtures might have just created. The iServ-compatible
    // release_query references `public.pgv_free()` per-database, not globally.
    ensure_pgv_free_available(port, "postgres");
    if database_exists(port, "example_db") {
        ensure_pgv_free_available(port, "example_db");
    }
}

/// Internal helper to start PostgreSQL with hba content and optional extra options
async fn start_postgres_internal(world: &mut DoormanWorld, hba_content: &str, extra_options: &str) {
    let tmp_dir = TempDir::new().expect("Failed to create temp dir");
    let db_path = tmp_dir.path().join("db");
    let port = pick_unused_port().expect("No free ports");

    if is_root() {
        // If we are root, we need to make sure the postgres user can access the temp dir
        Command::new("chown")
            .arg("-R")
            .arg("postgres:postgres")
            .arg(tmp_dir.path())
            .status()
            .expect("Failed to chown temp dir");
        // Also ensure it has proper permissions
        std::fs::set_permissions(tmp_dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("Failed to set permissions on temp dir");
    }

    // initdb with postgres user (suppress output, show only on error)
    let output = pg_command_builder(
        "initdb",
        &[
            "-D",
            db_path.to_str().unwrap(),
            "-U",
            "postgres",
            "--no-sync",
        ],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .expect("Failed to run initdb");
    if !output.status.success() {
        eprintln!(
            "initdb stdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        eprintln!(
            "initdb stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        panic!("initdb failed");
    }

    // Write custom pg_hba.conf
    let hba_path = db_path.join("pg_hba.conf");
    {
        let mut hba_file = std::fs::File::create(&hba_path).expect("Failed to create pg_hba.conf");
        hba_file
            .write_all(hba_content.as_bytes())
            .expect("Failed to write pg_hba.conf");
    }

    if is_root() {
        // Ensure postgres user owns the pg_hba.conf
        Command::new("chown")
            .arg("postgres:postgres")
            .arg(&hba_path)
            .status()
            .expect("Failed to chown pg_hba.conf");
    }

    // Create socket directory inside temp dir (avoids /tmp permission issues in containers)
    let socket_dir = tmp_dir.path().to_str().unwrap();

    let log_path = tmp_dir.path().join("pg.log");

    // Build pg_ctl options with extra options if provided
    let pg_options = if extra_options.is_empty() {
        format!("-p {port} -F -k {socket_dir}")
    } else {
        format!("-p {port} -F -k {socket_dir} {extra_options}")
    };

    // pg_ctl start (suppress output, logs go to pg.log)
    let _ = pg_command_builder(
        "pg_ctl",
        &[
            "-D",
            db_path.to_str().unwrap(),
            "-l",
            log_path.to_str().unwrap(),
            "-o",
            &pg_options,
            "start",
        ],
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .expect("Failed to start pg_ctl");

    // Start streaming pg.log to stdout if DEBUG is enabled
    let _log_stream_stop = if std::env::var("DEBUG").is_ok() {
        Some(stream_log_file(log_path.clone()))
    } else {
        None
    };

    // Wait for PG to be ready
    let mut success = false;
    for _ in 0..20 {
        let check = pg_command_builder(
            "pg_isready",
            &[
                "-p",
                &port.to_string(),
                "-h",
                "127.0.0.1",
                "-U",
                "postgres",
                "-t",
                "1",
            ],
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
        if let Ok(s) = check {
            if s.success() {
                success = true;
                break;
            }
        }
        sleep(Duration::from_millis(500)).await;
    }

    if !success {
        if let Ok(log_content) = std::fs::read_to_string(&log_path) {
            eprintln!("Postgres log:\n{log_content}");
        }
        // Try one more time with psql just to be sure
        let check_psql = pg_command_builder(
            "psql",
            &[
                "-p",
                &port.to_string(),
                "-h",
                "127.0.0.1",
                "-U",
                "postgres",
                "-c",
                "SELECT 1",
                "postgres",
            ],
        )
        .status();
        if let Ok(s) = check_psql {
            if s.success() {
                success = true;
            }
        }
    }
    assert!(success, "Postgres failed to start");

    // Same pgv_free pre-flight as the plain start_postgres path - fixtures
    // typically run on the default database and any `example_db` they create
    // is dealt with in `apply_fixtures`.
    ensure_pgv_free_available(port, "postgres");

    world.pg_tmp_dir = Some(tmp_dir);
    world.pg_port = Some(port);
    world.pg_db_path = Some(db_path);
}

/// Start PostgreSQL with inline pg_hba.conf content
#[given("PostgreSQL started with pg_hba.conf:")]
pub async fn start_postgres_with_hba(world: &mut DoormanWorld, step: &Step) {
    let hba_content = step
        .docstring
        .as_ref()
        .expect("hba_content not found")
        .to_string();
    start_postgres_internal(world, &hba_content, "").await;
}

/// Start PostgreSQL with inline pg_hba.conf content and extra options (e.g., max_connections)
#[given(expr = "PostgreSQL started with options {string} and pg_hba.conf:")]
pub async fn start_postgres_with_options_and_hba(
    world: &mut DoormanWorld,
    options: String,
    step: &Step,
) {
    let hba_content = step
        .docstring
        .as_ref()
        .expect("hba_content not found")
        .to_string();
    let options = world.replace_placeholders(&options);
    start_postgres_internal(world, &hba_content, &options).await;
}

pub(crate) fn build_psql_via_doorman(
    port: u16,
    user: &str,
    database: &str,
    query: &str,
    password: Option<&str>,
) -> Command {
    let mut cmd = Command::new("psql");
    cmd.args([
        "-h",
        "127.0.0.1",
        "-p",
        &port.to_string(),
        "-U",
        user,
        "-d",
        database,
        "-c",
        query,
    ]);
    cmd.env("PGSSLMODE", "disable");
    if let Some(pw) = password {
        cmd.env("PGPASSWORD", pw);
    } else {
        cmd.arg("-w");
        cmd.env_remove("PGPASSWORD");
    }
    cmd
}

#[then(
    expr = "psql connection to pg_doorman as user {string} to database {string} with password {string} succeeds"
)]
pub async fn psql_connection_succeeds(
    world: &mut DoormanWorld,
    user: String,
    database: String,
    password: String,
) {
    let port = world.doorman_port.expect("pg_doorman not started");
    let status = build_psql_via_doorman(port, &user, &database, "SELECT 1", Some(&password))
        .status()
        .expect("Failed to run psql");
    assert!(status.success(), "psql connection to pg_doorman failed");
}

#[then(
    expr = "psql connection to pg_doorman as user {string} to database {string} with password {string} fails"
)]
pub async fn psql_connection_fails(
    world: &mut DoormanWorld,
    user: String,
    database: String,
    password: String,
) {
    let port = world.doorman_port.expect("pg_doorman not started");
    let status = build_psql_via_doorman(port, &user, &database, "SELECT 1", Some(&password))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("Failed to run psql");
    assert!(
        !status.success(),
        "psql connection should have failed (user: {user}, database: {database})"
    );
}

#[then(
    expr = "psql connection to pg_doorman as user {string} to database {string} without password succeeds"
)]
pub async fn psql_connection_without_password_succeeds(
    world: &mut DoormanWorld,
    user: String,
    database: String,
) {
    let port = world.doorman_port.expect("pg_doorman not started");
    let status = build_psql_via_doorman(port, &user, &database, "SELECT 1", None)
        .status()
        .expect("Failed to run psql");
    assert!(
        status.success(),
        "psql without password failed (user: {user}, database: {database})"
    );
}

#[then(
    expr = "psql connection to pg_doorman as user {string} to database {string} without password fails"
)]
pub async fn psql_connection_without_password_fails(
    world: &mut DoormanWorld,
    user: String,
    database: String,
) {
    let port = world.doorman_port.expect("pg_doorman not started");
    let status = build_psql_via_doorman(port, &user, &database, "SELECT 1", None)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("Failed to run psql");
    assert!(
        !status.success(),
        "psql without password should have failed (user: {user}, database: {database})"
    );
}

#[then(
    expr = "psql query {string} via pg_doorman as user {string} to database {string} with password {string} fails"
)]
pub async fn psql_query_fails(
    world: &mut DoormanWorld,
    query: String,
    user: String,
    database: String,
    password: String,
) {
    let port = world.doorman_port.expect("pg_doorman not started");
    let mut cmd = build_psql_via_doorman(port, &user, &database, &query, Some(&password));
    cmd.args(["-t", "-A"]);
    let status = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("Failed to run psql");
    assert!(
        !status.success(),
        "psql query should have failed (user: {user}, database: {database})"
    );
}

#[then(
    expr = "psql query {string} via pg_doorman as user {string} to database {string} with password {string} returns {string}"
)]
pub async fn psql_query_returns(
    world: &mut DoormanWorld,
    query: String,
    user: String,
    database: String,
    password: String,
    expected: String,
) {
    let port = world.doorman_port.expect("pg_doorman not started");
    let mut cmd = build_psql_via_doorman(port, &user, &database, &query, Some(&password));
    cmd.args(["-t", "-A"]);
    let output = cmd.output().expect("Failed to run psql");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "psql query failed (exit code {:?}): stderr: {}",
        output.status.code(),
        stderr,
    );
    assert!(
        stdout.contains(&expected),
        "Expected output to contain '{}', got: '{}' (stderr: {})",
        expected,
        stdout.trim(),
        stderr.trim(),
    );
}

/// Extract password hash from pg_authid and store it as a dynamic variable
#[given(expr = "password hash for PG user {string} is stored as {string}")]
pub async fn store_password_hash(world: &mut DoormanWorld, pg_user: String, var_name: String) {
    let port = world.pg_port.expect("PG not started");
    let query = format!("SELECT rolpassword FROM pg_authid WHERE rolname = '{pg_user}'");
    let output = pg_command_builder(
        "psql",
        &[
            "-h",
            "127.0.0.1",
            "-p",
            &port.to_string(),
            "-U",
            "postgres",
            "-d",
            "postgres",
            "-t",
            "-A",
            "-c",
            &query,
        ],
    )
    .output()
    .expect("Failed to run psql");

    assert!(
        output.status.success(),
        "Failed to extract password hash for user '{}': {}",
        pg_user,
        String::from_utf8_lossy(&output.stderr)
    );

    let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        !hash.is_empty(),
        "Password hash for user '{pg_user}' is empty - user may not exist"
    );

    world.vars.insert(var_name, hash);
}

/// Build psql command connecting via Unix socket directory
fn build_psql_via_doorman_unix(
    socket_dir: &str,
    port: u16,
    user: &str,
    database: &str,
    query: &str,
) -> Command {
    let mut cmd = Command::new("psql");
    cmd.args([
        "-h",
        socket_dir,
        "-p",
        &port.to_string(),
        "-U",
        user,
        "-d",
        database,
        "-c",
        query,
    ]);
    cmd.env("PGSSLMODE", "disable");
    cmd.arg("-w");
    cmd.env_remove("PGPASSWORD");
    cmd
}

/// Same as `build_psql_via_doorman_unix` but supplies a password through
/// `PGPASSWORD` so the scenarios that exercise password-based auth over
/// Unix sockets do not need to drive the prompt manually.
fn build_psql_via_doorman_unix_with_password(
    socket_dir: &str,
    port: u16,
    user: &str,
    database: &str,
    query: &str,
    password: &str,
) -> Command {
    let mut cmd = Command::new("psql");
    cmd.args([
        "-h",
        socket_dir,
        "-p",
        &port.to_string(),
        "-U",
        user,
        "-d",
        database,
        "-c",
        query,
    ]);
    cmd.env("PGSSLMODE", "disable");
    cmd.arg("-w");
    cmd.env("PGPASSWORD", password);
    cmd
}

#[then(
    expr = "psql connection to pg_doorman via unix socket as user {string} to database {string} succeeds"
)]
pub async fn psql_unix_connection_succeeds(
    world: &mut DoormanWorld,
    user: String,
    database: String,
) {
    let port = world.doorman_port.expect("pg_doorman not started");
    let socket_dir = world
        .pg_tmp_dir
        .as_ref()
        .expect("pg_tmp_dir not set")
        .path()
        .to_str()
        .unwrap()
        .to_string();
    let output = build_psql_via_doorman_unix(&socket_dir, port, &user, &database, "SELECT 1")
        .output()
        .expect("Failed to run psql");
    assert!(
        output.status.success(),
        "psql unix socket connection failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[then(
    expr = "psql query {string} via pg_doorman unix socket as user {string} to database {string} returns {string}"
)]
pub async fn psql_unix_query_returns(
    world: &mut DoormanWorld,
    query: String,
    user: String,
    database: String,
    expected: String,
) {
    let port = world.doorman_port.expect("pg_doorman not started");
    let socket_dir = world
        .pg_tmp_dir
        .as_ref()
        .expect("pg_tmp_dir not set")
        .path()
        .to_str()
        .unwrap()
        .to_string();
    let mut cmd = build_psql_via_doorman_unix(&socket_dir, port, &user, &database, &query);
    cmd.args(["-t", "-A"]);
    let output = cmd.output().expect("Failed to run psql");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "psql unix query failed: stderr: {stderr}",
    );
    assert!(
        stdout.contains(&expected),
        "Expected '{}', got: '{}' (stderr: {})",
        expected,
        stdout.trim(),
        stderr.trim(),
    );
}

/// Run a query through the Unix socket using a password-authenticated
/// connection and assert it succeeds. Pairs with the password-authenticated
/// failure step below to round out HBA `local md5`/`local scram-sha-256`
/// scenarios.
#[then(
    expr = "psql query {string} via pg_doorman unix socket as user {string} to database {string} with password {string} returns {string}"
)]
pub async fn psql_unix_query_with_password_returns(
    world: &mut DoormanWorld,
    query: String,
    user: String,
    database: String,
    password: String,
    expected: String,
) {
    let port = world.doorman_port.expect("pg_doorman not started");
    let socket_dir = world
        .pg_tmp_dir
        .as_ref()
        .expect("pg_tmp_dir not set")
        .path()
        .to_str()
        .unwrap()
        .to_string();
    let mut cmd = build_psql_via_doorman_unix_with_password(
        &socket_dir,
        port,
        &user,
        &database,
        &query,
        &password,
    );
    cmd.args(["-t", "-A"]);
    let output = cmd.output().expect("Failed to run psql");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "psql unix query failed: stderr: {stderr}",
    );
    assert!(
        stdout.contains(&expected),
        "Expected '{}', got: '{}' (stderr: {})",
        expected,
        stdout.trim(),
        stderr.trim(),
    );
}

/// Run a query through the Unix socket with a wrong password and assert
/// authentication fails. Both ends accept any error message — psql exits
/// non-zero either way once auth refuses the credentials.
#[then(
    expr = "psql connection to pg_doorman via unix socket as user {string} to database {string} with password {string} fails"
)]
pub async fn psql_unix_connection_with_password_fails(
    world: &mut DoormanWorld,
    user: String,
    database: String,
    password: String,
) {
    let port = world.doorman_port.expect("pg_doorman not started");
    let socket_dir = world
        .pg_tmp_dir
        .as_ref()
        .expect("pg_tmp_dir not set")
        .path()
        .to_str()
        .unwrap()
        .to_string();
    let output = build_psql_via_doorman_unix_with_password(
        &socket_dir,
        port,
        &user,
        &database,
        "SELECT 1",
        &password,
    )
    .output()
    .expect("Failed to run psql");
    assert!(
        !output.status.success(),
        "psql unix socket connection unexpectedly succeeded: stdout={}",
        String::from_utf8_lossy(&output.stdout),
    );
}

/// Verify that a Unix socket psql connection fails — without checking the
/// exact error string. Used by feature scenarios that rely on HBA reject
/// or "no rule matched" wording, both of which produce different error
/// codes depending on which auth method the matcher fell through to.
#[then(
    expr = "psql connection to pg_doorman via unix socket as user {string} to database {string} fails"
)]
pub async fn psql_unix_connection_fails(world: &mut DoormanWorld, user: String, database: String) {
    let port = world.doorman_port.expect("pg_doorman not started");
    let socket_dir = world
        .pg_tmp_dir
        .as_ref()
        .expect("pg_tmp_dir not set")
        .path()
        .to_str()
        .unwrap()
        .to_string();
    let output = build_psql_via_doorman_unix(&socket_dir, port, &user, &database, "SELECT 1")
        .output()
        .expect("Failed to run psql");
    assert!(
        !output.status.success(),
        "psql unix socket connection unexpectedly succeeded: stdout={}",
        String::from_utf8_lossy(&output.stdout),
    );
}

/// Verify a Unix socket psql connection fails AND its stderr contains a
/// substring. Used to assert that pg_doorman replies with a structured
/// PostgreSQL ErrorResponse and not a bare connection drop.
#[then(
    expr = "psql connection to pg_doorman via unix socket as user {string} to database {string} fails with {string}"
)]
pub async fn psql_unix_connection_fails_with(
    world: &mut DoormanWorld,
    user: String,
    database: String,
    expected_error: String,
) {
    let port = world.doorman_port.expect("pg_doorman not started");
    let socket_dir = world
        .pg_tmp_dir
        .as_ref()
        .expect("pg_tmp_dir not set")
        .path()
        .to_str()
        .unwrap()
        .to_string();
    let output = build_psql_via_doorman_unix(&socket_dir, port, &user, &database, "SELECT 1")
        .output()
        .expect("Failed to run psql");
    assert!(
        !output.status.success(),
        "psql unix socket connection unexpectedly succeeded: stdout={}",
        String::from_utf8_lossy(&output.stdout),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&expected_error),
        "expected stderr to contain '{expected_error}', got: {stderr}",
    );
}

/// Restart the PostgreSQL instance that the BDD scenario already started.
///
/// Used by the dead-backend-detection scenario to simulate a real PostgreSQL
/// restart: existing TCP-level backend connections held by pg_doorman become
/// stale (the kernel closes them once the new postmaster comes up on the
/// same port), and the iServ `evict_dead_backends` scan must notice and
/// drop them so `replenish` can refill the pool.
///
/// Uses `pg_ctl restart -m fast` to drain the running instance gracefully
/// and bring it back on the same data dir and port. Caller must have started
/// PostgreSQL via `start_postgres` / `start_postgres_with_options_and_hba`
/// first so `world.pg_db_path` is populated.
#[when("PostgreSQL is restarted")]
pub async fn restart_postgres(world: &mut DoormanWorld) {
    let db_path = world
        .pg_db_path
        .clone()
        .expect("PostgreSQL must be started before it can be restarted");
    let port = world.pg_port.expect("pg_port not set");

    let status = pg_command_builder(
        "pg_ctl",
        &[
            "-D",
            db_path.to_str().unwrap(),
            "restart",
            "-m",
            "fast",
            "-w",
            "-t",
            "30",
        ],
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .expect("Failed to invoke pg_ctl restart");
    assert!(
        status.success(),
        "pg_ctl restart returned non-zero status: {status:?}"
    );

    // Wait for the new postmaster to accept connections - `pg_ctl -w` already
    // does that internally but we double-check with `pg_isready` to fail fast
    // if the restart half-succeeded.
    let mut ready = false;
    for _ in 0..20 {
        let check = pg_command_builder(
            "pg_isready",
            &["-p", &port.to_string(), "-h", "127.0.0.1", "-t", "1"],
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
        if matches!(check, Ok(s) if s.success()) {
            ready = true;
            break;
        }
        sleep(Duration::from_millis(250)).await;
    }
    assert!(
        ready,
        "PostgreSQL did not become ready within 5s after restart on port {port}"
    );
}

/// Stop PostgreSQL and pg_doorman when the world is dropped
impl Drop for DoormanWorld {
    fn drop(&mut self) {
        // Stop pg_doorman first
        if let Some(ref mut child) = self.doorman_process {
            crate::doorman_helper::stop_doorman(child);
        }
        // Then stop PostgreSQL (suppress output)
        if let Some(ref db_path) = self.pg_db_path {
            let _ = pg_command_builder(
                "pg_ctl",
                &["-D", db_path.to_str().unwrap(), "stop", "-m", "immediate"],
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        }
    }
}
