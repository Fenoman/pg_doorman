@rust @rust-1 @protocol-recovery
Feature: Protocol boundaries preserve complete results and statement namespaces
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
      client_anonymous_prepared_cache_size = 1
      response_flush_threshold = 65536
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

  @protocol-recovery-c1
  Scenario: A result before COPY FROM hands control back to the client
    Then mixed COPY case "prefix" completes identically on both connections

  @protocol-recovery-c2
  Scenario: A large result after COPY FROM is drained before release
    Then mixed COPY case "large suffix" completes identically on both connections

  Scenario: A small result after COPY FROM completes normally
    Then mixed COPY case "small suffix" completes identically on both connections

  Scenario: Multiple COPY FROM operations can surround streamed query results
    Then mixed COPY case "multiple" completes identically on both connections

  Scenario: COPY FROM can be followed by streaming COPY TO
    Then mixed COPY case "copy out suffix" completes identically on both connections

  @protocol-recovery-copy-flush
  Scenario Outline: Extended COPY completion does not wait for a later Sync
    Then extended COPY "<completion>" completes before a later Sync
    Examples:
      | completion |
      | done       |
      | fail       |

  @protocol-recovery-c3-close
  Scenario: Close in an error-skipped batch suffix preserves the named statement
    Then extended error case "Close" preserves the PostgreSQL statement namespace

  @protocol-recovery-c3-parse
  Scenario: A cached Parse in an error-skipped suffix cannot create a statement
    Then extended error case "cached Parse" preserves the PostgreSQL statement namespace

  Scenario Outline: Other namespace changes in an error-skipped suffix are restored
    Then extended error case "<case>" preserves the PostgreSQL statement namespace
    Examples:
      | case              |
      | named overwrite   |
      | unnamed overwrite |
      | unnamed Close     |
      | after Flush       |
      | Simple Query after Flush |
      | acknowledged Close |
      | repeated mutations |

  @protocol-recovery-c4
  Scenario: Ordinary Simple Query invalidates the unnamed statement
    Then Simple Query case "ordinary" invalidates the logical unnamed statement

  Scenario: A backend cache alias is not part of the logical client namespace
    Then backend cache aliases cannot be addressed as logical client statements

  @protocol-recovery-streamed-parse
  Scenario Outline: A cached Parse acknowledgement precedes a streamed result exactly once
    Then a cached Parse before streaming Execute with "<case>" is acknowledged exactly once
    Examples:
      | case      |
      | many rows |
      | large row |

  Scenario Outline: Simple Query fast paths invalidate the unnamed statement
    Then Simple Query case "<case>" invalidates the logical unnamed statement
    Examples:
      | case                |
      | empty               |
      | deferred BEGIN      |
      | intercepted DISCARD |
      | health check        |
