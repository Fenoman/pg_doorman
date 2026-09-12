@rust @rust-1 @copy-flush-recovery
Feature: COPY input preserves data across Flush and Sync
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
      release_query = ""
      [[pools.example_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1
      """
    When we login to postgres and pg_doorman as "example_user_1" with password "" and database "example_db"

  Scenario Outline: COPY control messages preserve buffered rows and transaction state
    Then COPY FROM via "<protocol>" preserves rows across "<control>"
    Examples:
      | protocol | control |
      | simple   | Flush   |
      | simple   | Sync    |
      | extended | Flush   |
      | extended | Sync    |

  @idle-backend-response
  Scenario Outline: Cancellation during idle COPY preserves the client and transaction
    Then cancellation during idle COPY via "<protocol>" preserves the client session
    Examples:
      | protocol       |
      | simple         |
      | extended_sync  |
      | extended_flush |
