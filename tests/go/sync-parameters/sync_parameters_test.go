package doorman

import (
	"database/sql"
	"os"
	"testing"

	_ "github.com/lib/pq"
	"github.com/stretchr/testify/assert"
)

func Test_SyncServerParameters(t *testing.T) {
	db, err := sql.Open("postgres", os.Getenv("DATABASE_URL"))
	assert.NoError(t, err)
	defer db.Close()
	var searchPath string
	assert.NoError(t, db.QueryRow(`SHOW search_path`).Scan(&searchPath))
	assert.Equal(t, "bucket_0", searchPath)
}
