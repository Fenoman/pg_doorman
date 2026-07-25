@go @sync-server-parameters
Feature: Pool-level sync_server_parameters override
  Test that pool-level sync_server_parameters override applies
  client-sent session parameters on checkout.

  Background:
    Given PostgreSQL started with pg_hba.conf:
      """
      local   all             all                                     trust
      host    all             all             127.0.0.1/32            trust
      host    all             all             ::1/128                 trust
      """
    And fixtures from "tests/fixture.sql" applied
    And pg_doorman hba file contains:
      """
      host all all 127.0.0.1/32 md5
      """

  Scenario: Pool with sync_server_parameters=true applies client search_path
    Given pg_doorman started with config:
      """
      [general]
      host = "127.0.0.1"
      port = ${DOORMAN_PORT}
      admin_username = "admin"
      admin_password = "admin"
      pg_hba = {path = "${DOORMAN_HBA_FILE}"}
      sync_server_parameters = false

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"
      sync_server_parameters = true

      [[pools.example_db.users]]
      username = "example_user_1"
      password = "md58a67a0c805a5ee0384ea28e0dea557b6"
      pool_size = 10
      """
    When I run shell command:
      """
      export DATABASE_URL_WITH_SEARCH_PATH="postgresql://example_user_1:test@127.0.0.1:${DOORMAN_PORT}/example_db?sslmode=disable&search_path=bucket_0"
      cd tests/go && go test -v -run Test_SyncServerParametersWithSearchPath ./sync-parameters
      """
    Then the command should succeed
    And the command output should contain "PASS: Test_SyncServerParametersWithSearchPath"

  Scenario: Pool with sync_server_parameters=false ignores client search_path
    Given pg_doorman started with config:
      """
      [general]
      host = "127.0.0.1"
      port = ${DOORMAN_PORT}
      admin_username = "admin"
      admin_password = "admin"
      pg_hba = {path = "${DOORMAN_HBA_FILE}"}
      sync_server_parameters = false

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"
      sync_server_parameters = false

      [[pools.example_db.users]]
      username = "example_user_1"
      password = "md58a67a0c805a5ee0384ea28e0dea557b6"
      pool_size = 10
      """
    When I run shell command:
      """
      export DATABASE_URL_WITH_SEARCH_PATH="postgresql://example_user_1:test@127.0.0.1:${DOORMAN_PORT}/example_db?sslmode=disable&search_path=bucket_0"
      cd tests/go && go test -v -run Test_SyncServerParametersWithSearchPath ./sync-parameters
      """
    Then the command should fail

  Scenario: General sync_server_parameters=true applies client search_path
    Given pg_doorman started with config:
      """
      [general]
      host = "127.0.0.1"
      port = ${DOORMAN_PORT}
      admin_username = "admin"
      admin_password = "admin"
      pg_hba = {path = "${DOORMAN_HBA_FILE}"}
      sync_server_parameters = true

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"

      [[pools.example_db.users]]
      username = "example_user_1"
      password = "md58a67a0c805a5ee0384ea28e0dea557b6"
      pool_size = 10
      """
    When I run shell command:
      """
      export DATABASE_URL_WITH_SEARCH_PATH="postgresql://example_user_1:test@127.0.0.1:${DOORMAN_PORT}/example_db?sslmode=disable&search_path=bucket_0"
      cd tests/go && go test -v -run Test_SyncServerParametersWithSearchPath ./sync-parameters
      """
    Then the command should succeed
    And the command output should contain "PASS: Test_SyncServerParametersWithSearchPath"

  Scenario: sync_server_parameters is empty in general and declared for one database
    Given pg_doorman started with config:
      """
      [general]
      host = "127.0.0.1"
      port = ${DOORMAN_PORT}
      admin_username = "admin"
      admin_password = "admin"
      pg_hba = {path = "${DOORMAN_HBA_FILE}"}

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"
      sync_server_parameters = true

      [[pools.example_db.users]]
      username = "example_user_1"
      password = "md58a67a0c805a5ee0384ea28e0dea557b6"
      pool_size = 10
      """
    When I run shell command:
      """
      export DATABASE_URL_WITH_SEARCH_PATH="postgresql://example_user_1:test@127.0.0.1:${DOORMAN_PORT}/example_db?sslmode=disable&search_path=bucket_0"
      cd tests/go && go test -v -run Test_SyncServerParametersWithSearchPath ./sync-parameters
      """
    Then the command should succeed
    And the command output should contain "PASS: Test_SyncServerParametersWithSearchPath"

  Scenario: sync_server_parameters is empty in general and declared for two databases
    Given pg_doorman started with config:
      """
      [general]
      host = "127.0.0.1"
      port = ${DOORMAN_PORT}
      admin_username = "admin"
      admin_password = "admin"
      pg_hba = {path = "${DOORMAN_HBA_FILE}"}

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"
      sync_server_parameters = true

      [pools.other_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"
      sync_server_parameters = false

      [pools.another_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"

      [[pools.example_db.users]]
      username = "example_user_1"
      password = "md58a67a0c805a5ee0384ea28e0dea557b6"
      pool_size = 10

      [[pools.another_db.users]]
      username = "example_user_1"
      password = "md58a67a0c805a5ee0384ea28e0dea557b6"
      pool_size = 10

      [[pools.other_db.users]]
      username = "example_user_1"
      password = "md58a67a0c805a5ee0384ea28e0dea557b6"
      pool_size = 10
      """
    When I run shell command:
      """
      export DATABASE_URL_WITH_SEARCH_PATH="postgresql://example_user_1:test@127.0.0.1:${DOORMAN_PORT}/example_db?sslmode=disable&search_path=bucket_0"
      cd tests/go && go test -v -run Test_SyncServerParametersWithSearchPath ./sync-parameters
      """
    Then the command should succeed
    And the command output should contain "PASS: Test_SyncServerParametersWithSearchPath"

    When I run shell command:
      """
      export DATABASE_URL_WITH_SEARCH_PATH="postgresql://example_user_1:test@127.0.0.1:${DOORMAN_PORT}/another_db?sslmode=disable&search_path=bucket_0"
      cd tests/go && go test -v -run Test_SyncServerParametersWithSearchPath ./sync-parameters
      """
    Then the command should fail

    When I run shell command:
      """
      export DATABASE_URL_WITH_SEARCH_PATH="postgresql://example_user_1:test@127.0.0.1:${DOORMAN_PORT}/other_db?sslmode=disable&search_path=bucket_0"
      cd tests/go && go test -v -run Test_SyncServerParametersWithSearchPath ./sync-parameters
      """
    Then the command should fail

  Scenario: Extended protocol prepared statements resolve correct schema with different search_path
    Given pg_doorman started with config:
      """
      [general]
      host = "127.0.0.1"
      port = ${DOORMAN_PORT}
      admin_username = "admin"
      admin_password = "admin"
      pg_hba = {path = "${DOORMAN_HBA_FILE}"}
      sync_server_parameters = false

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"
      sync_server_parameters = true

      [[pools.example_db.users]]
      username = "example_user_1"
      password = "md58a67a0c805a5ee0384ea28e0dea557b6"
      pool_size = 10
      """
    When I run shell command:
      """
      export DATABASE_URL_BASE="postgresql://example_user_1:test@127.0.0.1:${DOORMAN_PORT}/example_db?sslmode=disable"
      cd tests/go && go test -v -run Test_ExtendedProtocolPreparedStatementDifferentSchemas ./sync-parameters
      """
    Then the command should succeed
    And the command output should contain "PASS: Test_ExtendedProtocolPreparedStatementDifferentSchemas"

  Scenario: Prepared INSERT targets bucket_0 before RELOAD, stays in bucket_0 after RELOAD
    Given pg_doorman started with config:
      """
      [general]
      host = "127.0.0.1"
      port = ${DOORMAN_PORT}
      admin_username = "admin"
      admin_password = "admin"
      pg_hba = {path = "${DOORMAN_HBA_FILE}"}

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"
      sync_server_parameters = true

      [[pools.example_db.users]]
      username = "example_user_1"
      password = "md58a67a0c805a5ee0384ea28e0dea557b6"
      pool_size = 10
      """
    # Overwrite config: remove sync_server_parameters (defaults to false).
    When we overwrite pg_doorman config file with:
      """
      [general]
      host = "127.0.0.1"
      port = ${DOORMAN_PORT}
      admin_username = "admin"
      admin_password = "admin"
      pg_hba = {path = "${DOORMAN_HBA_FILE}"}

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"

      [[pools.example_db.users]]
      username = "example_user_1"
      password = "md58a67a0c805a5ee0384ea28e0dea557b6"
      pool_size = 10
      """
    # Go test: prepare INSERT → RELOAD → execute → verify bucket_0 → new connection → verify public.
    When I run shell command:
      """
      export DATABASE_URL_WITH_SEARCH_PATH="postgresql://example_user_1:test@127.0.0.1:${DOORMAN_PORT}/example_db?sslmode=disable&search_path=bucket_0"
      export DOORMAN_PORT="${DOORMAN_PORT}"
      cd tests/go && go test -v -run Test_PreparedInsertTargetsCorrectSchemaAfterReload ./sync-parameters
      """
    Then the command should succeed
    And the command output should contain "PASS: Test_PreparedInsertTargetsCorrectSchemaAfterReload"

  Scenario: Different clients send different search_path to the same pool
    Given pg_doorman started with config:
      """
      [general]
      host = "127.0.0.1"
      port = ${DOORMAN_PORT}
      admin_username = "admin"
      admin_password = "admin"
      pg_hba = {path = "${DOORMAN_HBA_FILE}"}
      sync_server_parameters = false

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"
      sync_server_parameters = true

      [[pools.example_db.users]]
      username = "example_user_1"
      password = "md58a67a0c805a5ee0384ea28e0dea557b6"
      pool_size = 10
      """
    When I run shell command:
      """
      export DATABASE_URL_BASE="postgresql://example_user_1:test@127.0.0.1:${DOORMAN_PORT}/example_db?sslmode=disable"
      cd tests/go && go test -v -run Test_DifferentSearchPathsInSamePool ./sync-parameters
      """
    Then the command should succeed
    And the command output should contain "PASS: Test_DifferentSearchPathsInSamePool"

  Scenario: sync_server_parameters is not declared
    Given pg_doorman started with config:
      """
      [general]
      host = "127.0.0.1"
      port = ${DOORMAN_PORT}
      admin_username = "admin"
      admin_password = "admin"
      pg_hba = {path = "${DOORMAN_HBA_FILE}"}

      [pools.example_db]
      server_host = "127.0.0.1"
      server_port = ${PG_PORT}
      pool_mode = "transaction"

      [[pools.example_db.users]]
      username = "example_user_1"
      password = "md58a67a0c805a5ee0384ea28e0dea557b6"
      pool_size = 10
      """
    When I run shell command:
      """
      export DATABASE_URL_WITH_SEARCH_PATH="postgresql://example_user_1:test@127.0.0.1:${DOORMAN_PORT}/example_db?sslmode=disable&search_path=bucket_0"
      cd tests/go && go test -v -run Test_SyncServerParametersWithSearchPath ./sync-parameters
      """
    Then the command should fail
