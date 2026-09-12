@dotnet @pipeline-cancel-disconnect-bug
Feature: Pool recovery after a complete streamed Npgsql response and TCP RST
  Client A verifies a 4 MiB response through the 2 KiB streaming threshold,
  then closes its transport with TCP RST. Client B opens a new frontend and
  must receive another complete response from the same one-slot backend pool.

  Background:
    Given PostgreSQL started with pg_hba.conf:
      """
      local   all             all                                     trust
      host    all             all             127.0.0.1/32            trust
      host    all             all             ::1/128                 trust
      """
    And fixtures from "tests/fixture.sql" applied
    And self-signed SSL certificates are generated
    And pg_doorman started with config:
      """
      [general]
      host = "127.0.0.1"
      port = ${DOORMAN_PORT}
      connect_timeout = 5000
      admin_username = "admin"
      admin_password = "admin"
      prepared_statements = true
      prepared_statements_cache_size = 10000
      pg_hba.content = "host all all 127.0.0.1/32 trust"
      worker_threads = 1
      message_size_to_be_stream = 2048
      cleanup_server_connections = false

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"

      [[pools.example_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1
      """

  @pipeline-cancel-disconnect-bug
  Scenario: Npgsql closes its socket after a verified 4 MiB response - next client must work
    When I run shell command:
      """
      export DATABASE_URL="Host=127.0.0.1;Port=${DOORMAN_PORT};Database=example_db;Username=example_user_1;Password="
      tests/dotnet/run_test.sh pipeline_cancel_disconnect pipeline_cancel_disconnect.cs
      """
    Then the command should succeed
    And the command output should contain "Client A: Transport closed after verified 4 MiB response"
    And the command output should contain "Client B: Query completed successfully"
    And the command output should contain "pipeline_cancel_disconnect complete"
