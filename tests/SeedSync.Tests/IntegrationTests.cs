using SeedSync.Core.Models;
using SeedSync.Core.Services;

namespace SeedSync.Tests;

/// <summary>
/// Integration tests for end-to-end sync functionality.
/// These tests verify that two SyncEngine instances can communicate and sync files.
/// </summary>
public class IntegrationTests : IAsyncLifetime
{
    private readonly string _testDir;
    private readonly string _engine1DataPath;
    private readonly string _engine2DataPath;
    private readonly string _folder1Path;
    private readonly string _folder2Path;

    public IntegrationTests()
    {
        _testDir = Path.Combine(Path.GetTempPath(), $"SeedSyncIntegration_{Guid.NewGuid():N}");
        _engine1DataPath = Path.Combine(_testDir, "engine1_data");
        _engine2DataPath = Path.Combine(_testDir, "engine2_data");
        _folder1Path = Path.Combine(_testDir, "folder1");
        _folder2Path = Path.Combine(_testDir, "folder2");
    }

    public Task InitializeAsync()
    {
        Directory.CreateDirectory(_testDir);
        Directory.CreateDirectory(_engine1DataPath);
        Directory.CreateDirectory(_engine2DataPath);
        Directory.CreateDirectory(_folder1Path);
        Directory.CreateDirectory(_folder2Path);
        return Task.CompletedTask;
    }

    public Task DisposeAsync()
    {
        try
        {
            if (Directory.Exists(_testDir))
            {
                Directory.Delete(_testDir, recursive: true);
            }
        }
        catch
        {
            // Ignore cleanup errors
        }
        return Task.CompletedTask;
    }

    [Fact]
    public async Task TwoEngines_CanJoinSameShare()
    {
        // Arrange - Engine 1 creates a share
        await using var engine1 = new SyncEngine(_engine1DataPath);
        await engine1.StartAsync();

        await File.WriteAllTextAsync(Path.Combine(_folder1Path, "shared.txt"), "Shared content");
        var (share1, keys) = await engine1.CreateShareAsync(_folder1Path);

        // Act - Engine 2 joins with RO key
        await using var engine2 = new SyncEngine(_engine2DataPath);
        await engine2.StartAsync();

        var share2 = await engine2.AddShareAsync(keys.ReadOnlyKey, _folder2Path);

        // Assert - Both engines are tracking the same share
        Assert.Equal(share1.Id, share2.Id);
        Assert.Equal(AccessLevel.ReadWrite, share1.AccessLevel);
        Assert.Equal(AccessLevel.ReadOnly, share2.AccessLevel);

        // Both should have the share active
        Assert.Contains(share1.Id, engine1.GetActiveShareIds());
        Assert.Contains(share2.Id, engine2.GetActiveShareIds());
    }

    [Fact]
    public async Task TwoEngines_BothWithRwKey_CanJoinSameShare()
    {
        // Arrange - Engine 1 creates a share
        await using var engine1 = new SyncEngine(_engine1DataPath);
        await engine1.StartAsync();

        await File.WriteAllTextAsync(Path.Combine(_folder1Path, "shared.txt"), "Shared content");
        var (share1, keys) = await engine1.CreateShareAsync(_folder1Path);

        // Act - Engine 2 joins with RW key
        await using var engine2 = new SyncEngine(_engine2DataPath);
        await engine2.StartAsync();

        var share2 = await engine2.AddShareAsync(keys.ReadWriteKey, _folder2Path);

        // Assert - Both have ReadWrite access
        Assert.Equal(AccessLevel.ReadWrite, share1.AccessLevel);
        Assert.Equal(AccessLevel.ReadWrite, share2.AccessLevel);
    }

    [Fact]
    public async Task Engine_CreatesValidTorrentFile()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        var testContent = "Test file content for torrent creation";
        await File.WriteAllTextAsync(Path.Combine(_folder1Path, "test.txt"), testContent);

        // Act
        var (share, _) = await engine.CreateShareAsync(_folder1Path);

        // Assert - Torrent file should be created
        var torrentPath = Path.Combine(_engine1DataPath, $"{share.Id}.torrent");
        Assert.True(File.Exists(torrentPath), "Torrent file should be created");

