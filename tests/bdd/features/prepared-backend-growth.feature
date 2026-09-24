@rust @rust-3 @prepared-cache @prepared-backend-growth
Feature: Repeated Parse does not multiply statements on backends
  In transaction pooling consecutive transactions of a client land on
  different backends. Each backend must hold one statement per distinct
  query however often clients Parse it. The bound is the number of
  distinct queries, far below server_prepared_statements_cache_size:
  a bound equal to that cache limit could never fail.

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
      prepared_statements = true

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"

      [[pools.example_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = 4
      """

  Scenario: Anonymous Parse from many clients keeps one statement per query on every backend
    When I run pgbench for "prepared_backend_growth" with options "PGSSLMODE=disable -n -M extended -c 16 -j 4 -t 200 -h 127.0.0.1 -p ${DOORMAN_PORT} -U example_user_1 example_db" and script:
      """
      SELECT 1 AS growth_probe;
      SELECT now() AS growth_probe;
      """
    # Four open transactions pin all four backends at once.
    And we create session "b1" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "BEGIN" to session "b1"
    And we create session "b2" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "BEGIN" to session "b2"
    And we create session "b3" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "BEGIN" to session "b3"
    And we create session "b4" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "BEGIN" to session "b4"
    Then we send SimpleQuery "SELECT CASE WHEN count(*) <= 2 THEN 'ok' ELSE 'backend holds ' || count(*) || ' DOORMAN statements' END FROM pg_prepared_statements WHERE name LIKE 'DOORMAN%' AND name NOT IN ('DOORMAN_release_begin', 'DOORMAN_release', 'DOORMAN_release_commit')" to session "b1" and store response
    And session "b1" should receive DataRow with "ok"
    And we send SimpleQuery "SELECT CASE WHEN count(*) <= 2 THEN 'ok' ELSE 'backend holds ' || count(*) || ' DOORMAN statements' END FROM pg_prepared_statements WHERE name LIKE 'DOORMAN%' AND name NOT IN ('DOORMAN_release_begin', 'DOORMAN_release', 'DOORMAN_release_commit')" to session "b2" and store response
    And session "b2" should receive DataRow with "ok"
    And we send SimpleQuery "SELECT CASE WHEN count(*) <= 2 THEN 'ok' ELSE 'backend holds ' || count(*) || ' DOORMAN statements' END FROM pg_prepared_statements WHERE name LIKE 'DOORMAN%' AND name NOT IN ('DOORMAN_release_begin', 'DOORMAN_release', 'DOORMAN_release_commit')" to session "b3" and store response
    And session "b3" should receive DataRow with "ok"
    And we send SimpleQuery "SELECT CASE WHEN count(*) <= 2 THEN 'ok' ELSE 'backend holds ' || count(*) || ' DOORMAN statements' END FROM pg_prepared_statements WHERE name LIKE 'DOORMAN%' AND name NOT IN ('DOORMAN_release_begin', 'DOORMAN_release', 'DOORMAN_release_commit')" to session "b4" and store response
    And session "b4" should receive DataRow with "ok"
