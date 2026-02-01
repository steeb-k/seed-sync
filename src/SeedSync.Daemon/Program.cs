using System.Text.Json.Serialization;
using SeedSync.Core.Services;
using SeedSync.Daemon;
using SeedSync.Daemon.Services;

var builder = WebApplication.CreateBuilder(args);

// Serialize enums as strings so API consumers (e.g. GUI) get "ReadWrite"/"ReadOnly" not 0/1
builder.Services.ConfigureHttpJsonOptions(options =>
{
    options.SerializerOptions.Converters.Add(new JsonStringEnumConverter());
});

// Configure the app data path
var appDataPath = Path.Combine(
    Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
    "S.E.E.D");
Directory.CreateDirectory(appDataPath);

// Register services
builder.Services.AddSingleton(new ShareRepository(appDataPath));
builder.Services.AddSingleton<AccessController>();
builder.Services.AddSingleton(sp => new SyncService(
    sp.GetRequiredService<ShareRepository>(),
    sp.GetRequiredService<AccessController>(),
    sp.GetRequiredService<ILogger<SyncService>>(),
    appDataPath));

// Add the background worker
builder.Services.AddHostedService<SyncWorker>();

// Configure to only listen on localhost
builder.WebHost.UseUrls("http://127.0.0.1:9876");

var app = builder.Build();

// Root: show app info so visiting http://127.0.0.1:9876 returns something useful
app.MapGet("/", () => Results.Json(new
{
    name = "S.E.E.D.",
    description = "Secure Environment Exchange Daemon",
    version = "1.0.0",
    api = "http://127.0.0.1:9876/api",
    endpoints = new
    {
        health = "GET /api/health",
        shares = "GET /api/shares",
        share = "GET /api/shares/{id}",
        createShare = "POST /api/shares",
        addShare = "POST /api/shares/add",
        removeShare = "DELETE /api/shares/{id}"
    }
}));

// API Endpoints
var api = app.MapGroup("/api");

// List all shares
api.MapGet("/shares", (SyncService syncService) =>
{
    var shares = syncService.ListShares();
    return Results.Ok(shares);
});

// Get share status
api.MapGet("/shares/{shareId}", (string shareId, SyncService syncService) =>
{
    var status = syncService.GetShareStatus(shareId);
    return status is null ? Results.NotFound() : Results.Ok(status);
});

// Create new share
api.MapPost("/shares", async (CreateShareRequest request, SyncService syncService) =>
{
    try
    {
        var result = await syncService.CreateShareAsync(request);
        return Results.Created($"/api/shares/{result.ShareId}", result);
    }
    catch (DirectoryNotFoundException ex)
    {
        return Results.BadRequest(new { error = ex.Message });
    }
    catch (Exception ex)
    {
        return Results.Problem(ex.Message);
    }
});

// Add existing share
api.MapPost("/shares/add", async (AddShareRequest request, SyncService syncService) =>
{
    try
    {
        var result = await syncService.AddShareAsync(request);
        return Results.Created($"/api/shares/{result.ShareId}", result);
    }
    catch (ArgumentException ex)
    {
        return Results.BadRequest(new { error = ex.Message });
    }
    catch (Exception ex)
    {
        return Results.Problem(ex.Message);
    }
});

// Remove share
api.MapDelete("/shares/{shareId}", async (string shareId, SyncService syncService) =>
{
    try
    {
        await syncService.RemoveShareAsync(shareId);
        return Results.NoContent();
    }
    catch (Exception ex)
    {
        return Results.Problem(ex.Message);
    }
});

// Health check
api.MapGet("/health", () => Results.Ok(new { status = "healthy", version = "1.0.0" }));

Console.WriteLine("S.E.E.D. Daemon starting on http://127.0.0.1:9876");
app.Run();
