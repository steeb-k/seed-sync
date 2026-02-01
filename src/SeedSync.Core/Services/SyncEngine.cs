using MonoTorrent;
using MonoTorrent.Client;
using MonoTorrent.BEncoding;
using SeedSync.Core.Models;

namespace SeedSync.Core.Services;

/// <summary>
/// The core sync engine that manages BitTorrent-based folder synchronization.
/// </summary>
public sealed class SyncEngine : IAsyncDisposable
{
    private readonly ClientEngine _engine;
    private readonly Dictionary<string, TorrentManager> _activeShares = new();
    private readonly Dictionary<string, FileSystemWatcher> _watchers = new();
    private readonly Dictionary<string, Share> _shareConfigs = new();
    private readonly Dictionary<string, CancellationTokenSource> _debounceTokens = new();
    private readonly string _dataPath;
    private readonly TimeSpan _debounceDelay = TimeSpan.FromSeconds(2);
    private bool _isRunning;

    /// <summary>Default BitTorrent listen port so peers can connect; use same port on both sides when possible.</summary>
    public const int DefaultListenPort = 6881;

    /// <summary>
    /// Creates a new sync engine instance.
    /// </summary>
    /// <param name="dataPath">Path where engine data (torrents, state) is stored.</param>
    /// <param name="listenPort">Port to listen on for incoming connections (0 for random, default 6881).</param>
    public SyncEngine(string dataPath, int listenPort = DefaultListenPort)
    {
        _dataPath = dataPath;
        Directory.CreateDirectory(dataPath);

        var settingsBuilder = new EngineSettingsBuilder
        {
            AllowLocalPeerDiscovery = true,
            AllowPortForwarding = true,
            AutoSaveLoadDhtCache = true,
            AutoSaveLoadFastResume = true,
            AutoSaveLoadMagnetLinkMetadata = true,
            CacheDirectory = Path.Combine(dataPath, "cache"),
            MaximumConnections = 100,
            ListenPort = listenPort == 0 ? 6881 : listenPort
        };

        _engine = new ClientEngine(settingsBuilder.ToSettings());
    }

    /// <summary>
    /// Starts the sync engine.
    /// </summary>
    public async Task StartAsync()
    {
        if (_isRunning) return;

        // DHT and local peer discovery are enabled via settings
        // The engine starts when we add and start torrents
        _isRunning = true;
        await Task.CompletedTask;
    }

    /// <summary>
    /// Stops the sync engine.
    /// </summary>
    public async Task StopAsync()
    {
        if (!_isRunning) return;

        // Stop all file watchers
        foreach (var shareId in _watchers.Keys.ToList())
        {
            StopFileWatcher(shareId);
        }

        foreach (var manager in _activeShares.Values)
        {
            try
            {
                await manager.StopAsync();
            }
            catch (OperationCanceledException)
            {
                // Ignore cancellation during shutdown
            }
            catch
            {
                // Ignore errors during shutdown
            }
        }

        _isRunning = false;
    }

    /// <summary>
    /// Creates a new share for a folder and returns the generated keys.
    /// </summary>
    /// <param name="folderPath">Path to the folder to share.</param>
    /// <param name="defaultPath">Default path suggested for clients.</param>
    /// <param name="ignorePatterns">Patterns for files to ignore.</param>
    /// <returns>The share configuration with keys.</returns>
    public async Task<(Share Share, ShareKeys Keys)> CreateShareAsync(
        string folderPath,
        string? defaultPath = null,
        IEnumerable<string>? ignorePatterns = null)
    {
        if (!Directory.Exists(folderPath))
            throw new DirectoryNotFoundException($"Folder not found: {folderPath}");

        var keys = KeyGenerator.GenerateKeys();
        var ignoreList = ignorePatterns?.ToList() ?? [];

        var share = new Share
        {
            Id = keys.ShareId,
            LocalPath = Path.GetFullPath(folderPath),
            Key = keys.ReadWriteKey,
            AccessLevel = AccessLevel.ReadWrite,
            DefaultPath = defaultPath ?? GetDefaultPathSuggestion(folderPath),
            IgnorePatterns = ignoreList,
            Name = Path.GetFileName(folderPath)
        };

        await StartSyncingShareAsync(share, keys);

        // Embed actual torrent info hash in RO key so joiners use the same swarm
        if (_activeShares.TryGetValue(share.Id, out var manager) && manager.Torrent != null)
        {
            var infoHashBytes = manager.Torrent.InfoHash.ToArray();
            keys = new ShareKeys
            {
                ShareId = keys.ShareId,
                ReadWriteKey = keys.ReadWriteKey,
                ReadOnlyKey = KeyGenerator.WithInfoHash(keys.ReadOnlyKey, infoHashBytes)
            };
        }

        return (share, keys);
    }

