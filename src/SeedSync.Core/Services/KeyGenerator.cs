using System.Security.Cryptography;
using SeedSync.Core.Models;

namespace SeedSync.Core.Services;

/// <summary>
/// Generates cryptographic keys for shares.
/// RW and RO keys are independent secrets generated using CSPRNG.
/// </summary>
public static class KeyGenerator
{
    private const int KeyLengthBytes = 32; // 256-bit keys
    private const string RwPrefix = "SEEDRW";
    private const string RoPrefix = "SEEDRO";

    /// <summary>
    /// Generates a new set of keys for a share.
    /// Both RW and RO keys are independently generated random secrets.
    /// </summary>
    /// <returns>A new ShareKeys instance with RW key, RO key, and ShareId.</returns>
    public static ShareKeys GenerateKeys()
    {
        // Generate independent random bytes for each key
        var rwBytes = RandomNumberGenerator.GetBytes(KeyLengthBytes);
        var roBytes = RandomNumberGenerator.GetBytes(KeyLengthBytes);
        var idBytes = RandomNumberGenerator.GetBytes(16); // 128-bit share ID

        var shareId = Convert.ToHexString(idBytes).ToLowerInvariant();
        var rwKey = $"{RwPrefix}{shareId}{Convert.ToBase64String(rwBytes).Replace("+", "-").Replace("/", "_").TrimEnd('=')}";
        var roKey = $"{RoPrefix}{shareId}{Convert.ToBase64String(roBytes).Replace("+", "-").Replace("/", "_").TrimEnd('=')}";

        return new ShareKeys
        {
            ShareId = shareId,
            ReadWriteKey = rwKey,
            ReadOnlyKey = roKey
        };
    }

    /// <summary>
    /// Parses a key string and extracts the share ID and access level.
    /// </summary>
    /// <param name="key">The key to parse.</param>
    /// <returns>Tuple of (ShareId, AccessLevel) or null if invalid.</returns>
    public static (string ShareId, AccessLevel AccessLevel)? ParseKey(string key)
    {
        if (string.IsNullOrWhiteSpace(key))
            return null;

        if (key.StartsWith(RwPrefix, StringComparison.OrdinalIgnoreCase) && key.Length > RwPrefix.Length + 32)
        {
            var shareId = key.Substring(RwPrefix.Length, 32);
            return (shareId, AccessLevel.ReadWrite);
        }

        if (key.StartsWith(RoPrefix, StringComparison.OrdinalIgnoreCase) && key.Length > RoPrefix.Length + 32)
        {
            var shareId = key.Substring(RoPrefix.Length, 32);
            return (shareId, AccessLevel.ReadOnly);
        }

        return null;
    }

    /// <summary>
    /// Validates that a key is well-formed.
    /// </summary>
    /// <param name="key">The key to validate.</param>
    /// <returns>True if the key is valid, false otherwise.</returns>
    public static bool IsValidKey(string key)
    {
        return ParseKey(key) != null;
    }

    /// <summary>
    /// Gets the access level from a key string.
    /// </summary>
    /// <param name="key">The key to check.</param>
    /// <returns>The access level, or null if the key is invalid.</returns>
    public static AccessLevel? GetAccessLevel(string key)
    {
        return ParseKey(key)?.AccessLevel;
    }

    /// <summary>
    /// Gets the share ID from a key string.
    /// </summary>
    /// <param name="key">The key to parse.</param>
    /// <returns>The share ID, or null if the key is invalid.</returns>
    public static string? GetShareId(string key)
    {
        return ParseKey(key)?.ShareId;
    }

    /// <summary>
    /// Derives an info hash for BitTorrent from the share ID.
    /// This is used to form the swarm - all peers with either RW or RO key
    /// for the same share will have the same info hash.
    /// </summary>
    /// <param name="shareId">The share ID.</param>
    /// <returns>A 20-byte info hash suitable for BitTorrent.</returns>
    public static byte[] DeriveInfoHash(string shareId)
    {
        // Use SHA1 to get a 20-byte hash (BitTorrent info hash size)
        var shareIdBytes = System.Text.Encoding.UTF8.GetBytes($"SEED-SYNC-{shareId}");
        return SHA1.HashData(shareIdBytes);
    }
}
