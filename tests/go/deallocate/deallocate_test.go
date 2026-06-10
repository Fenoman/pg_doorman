package doorman_test

import (
	"context"
	"os"
	"testing"

	"github.com/jackc/pgx/v5/pgxpool"
	"github.com/stretchr/testify/assert"
)

// PREPARE then DEALLOCATE round-trip through pg_doorman.
//
// pg_doorman forwards every simple-query DEALLOCATE to the backend. The legacy
// test exercised `DEALLOCATE "test"` with no preceding PREPARE and depended on
// the silent-OK behaviour - which masked the real PostgreSQL
// SQLSTATE 26000 `prepared statement "test" does not exist` and
// caused 42P05 on re-PREPARE of the same name in transaction-pool
// mode (see tests/python/test_deallocate_intercept.py).
//
// After the fix pg_doorman forwards DEALLOCATE honestly. A
// PREPARE+DEALLOCATE round trip exercises both the client-side
// cache update and the forwarded result. DEALLOCATE of a missing
// name now (correctly) returns the backend's 26000 error, matching
// native PostgreSQL semantics.
func TestDeallocate(t *testing.T) {
	ctx := context.Background()
	db, err := pgxpool.New(ctx, os.Getenv("DATABASE_URL"))
	assert.NoError(t, err)
	defer db.Close()

	_, err = db.Exec(ctx, "prepare \"test\" as select 1")
	assert.NoError(t, err, "PREPARE must succeed")

	_, err = db.Exec(ctx, "deallocate \"test\"")
	assert.NoError(t, err, "DEALLOCATE of an existing statement must succeed")

	// DEALLOCATE of a missing name now matches native PostgreSQL -
	// 26000 instead of the legacy silent ack.
	_, err = db.Exec(ctx, "deallocate \"nonexistent\"")
	assert.Error(t, err, "DEALLOCATE of a missing name must surface the backend's 26000 error")
}
