package doorman

import (
	"context"
	"database/sql"
	"fmt"
	"os"
	"testing"

	"github.com/jackc/pgx/v5"
	_ "github.com/lib/pq"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func setupTestSchemas(t *testing.T, dsn string) {
	t.Helper()
	ctx := context.Background()

	conn, err := pgx.Connect(ctx, dsn)
	require.NoError(t, err)
	defer conn.Close(ctx)

	for _, s := range []string{"schema_a", "schema_b"} {
		_, _ = conn.Exec(ctx, "DROP SCHEMA IF EXISTS "+s+" CASCADE")
		_, err = conn.Exec(ctx, "CREATE SCHEMA "+s)
		require.NoError(t, err)
		_, err = conn.Exec(ctx, "CREATE TABLE "+s+".users (id int, name text)")
		require.NoError(t, err)
	}
	_, err = conn.Exec(ctx, "INSERT INTO schema_a.users VALUES (1, 'alice')")
	require.NoError(t, err)
	_, err = conn.Exec(ctx, "INSERT INTO schema_b.users VALUES (2, 'bob')")
	require.NoError(t, err)
}

func setupBucketTables(t *testing.T, dsn string) {
	t.Helper()
	ctx := context.Background()

	conn, err := pgx.Connect(ctx, dsn)
	require.NoError(t, err)
	defer conn.Close(ctx)

	_, _ = conn.Exec(ctx, "DROP TABLE IF EXISTS public.items")
	_, _ = conn.Exec(ctx, "DROP TABLE IF EXISTS bucket_0.items")
	_, _ = conn.Exec(ctx, "DROP SCHEMA IF EXISTS bucket_0 CASCADE")
	_, err = conn.Exec(ctx, "CREATE SCHEMA bucket_0")
	require.NoError(t, err)
	_, err = conn.Exec(ctx, "CREATE TABLE public.items (id int, val text)")
	require.NoError(t, err)
	_, err = conn.Exec(ctx, "CREATE TABLE bucket_0.items (id int, val text)")
	require.NoError(t, err)
}

func Test_ExtendedProtocolPreparedStatementDifferentSchemas(t *testing.T) {
	doormanDSN := os.Getenv("DATABASE_URL_BASE")
	setupTestSchemas(t, doormanDSN)

	ctx := context.Background()

	type testCase struct {
		searchPath   string
		expectedID   int
		expectedName string
	}

	cases := []testCase{
		{"schema_a", 1, "alice"},
		{"schema_b", 2, "bob"},
	}

	for _, tc := range cases {
		t.Run(tc.searchPath, func(t *testing.T) {
			dsn := doormanDSN + "&search_path=" + tc.searchPath
			conn, err := pgx.Connect(ctx, dsn)
			require.NoError(t, err)
			defer conn.Close(ctx)

			rows, err := conn.Query(ctx, "SELECT id, name FROM users")
			require.NoError(t, err)
			defer rows.Close()

			require.True(t, rows.Next())
			var id int
			var name string
			require.NoError(t, rows.Scan(&id, &name))
			assert.Equal(t, tc.expectedID, id)
			assert.Equal(t, tc.expectedName, name)
			assert.False(t, rows.Next())
		})
	}
}

func Test_PreparedInsertTargetsCorrectSchemaAfterReload(t *testing.T) {
	dsnWithSearchPath := os.Getenv("DATABASE_URL_WITH_SEARCH_PATH")
	adminPort := os.Getenv("DOORMAN_PORT")
	ctx := context.Background()

	setupBucketTables(t, dsnWithSearchPath)

	conn, err := pgx.Connect(ctx, dsnWithSearchPath)
	require.NoError(t, err)
	defer conn.Close(ctx)

	// Prepare INSERT via extended protocol.
	// sync_server_parameters = true, search_path = bucket_0 is synced,
	// so the statement uses schema bucket_0
	PreparedStatement, err := conn.Prepare(ctx, "insert_item", "INSERT INTO items (id, val) VALUES ($1, $2)")
	require.NoError(t, err)
	assert.Equal(t, "insert_item", PreparedStatement.Name)

	// Trigger RELOAD for cleaning sync_server_parameters
	adminAddr := fmt.Sprintf("127.0.0.1:%s", adminPort)
	adminDB, err := sql.Open("postgres", fmt.Sprintf("postgresql://admin:admin@%s/pgbouncer?sslmode=disable", adminAddr))
	require.NoError(t, err)
	defer adminDB.Close()

	_, err = adminDB.Exec("RELOAD")
	require.NoError(t, err)
	adminDB.Close()

	// Execute the prepared statement.
	_, err = conn.Exec(ctx, "insert_item", 1, "test")
	require.NoError(t, err)

	// Verify: row in bucket_0.items, public.items is empty.
	var count int
	err = conn.QueryRow(ctx, "SELECT count(*) FROM bucket_0.items").Scan(&count)
	require.NoError(t, err)
	assert.Equal(t, 1, count, "row should be in bucket_0.items")

	err = conn.QueryRow(ctx, "SELECT count(*) FROM public.items").Scan(&count)
	require.NoError(t, err)
	assert.Equal(t, 0, count, "public.items should be empty")

	// New connection after RELOAD: sync_server_parameters is off,
	// search_path is NOT synced, so INSERT goes to default (public).
	afterReloadConn, err := pgx.Connect(ctx, dsnWithSearchPath)
	require.NoError(t, err)
	defer afterReloadConn.Close(ctx)

	_, err = afterReloadConn.Exec(ctx, "INSERT INTO items (id, val) VALUES ($1, $2)", 2, "test2")
	require.NoError(t, err)

	err = afterReloadConn.QueryRow(ctx, "SELECT count(*) FROM public.items").Scan(&count)
	require.NoError(t, err)
	assert.Equal(t, 1, count, "new row should be in public.items")

	err = afterReloadConn.QueryRow(ctx, "SELECT count(*) FROM bucket_0.items").Scan(&count)
	require.NoError(t, err)
	assert.Equal(t, 1, count, "bucket_0.items should still have 1 row")
}

// Test_PreparedInsertTargetsCorrectSchemaAfterPoolLevelReload verifies that
// when sync_server_parameters is enabled at the pool level, a PREPARE binds
// to the client's search_path. After RELOAD removes the pool-level override
// (effective value becomes false), the in-flight backend still carries the
// old search_path, so the named prepared statement resolves to the same
// bucket_0 target. A new connection after RELOAD gets the default search_path
// and INSERT goes to public.
func Test_PreparedInsertTargetsCorrectSchemaAfterPoolLevelReload(t *testing.T) {
	dsnWithSearchPath := os.Getenv("DATABASE_URL_WITH_SEARCH_PATH")
	adminPort := os.Getenv("DOORMAN_PORT")
	ctx := context.Background()

	setupBucketTables(t, dsnWithSearchPath)

	conn, err := pgx.Connect(ctx, dsnWithSearchPath)
	require.NoError(t, err)
	defer conn.Close(ctx)

	// Prepare INSERT via extended protocol.
	// Pool-level sync_server_parameters = true, search_path = bucket_0 is
	// synced, so the statement binds to schema bucket_0.
	PreparedStatement, err := conn.Prepare(ctx, "insert_item", "INSERT INTO items (id, val) VALUES ($1, $2)")
	require.NoError(t, err)
	assert.Equal(t, "insert_item", PreparedStatement.Name)

	// RELOAD: config now has pool-level sync_server_parameters removed
	// (defaults to false). The pool is rebuilt, but the in-flight backend
	// connection still carries search_path = bucket_0.
	adminAddr := fmt.Sprintf("127.0.0.1:%s", adminPort)
	adminDB, err := sql.Open("postgres", fmt.Sprintf("postgresql://admin:admin@%s/pgbouncer?sslmode=disable", adminAddr))
	require.NoError(t, err)
	defer adminDB.Close()

	_, err = adminDB.Exec("RELOAD")
	require.NoError(t, err)
	adminDB.Close()

	// Execute the prepared INSERT by name.
	// The in-flight backend retains search_path = bucket_0 from checkout,
	// so even though pool-level sync_server_parameters is now off, the
	// backend's search_path has not changed and the re-plan resolves to
	// the same bucket_0 target.
	_, err = conn.Exec(ctx, "insert_item", 1, "test")
	require.NoError(t, err)

	// Verify: row in bucket_0.items, public.items is empty.
	var count int
	err = conn.QueryRow(ctx, "SELECT count(*) FROM bucket_0.items").Scan(&count)
	require.NoError(t, err)
	assert.Equal(t, 1, count, "row should be in bucket_0.items (in-flight backend retains search_path)")

	err = conn.QueryRow(ctx, "SELECT count(*) FROM public.items").Scan(&count)
	require.NoError(t, err)
	assert.Equal(t, 0, count, "public.items should be empty")

	// New connection after RELOAD: pool-level sync_server_parameters is off,
	// search_path is NOT synced, so INSERT goes to default (public).
	afterReloadConn, err := pgx.Connect(ctx, dsnWithSearchPath)
	require.NoError(t, err)
	defer afterReloadConn.Close(ctx)

	_, err = afterReloadConn.Exec(ctx, "INSERT INTO items (id, val) VALUES ($1, $2)", 2, "test2")
	require.NoError(t, err)

	err = afterReloadConn.QueryRow(ctx, "SELECT count(*) FROM public.items").Scan(&count)
	require.NoError(t, err)
	assert.Equal(t, 1, count, "new row should be in public.items")

	err = afterReloadConn.QueryRow(ctx, "SELECT count(*) FROM bucket_0.items").Scan(&count)
	require.NoError(t, err)
	assert.Equal(t, 1, count, "bucket_0.items should still have 1 row")
}
