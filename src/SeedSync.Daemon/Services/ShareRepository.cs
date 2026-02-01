using System.Text.Json;
using SeedSync.Core.Models;

namespace SeedSync.Daemon.Services;

/// <summary>
/// Persists share configurations to disk.
/// </summary>
public sealed class ShareRepository
{
    private readonly string _configPath;
    private readonly object _lock = new();

    public ShareRepository(string appDataPath)
    {
        _configPath = Path.Combine(appDataPath, "shares.json");
        Directory.CreateDirectory(appDataPath);
    }

    /// <summary>
    /// Loads all saved shares.
    /// </summary>
    public List<ShareConfig> LoadShares()
    {
        lock (_lock)
        {
            if (!File.Exists(_configPath))
                return [];

            try
            {
                var json = File.ReadAllText(_configPath);
                return JsonSerializer.Deserialize<List<ShareConfig>>(json) ?? [];
            }
            catch
            {
                return [];
            }
        }
    }

    /// <summary>
    /// Saves a share configuration.
    /// </summary>
    public void SaveShare(ShareConfig config)
    {
        lock (_lock)
        {
            var shares = LoadShares();
            var existing = shares.FindIndex(s => s.Id == config.Id);

            if (existing >= 0)
                shares[existing] = config;
            else
                shares.Add(config);

            SaveAll(shares);
        }
    }

    /// <summary>
    /// Removes a share configuration.
    /// </summary>
    public void RemoveShare(string shareId)
    {
        lock (_lock)
        {
            var shares = LoadShares();
            shares.RemoveAll(s => s.Id == shareId);
            SaveAll(shares);
        }
    }

    /// <summary>
    /// Gets a specific share configuration.
    /// </summary>
    public ShareConfig? GetShare(string shareId)
    {
        return LoadShares().FirstOrDefault(s => s.Id == shareId);
    }

    private void SaveAll(List<ShareConfig> shares)
    {
        var json = JsonSerializer.Serialize(shares, new JsonSerializerOptions
        {
            WriteIndented = true
        });
        File.WriteAllText(_configPath, json);
    }
}

/// <summary>
/// Persistable share configuration.
/// </summary>
public sealed class ShareConfig
{
    public required string Id { get; init; }
    public required string LocalPath { get; set; }
    public required string Key { get; init; }
    public required AccessLevel AccessLevel { get; init; }
    public string? DefaultPath { get; set; }
    public List<string> IgnorePatterns { get; set; } = [];
    public string? Name { get; set; }
    public DateTime CreatedAt { get; init; } = DateTime.UtcNow;

    /// <summary>
    /// Stores the RW key if this is the creator of the share.
    /// </summary>
    public string? ReadWriteKey { get; set; }

    /// <summary>
    /// Stores the RO key for sharing with others.
    /// </summary>
    public string? ReadOnlyKey { get; set; }
}
