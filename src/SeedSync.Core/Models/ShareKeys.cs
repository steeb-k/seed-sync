namespace SeedSync.Core.Models;

/// <summary>
/// Contains the cryptographic keys for a share.
/// RW and RO keys are independent secrets - the RO key cannot be used to derive the RW key.
/// </summary>
public sealed class ShareKeys
{
    /// <summary>
    /// The read-write key. This is a secret that should only be shared with trusted users.
    /// Possessing this key allows full read/write access to the share.
    /// </summary>
    public required string ReadWriteKey { get; init; }

    /// <summary>
    /// The read-only key. Safe to distribute to untrusted users.
    /// Possessing this key only allows downloading files, not uploading changes.
    /// </summary>
    public required string ReadOnlyKey { get; init; }

    /// <summary>
    /// A unique identifier for the share, derived from the keys.
    /// Used internally to identify the share across peers.
    /// </summary>
    public required string ShareId { get; init; }
}
