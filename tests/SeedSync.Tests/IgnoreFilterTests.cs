using SeedSync.Core.Services;

namespace SeedSync.Tests;

public class IgnoreFilterTests
{
    [Theory]
    [InlineData("*.log", "debug.log", true)]
    [InlineData("*.log", "logs/debug.log", true)]
    [InlineData("*.log", "debug.txt", false)]
    [InlineData(".git/", ".git/config", true)]
    [InlineData(".git/", "src/.git/config", true)]
    [InlineData("node_modules/", "node_modules/package/index.js", true)]
    [InlineData("node_modules/", "src/node_modules/package/index.js", true)]
    [InlineData("/root.txt", "root.txt", true)]
    [InlineData("/root.txt", "subdir/root.txt", false)]
    [InlineData("build/", "build/output.dll", true)]
    [InlineData("build/", "src/build/output.dll", true)]
    public void ShouldIgnore_MatchesPatternCorrectly(string pattern, string path, bool expected)
    {
        // Arrange
        var filter = new IgnoreFilter([pattern]);

        // Act
        var result = filter.ShouldIgnore(path);

        // Assert
        Assert.Equal(expected, result);
    }

    [Fact]
    public void ShouldIgnore_HandlesNegationPattern()
    {
        // Arrange
        var filter = new IgnoreFilter([
            "*.log",
            "!important.log"
        ]);

        // Assert
        Assert.True(filter.ShouldIgnore("debug.log"));
        Assert.False(filter.ShouldIgnore("important.log"));
    }

    [Fact]
    public void ShouldIgnore_IgnoresComments()
    {
        // Arrange
        var filter = new IgnoreFilter([
            "# This is a comment",
            "*.log"
        ]);

        // Assert
        Assert.True(filter.ShouldIgnore("test.log"));
        Assert.False(filter.ShouldIgnore("# This is a comment"));
    }

    [Fact]
    public void ShouldIgnore_HandlesWindowsPathSeparators()
    {
        // Arrange
        var filter = new IgnoreFilter(["node_modules/"]);

        // Act & Assert
        Assert.True(filter.ShouldIgnore(@"node_modules\package\index.js"));
        Assert.True(filter.ShouldIgnore(@"src\node_modules\package\index.js"));
    }

    [Fact]
    public void ShouldIgnore_HandlesDoubleWildcard()
    {
        // Arrange
        var filter = new IgnoreFilter(["**/logs/**"]);

        // Act & Assert
        Assert.True(filter.ShouldIgnore("logs/debug.log"));
        Assert.True(filter.ShouldIgnore("app/logs/debug.log"));
        Assert.True(filter.ShouldIgnore("app/logs/2024/01/debug.log"));
    }

    [Fact]
    public void FilterPaths_ReturnsNonIgnoredPaths()
    {
        // Arrange
        var filter = new IgnoreFilter(["*.log", "temp/"]);
        var paths = new[]
        {
            "src/main.cs",
            "debug.log",
            "temp/cache.dat",
            "docs/readme.md"
        };

        // Act
        var result = filter.FilterPaths(paths).ToList();

        // Assert
        Assert.Equal(2, result.Count);
        Assert.Contains("src/main.cs", result);
        Assert.Contains("docs/readme.md", result);
    }

    [Fact]
    public void CreateDefault_IncludesCommonPatterns()
    {
        // Arrange
        var filter = IgnoreFilter.CreateDefault();

        // Assert
        Assert.True(filter.ShouldIgnore(".git/config"));
        Assert.True(filter.ShouldIgnore("node_modules/package/index.js"));
        Assert.True(filter.ShouldIgnore("bin/Debug/app.dll"));
        Assert.True(filter.ShouldIgnore(".DS_Store"));
        Assert.True(filter.ShouldIgnore("Thumbs.db"));

        Assert.False(filter.ShouldIgnore("src/main.cs"));
        Assert.False(filter.ShouldIgnore("README.md"));
    }

    [Fact]
    public void AddPattern_IgnoresEmptyPatterns()
    {
        // Arrange
        var filter = new IgnoreFilter();

        // Act
        filter.AddPattern("");
        filter.AddPattern("   ");

        // Assert
        Assert.False(filter.ShouldIgnore("anything.txt"));
    }

    [Theory]
    [InlineData("")]
    [InlineData(null)]
    public void ShouldIgnore_ReturnsFalseForEmptyPath(string? path)
    {
        // Arrange
        var filter = new IgnoreFilter(["*.log"]);

        // Act
        var result = filter.ShouldIgnore(path!);

        // Assert
        Assert.False(result);
    }
}
