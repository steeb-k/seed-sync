using SeedSync.Core.Models;
using SeedSync.Core.Services;

namespace SeedSync.Daemon.Services;

/// <summary>
/// Main service that manages the sync engine lifecycle.
/// </summary>
public sealed class SyncService : IAsyncDisposable
{
    private readonly SyncEngine _engine;
    private readonly ShareRepository _repository;
    private readonly AccessController _accessController;
    private readonly ILogger<SyncService> _logger;
    private readonly Dictionary<string, Share> _activeShares = new();

    public SyncService(
        ShareRepository repository,
        AccessController accessController,
        ILogger<SyncService> logger,
        string dataPath)
    {
        _repository = repository;
        _accessController = accessController;
        _logger = logger;
        _engine = new SyncEngine(Path.Combine(dataPath, "engine"));
    }

    /// <summary>
    /// Starts the sync service and loads saved shares.
    /// </summary>
    public async Task StartAsync()
    {
        _logger.LogInformation("Starting sync service...");
        await _engine.StartAsync();

        // Load and start all saved shares
        var shares = _repository.LoadShares();
        foreach (var config in shares)
        {
            try
            {
                await RestoreShareAsync(config);
                _logger.LogInformation("Restored share {ShareId}: {Path}", config.Id, config.LocalPath);
            }
            catch (Exception ex)
            {
                _logger.LogError(ex, "Failed to restore share {ShareId}", config.Id);
            }
        }

        _logger.LogInformation("Sync service started with {Count} shares", _activeShares.Count);
    }

    /// <summary>
    /// Stops the sync service.
    /// </summary>
    public async Task StopAsync()
    {
        _logger.LogInformation("Stopping sync service...");
        await _engine.StopAsync();
        _activeShares.Clear();
    }

    /// <summary>
    /// Creates a new share.
    /// </summary>
    public async Task<CreateShareResult> CreateShareAsync(CreateShareRequest request)
    {
        if (!Directory.Exists(request.FolderPath))
            throw new DirectoryNotFoundException($"Folder not found: {request.FolderPath}");

        var (share, keys) = await _engine.CreateShareAsync(
            request.FolderPath,
            request.DefaultPath,
            request.IgnorePatterns);

        // Save configuration
        var config = new ShareConfig
        {
            Id = share.Id,
            LocalPath = share.LocalPath,
            Key = keys.ReadWriteKey,
            AccessLevel = AccessLevel.ReadWrite,
            DefaultPath = share.DefaultPath,
            IgnorePatterns = share.IgnorePatterns,
            Name = request.Name ?? share.Name,
            ReadWriteKey = keys.ReadWriteKey,
            ReadOnlyKey = keys.ReadOnlyKey
        };
        _repository.SaveShare(config);

        _activeShares[share.Id] = share;
        _accessController.RegisterAccess(share.Id, AccessLevel.ReadWrite);

        _logger.LogInformation("Created new share {ShareId}: {Path}", share.Id, share.LocalPath);

        return new CreateShareResult
        {
            ShareId = share.Id,
            ReadWriteKey = keys.ReadWriteKey,
            ReadOnlyKey = keys.ReadOnlyKey
        };
    }

    /// <summary>
    /// Adds an existing share by key.
    /// </summary>
    public async Task<AddShareResult> AddShareAsync(AddShareRequest request)
    {
        var parsed = KeyGenerator.ParseKey(request.Key)
            ?? throw new ArgumentException("Invalid key format");

        var share = await _engine.AddShareAsync(request.Key, request.LocalPath);

        // Save configuration
        var config = new ShareConfig
        {
            Id = share.Id,
            LocalPath = share.LocalPath,
            Key = request.Key,
            AccessLevel = parsed.AccessLevel,
            Name = request.Name ?? share.Name
        };
        _repository.SaveShare(config);

        _activeShares[share.Id] = share;
        _accessController.RegisterAccess(share.Id, parsed.AccessLevel);

        _logger.LogInformation("Added share {ShareId} with {AccessLevel} access: {Path}",
            share.Id, parsed.AccessLevel, share.LocalPath);

        return new AddShareResult
        {
            ShareId = share.Id,
            AccessLevel = parsed.AccessLevel,
            IsReadWrite = parsed.AccessLevel == AccessLevel.ReadWrite
        };
    }