    /// <summary>
    /// Adds an existing share using a key.
    /// </summary>
    /// <param name="key">The RW or RO key for the share.</param>
    /// <param name="localPath">Local path to sync to.</param>
    /// <returns>The share configuration.</returns>
    public async Task<Share> AddShareAsync(string key, string localPath)
    {
        var parsed = KeyGenerator.ParseKey(key)
            ?? throw new ArgumentException("Invalid key format", nameof(key));

        Directory.CreateDirectory(localPath);

        var share = new Share
        {
            Id = parsed.ShareId,
            LocalPath = Path.GetFullPath(localPath),
            Key = key,
            AccessLevel = parsed.AccessLevel,
            Name = Path.GetFileName(localPath)
        };

        await StartSyncingShareAsync(share, null);

        return share;
    }

    /// <summary>
    /// Removes a share from syncing.
    /// </summary>
    /// <param name="shareId">The share ID to remove.</param>
    public async Task RemoveShareAsync(string shareId)
    {
        // Stop file watcher first
        StopFileWatcher(shareId);
        _shareConfigs.Remove(shareId);

        if (_activeShares.TryGetValue(shareId, out var manager))
        {
            // Stop the manager and wait for it to complete
            try
            {
                await manager.StopAsync();
            }
            catch
            {
                // Ignore stop errors - we're removing anyway
            }

            // Wait for manager to fully stop (MonoTorrent requires this before removal)
            // Check for both Stopped and Stopping states as the manager may get stuck
            var timeout = DateTime.UtcNow.AddSeconds(5);
            while (manager.State != TorrentState.Stopped &&
                   manager.State != TorrentState.Error &&
                   DateTime.UtcNow < timeout)
            {
                await Task.Delay(100);
            }

            try
            {
                await _engine.RemoveAsync(manager, RemoveMode.CacheDataOnly);
            }
            catch
            {
                // If normal removal fails, try to unregister the manager anyway
                _activeShares.Remove(shareId);
                return;
            }

            _activeShares.Remove(shareId);
        }
    }

    /// <summary>
    /// Gets the status of a share.
    /// </summary>
    /// <param name="shareId">The share ID.</param>
    /// <returns>Status information or null if not found.</returns>
    public ShareStatusInfo? GetShareStatus(string shareId)
    {
        if (!_activeShares.TryGetValue(shareId, out var manager))
            return null;

        return new ShareStatusInfo
        {
            ShareId = shareId,
            State = manager.State,
            Progress = manager.Progress,
            DownloadSpeed = manager.Monitor.DownloadSpeed,
            UploadSpeed = manager.Monitor.UploadSpeed,
            ConnectedPeers = manager.Peers.Available
        };
    }

    /// <summary>
    /// Lists all active shares.
    /// </summary>
    public IReadOnlyCollection<string> GetActiveShareIds() => _activeShares.Keys.ToList();

    /// <summary>
    /// Event raised when a share's files have changed and need re-syncing.
    /// </summary>
    public event EventHandler<ShareChangedEventArgs>? ShareChanged;

    private void SetupFileWatcher(Share share)
    {
        // Only watch for changes on ReadWrite shares
        if (share.AccessLevel != AccessLevel.ReadWrite)
            return;

        if (_watchers.ContainsKey(share.Id))
            return;

        var watcher = new FileSystemWatcher(share.LocalPath)
        {
            NotifyFilter = NotifyFilters.FileName | NotifyFilters.DirectoryName |
                          NotifyFilters.LastWrite | NotifyFilters.Size,
            IncludeSubdirectories = true,
            EnableRaisingEvents = true
        };

        watcher.Created += (s, e) => OnFileChanged(share.Id, e.FullPath);
        watcher.Changed += (s, e) => OnFileChanged(share.Id, e.FullPath);
        watcher.Deleted += (s, e) => OnFileChanged(share.Id, e.FullPath);
        watcher.Renamed += (s, e) => OnFileChanged(share.Id, e.FullPath);

        _watchers[share.Id] = watcher;
    }

