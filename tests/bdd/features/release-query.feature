@rust @rust-3 @release-query
Feature: Configurable release query
  The release query runs after checkin cleanup when a backend connection is
  returned to the pool. It must be configurable without changing session or
  transaction pooling semantics.

  Background:
    Given PostgreSQL started with options "-c log_statement=all -c logging_collector=off" and pg_hba.conf:
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

      [pools.release_tx]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = "SELECT 42 AS pgdoorman_release_tx_marker"

      [[pools.release_tx.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2

      [pools.release_session]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "session"
      release_query = "SELECT 43 AS pgdoorman_release_session_marker"

      [[pools.release_session.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2

      [pools.release_disabled]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = ""

      [[pools.release_disabled.users]]
      username = "example_user_1"
      password = ""
      pool_size = 2

      [pools.release_failing]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = "SELECT 1 / (SELECT denominator FROM release_failure_control)"

      [[pools.release_failing.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1

      [pools.release_blocked]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      release_query = "SELECT denominator FROM release_failure_control"

      [[pools.release_blocked.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1

      [pools.release_default]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"

      [[pools.release_default.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1

      [pools.release_sync]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      server_database = "example_db"
      pool_mode = "transaction"
      sync_server_parameters = true

      [[pools.release_sync.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1
      """

  Scenario: custom release_query runs in transaction mode
    When we create session "tx" to pg_doorman as "example_user_1" with password "" and database "release_tx"
    And we truncate PostgreSQL log
    And we send SimpleQuery "SELECT 1" to session "tx"
    And we sleep 300ms
    Then PostgreSQL log should contain "pgdoorman_release_tx_marker"

  Scenario: custom release_query runs when a session-mode backend is released
    When we create session "session" to pg_doorman as "example_user_1" with password "" and database "release_session"
    And we truncate PostgreSQL log
    And we send SimpleQuery "SELECT 1" to session "session"
    And we sleep 300ms
    Then PostgreSQL log should not contain "pgdoorman_release_session_marker"
    When we close session "session"
    And we sleep 300ms
    Then PostgreSQL log should contain "pgdoorman_release_session_marker"

  Scenario: empty release_query disables release cleanup
    When we create session "disabled" to pg_doorman as "example_user_1" with password "" and database "release_disabled"
    And we truncate PostgreSQL log
    And we send SimpleQuery "SELECT 1" to session "disabled"
    And we sleep 300ms
    Then PostgreSQL log should not contain "pg_advisory_unlock_all"
    And PostgreSQL log should not contain "pgv_free"

  @release-query-failure-result
  Scenario: release_query failure does not hide a committed result or disconnect the client
    When we create session "failing" to pg_doorman as "example_user_1" with password "" and database "release_failing"
    And we send SimpleQuery "CREATE TABLE release_result_once(id integer PRIMARY KEY); WITH armed AS (UPDATE release_failure_control SET denominator = 0 RETURNING denominator), inserted AS (INSERT INTO release_result_once VALUES (1) RETURNING id) SELECT inserted.id FROM inserted CROSS JOIN armed" to session "failing" and store response
    Then session "failing" should receive DataRow with "1"
    When we send SimpleQuery "SELECT count(*) FROM release_result_once" to session "failing" and store response
    Then session "failing" should receive DataRow with "1"

  @release-query-response-before-cleanup
  Scenario: client receives the completed query before blocked release cleanup finishes
    When we create session "slow" to pg_doorman as "example_user_1" with password "" and database "release_blocked" and store backend key
    And we create session "blocker" to postgres as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "BEGIN; LOCK TABLE release_failure_control IN ACCESS EXCLUSIVE MODE" to session "blocker" and store response
    And we send SimpleQuery "SELECT 42" to session "slow" without waiting
    Then we read SimpleQuery response from session "slow" within 500ms
    And session "slow" should receive DataRow with "42"
    When we send cancel request for session "slow"
    And we send SimpleQuery "COMMIT" to session "blocker" and store response
    And we send SimpleQuery "SELECT 43" to session "slow" and store response
    Then session "slow" should receive DataRow with "43"

  @release-query-default-without-pg-variables
  Scenario: the default release_query keeps backends on a database without pgv_free
    Given PostgreSQL database "example_db" has no pgv_free function
    When we create session "one" to pg_doorman as "example_user_1" with password "" and database "release_default"
    And we send SimpleQuery "SELECT pg_advisory_lock(42)" to session "one"
    And we send SimpleQuery "SELECT pg_backend_pid()" to session "one" and store backend_pid as "first"
    And we send SimpleQuery "SELECT pg_backend_pid()" to session "one" and store backend_pid as "second"
    Then named backend_pid "second" from session "one" is same as "first"
    When we create session "pg" to postgres as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory'" to session "pg" and store response
    Then session "pg" should receive DataRow with "0"

  @release-query-pipelined
  Scenario: the default release_query frees an advisory lock without waiting for the next query
    When we create session "one" to pg_doorman as "example_user_1" with password "" and database "release_default"
    And we send SimpleQuery "SELECT pg_advisory_lock(42)" to session "one"
    And we sleep 200ms
    And we create session "pg" to postgres as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "SELECT pg_try_advisory_lock(42)" to session "pg" and store response
    Then session "pg" should receive DataRow with "t"

  @release-query-pipelined
  Scenario: statements that refuse a transaction block run after the default release_query
    When we create session "one" to pg_doorman as "example_user_1" with password "" and database "release_default"
    And we send SimpleQuery "SELECT 1" to session "one"
    And we send SimpleQuery "VACUUM release_failure_control" to session "one" and store response
    Then session "one" should receive CommandComplete "VACUUM"
    When we send Parse "" with query "VACUUM release_failure_control" to session "one"
    And we send Bind "" to "" with params "" to session "one"
    And we send Execute "" to session "one"
    And we send Sync to session "one"
    Then session "one" should receive CommandComplete "VACUUM"

  @release-query-pipelined
  Scenario: a release reply that arrives while the client is in the middle of its batch is read away
    Given PostgreSQL database "example_db" has no pgv_free function
    When we create session "pg" to postgres as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "CREATE FUNCTION public.pgv_free() RETURNS void LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(0.3); END $$" to session "pg" and store response
    Then session "pg" should receive CommandComplete "CREATE FUNCTION"
    When we create session "app" to pg_doorman as "example_user_1" with password "" and database "release_default"
    And we send SimpleQuery "SELECT 1" to session "app"
    And we send Parse "" with query "SELECT 42" to session "app"
    And we sleep 600ms
    And we send Bind "" to "" with params "" to session "app"
    And we send Execute "" to session "app"
    And we send Sync to session "app"
    Then session "app" should receive DataRow with "42"

  @release-query-coalesced
  Scenario: the default release_query runs ahead of the first query of a client waiting for the backend
    When we create session "holder" to pg_doorman as "example_user_1" with password "" and database "release_default"
    And we create session "waiter" to pg_doorman as "example_user_1" with password "" and database "release_default"
    And we send SimpleQuery "SELECT 1" to session "holder"
    And we send SimpleQuery "BEGIN" to session "holder"
    And we send SimpleQuery "SELECT pg_advisory_lock(42)" to session "holder"
    And we send SimpleQuery "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND pid = pg_backend_pid()" to session "waiter" without waiting
    And we sleep 200ms
    And we send SimpleQuery "COMMIT" to session "holder"
    Then we read SimpleQuery response from session "waiter" within 2000ms
    And session "waiter" should receive DataRow with "0"

  @release-query-coalesced
  Scenario: the first query of a client waiting for the backend is not executed behind a failed release
    Given PostgreSQL database "example_db" has no pgv_free function
    When we create session "pg" to postgres as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "CREATE TABLE release_victim(id integer)" to session "pg"
    And we send SimpleQuery "CREATE SEQUENCE release_failures MINVALUE 0 START 1" to session "pg"
    And we send SimpleQuery "CREATE FUNCTION public.pgv_free() RETURNS void LANGUAGE plpgsql AS $$ BEGIN IF nextval('release_failures') = 0 THEN RAISE EXCEPTION 'release failed on purpose'; END IF; END $$" to session "pg" and store response
    Then session "pg" should receive CommandComplete "CREATE FUNCTION"
    When we create session "holder" to pg_doorman as "example_user_1" with password "" and database "release_default"
    And we create session "waiter" to pg_doorman as "example_user_1" with password "" and database "release_default"
    And we send SimpleQuery "SELECT 1" to session "holder"
    And we send SimpleQuery "BEGIN" to session "holder"
    And we send SimpleQuery "SELECT setval('release_failures', 0, false)" to session "holder"
    And we send SimpleQuery "INSERT INTO release_victim VALUES (1)" to session "waiter" without waiting
    And we sleep 200ms
    And we send SimpleQuery "COMMIT" to session "holder"
    Then we read SimpleQuery response from session "waiter" within 2000ms
    And session "waiter" should receive error containing "was not executed" with code "08006"
    When we send SimpleQuery "SELECT count(*) FROM release_victim" to session "waiter" and store response
    Then session "waiter" should receive DataRow with "0"

  @release-query-pipelined-failure
  Scenario: a query sent behind a failed default release_query is not executed and the client stays connected
    Given PostgreSQL database "example_db" has no pgv_free function
    When we create session "pg" to postgres as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "CREATE TABLE release_victim(id integer)" to session "pg"
    And we send SimpleQuery "CREATE SEQUENCE release_failures MINVALUE 0 START 1" to session "pg"
    And we send SimpleQuery "CREATE FUNCTION public.pgv_free() RETURNS void LANGUAGE plpgsql AS $$ BEGIN IF nextval('release_failures') = 0 THEN PERFORM pg_sleep(0.3); RAISE EXCEPTION 'release failed on purpose'; END IF; END $$" to session "pg" and store response
    Then session "pg" should receive CommandComplete "CREATE FUNCTION"
    When we create session "app" to pg_doorman as "example_user_1" with password "" and database "release_default"
    And we send SimpleQuery "SELECT setval('release_failures', 0, false)" to session "app"
    And we send SimpleQuery "INSERT INTO release_victim VALUES (1)" to session "app" and store response
    Then session "app" should receive error containing "was not executed" with code "08006"
    When we send SimpleQuery "SELECT setval('release_failures', 0, false)" to session "app"
    And we send Parse "" with query "INSERT INTO release_victim VALUES (2)" to session "app"
    And we send Bind "" to "" with params "" to session "app"
    And we send Execute "" to session "app"
    And we send Sync to session "app"
    Then session "app" should receive error containing "was not executed" with code "08006"
    When we send SimpleQuery "SELECT count(*) FROM release_victim" to session "app" and store response
    Then session "app" should receive DataRow with "0"

  @release-query-pipelined-failure
  Scenario: a backend whose default release_query failed while idle is replaced unseen
    Given PostgreSQL database "example_db" has no pgv_free function
    When we create session "pg" to postgres as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "CREATE TABLE release_victim(id integer)" to session "pg"
    And we send SimpleQuery "CREATE SEQUENCE release_failures MINVALUE 0 START 1" to session "pg"
    And we send SimpleQuery "CREATE FUNCTION public.pgv_free() RETURNS void LANGUAGE plpgsql AS $$ BEGIN IF nextval('release_failures') = 0 THEN RAISE EXCEPTION 'release failed on purpose'; END IF; END $$" to session "pg" and store response
    Then session "pg" should receive CommandComplete "CREATE FUNCTION"
    When we create session "app" to pg_doorman as "example_user_1" with password "" and database "release_default"
    And we send SimpleQuery "SELECT pg_backend_pid()" to session "app" and store backend_pid as "first"
    And we send SimpleQuery "SELECT setval('release_failures', 0, false)" to session "app"
    And we sleep 300ms
    And we send SimpleQuery "INSERT INTO release_victim VALUES (1)" to session "app" and store response
    Then session "app" should receive CommandComplete "INSERT 0 1"
    When we send SimpleQuery "SELECT pg_backend_pid()" to session "app" and store backend_pid as "second"
    Then named backend_pid "second" from session "app" is different from "first"

  @release-query-pipelined-sync-parameters
  Scenario: parameters RESET ALL restores are known before the next client's are synced
    Given PostgreSQL database "example_db" has no pgv_free function
    When we create session "pg" to postgres as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "CREATE FUNCTION public.pgv_free() RETURNS void LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(0.5); END $$" to session "pg" and store response
    Then session "pg" should receive CommandComplete "CREATE FUNCTION"
    When I run shell command:
      """
      psql "postgresql://example_user_1@127.0.0.1:${DOORMAN_PORT}/release_sync?application_name=first" -Atc "SET application_name = 'left_by_first'"
      psql "postgresql://example_user_1@127.0.0.1:${DOORMAN_PORT}/release_sync?application_name=left_by_first" -Atc "SHOW application_name"
      """
    Then the command should succeed
    And the command output should contain "left_by_first"
