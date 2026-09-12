@rust @rust-4 @admin-pause-reconnect @admin-reload-generations
Feature: Admin controls survive pool generation changes
  Existing clients keep their pool generation across RELOAD. Admin controls
  must continue to reach them and a replacement must inherit PAUSE.

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
      query_wait_timeout = 2000
      server_idle_check_timeout = 0

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      release_query = ""

      [[pools.example_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2
      min_pool_size = 0
      """
    When we create session "old" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "SELECT pg_backend_pid()" to session "old" and store backend_pid as "before_reload"
    And we create admin session "admin" to pg_doorman as "admin" with password "admin"

  Scenario: PAUSE survives RELOAD and RESUME wakes an existing waiter
    When we execute "PAUSE example_db" on admin session "admin" and store response
    And we send SimpleQuery "SELECT 42" to session "old" without waiting
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
      release_query = ""

      [[pools.example_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2
      min_pool_size = 1
      """
    And we execute "RELOAD" on admin session "admin" and store response
    And we execute "SHOW POOLS" on admin session "admin" and store response
    Then admin session "admin" column "paused" should be between 1 and 1
    When we execute "RESUME example_db" on admin session "admin" and store response
    Then we read SimpleQuery response from session "old" within 500ms
    And session "old" should receive DataRow with "42"
    When we create session "fresh" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "SELECT 43" to session "fresh"

  Scenario: RECONNECT after RELOAD rotates an existing generation
    When we overwrite pg_doorman config file with:
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
      release_query = ""

      [[pools.example_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2
      min_pool_size = 1
      """
    And we execute "RELOAD" on admin session "admin" and store response
    And we execute "RECONNECT example_db" on admin session "admin" and store response
    And we send SimpleQuery "SELECT pg_backend_pid()" to session "old" and store backend_pid as "after_reconnect"
    Then named backend_pid "after_reconnect" from session "old" is different from "before_reload"
