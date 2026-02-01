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
    private const int InfoHashHexLength = 40; // 20 bytes = 40 hex chars
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
    /// Parses a key string and extracts the share ID, access level, and optional info hash (RO keys only).
    /// </summary>
    /// <param name="key">The key to parse.</param>
    /// <returns>Tuple of (ShareId, AccessLevel, InfoHash or null) or null if invalid.</returns>
    public static (string ShareId, AccessLevel AccessLevel, byte[]? InfoHash)? ParseKey(string key)
    {
        if (string.IsNullOrWhiteSpace(key))
            return null;

        if (key.StartsWith(RwPrefix, StringComparison.OrdinalIgnoreCase) && key.Length > RwPrefix.Length + 32)
        {
            var shareId = key.Substring(RwPrefix.Length, 32);
            return (shareId, AccessLevel.ReadWrite, null);
        }

        if (key.StartsWith(RoPrefix, StringComparison.OrdinalIgnoreCase) && key.Length > RoPrefix.Length + 32)
        {
            var afterPrefix = key.Substring(RoPrefix.Length);
            var shareId = afterPrefix.Substring(0, 32);
            // New format: SEEDRO + shareId(32) + infoHashHex(40) + secret
            if (afterPrefix.Length >= 32 + InfoHashHexLength &&
                IsAllHex(afterPrefix.AsSpan(32, InfoHashHexLength)))
            {
                var infoHashHex = afterPrefix.Substring(32, InfoHashHexLength);
                var infoHash = Convert.FromHexString(infoHashHex);
                return (shareId, AccessLevel.ReadOnly, infoHash);
            }
            // Legacy format: SEEDRO + shareId(32) + secret (no embedded info hash)
            return (shareId, AccessLevel.ReadOnly, null);
        }

        return null;
    }

    private static bool IsAllHex(ReadOnlySpan<char> s)
    {
        foreach (var c in s)
        {
            if (!char.IsAsciiHexDigit(c))
                return false;
        }
        return true;
    }

    /// <summary>
    /// Builds an RO key that includes the torrent info hash so joiners use the same swarm as the creator.
    /// </summary>
    /// <param name="roKey">Existing RO key (SEEDRO + shareId + secret).</param>
    /// <param name="infoHash">The actual torrent info hash (20 bytes).</param>
    /// <returns>RO key with embedded info hash: SEEDRO + shareId + infoHashHex + secret.</returns>
    public static string WithInfoHash(string roKey, byte[] infoHash)
    {
        if (infoHash == null || infoHash.Length != 20)
            throw new ArgumentException("Info hash must be 20 bytes.", nameof(infoHash));
        if (string.IsNullOrEmpty(roKey) || !roKey.StartsWith(RoPrefix, StringComparison.OrdinalIgnoreCase) || roKey.Length <= RoPrefix.Length + 32)
            throw new ArgumentException("Invalid RO key format.", nameof(roKey));

        var shareId = roKey.Substring(RoPrefix.Length, 32);
        var secret = roKey.Substring(RoPrefix.Length + 32);
        var infoHashHex = Convert.ToHexString(infoHash).ToLowerInvariant();
        return $"{RoPrefix}{shareId}{infoHashHex}{secret}";
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
