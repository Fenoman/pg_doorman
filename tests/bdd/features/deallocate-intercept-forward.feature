@python @deallocate-forward
Feature: simple-query DEALLOCATE must reach the backend (no synthetic ack)

  Regression. pg_doorman intercepted simple-query
  `DEALLOCATE <name>` / `DEALLOCATE ALL` with a synthetic CommandComplete
  and never forwarded the message to the backend. In transaction-pool
  mode the backend kept the prepared statement, so the next simple-query
  `PREPARE <name>` failed with SQLSTATE 42P05
  `prepared statement "<name>" already exists` - same shape as the
  legitimate-client-request-dropped bug class that fix beeaea7 closed
  for cancel, and ffae1e5 closed for GSSENC reject.

  Background:
    Given PostgreSQL started with pg_hba.conf:
      """
      local   all             all                                     trust
      host    all             all             127.0.0.1/32            trust
      host    all             all             ::1/128                 trust
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

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"

      [[pools.example_db.users]]
      username = "example_user_1"
      password = "md58a67a0c805a5ee0384ea28e0dea557b6"
      pool_size = 10
      """

  Scenario: simple-query PREPARE/DEALLOCATE/PREPARE both for named and ALL succeed
    When I run shell command:
      """
      cd tests/python && \
      export DATABASE_URL="postgresql://example_user_1:test@127.0.0.1:${DOORMAN_PORT}/example_db" && \
      python3 -m pytest -v ./test_deallocate_intercept.py
      """
    Then the command should succeed
