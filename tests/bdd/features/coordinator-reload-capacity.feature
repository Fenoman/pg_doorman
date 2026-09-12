@rust @rust-4 @coordinator-reload-capacity
Feature: Reload preserves one database budget across pool generations
  Existing transactions keep their backend and temporary state. Excess
  connections retire on return before another lease can reuse them.

  Background:
    Given PostgreSQL started with pg_hba.conf:
      """
      local all all trust
      host all all 127.0.0.1/32 trust
      """
    And fixtures from "tests/fixture.sql" applied

  Scenario Outline: Reducing or enabling a cap preserves active transactions
    Given pg_doorman started with config:
      """
      [general]
      host = "127.0.0.1"
      port = ${DOORMAN_PORT}
      admin_username = "admin"
      admin_password = "admin"
      pg_hba.content = "host all all 127.0.0.1/32 trust"
      query_wait_timeout = 2000
      server_idle_check_timeout = 0

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"
      release_query = ""
      max_db_connections = <initial_cap>
      reserve_pool_size = 0
      min_connection_lifetime = 0

      [[pools.example_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2
      min_pool_size = 0
      """
    When we create session "first" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "BEGIN; CREATE TEMP TABLE c1_state(v int); INSERT INTO c1_state VALUES (41)" to session "first"
    And we create session "second" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "BEGIN; CREATE TEMP TABLE c1_state(v int); INSERT INTO c1_state VALUES (42)" to session "second"
    And we create admin session "admin" to pg_doorman as "admin" with password "admin"
    And we overwrite pg_doorman config file with:
      """
      [general]
      host = "127.0.0.1"
      port = ${DOORMAN_PORT}
      admin_username = "admin"
      admin_password = "admin"
      pg_hba.content = "host all all 127.0.0.1/32 trust"
      query_wait_timeout = 2000
      server_idle_check_timeout = 0

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"
      release_query = ""
      max_db_connections = 1
      reserve_pool_size = 0
      min_connection_lifetime = 0

      [[pools.example_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2
      min_pool_size = 0
      """
    And we execute "RELOAD" on admin session "admin" and store response
    And we execute "SHOW POOL_COORDINATOR" on admin session "admin" and store response
    Then admin session "admin" column "max_db_conn" should be between 1 and 1
    And admin session "admin" column "current" should be between 2 and 2
    When we send SimpleQuery "SELECT v FROM c1_state" to session "first" and store response
    Then session "first" should receive DataRow with "41"
    When we send SimpleQuery "SELECT v FROM c1_state" to session "second" and store response
    Then session "second" should receive DataRow with "42"
    When we send SimpleQuery "COMMIT" to session "first"
    Then admin session "admin" eventually shows coordinator current 1
    When we send SimpleQuery "SELECT v FROM c1_state" to session "second" and store response
    Then session "second" should receive DataRow with "42"
    When we send SimpleQuery "COMMIT" to session "second"
    Then admin session "admin" eventually shows coordinator current 1

    Examples:
      | initial_cap |
      | 2           |
      | 0           |
