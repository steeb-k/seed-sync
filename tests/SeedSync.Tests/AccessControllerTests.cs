using SeedSync.Core.Models;
using SeedSync.Core.Services;

namespace SeedSync.Tests;

public class AccessControllerTests
{
    [Fact]
    public void RegisterAccess_StoresAccessLevel()
    {
        // Arrange
        var controller = new AccessController();
        const string shareId = "test-share";

        // Act
        controller.RegisterAccess(shareId, AccessLevel.ReadWrite);

        // Assert
        Assert.Equal(AccessLevel.ReadWrite, controller.GetAccessLevel(shareId));
    }

    [Fact]
    public void GetAccessLevel_ReturnsNullForUnregisteredShare()
    {
        // Arrange
        var controller = new AccessController();

        // Act
        var level = controller.GetAccessLevel("nonexistent");

        // Assert
        Assert.Null(level);
    }

    [Fact]
    public void CanUpload_ReturnsTrueForRwAccess()
    {
        // Arrange
        var controller = new AccessController();
        const string shareId = "test-share";
        controller.RegisterAccess(shareId, AccessLevel.ReadWrite);

        // Act & Assert
        Assert.True(controller.CanUpload(shareId));
    }

    [Fact]
    public void CanUpload_ReturnsFalseForRoAccess()
    {
        // Arrange
        var controller = new AccessController();
        const string shareId = "test-share";
        controller.RegisterAccess(shareId, AccessLevel.ReadOnly);

        // Act & Assert
        Assert.False(controller.CanUpload(shareId));
    }

    [Fact]
    public void CanDownload_ReturnsTrueForBothAccessLevels()
    {
        // Arrange
        var controller = new AccessController();
        controller.RegisterAccess("rw-share", AccessLevel.ReadWrite);
        controller.RegisterAccess("ro-share", AccessLevel.ReadOnly);

        // Act & Assert
        Assert.True(controller.CanDownload("rw-share"));
        Assert.True(controller.CanDownload("ro-share"));
    }

    [Fact]
    public void CanDownload_ReturnsFalseForUnregisteredShare()
    {
        // Arrange
        var controller = new AccessController();

        // Act & Assert
        Assert.False(controller.CanDownload("nonexistent"));
    }

    [Fact]
    public void UnregisterAccess_RemovesShare()
    {
        // Arrange
        var controller = new AccessController();
        const string shareId = "test-share";
        controller.RegisterAccess(shareId, AccessLevel.ReadWrite);

        // Act
        controller.UnregisterAccess(shareId);

        // Assert
        Assert.Null(controller.GetAccessLevel(shareId));
        Assert.False(controller.CanUpload(shareId));
        Assert.False(controller.CanDownload(shareId));
    }

    [Theory]
    [InlineData(AccessLevel.ReadWrite, FileOperation.Read, true)]
    [InlineData(AccessLevel.ReadWrite, FileOperation.Write, true)]
    [InlineData(AccessLevel.ReadWrite, FileOperation.Delete, true)]
    [InlineData(AccessLevel.ReadWrite, FileOperation.Create, true)]
    [InlineData(AccessLevel.ReadOnly, FileOperation.Read, true)]
    [InlineData(AccessLevel.ReadOnly, FileOperation.Write, false)]
    [InlineData(AccessLevel.ReadOnly, FileOperation.Delete, false)]
    [InlineData(AccessLevel.ReadOnly, FileOperation.Create, false)]
    public void IsOperationAllowed_ReturnsCorrectResult(
        AccessLevel accessLevel, FileOperation operation, bool expected)
    {
        // Arrange
        var controller = new AccessController();
        const string shareId = "test-share";
        controller.RegisterAccess(shareId, accessLevel);

        // Act
        var result = controller.IsOperationAllowed(shareId, operation);

        // Assert
        Assert.Equal(expected, result);
    }

    [Fact]
    public void IsOperationAllowed_ReturnsFalseForUnregisteredShare()
    {
        // Arrange
        var controller = new AccessController();

        // Act & Assert
        Assert.False(controller.IsOperationAllowed("nonexistent", FileOperation.Read));
    }
}
