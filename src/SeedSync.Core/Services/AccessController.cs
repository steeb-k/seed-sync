using SeedSync.Core.Models;

namespace SeedSync.Core.Services;

/// <summary>
/// Controls access permissions for shares based on the key type used to join.
/// RO keys only allow downloading; RW keys allow full read/write access.
/// </summary>
public sealed class AccessController
{
    private readonly Dictionary<string, AccessLevel> _shareAccessLevels = new();

    /// <summary>
    /// Registers access level for a share.
    /// </summary>
    /// <param name="shareId">The share ID.</param>
    /// <param name="accessLevel">The access level granted by the key used.</param>
    public void RegisterAccess(string shareId, AccessLevel accessLevel)
    {
        _shareAccessLevels[shareId] = accessLevel;
    }

    /// <summary>
    /// Unregisters access for a share.
    /// </summary>
    /// <param name="shareId">The share ID.</param>
    public void UnregisterAccess(string shareId)
    {
        _shareAccessLevels.Remove(shareId);
    }

    /// <summary>
    /// Gets the access level for a share.
    /// </summary>
    /// <param name="shareId">The share ID.</param>
    /// <returns>The access level, or null if not registered.</returns>
    public AccessLevel? GetAccessLevel(string shareId)
    {
        return _shareAccessLevels.TryGetValue(shareId, out var level) ? level : null;
    }

    /// <summary>
    /// Checks if uploading changes is allowed for a share.
    /// </summary>
    /// <param name="shareId">The share ID.</param>
    /// <returns>True if upload is allowed (RW access), false otherwise.</returns>
    public bool CanUpload(string shareId)
    {
        return _shareAccessLevels.TryGetValue(shareId, out var level) && level == AccessLevel.ReadWrite;
    }

    /// <summary>
    /// Checks if downloading is allowed for a share.
    /// </summary>
    /// <param name="shareId">The share ID.</param>
    /// <returns>True if download is allowed (any access), false if not registered.</returns>
    public bool CanDownload(string shareId)
    {
        return _shareAccessLevels.ContainsKey(shareId);
    }

    /// <summary>
    /// Checks if a specific file operation is allowed.
    /// </summary>
    /// <param name="shareId">The share ID.</param>
    /// <param name="operation">The file operation to check.</param>
    /// <returns>True if the operation is allowed.</returns>
    public bool IsOperationAllowed(string shareId, FileOperation operation)
    {
        if (!_shareAccessLevels.TryGetValue(shareId, out var level))
            return false;

        return operation switch
        {
            FileOperation.Read => true, // Both RO and RW can read
            FileOperation.Write => level == AccessLevel.ReadWrite,
            FileOperation.Delete => level == AccessLevel.ReadWrite,
            FileOperation.Create => level == AccessLevel.ReadWrite,
            _ => false
        };
    }
}

/// <summary>
/// Types of file operations that can be performed on a share.
/// </summary>
public enum FileOperation
{
    /// <summary>
    /// Reading/downloading a file.
    /// </summary>
    Read,

    /// <summary>
    /// Writing/modifying a file.
    /// </summary>
    Write,

    /// <summary>
    /// Deleting a file.
    /// </summary>
    Delete,

    /// <summary>
    /// Creating a new file.
    /// </summary>
    Create
}
