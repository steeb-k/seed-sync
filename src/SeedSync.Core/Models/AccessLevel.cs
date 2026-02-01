namespace SeedSync.Core.Models;

/// <summary>
/// Defines the access level for a share connection.
/// </summary>
public enum AccessLevel
{
    /// <summary>
    /// Read-only access: can download files but cannot upload changes.
    /// </summary>
    ReadOnly,

    /// <summary>
    /// Read-write access: can download and upload files, affecting all peers.
    /// </summary>
    ReadWrite
}