        var torrentBytes = await File.ReadAllBytesAsync(torrentPath);
        Assert.True(torrentBytes.Length > 0, "Torrent file should not be empty");
    }

    [Fact]
    public async Task Engine_LoadsExistingTorrentOnRestart()
    {
        string shareId;
        ShareKeys keys;

        // Create share and stop engine
        {
            await using var engine = new SyncEngine(_engine1DataPath);
            await engine.StartAsync();

            await File.WriteAllTextAsync(Path.Combine(_folder1Path, "test.txt"), "Content");
            var result = await engine.CreateShareAsync(_folder1Path);
            shareId = result.Share.Id;
            keys = result.Keys;
        }

        // Verify torrent file exists
        var torrentPath = Path.Combine(_engine1DataPath, $"{shareId}.torrent");
        Assert.True(File.Exists(torrentPath));

        // Create new engine instance and add the same share
        await using var engine2 = new SyncEngine(_engine1DataPath);
        await engine2.StartAsync();

        // Add the share using the key - should load existing torrent
        var share = await engine2.AddShareAsync(keys.ReadWriteKey, _folder1Path);

        Assert.Equal(shareId, share.Id);
        Assert.NotNull(engine2.GetShareStatus(shareId));
    }

    [Fact]
    public async Task ShareStatus_ShowsCorrectState()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        await File.WriteAllTextAsync(Path.Combine(_folder1Path, "test.txt"), "Content");
        var (share, _) = await engine.CreateShareAsync(_folder1Path);

        // Act
        var status = engine.GetShareStatus(share.Id);

        // Assert
        Assert.NotNull(status);
        Assert.Equal(share.Id, status.ShareId);
        Assert.True(status.Progress >= 0 && status.Progress <= 100);
    }

    [Fact]
    public async Task MultipleShares_AreTrackedIndependently()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        var folder1 = Path.Combine(_testDir, "multiShare1");
        var folder2 = Path.Combine(_testDir, "multiShare2");
        Directory.CreateDirectory(folder1);
        Directory.CreateDirectory(folder2);

        await File.WriteAllTextAsync(Path.Combine(folder1, "file1.txt"), "Content 1");
        await File.WriteAllTextAsync(Path.Combine(folder2, "file2.txt"), "Content 2");

        // Act
        var (share1, keys1) = await engine.CreateShareAsync(folder1);
        var (share2, keys2) = await engine.CreateShareAsync(folder2);

        // Assert
        Assert.NotEqual(share1.Id, share2.Id);
        Assert.NotEqual(keys1.ReadWriteKey, keys2.ReadWriteKey);
        Assert.NotEqual(keys1.ReadOnlyKey, keys2.ReadOnlyKey);

        var activeIds = engine.GetActiveShareIds();
        Assert.Contains(share1.Id, activeIds);
        Assert.Contains(share2.Id, activeIds);
        Assert.Equal(2, activeIds.Count);
    }

    [Fact]
    public async Task RemoveShare_CleansUpResources()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        await File.WriteAllTextAsync(Path.Combine(_folder1Path, "test.txt"), "Content");
        var (share, _) = await engine.CreateShareAsync(_folder1Path);

        Assert.Contains(share.Id, engine.GetActiveShareIds());

        // Act
        await engine.RemoveShareAsync(share.Id);

        // Assert
        Assert.DoesNotContain(share.Id, engine.GetActiveShareIds());
        Assert.Null(engine.GetShareStatus(share.Id));
    }

    [Fact]
    public async Task ShareChanged_Event_RaisedOnFileChange()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        await File.WriteAllTextAsync(Path.Combine(_folder1Path, "initial.txt"), "Initial");
        var (share, _) = await engine.CreateShareAsync(_folder1Path);

        var eventReceived = new TaskCompletionSource<bool>();
        engine.ShareChanged += (s, e) =>
        {
            if (e.ShareId == share.Id && e.ChangeType == ShareChangeType.FilesUpdated)
                eventReceived.TrySetResult(true);
        };

        // Act - modify a file
        await Task.Delay(100);
        await File.WriteAllTextAsync(Path.Combine(_folder1Path, "new.txt"), "New content");

        // Assert - event should be raised (within debounce + buffer time)
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(5));
        cts.Token.Register(() => eventReceived.TrySetResult(false));

        var result = await eventReceived.Task;
        Assert.True(result, "ShareChanged event should be raised on file change");
    }

    [Fact]
    public async Task IgnorePatterns_AreRespected()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        // Create files including ones that should be ignored
        await File.WriteAllTextAsync(Path.Combine(_folder1Path, "keep.txt"), "Keep this");
        await File.WriteAllTextAsync(Path.Combine(_folder1Path, "ignore.log"), "Ignore this");

        var ignorePatterns = new[] { "*.log" };

        // Act
        var (share, _) = await engine.CreateShareAsync(_folder1Path, ignorePatterns: ignorePatterns);

        // Assert
        Assert.Contains("*.log", share.IgnorePatterns);
    }
}
