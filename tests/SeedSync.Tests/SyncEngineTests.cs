using SeedSync.Core.Models;
using SeedSync.Core.Services;

namespace SeedSync.Tests;

public class SyncEngineTests : IAsyncLifetime
{
    private readonly string _testDir;
    private readonly string _engine1DataPath;
    private readonly string _engine2DataPath;
    private readonly string _folder1Path;
    private readonly string _folder2Path;

    public SyncEngineTests()
    {
        _testDir = Path.Combine(Path.GetTempPath(), $"SeedSyncTests_{Guid.NewGuid():N}");
        _engine1DataPath = Path.Combine(_testDir, "engine1");
        _engine2DataPath = Path.Combine(_testDir, "engine2");
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
    public async Task CreateShare_ReturnsValidShareAndKeys()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        var testFile = Path.Combine(_folder1Path, "test.txt");
        await File.WriteAllTextAsync(testFile, "Hello, World!");

        // Act
        var (share, keys) = await engine.CreateShareAsync(_folder1Path);

        // Assert
        Assert.NotNull(share);
        Assert.NotNull(keys);
        Assert.Equal(_folder1Path, share.LocalPath);
        Assert.Equal(AccessLevel.ReadWrite, share.AccessLevel);
        Assert.Equal(keys.ShareId, share.Id);
        Assert.NotEmpty(keys.ReadWriteKey);
        Assert.NotEmpty(keys.ReadOnlyKey);
    }

    [Fact]
    public async Task AddShare_WithValidKey_JoinsShare()
    {
        // Arrange
        await using var engine1 = new SyncEngine(_engine1DataPath);
        await engine1.StartAsync();

        var testFile = Path.Combine(_folder1Path, "test.txt");
        await File.WriteAllTextAsync(testFile, "Hello, World!");

        var (_, keys) = await engine1.CreateShareAsync(_folder1Path);

        await using var engine2 = new SyncEngine(_engine2DataPath);
        await engine2.StartAsync();

        // Act
        var share2 = await engine2.AddShareAsync(keys.ReadOnlyKey, _folder2Path);

        // Assert
        Assert.NotNull(share2);
        Assert.Equal(keys.ShareId, share2.Id);
        Assert.Equal(AccessLevel.ReadOnly, share2.AccessLevel);
        Assert.Equal(_folder2Path, share2.LocalPath);
    }

    [Fact]
    public async Task AddShare_WithRwKey_HasReadWriteAccess()
    {
        // Arrange
        await using var engine1 = new SyncEngine(_engine1DataPath);
        await engine1.StartAsync();

        var testFile = Path.Combine(_folder1Path, "test.txt");
        await File.WriteAllTextAsync(testFile, "Hello, World!");

        var (_, keys) = await engine1.CreateShareAsync(_folder1Path);

        await using var engine2 = new SyncEngine(_engine2DataPath);
        await engine2.StartAsync();

        // Act
        var share2 = await engine2.AddShareAsync(keys.ReadWriteKey, _folder2Path);

        // Assert
        Assert.Equal(AccessLevel.ReadWrite, share2.AccessLevel);
    }

    [Fact]
    public async Task GetShareStatus_ReturnsStatusForActiveShare()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        var testFile = Path.Combine(_folder1Path, "test.txt");
        await File.WriteAllTextAsync(testFile, "Hello, World!");

        var (share, _) = await engine.CreateShareAsync(_folder1Path);

        // Act
        var status = engine.GetShareStatus(share.Id);

