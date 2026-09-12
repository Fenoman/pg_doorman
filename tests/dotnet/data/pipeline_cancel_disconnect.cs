using System.Data;
using System.Net.Sockets;
using System.Reflection;
using Npgsql;

// Verify pool recovery after a complete streamed response and a frontend RST.
// Use DATABASE_URL environment variable if set, otherwise use default
string baseConnectionString = Environment.GetEnvironmentVariable("DATABASE_URL")
    ?? "Host=127.0.0.1;Port=6433;Database=example_db;Username=example_user_1;Password=test;";

// Keep one Npgsql pool slot; ClearPool discards the physically closed frontend.
// Timeout=0 — no connection timeout
string connectionString = baseConnectionString + "Timeout=0;Maximum Pool Size=1;";

string payload = new('0', 4 * 1024 * 1024);
bool transportClosedAfterVerifiedRow = false;

Console.WriteLine("Test: Complete 4 MiB response, then TCP RST and pool recovery");

try
{
    await RunWithPhysicalBreakConnection(connectionString);
}
catch (Exception) when (transportClosedAfterVerifiedRow)
{
    // Disposing the reader/connection after the deliberate RST may fail.
}
if (!transportClosedAfterVerifiedRow)
    throw new Exception("Client A did not verify a row and close its transport");
Console.WriteLine("Client A: Transport closed after verified 4 MiB response");

// A new frontend must receive a complete response from the same pool.
await Run(connectionString);
Console.WriteLine("Client B: Query completed successfully");

Console.WriteLine("pipeline_cancel_disconnect complete");

async Task RunWithPhysicalBreakConnection(string connStr)
{
    await using var connection = new NpgsqlConnection(connStr);
    await using var cmd = connection.CreateCommand();
    cmd.CommandText = "SELECT @payload";
    cmd.Parameters.Add(new NpgsqlParameter("payload", payload));
    await connection.OpenAsync();
    await using var reader = await cmd.ExecuteReaderAsync(CommandBehavior.SequentialAccess);
    if (!await reader.ReadAsync() || reader.GetString(0) != payload)
        throw new Exception("Client A did not receive the complete 4 MiB payload");
    KillTransport(connection);
    transportClosedAfterVerifiedRow = true;
    await connection.CloseAsync();
}

async Task Run(string connStr)
{
    await using var connection = new NpgsqlConnection(connStr);
    await using var cmd = connection.CreateCommand();
    cmd.CommandText = "SELECT @payload";
    cmd.Parameters.Add(new NpgsqlParameter("payload", payload));
    await connection.OpenAsync();
    await using var reader = await cmd.ExecuteReaderAsync(CommandBehavior.SequentialAccess);
    if (!await reader.ReadAsync() || reader.GetString(0) != payload)
        throw new Exception("Client B did not receive the complete 4 MiB payload");
    if (await reader.ReadAsync())
        throw new Exception("Client B received an unexpected extra row");
    await connection.CloseAsync();
}

void KillTransport(NpgsqlConnection connection)
{
    var connectorProp = typeof(NpgsqlConnection).GetProperty(
                            "Connector",
                            BindingFlags.Instance | BindingFlags.NonPublic)
                        ?? throw new MissingMemberException("NpgsqlConnection.Connector not found.");

    var connector = connectorProp.GetValue(connection)
                    ?? throw new InvalidOperationException("Connection has no bound connector.");

    var t = connector.GetType();

    var socketField = t.GetField("_socket", BindingFlags.Instance | BindingFlags.NonPublic);
    var streamField = t.GetField("_stream", BindingFlags.Instance | BindingFlags.NonPublic);
    var baseStreamField = t.GetField("_baseStream", BindingFlags.Instance | BindingFlags.NonPublic);

    if (socketField?.GetValue(connector) is not Socket socket)
        throw new MissingMemberException("Npgsql connector socket not found");
    socket.LingerState = new LingerOption(enable: true, seconds: 0);
    socket.Dispose();

    try { (streamField?.GetValue(connector) as IDisposable)?.Dispose(); } catch { }
    try { (baseStreamField?.GetValue(connector) as IDisposable)?.Dispose(); } catch { }

    NpgsqlConnection.ClearPool(connection);
}
