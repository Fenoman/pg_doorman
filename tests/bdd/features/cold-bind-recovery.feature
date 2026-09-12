@rust @rust-1 @cold-bind-recovery @protocol-recovery
Feature: Cold backend statements preserve frontend protocol order
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
      server_prepared_statements_cache_size = 1
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

  Scenario Outline: A cold statement can be used after Flush without ending its transaction
    Then a cold "<operation>" after "<boundary>" preserves the open protocol cycle using "<terminal>"
    Examples:
      | operation | boundary        | terminal |
      | Bind      | Execute         | Sync     |
      | Bind      | Describe        | Sync     |
      | Bind      | PortalSuspended | Sync     |
      | Describe  | Execute         | Sync     |
      | Describe  | Describe        | Sync     |
      | Describe  | PortalSuspended | Sync     |
      | Bind      | Execute         | Flush    |
      | Bind      | Describe        | Flush    |
      | Bind      | PortalSuspended | Flush    |
      | Describe  | Execute         | Flush    |
      | Describe  | Describe        | Flush    |
      | Describe  | PortalSuspended | Flush    |

  Scenario Outline: Reprepare follows error recovery and frontend acknowledgements
    Then cold reprepare error case "<case>" preserves savepoint recovery and statement names
    Examples:
      | case                  |
      | aborted transaction   |
      | buffered rollback     |
      | acknowledgement order |

  Scenario Outline: Hidden ParseComplete does not shift streamed frontend replies
    Then a hidden reprepare before "<case>" preserves synthetic ParseComplete order
    Examples:
      | case      |
      | many rows |
      | large row |