    private void OnFileChanged(string shareId, string filePath)
    {
        if (!_shareConfigs.TryGetValue(shareId, out var share))
            return;

        // Ignore changes to metadata files
        var relativePath = Path.GetRelativePath(share.LocalPath, filePath);
        if (relativePath.StartsWith(".") || 
            relativePath.EndsWith(".torrent", StringComparison.OrdinalIgnoreCase) ||
            ShouldIgnore(relativePath, share.IgnorePatterns))
            return;

        // Debounce: cancel any pending resync and schedule a new one
        if (_debounceTokens.TryGetValue(shareId, out var existingCts))
        {
            existingCts.Cancel();
            existingCts.Dispose();
        }

        var cts = new CancellationTokenSource();
        _debounceTokens[shareId] = cts;

        _ = Task.Run(async () =>
        {
            try
            {
                await Task.Delay(_debounceDelay, cts.Token);
                await ResyncShareAsync(shareId);
            }
            catch (OperationCanceledException)
            {
                // Debounce cancelled, new change incoming
            }
            catch (Exception ex)
            {
                Console.WriteLine($"Error resyncing share {shareId}: {ex.Message}");
            }
        });
    }

    private async Task ResyncShareAsync(string shareId)
    {
        if (!_shareConfigs.TryGetValue(shareId, out var share))
            return;

        if (!_activeShares.TryGetValue(shareId, out var oldManager))
            return;

        // Stop the old manager
        try
        {
            await oldManager.StopAsync();
            await _engine.RemoveAsync(oldManager, RemoveMode.CacheDataOnly);
        }
        catch
        {
            // Ignore errors during removal
        }

        _activeShares.Remove(shareId);

        // Delete old torrent file to force recreation
        var torrentPath = Path.Combine(_dataPath, $"{shareId}.torrent");
        if (File.Exists(torrentPath))
        {
            try { File.Delete(torrentPath); } catch { }
        }

        // Recreate the torrent with current files
        await StartSyncingShareAsync(share, null);

        // Raise event
        ShareChanged?.Invoke(this, new ShareChangedEventArgs(shareId, ShareChangeType.FilesUpdated));
    }

    private void StopFileWatcher(string shareId)
    {
        if (_watchers.TryGetValue(shareId, out var watcher))
        {
            watcher.EnableRaisingEvents = false;
            watcher.Dispose();
            _watchers.Remove(shareId);
        }

        if (_debounceTokens.TryGetValue(shareId, out var cts))
        {
            cts.Cancel();
            cts.Dispose();
            _debounceTokens.Remove(shareId);
        }
    }