        // Assert
        Assert.NotNull(status);
        Assert.Equal(share.Id, status.ShareId);
    }

    [Fact]
    public async Task GetShareStatus_ReturnsNullForUnknownShare()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        // Act
        var status = engine.GetShareStatus("nonexistent");

        // Assert
        Assert.Null(status);
    }

    [Fact]
    public async Task RemoveShare_StopsTrackingShare()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        var testFile = Path.Combine(_folder1Path, "test.txt");
        await File.WriteAllTextAsync(testFile, "Hello, World!");

        var (share, _) = await engine.CreateShareAsync(_folder1Path);
        Assert.NotNull(engine.GetShareStatus(share.Id));

        // Act
        await engine.RemoveShareAsync(share.Id);

        // Assert
        Assert.Null(engine.GetShareStatus(share.Id));
    }

    [Fact]
    public async Task GetActiveShareIds_ReturnsAllActiveShares()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        var folder1 = Path.Combine(_testDir, "shareFolder1");
        var folder2 = Path.Combine(_testDir, "shareFolder2");
        Directory.CreateDirectory(folder1);
        Directory.CreateDirectory(folder2);

        await File.WriteAllTextAsync(Path.Combine(folder1, "test1.txt"), "Test 1");
        await File.WriteAllTextAsync(Path.Combine(folder2, "test2.txt"), "Test 2");

        var (share1, _) = await engine.CreateShareAsync(folder1);
        var (share2, _) = await engine.CreateShareAsync(folder2);

        // Act
        var activeIds = engine.GetActiveShareIds();

        // Assert
        Assert.Contains(share1.Id, activeIds);
        Assert.Contains(share2.Id, activeIds);
        Assert.Equal(2, activeIds.Count);
    }

    [Fact]
    public async Task CreateShare_WithIgnorePatterns_StoresPatterns()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        var testFile = Path.Combine(_folder1Path, "test.txt");
        await File.WriteAllTextAsync(testFile, "Hello, World!");

        var ignorePatterns = new[] { "*.log", ".git", "node_modules" };

        // Act
        var (share, _) = await engine.CreateShareAsync(_folder1Path, ignorePatterns: ignorePatterns);

        // Assert
        Assert.Equal(ignorePatterns, share.IgnorePatterns);
    }

    [Fact]
    public async Task CreateShare_WithDefaultPath_StoresDefaultPath()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        var testFile = Path.Combine(_folder1Path, "test.txt");
        await File.WriteAllTextAsync(testFile, "Hello, World!");

        var defaultPath = @"C:\Users\Shared\MySync";

        // Act
        var (share, _) = await engine.CreateShareAsync(_folder1Path, defaultPath: defaultPath);

        // Assert
        Assert.Equal(defaultPath, share.DefaultPath);
    }

    [Fact]
    public async Task FileWatcher_ReadWriteShare_RaisesShareChangedOnFileCreate()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        var testFile = Path.Combine(_folder1Path, "initial.txt");
        await File.WriteAllTextAsync(testFile, "Initial content");

        var (share, _) = await engine.CreateShareAsync(_folder1Path);

        var eventRaised = new TaskCompletionSource<ShareChangedEventArgs>();
        engine.ShareChanged += (sender, args) =>
        {
            if (args.ChangeType == ShareChangeType.FilesUpdated)
                eventRaised.TrySetResult(args);
        };

        // Act - create a new file
        var newFile = Path.Combine(_folder1Path, "newfile.txt");
        await File.WriteAllTextAsync(newFile, "New file content");

        // Assert - wait for debounced event (2s debounce + buffer)
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(5));
        cts.Token.Register(() => eventRaised.TrySetCanceled());

        var args = await eventRaised.Task;
        Assert.Equal(share.Id, args.ShareId);
        Assert.Equal(ShareChangeType.FilesUpdated, args.ChangeType);
    }

    [Fact]
    public async Task FileWatcher_ReadWriteShare_RaisesShareChangedOnFileModify()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        var testFile = Path.Combine(_folder1Path, "test.txt");
        await File.WriteAllTextAsync(testFile, "Initial content");

        var (share, _) = await engine.CreateShareAsync(_folder1Path);

        var eventRaised = new TaskCompletionSource<ShareChangedEventArgs>();
        engine.ShareChanged += (sender, args) =>
        {
            if (args.ChangeType == ShareChangeType.FilesUpdated)
                eventRaised.TrySetResult(args);
        };

        // Act - modify the existing file
        await Task.Delay(100); // Small delay to ensure watcher is ready
        await File.WriteAllTextAsync(testFile, "Modified content");

        // Assert - wait for debounced event
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(5));
        cts.Token.Register(() => eventRaised.TrySetCanceled());

        var args = await eventRaised.Task;
        Assert.Equal(share.Id, args.ShareId);
        Assert.Equal(ShareChangeType.FilesUpdated, args.ChangeType);
    }

    [Fact]
    public async Task FileWatcher_ReadWriteShare_RaisesShareChangedOnFileDelete()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        var testFile = Path.Combine(_folder1Path, "initial.txt");
        var fileToDelete = Path.Combine(_folder1Path, "deleteme.txt");
        await File.WriteAllTextAsync(testFile, "Keep this");
        await File.WriteAllTextAsync(fileToDelete, "Content to delete");

        var (share, _) = await engine.CreateShareAsync(_folder1Path);

        var eventRaised = new TaskCompletionSource<ShareChangedEventArgs>();
        engine.ShareChanged += (sender, args) =>
        {
            if (args.ChangeType == ShareChangeType.FilesUpdated)
                eventRaised.TrySetResult(args);
        };

        // Act - create then delete a new file (not one from the torrent)
        await Task.Delay(200);
        var tempFile = Path.Combine(_folder1Path, "tempfile.txt");
        await File.WriteAllTextAsync(tempFile, "Temporary");
        await Task.Delay(100);
        File.Delete(tempFile);

        // Assert - wait for debounced event
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(5));
        cts.Token.Register(() => eventRaised.TrySetCanceled());

        var args = await eventRaised.Task;
        Assert.Equal(share.Id, args.ShareId);
        Assert.Equal(ShareChangeType.FilesUpdated, args.ChangeType);
    }

    [Fact]
    public async Task FileWatcher_ReadOnlyShare_DoesNotRaiseShareChanged()
    {
        // Arrange
        await using var engine1 = new SyncEngine(_engine1DataPath);
        await engine1.StartAsync();

        var testFile = Path.Combine(_folder1Path, "test.txt");
        await File.WriteAllTextAsync(testFile, "Initial content");

        var (_, keys) = await engine1.CreateShareAsync(_folder1Path);

        await using var engine2 = new SyncEngine(_engine2DataPath);
        await engine2.StartAsync();

        // Join with ReadOnly key
        var share2 = await engine2.AddShareAsync(keys.ReadOnlyKey, _folder2Path);
        Assert.Equal(AccessLevel.ReadOnly, share2.AccessLevel);

        var eventRaised = false;
        engine2.ShareChanged += (sender, args) =>
        {
            if (args.ChangeType == ShareChangeType.FilesUpdated)
                eventRaised = true;
        };

        // Act - create a file in the ReadOnly share's folder
        var newFile = Path.Combine(_folder2Path, "localfile.txt");
        await File.WriteAllTextAsync(newFile, "Local content");

        // Wait longer than debounce period
        await Task.Delay(TimeSpan.FromSeconds(3));

        // Assert - no event should be raised for ReadOnly share
        Assert.False(eventRaised, "ShareChanged should not fire for ReadOnly shares");
    }

    [Fact]
    public async Task FileWatcher_IgnoresMetadataFiles()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        var testFile = Path.Combine(_folder1Path, "test.txt");
        await File.WriteAllTextAsync(testFile, "Initial content");

        var (share, _) = await engine.CreateShareAsync(_folder1Path);

        var eventCount = 0;
        engine.ShareChanged += (sender, args) =>
        {
            if (args.ChangeType == ShareChangeType.FilesUpdated)
                Interlocked.Increment(ref eventCount);
        };

        // Act - create a dotfile and a .torrent file (should be ignored)
        await Task.Delay(100);
        await File.WriteAllTextAsync(Path.Combine(_folder1Path, ".hidden"), "hidden");
        await File.WriteAllTextAsync(Path.Combine(_folder1Path, "data.torrent"), "torrent");

        // Wait for potential debounce
        await Task.Delay(TimeSpan.FromSeconds(3));

        // Assert - no events should be raised for ignored files
        Assert.Equal(0, eventCount);
    }

    [Fact]
    public async Task FileWatcher_DebouncesCombinesMultipleChanges()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        var testFile = Path.Combine(_folder1Path, "test.txt");
        await File.WriteAllTextAsync(testFile, "Initial");

        var (share, _) = await engine.CreateShareAsync(_folder1Path);

        var eventCount = 0;
        engine.ShareChanged += (sender, args) =>
        {
            if (args.ChangeType == ShareChangeType.FilesUpdated)
                Interlocked.Increment(ref eventCount);
        };

        // Act - make multiple rapid changes (should result in single debounced event)
        await Task.Delay(100);
        for (int i = 0; i < 5; i++)
        {
            await File.WriteAllTextAsync(Path.Combine(_folder1Path, $"file{i}.txt"), $"Content {i}");
            await Task.Delay(100); // Rapid changes within debounce window
        }

        // Wait for debounce to complete
        await Task.Delay(TimeSpan.FromSeconds(4));

        // Assert - only one or two events (debounced)
        Assert.True(eventCount <= 2, $"Expected 1-2 debounced events, got {eventCount}");
        Assert.True(eventCount >= 1, "Expected at least 1 event");
    }

    [Fact]
    public async Task FileWatcher_StopsWhenShareRemoved()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        var testFile = Path.Combine(_folder1Path, "test.txt");
        await File.WriteAllTextAsync(testFile, "Initial");

        var (share, _) = await engine.CreateShareAsync(_folder1Path);

        // Remove the share
        await engine.RemoveShareAsync(share.Id);

        var eventRaised = false;
        engine.ShareChanged += (sender, args) =>
        {
            if (args.ChangeType == ShareChangeType.FilesUpdated)
                eventRaised = true;
        };

        // Act - modify file after share is removed
        await File.WriteAllTextAsync(testFile, "Modified after removal");

        // Wait for potential event
        await Task.Delay(TimeSpan.FromSeconds(3));

        // Assert - no event should fire after share is removed
        Assert.False(eventRaised, "No events should fire after share is removed");
    }

    [Fact]
    public async Task ShareChanged_Event_HasCorrectShareId()
    {
        // Arrange
        await using var engine = new SyncEngine(_engine1DataPath);
        await engine.StartAsync();

        // Create two shares
        var folder1 = Path.Combine(_testDir, "watchFolder1");
        var folder2 = Path.Combine(_testDir, "watchFolder2");
        Directory.CreateDirectory(folder1);
        Directory.CreateDirectory(folder2);

        await File.WriteAllTextAsync(Path.Combine(folder1, "test1.txt"), "Test 1");
        await File.WriteAllTextAsync(Path.Combine(folder2, "test2.txt"), "Test 2");

        var (share1, _) = await engine.CreateShareAsync(folder1);
        var (share2, _) = await engine.CreateShareAsync(folder2);

        var receivedShareId = "";
        var eventRaised = new TaskCompletionSource<string>();
        engine.ShareChanged += (sender, args) =>
        {
            if (args.ChangeType == ShareChangeType.FilesUpdated)
                eventRaised.TrySetResult(args.ShareId);
        };

        // Act - modify file in share2
        await Task.Delay(100);
        await File.WriteAllTextAsync(Path.Combine(folder2, "newfile.txt"), "New content");

        // Assert - event should have share2's ID
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(5));
        cts.Token.Register(() => eventRaised.TrySetCanceled());

        receivedShareId = await eventRaised.Task;
        Assert.Equal(share2.Id, receivedShareId);
    }
}
