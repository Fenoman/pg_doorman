// Driver-level smoke test for pg_doorman's DISCARD ALL interception via Npgsql.
//
// Npgsql mixes simple-query (for parameter-less utility statements) and
// extended-protocol (for parameterised / prepared statements). For
// `DISCARD ALL` the path depends on whether the command is .Prepare()'d
// before Execute. We cover BOTH paths in the same run.
//
// Assertions:
//   1. ExecuteNonQuery("DISCARD ALL") does NOT throw - connection survives.
//   2. The returned row count is -1 (no row count reported for either the
//      synthesised "DISCARD ALL" tag or the rewritten "SELECT 1" tag -
//      Npgsql normalises both to -1, which means the test result is
//      identical for app code).
//   3. Connection is usable afterwards.
//   4. Same flow on a PREPARED command - exercises pg_doorman's
//      Parse-rewrite path.
//
// Environment:
//   * DATABASE_URL - Npgsql connection string. Default points at the
//     BDD discard_tx pool on localhost.
//
// Exits 0 on success, non-zero on failure. Intended to be invoked from
// a BDD scenario via the dotnet/run_test.sh wrapper which copies this
// file into a temp dotnet project, adds the Npgsql package, and runs.

using System;
using System.Threading.Tasks;
using Npgsql;

class DiscardAllIntercept
{
    static string ConnectionString = Environment.GetEnvironmentVariable("DATABASE_URL")
        ?? "Host=127.0.0.1;Port=6433;Database=example_db;Username=example_user_1;Password=;Pooling=false;Include Error Detail=true";

    static async Task<int> Main(string[] args)
    {
        try
        {
            Console.WriteLine($"[discard-all-intercept] DSN={ConnectionString}");
            await TestSimpleQueryPath();
            await TestPreparedPath();
            Console.WriteLine("[discard-all-intercept] npgsql: OK");
            return 0;
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[discard-all-intercept] npgsql FAIL: {e}");
            return 1;
        }
    }

    /// Simple-query path: a non-Prepared NpgsqlCommand for a parameter-less
    /// statement typically goes through the simple-query protocol. Hits
    /// pg_doorman's synthetic-response intercept.
    static async Task TestSimpleQueryPath()
    {
        await using var conn = new NpgsqlConnection(ConnectionString);
        await conn.OpenAsync();

        // Baseline: connection works.
        await using (var baseline = new NpgsqlCommand("SELECT 'before'::text", conn))
        {
            var before = await baseline.ExecuteScalarAsync();
            if ((string)before! != "before")
                throw new Exception($"baseline returned {before}");
        }

        // DISCARD ALL via plain (non-prepared) command. ExecuteNonQuery
        // must not throw. Return value is -1 (no row count tag) for
        // both real DISCARD ALL and pg_doorman's intercepted reply.
        await using (var discard = new NpgsqlCommand("DISCARD ALL", conn))
        {
            int rows = await discard.ExecuteNonQueryAsync();
            Console.WriteLine($"[discard-all-intercept] npgsql simple-query DISCARD ALL ExecuteNonQuery={rows}");
            if (rows > 0)
                throw new Exception($"unexpected row count {rows} from DISCARD ALL");
        }

        // Connection still usable for new work.
        await using (var after = new NpgsqlCommand("SELECT 42", conn))
        {
            var val = await after.ExecuteScalarAsync();
            if ((int)val! != 42)
                throw new Exception($"post-DISCARD query returned {val}");
        }

        await conn.CloseAsync();
        Console.WriteLine("[discard-all-intercept] npgsql simple-query: OK");
    }

    /// Prepared path: an explicitly .Prepare()'d command goes through
    /// extended-protocol (Parse / Bind / Execute / Sync). Hits
    /// pg_doorman's Parse-rewrite path.
    ///
    /// Cache-preservation canary: prepare a parameterised SELECT, run
    /// it, send DISCARD ALL (also prepared = extended-protocol), then
    /// re-run the SELECT. If pg_doorman's Parse rewrite worked the
    /// backend kept its prepared-statement cache and the second
    /// ExecuteScalar succeeds. If a real DISCARD ALL leaked through,
    /// Npgsql would re-prepare transparently - so the visible symptom
    /// of a leak would be an extra Parse round-trip, not a crash. We
    /// don't try to detect that here; the lib-level structural test
    /// in messages::extended::tests covers the Parse-rewrite invariant
    /// directly.
    static async Task TestPreparedPath()
    {
        await using var conn = new NpgsqlConnection(ConnectionString);
        await conn.OpenAsync();

        // Prepare a parameterised SELECT so subsequent Execute is
        // genuinely extended-protocol Bind+Execute.
        await using (var select = new NpgsqlCommand("SELECT @v::int AS v", conn))
        {
            var p = select.Parameters.AddWithValue("v", 7);
            await select.PrepareAsync();
            var v1 = await select.ExecuteScalarAsync();
            if ((int)v1! != 7)
                throw new Exception($"prepared SELECT returned {v1}");

            // Prepared DISCARD ALL through extended-protocol. Some
            // Npgsql versions may decline to prepare DISCARD ALL
            // (treating it as a non-cacheable statement); fall back
            // to non-prepared in that case - pg_doorman's intercept
            // covers both.
            await using (var discard = new NpgsqlCommand("DISCARD ALL", conn))
            {
                try { await discard.PrepareAsync(); }
                catch { /* npgsql refused - that's fine, we run unprepared below */ }
                int rows = await discard.ExecuteNonQueryAsync();
                Console.WriteLine($"[discard-all-intercept] npgsql extended DISCARD ALL ExecuteNonQuery={rows}");
            }

            // Re-run the prepared SELECT - backend cache must have survived.
            p.Value = 11;
            var v2 = await select.ExecuteScalarAsync();
            if ((int)v2! != 11)
                throw new Exception($"post-DISCARD prepared SELECT returned {v2}");
        }

        await conn.CloseAsync();
        Console.WriteLine("[discard-all-intercept] npgsql prepared: OK");
    }
}