    private async Task StartSyncingShareAsync(Share share, ShareKeys? keys)
    {
        // Joiner: use info hash from key if present (new RO key format), else derived. Creator with empty folder: use derived.
        byte[] magnetInfoHash;
        if (keys == null)
        {
            var parsed = KeyGenerator.ParseKey(share.Key);
            magnetInfoHash = parsed?.InfoHash ?? KeyGenerator.DeriveInfoHash(share.Id);
        }
        else
        {
            magnetInfoHash = KeyGenerator.DeriveInfoHash(share.Id);
        }
        var infoHashHex = Convert.ToHexString(magnetInfoHash).ToLowerInvariant();
        var trParams = string.Join("", GetDefaultTrackers().Select(u => $"&tr={Uri.EscapeDataString(u)}"));
        var magnetUri = $"magnet:?xt=urn:btih:{infoHashHex}{trParams}";

        // Create or use existing torrent for the folder
        var torrentPath = Path.Combine(_dataPath, $"{share.Id}.torrent");
        TorrentManager manager;
        bool needsHashCheck = false;

        // Use CreateContainingDirectory=false so files go directly in the user's specified path,
        // not in a subdirectory named after the original folder. This allows users on different
        // machines to use different folder names/locations.
        var torrentSettings = new TorrentSettingsBuilder { CreateContainingDirectory = false }.ToSettings();

        if (File.Exists(torrentPath))
        {
            // Load existing torrent (we likely have the files already)
            var torrent = await Torrent.LoadAsync(torrentPath);
            manager = await _engine.AddAsync(torrent, share.LocalPath, torrentSettings);
            needsHashCheck = true; // Verify local files
        }
        else
        {
            // Create new torrent from folder
            var files = GetFilesToSync(share.LocalPath, share.IgnorePatterns);

            if (files.Count > 0)
            {
                var creator = new TorrentCreator();
                creator.Private = false;

                // Build file mappings - use TorrentFileSource with the folder
                // This creates a multi-file torrent where file paths are relative to the folder
                var fileSource = new TorrentFileSource(share.LocalPath);

                var torrentInfo = await creator.CreateAsync(fileSource);

                // Save the torrent
                await File.WriteAllBytesAsync(torrentPath, torrentInfo.Encode());

                var torrent = await Torrent.LoadAsync(torrentPath);
                manager = await _engine.AddAsync(torrent, share.LocalPath, torrentSettings);
                needsHashCheck = true; // We have the files, need to verify
            }
            else
            {
                // Empty folder - use magnet link to join swarm (joiner path)
                var magnet = MagnetLink.Parse(magnetUri);
                manager = await _engine.AddAsync(magnet, share.LocalPath, torrentSettings);
                needsHashCheck = false; // No local files to verify
            }
        }

        // Hook up event handlers for download notifications
        manager.TorrentStateChanged += (sender, e) => OnTorrentStateChanged(share.Id, e);
        manager.PieceHashed += (sender, e) => OnPieceHashed(share.Id, e);

        // Add public trackers for peer discovery (DHT/LPD alone often not enough across NAT)
        if (manager.Torrent == null || !manager.Torrent.IsPrivate)
        {
            var trackerManager = manager.TrackerManager;
            var urls = GetDefaultTrackers();
            _ = Task.Run(async () =>
            {
                foreach (var trackerUrl in urls)
                {
                    try
                    {
                        await trackerManager.AddTrackerAsync(new Uri(trackerUrl));
                    }
                    catch
                    {
                        // Ignore invalid or unreachable trackers
                    }
                }
            });
        }

        // For creator/existing torrent: hash check first to verify local files, then auto-start (→ Seeding)
        // For joiner (magnet): just start (→ Downloading/Metadata)
        if (needsHashCheck)
        {
            Console.WriteLine($"[SEED] Starting hash check for share {share.Id}...");
            await manager.HashCheckAsync(autoStart: true);
            Console.WriteLine($"[SEED] Hash check complete. State: {manager.State}, Progress: {manager.Progress:P1}");
        }
        else
        {
            await manager.StartAsync();
            Console.WriteLine($"[SEED] Started magnet download. State: {manager.State}");
        }
        _activeShares[share.Id] = manager;
        _shareConfigs[share.Id] = share;
        share.Status = ShareStatus.Syncing;

        // Start watching for file changes (ReadWrite shares only)
        SetupFileWatcher(share);
    }

    private void OnTorrentStateChanged(string shareId, TorrentStateChangedEventArgs e)
    {
        Console.WriteLine($"[SEED] Share {shareId}: State changed {e.OldState} -> {e.NewState}");
        
        if (!_shareConfigs.TryGetValue(shareId, out var share))
            return;

        switch (e.NewState)
        {
            case TorrentState.Seeding:
                // Download complete, now seeding
                share.Status = ShareStatus.UpToDate;
                ShareChanged?.Invoke(this, new ShareChangedEventArgs(shareId, ShareChangeType.SyncCompleted));
                break;
            case TorrentState.Downloading:
                share.Status = ShareStatus.Syncing;
                break;
            case TorrentState.Error:
                share.Status = ShareStatus.Error;
                break;
            case TorrentState.Stopped:
                share.Status = ShareStatus.Idle;
                break;
        }
    }

    private void OnPieceHashed(string shareId, PieceHashedEventArgs e)
    {
        // Log piece hash results for debugging
        if (!e.HashPassed)
        {
            Console.WriteLine($"[SEED] Share {shareId}: Piece {e.PieceIndex} FAILED hash check");
        }
        
        // A piece was verified - could be from download or local hash check
        // Only notify if the piece passed verification and was actually downloaded
        if (e.HashPassed && _shareConfigs.TryGetValue(shareId, out var share))
        {
            // Check if we're downloading (not just initial hash check)
            if (_activeShares.TryGetValue(shareId, out var manager) &&
                manager.State == TorrentState.Downloading)
            {
                // New data was downloaded - raise event
                ShareChanged?.Invoke(this, new ShareChangedEventArgs(shareId, ShareChangeType.FilesDownloaded));
            }
        }
    }

