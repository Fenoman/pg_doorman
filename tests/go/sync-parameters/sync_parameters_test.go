package doorman

import (
	"database/sql"
	"fmt"
	"os"
	"testing"

	_ "github.com/lib/pq"
	"github.com/stretchr/testify/assert"
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
