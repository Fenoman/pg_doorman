@rust @rust-2 @client-write-timeout
Feature: A client that pauses reading keeps its response
  A client may stop reading in the middle of a result, for example while it
  processes one row before it reads the next one. The pause is bounded by
  client_write_timeout, not by proxy_copy_data_timeout, which bounds waits for
  the backend. Here proxy_copy_data_timeout is 1s and client_write_timeout keeps
  its default, so a 3s pause must not cost the client its response.

  Background:
    Given PostgreSQL started with pg_hba.conf:
      """
      local all all trust
      host all all 127.0.0.1/32 trust
      """
    And fixtures from "tests/fixture.sql" applied
    And pg_doorman started with config:
      """
      [general]
      host = "127.0.0.1"
      port = ${DOORMAN_PORT}
      admin_username = "admin"
      admin_password = "admin"
      pg_hba.content = "host all all 127.0.0.1/32 trust"
      proxy_copy_data_timeout = 1000

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"

      [[pools.example_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2
      """

  @client-write-timeout-buffered-rows
  Scenario: a pause in the middle of many small rows does not end the response
    # 20 MB of 1 KB rows does not fit into the socket buffers (a few MB), so
    # pg_doorman waits on the write while the client sleeps.
    When we create session "reader" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "SELECT repeat('x', 1000) FROM generate_series(1, 20000)" to session "reader" without waiting
    And we sleep 3000ms
    Then we read SimpleQuery response from session "reader" within 15000ms
    And session "reader" should receive 20000 DataRows
    And session "reader" DataRows should hold 20000000 bytes of "x"
    And session "reader" should receive CommandComplete "SELECT 20000"
    When we create admin session "admin" to pg_doorman as "admin" with password "admin"
    And we execute "SHOW POOLS" on admin session "admin" and store response
    Then admin session "admin" column "sv_active" for row with "user" = "example_user_1" should be between 0 and 0

  @client-write-timeout-streamed-row
  Scenario: a pause in the middle of a streamed large row does not end the response
    # The first row is larger than message_size_to_be_stream (1 MB), so it is
    # streamed to the client while the client sleeps.
    When we create session "reader" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "SELECT repeat('x', CASE WHEN g = 1 THEN 16000000 ELSE 1000 END) FROM generate_series(1, 1000) g" to session "reader" without waiting
    And we sleep 3000ms
    Then we read SimpleQuery response from session "reader" within 15000ms
    And session "reader" should receive 1000 DataRows
    And session "reader" DataRows should hold 16999000 bytes of "x"
    And session "reader" should receive CommandComplete "SELECT 1000"
    When we create admin session "admin" to pg_doorman as "admin" with password "admin"
    And we execute "SHOW POOLS" on admin session "admin" and store response
    Then admin session "admin" column "sv_active" for row with "user" = "example_user_1" should be between 0 and 0