    /// <summary>Public UDP trackers used for peer discovery when DHT/LPD alone are not enough.</summary>
    private static List<string> GetDefaultTrackers()
    {
        return
        [
            "udp://tracker.opentrackr.org:1337/announce",
            "udp://open.stealth.si:80/announce",
            "udp://tracker.torrent.eu.org:451/announce",
            "udp://exodus.desync.com:6969/announce"
        ];
    }

    private static List<string> GetFilesToSync(string folderPath, IEnumerable<string> ignorePatterns)
    {
        var files = new List<string>();
        var patterns = ignorePatterns.ToList();

        foreach (var file in Directory.EnumerateFiles(folderPath, "*", SearchOption.AllDirectories))
        {
            var relativePath = Path.GetRelativePath(folderPath, file);

            // Check against ignore patterns
            if (!ShouldIgnore(relativePath, patterns))
            {
                files.Add(file);
            }
        }

        return files;
    }

    private static bool ShouldIgnore(string relativePath, IEnumerable<string> patterns)
    {
        foreach (var pattern in patterns)
        {
            // Simple glob matching - can be enhanced with proper glob library
            if (MatchesPattern(relativePath, pattern))
                return true;
        }
        return false;
    }

    private static bool MatchesPattern(string path, string pattern)
    {
        // Simple pattern matching for common cases
        // Supports: *.ext, folder/*, .gitignore-style patterns

        var normalizedPath = path.Replace('\\', '/');
        var normalizedPattern = pattern.Replace('\\', '/');

        // Exact match
        if (normalizedPath.Equals(normalizedPattern, StringComparison.OrdinalIgnoreCase))
            return true;

        // Directory prefix match (pattern ends with /)
        if (normalizedPattern.EndsWith('/'))
        {
            var dirPattern = normalizedPattern.TrimEnd('/');
            if (normalizedPath.StartsWith(dirPattern + "/", StringComparison.OrdinalIgnoreCase))
                return true;
        }

        // Simple wildcard at start (*.ext)
        if (normalizedPattern.StartsWith("*."))
        {
            var ext = normalizedPattern[1..];
            if (normalizedPath.EndsWith(ext, StringComparison.OrdinalIgnoreCase))
                return true;
        }

        // Directory anywhere in path
        if (!normalizedPattern.Contains('/'))
        {
            var parts = normalizedPath.Split('/');
            if (parts.Any(p => p.Equals(normalizedPattern, StringComparison.OrdinalIgnoreCase)))
                return true;
        }

        return false;
    }

    private static string GetDefaultPathSuggestion(string folderPath)
    {
        var folderName = Path.GetFileName(folderPath);
        var documentsPath = Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments);
        return Path.Combine(documentsPath, "S.E.E.D", folderName);
    }

    /// <inheritdoc />
    public async ValueTask DisposeAsync()
    {
        await StopAsync();
        _engine.Dispose();
    }
}

/// <summary>
/// Status information for a share.
/// </summary>
public sealed class ShareStatusInfo
{
    public required string ShareId { get; init; }
    public TorrentState State { get; init; }
    public double Progress { get; init; }
    public long DownloadSpeed { get; init; }
    public long UploadSpeed { get; init; }
    public int ConnectedPeers { get; init; }
}

/// <summary>
/// Event arguments for share change events.
/// </summary>
public sealed class ShareChangedEventArgs : EventArgs
{
    public string ShareId { get; }
    public ShareChangeType ChangeType { get; }

    public ShareChangedEventArgs(string shareId, ShareChangeType changeType)
    {
        ShareId = shareId;
        ChangeType = changeType;
    }
}

/// <summary>
/// Type of change that occurred to a share.
/// </summary>
public enum ShareChangeType
{
    /// <summary>Files in the share were updated locally.</summary>
    FilesUpdated,
    /// <summary>New files were downloaded from peers.</summary>
    FilesDownloaded,
    /// <summary>Share sync completed.</summary>
    SyncCompleted
}
