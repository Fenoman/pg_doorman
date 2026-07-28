package doorman

import (
	"database/sql"
	"fmt"
	"os"
	"testing"

	_ "github.com/lib/pq"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func Test_SyncServerParametersWithSearchPath(t *testing.T) {
	db, err := sql.Open("postgres", os.Getenv("DATABASE_URL_WITH_SEARCH_PATH"))
	assert.NoError(t, err)
	defer db.Close()
	var searchPath string
	assert.NoError(t, db.QueryRow(`SHOW search_path`).Scan(&searchPath))
	assert.Equal(t, "bucket_0", searchPath)
}

func Test_DifferentSearchPathsInSamePool(t *testing.T) {
	baseDSN := os.Getenv("DATABASE_URL_BASE")

	cases := []struct {
		name       string
		searchPath string
	}{
		{"bucket_0", "bucket_0"},
		{"bucket_100555", "bucket_100555"},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			dsn := fmt.Sprintf("%s&search_path=%s", baseDSN, tc.searchPath)
			db, err := sql.Open("postgres", dsn)
			assert.NoError(t, err)
			defer db.Close()

			var got string
			assert.NoError(t, db.QueryRow(`SHOW search_path`).Scan(&got))
			assert.Equal(t, tc.searchPath, got)
		})
	}
}

// Test_SyncServerParametersActivatedAfterReload verifies that enabling
// general.sync_server_parameters via RELOAD activates parameter syncing.
// Before RELOAD: search_path from DSN is ignored (INSERT goes to public).
// After RELOAD:  search_path from DSN is synced (INSERT goes to bucket_0).
func Test_SyncServerParametersActivatedAfterReload(t *testing.T) {
	dsnWithSearchPath := os.Getenv("DATABASE_URL_WITH_SEARCH_PATH")
	adminPort := os.Getenv("DOORMAN_PORT")

	setupBucketTables(t, dsnWithSearchPath)

	// ---- Phase 1: sync_server_parameters is OFF (not in config) ----
	db, err := sql.Open("postgres", dsnWithSearchPath)
	require.NoError(t, err)
	defer db.Close()

	var searchPath string
	require.NoError(t, db.QueryRow("SHOW search_path").Scan(&searchPath))
	assert.Equal(t, `"$user", public`, searchPath, "sync_server_parameters is off, search_path stays default")

	_, err = db.Exec("INSERT INTO items (id, val) VALUES ($1, $2)", 1, "before_reload")
	require.NoError(t, err)

	var count int
	require.NoError(t, db.QueryRow("SELECT count(*) FROM public.items").Scan(&count))
	assert.Equal(t, 1, count, "row should be in public.items (search_path not synced)")

	require.NoError(t, db.QueryRow("SELECT count(*) FROM bucket_0.items").Scan(&count))
	assert.Equal(t, 0, count, "bucket_0.items should be empty")
	db.Close()

	// ---- Phase 2: RELOAD enables general.sync_server_parameters ----
	adminAddr := fmt.Sprintf("127.0.0.1:%s", adminPort)
	adminDB, err := sql.Open("postgres", fmt.Sprintf("postgresql://admin:admin@%s/pgbouncer?sslmode=disable", adminAddr))
	require.NoError(t, err)
	defer adminDB.Close()

	_, err = adminDB.Exec("RELOAD")
	require.NoError(t, err)
	adminDB.Close()

	// ---- Phase 3: new connection, search_path is now synced ----
	db2, err := sql.Open("postgres", dsnWithSearchPath)
	require.NoError(t, err)
	defer db2.Close()

	require.NoError(t, db2.QueryRow("SHOW search_path").Scan(&searchPath))
	assert.Equal(t, "bucket_0", searchPath, "sync_server_parameters is on, search_path is synced")

	_, err = db2.Exec("INSERT INTO items (id, val) VALUES ($1, $2)", 2, "after_reload")
	require.NoError(t, err)

	require.NoError(t, db2.QueryRow("SELECT count(*) FROM bucket_0.items").Scan(&count))
	assert.Equal(t, 1, count, "new row should be in bucket_0.items (search_path synced)")

	require.NoError(t, db2.QueryRow("SELECT count(*) FROM public.items").Scan(&count))
	assert.Equal(t, 1, count, "public.items should still have 1 row from phase 1")
}
