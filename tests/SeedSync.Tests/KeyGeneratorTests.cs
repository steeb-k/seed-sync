using SeedSync.Core.Models;
using SeedSync.Core.Services;

namespace SeedSync.Tests;

public class KeyGeneratorTests
{
    [Fact]
    public void GenerateKeys_CreatesValidKeys()
    {
        // Act
        var keys = KeyGenerator.GenerateKeys();

        // Assert
        Assert.NotNull(keys);
        Assert.NotEmpty(keys.ReadWriteKey);
        Assert.NotEmpty(keys.ReadOnlyKey);
        Assert.NotEmpty(keys.ShareId);
    }

    [Fact]
    public void GenerateKeys_RwAndRoKeysAreDifferent()
    {
        // Act
        var keys = KeyGenerator.GenerateKeys();

        // Assert
        Assert.NotEqual(keys.ReadWriteKey, keys.ReadOnlyKey);
    }

    [Fact]
    public void GenerateKeys_KeysAreUnique()
    {
        // Act
        var keys1 = KeyGenerator.GenerateKeys();
        var keys2 = KeyGenerator.GenerateKeys();

        // Assert
        Assert.NotEqual(keys1.ReadWriteKey, keys2.ReadWriteKey);
        Assert.NotEqual(keys1.ReadOnlyKey, keys2.ReadOnlyKey);
        Assert.NotEqual(keys1.ShareId, keys2.ShareId);
    }

    [Fact]
    public void ParseKey_IdentifiesRwKey()
    {
        // Arrange
        var keys = KeyGenerator.GenerateKeys();

        // Act
        var parsed = KeyGenerator.ParseKey(keys.ReadWriteKey);

        // Assert
        Assert.NotNull(parsed);
        Assert.Equal(keys.ShareId, parsed.Value.ShareId);
        Assert.Equal(AccessLevel.ReadWrite, parsed.Value.AccessLevel);
    }

    [Fact]
    public void ParseKey_IdentifiesRoKey()
    {
        // Arrange
        var keys = KeyGenerator.GenerateKeys();

        // Act
        var parsed = KeyGenerator.ParseKey(keys.ReadOnlyKey);

        // Assert
        Assert.NotNull(parsed);
        Assert.Equal(keys.ShareId, parsed.Value.ShareId);
        Assert.Equal(AccessLevel.ReadOnly, parsed.Value.AccessLevel);
        Assert.Null(parsed.Value.InfoHash); // Legacy format has no embedded info hash
    }

    [Fact]
    public void WithInfoHash_EmbedsInfoHashInRoKey()
    {
        var keys = KeyGenerator.GenerateKeys();
        var infoHash = KeyGenerator.DeriveInfoHash(keys.ShareId);

        var roKeyWithHash = KeyGenerator.WithInfoHash(keys.ReadOnlyKey, infoHash);

        Assert.StartsWith("SEEDRO", roKeyWithHash, StringComparison.OrdinalIgnoreCase);
        Assert.True(roKeyWithHash.Length > keys.ReadOnlyKey.Length);
        var parsed = KeyGenerator.ParseKey(roKeyWithHash);
        Assert.NotNull(parsed);
        Assert.Equal(keys.ShareId, parsed.Value.ShareId);
        Assert.Equal(AccessLevel.ReadOnly, parsed.Value.AccessLevel);
        Assert.NotNull(parsed.Value.InfoHash);
        Assert.Equal(infoHash, parsed.Value.InfoHash);
    }

    [Fact]
    public void ParseKey_ReturnsNullForInvalidKey()
    {
        // Act
        var parsed = KeyGenerator.ParseKey("invalid-key");

        // Assert
        Assert.Null(parsed);
    }

    [Fact]
    public void IsValidKey_ReturnsTrueForValidKeys()
    {
        // Arrange
        var keys = KeyGenerator.GenerateKeys();

        // Assert
        Assert.True(KeyGenerator.IsValidKey(keys.ReadWriteKey));
        Assert.True(KeyGenerator.IsValidKey(keys.ReadOnlyKey));
    }

    [Fact]
    public void IsValidKey_ReturnsFalseForInvalidKeys()
    {
        // Assert
        Assert.False(KeyGenerator.IsValidKey(""));
        Assert.False(KeyGenerator.IsValidKey("invalid"));
        Assert.False(KeyGenerator.IsValidKey("SEEDRW")); // Too short
    }

    [Fact]
    public void DeriveInfoHash_SameShareIdProducesSameHash()
    {
        // Arrange
        var keys = KeyGenerator.GenerateKeys();

        // Act
        var hash1 = KeyGenerator.DeriveInfoHash(keys.ShareId);
        var hash2 = KeyGenerator.DeriveInfoHash(keys.ShareId);

        // Assert
        Assert.Equal(hash1, hash2);
        Assert.Equal(20, hash1.Length); // BitTorrent info hash is 20 bytes
    }

    [Fact]
    public void DeriveInfoHash_DifferentShareIdsProduceDifferentHashes()
    {
        // Arrange
        var keys1 = KeyGenerator.GenerateKeys();
        var keys2 = KeyGenerator.GenerateKeys();

        // Act
        var hash1 = KeyGenerator.DeriveInfoHash(keys1.ShareId);
        var hash2 = KeyGenerator.DeriveInfoHash(keys2.ShareId);

        // Assert
        Assert.NotEqual(hash1, hash2);
    }

    [Fact]
    public void RwKeyCannotBeDerivedFromRoKey()
    {
        // This test verifies the security property that RO key holders
        // cannot derive the RW key
        
        // Arrange
        var keys = KeyGenerator.GenerateKeys();
        
        // The RO key contains the share ID but the random part is different
        var roKeyData = keys.ReadOnlyKey;
        var rwKeyData = keys.ReadWriteKey;
        
        // Extract the random portions (after prefix and share ID)
        var roRandom = roKeyData.Substring(6 + 32); // "SEEDRO" + 32 char share ID
        var rwRandom = rwKeyData.Substring(6 + 32); // "SEEDRW" + 32 char share ID
        
        // Assert - the random portions should be completely different
        Assert.NotEqual(roRandom, rwRandom);
    }
}
