using SeedSync.Daemon.Services;

namespace SeedSync.Daemon;

/// <summary>
/// Background worker that manages the sync service lifecycle.
/// </summary>
public sealed class SyncWorker : BackgroundService
{
    private readonly SyncService _syncService;
    private readonly ILogger<SyncWorker> _logger;

    public SyncWorker(SyncService syncService, ILogger<SyncWorker> logger)
    {
        _syncService = syncService;
        _logger = logger;
    }

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        _logger.LogInformation("S.E.E.D. Daemon starting...");

        try
        {
            await _syncService.StartAsync();
            _logger.LogInformation("S.E.E.D. Daemon is running");

            // Keep running until stopped
            await Task.Delay(Timeout.Infinite, stoppingToken);
        }
        catch (OperationCanceledException)
        {
            // Expected on shutdown
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Error in sync service");
        }
        finally
        {
            await _syncService.StopAsync();
            _logger.LogInformation("S.E.E.D. Daemon stopped");
        }
    }
}
