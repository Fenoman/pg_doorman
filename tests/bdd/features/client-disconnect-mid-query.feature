@rust @rust-2 @client-disconnect-mid-query
Feature: A client that disconnects mid-query does not leave its query running outside the pool
  PostgreSQL does not notice a closed socket until it writes to it, so a
  backend dropped while its query runs keeps executing that query next to
  the replacement backend. The pooler keeps the backend until the abandoned
  query ended: a short query finishes and the backend is reused, a long one
  is canceled first.

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
      query_wait_timeout = "5s"
      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"
      [[pools.example_db.users]]
      username = "example_user_1"
      password = ""
      pool_size = 1
      """

  Scenario Outline: A query that finishes soon keeps its backend in the pool
    When we create session "gone" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "SELECT pg_backend_pid()" to session "gone" and store backend_pid
    And we send SimpleQuery "SELECT pg_sleep(0.3)" to session "gone" without waiting
    And we sleep 100ms
    And we <disconnect> session "gone"
    And we sleep 1000ms
    # The backend is idle in the pool, not counted as serving a client.
    And we create admin session "admin" to pg_doorman as "admin" with password "admin"
    And we execute "SHOW POOLS" on admin session "admin" and store response
    Then admin session "admin" column "sv_active" for row with "user" = "example_user_1" should be between 0 and 0
    And admin session "admin" column "sv_idle" for row with "user" = "example_user_1" should be between 1 and 1
    When we create session "next" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "SELECT pg_backend_pid()" to session "next" and store backend_pid
    Then backend_pid from session "next" should equal backend_pid from session "gone"

    Examples:
      | disconnect                        |
      | close                             |
      | abort TCP connection with RST for |

  Scenario Outline: A long query is canceled before its pool slot is reused
    When we create session "gone" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "SELECT pg_sleep(30)" to session "gone" without waiting
    And we sleep 200ms
    And we <disconnect> session "gone"
    And we create session "next" to pg_doorman as "example_user_1" with password "" and database "example_db"
    And we send SimpleQuery "SELECT count(*) FROM pg_stat_activity WHERE query = 'SELECT pg_sleep(30)' AND state = 'active'" to session "next" and store response
    Then session "next" should receive DataRow with "0"

    Examples:
      | disconnect                        |
      | close                             |
      | abort TCP connection with RST for |
