namespace SeedSync.Core.Models;

/// <summary>
/// Represents a folder share configuration.
/// </summary>
public sealed class Share
{
    /// <summary>
    /// Unique identifier for this share.
    /// </summary>
    public required string Id { get; init; }

    /// <summary>
    /// The local path to the folder being shared.
    /// </summary>
    public required string LocalPath { get; set; }

    /// <summary>
    /// The key used to join this share (either RW or RO key).
    /// </summary>
    public required string Key { get; init; }

    /// <summary>
    /// The access level this peer has for the share.
    /// </summary>
    public required AccessLevel AccessLevel { get; init; }

    /// <summary>
    /// The default directory path suggested for new clients joining the share.
    /// Can be overridden by the user when adding a share.
    /// </summary>
    public string? DefaultPath { get; set; }

    /// <summary>
    /// List of ignore patterns (glob-style) for files that should not be synced.
    /// </summary>
    public List<string> IgnorePatterns { get; set; } = [];

    /// <summary>
    /// Display name for the share (optional, defaults to folder name).
    /// </summary>
    public string? Name { get; set; }

    /// <summary>
    /// Current sync status of the share.
    /// </summary>
    public ShareStatus Status { get; set; } = ShareStatus.Idle;

    /// <summary>
    /// Timestamp of when this share was created or added.
    /// </summary>
    public DateTime CreatedAt { get; init; } = DateTime.UtcNow;

    /// <summary>
    /// True if this peer is the origin creator of the share (created the torrent from local files).
    /// False if this peer joined an existing share using a key.
    /// </summary>
    public bool IsCreator { get; init; } = false;
}

/// <summary>
/// Current status of a share's synchronization.
/// </summary>
public enum ShareStatus
{
    /// <summary>
    /// Share is not actively syncing.
    /// </summary>
    Idle,

    /// <summary>
    /// Share is currently syncing files.
    /// </summary>
    Syncing,

    /// <summary>
    /// Share is up to date with all peers.
    /// </summary>
    UpToDate,

    /// <summary>
    /// Share encountered an error.
    /// </summary>
    Error
}
