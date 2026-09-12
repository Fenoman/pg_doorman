@rust @rust-3 @checkout-client-disconnect
Feature: Disconnected checkout waiters preserve warm backends
  Scenario Outline: A disconnected waiter does not consume the returning backend
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
      query_wait_timeout = "5s"
      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"
      release_query = ""
      max_db_connections = 1
      [[pools.example_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = <pool_size>
      """
    When we create session "holder" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we create session "waiter" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "BEGIN; CREATE TEMP TABLE checkout_warm AS SELECT 42 AS marker" to session "holder" and store response
    And we send SimpleQuery "SELECT pg_backend_pid()" to session "holder" and store backend_pid as "before"
    And we send SimpleQuery "SELECT pg_sleep(1)" to session "waiter" without waiting
    And we sleep 100ms
    And we <disconnect> session "waiter"
    And we sleep 100ms
    And we send SimpleQuery "COMMIT" to session "holder" and store response
    And we sleep 100ms
    And we send SimpleQuery "SELECT marker FROM checkout_warm" to session "holder" and store response
    Then session "holder" should receive DataRow with "42"
    When we send SimpleQuery "SELECT pg_backend_pid()" to session "holder" and store backend_pid as "after"
    Then named backend_pid "after" from session "holder" is same as "before"

    Examples:
      | pool_size | disconnect                        |
      | 1         | close                             |
      | 1         | abort TCP connection with RST for |
      | 3         | close                             |
      | 3         | abort TCP connection with RST for |