    /// <summary>
    /// Removes a share.
    /// </summary>
    public async Task RemoveShareAsync(string shareId)
    {
        await _engine.RemoveShareAsync(shareId);
        _repository.RemoveShare(shareId);
        _activeShares.Remove(shareId);
        _accessController.UnregisterAccess(shareId);

        _logger.LogInformation("Removed share {ShareId}", shareId);
    }

    /// <summary>
    /// Lists all shares.
    /// </summary>
    public List<ShareInfo> ListShares()
    {
        var configs = _repository.LoadShares();
        return configs.Select(c =>
        {
            var status = _engine.GetShareStatus(c.Id);
            return new ShareInfo
            {
                Id = c.Id,
                Name = c.Name ?? Path.GetFileName(c.LocalPath),
                LocalPath = c.LocalPath,
                AccessLevel = c.AccessLevel,
                Status = status?.State.ToString() ?? "Unknown",
                Progress = status?.Progress ?? 0,
                ConnectedPeers = status?.ConnectedPeers ?? 0,
                ReadWriteKey = c.ReadWriteKey,
                ReadOnlyKey = c.ReadOnlyKey
            };
        }).ToList();
    }

    /// <summary>
    /// Gets status of a specific share.
    /// </summary>
    public ShareInfo? GetShareStatus(string shareId)
    {
        var config = _repository.GetShare(shareId);
        if (config == null) return null;

        var status = _engine.GetShareStatus(shareId);
        return new ShareInfo
        {
            Id = config.Id,
            Name = config.Name ?? Path.GetFileName(config.LocalPath),
            LocalPath = config.LocalPath,
            AccessLevel = config.AccessLevel,
            Status = status?.State.ToString() ?? "Unknown",
            Progress = status?.Progress ?? 0,
            ConnectedPeers = status?.ConnectedPeers ?? 0,
            ReadWriteKey = config.ReadWriteKey,
            ReadOnlyKey = config.ReadOnlyKey
        };
    }

    private async Task RestoreShareAsync(ShareConfig config)
    {
        var share = await _engine.AddShareAsync(config.Key, config.LocalPath);
        share.IgnorePatterns = config.IgnorePatterns;
        share.Name = config.Name;
        share.DefaultPath = config.DefaultPath;

        _activeShares[share.Id] = share;
        _accessController.RegisterAccess(share.Id, config.AccessLevel);
    }

    public async ValueTask DisposeAsync()
    {
        await _engine.DisposeAsync();
    }
}

// Request/Response DTOs
public sealed class CreateShareRequest
{
    public required string FolderPath { get; init; }
    public string? Name { get; init; }
    public string? DefaultPath { get; init; }
    public List<string>? IgnorePatterns { get; init; }
}

public sealed class CreateShareResult
{
    public required string ShareId { get; init; }
    public required string ReadWriteKey { get; init; }
    public required string ReadOnlyKey { get; init; }
}

public sealed class AddShareRequest
{
    public required string Key { get; init; }
    public required string LocalPath { get; init; }
    public string? Name { get; init; }
}

public sealed class AddShareResult
{
    public required string ShareId { get; init; }
    public required AccessLevel AccessLevel { get; init; }
    public bool IsReadWrite { get; init; }
}

public sealed class ShareInfo
{
    public required string Id { get; init; }
    public required string Name { get; init; }
    public required string LocalPath { get; init; }
    public required AccessLevel AccessLevel { get; init; }
    public required string Status { get; init; }
    public double Progress { get; init; }
    public int ConnectedPeers { get; init; }
    public string? ReadWriteKey { get; init; }
    public string? ReadOnlyKey { get; init; }
}
