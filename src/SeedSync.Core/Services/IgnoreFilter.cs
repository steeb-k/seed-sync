using System.Text.RegularExpressions;

namespace SeedSync.Core.Services;

/// <summary>
/// Filters files based on gitignore-style patterns.
/// Supports common patterns like *.ext, folder/, !negation, etc.
/// </summary>
public sealed class IgnoreFilter
{
    private readonly List<(Regex Pattern, bool IsNegation)> _patterns = [];

    /// <summary>
    /// Creates an empty ignore filter.
    /// </summary>
    public IgnoreFilter()
    {
    }

    /// <summary>
    /// Creates an ignore filter with the specified patterns.
    /// </summary>
    /// <param name="patterns">The patterns to use.</param>
    public IgnoreFilter(IEnumerable<string> patterns)
    {
        foreach (var pattern in patterns)
        {
            AddPattern(pattern);
        }
    }

    /// <summary>
    /// Adds a pattern to the filter.
    /// </summary>
    /// <param name="pattern">The gitignore-style pattern.</param>
    public void AddPattern(string pattern)
    {
        if (string.IsNullOrWhiteSpace(pattern))
            return;

        pattern = pattern.Trim();

        // Skip comments
        if (pattern.StartsWith('#'))
            return;

        var isNegation = pattern.StartsWith('!');
        if (isNegation)
            pattern = pattern[1..];

        var regex = ConvertToRegex(pattern);
        _patterns.Add((regex, isNegation));
    }

    /// <summary>
    /// Checks if a file path should be ignored.
    /// </summary>
    /// <param name="relativePath">The path relative to the share root.</param>
    /// <returns>True if the file should be ignored.</returns>
    public bool ShouldIgnore(string relativePath)
    {
        if (string.IsNullOrEmpty(relativePath))
            return false;

        // Normalize path separators
        relativePath = relativePath.Replace('\\', '/');

        var shouldIgnore = false;

        foreach (var (pattern, isNegation) in _patterns)
        {
            if (pattern.IsMatch(relativePath))
            {
                shouldIgnore = !isNegation;
            }
        }

        return shouldIgnore;
    }

    /// <summary>
    /// Filters a list of file paths, returning only those that should not be ignored.
    /// </summary>
    /// <param name="relativePaths">The paths to filter.</param>
    /// <returns>Paths that should not be ignored.</returns>
    public IEnumerable<string> FilterPaths(IEnumerable<string> relativePaths)
    {
        return relativePaths.Where(p => !ShouldIgnore(p));
    }

    /// <summary>
    /// Gets all patterns as strings.
    /// </summary>
    public IReadOnlyList<string> GetPatterns()
    {
        return _patterns.Select(p => (p.IsNegation ? "!" : "") + p.Pattern.ToString()).ToList();
    }

    private static Regex ConvertToRegex(string pattern)
    {
        // Handle directory-only patterns (ending with /)
        var matchDirOnly = pattern.EndsWith('/');
        if (matchDirOnly)
            pattern = pattern.TrimEnd('/');

        // Handle patterns starting with /
        var anchoredToRoot = pattern.StartsWith('/');
        if (anchoredToRoot)
            pattern = pattern[1..];

        // Escape special regex characters except * and ?
        // Handle **/ at start (match any prefix including empty)
        var hasLeadingDoubleStar = pattern.StartsWith("**/");
        if (hasLeadingDoubleStar)
            pattern = pattern[3..]; // Remove **/

        // Handle /** at end (match any suffix)
        var hasTrailingDoubleStar = pattern.EndsWith("/**");
        if (hasTrailingDoubleStar)
            pattern = pattern[..^3]; // Remove /**

        var escaped = Regex.Escape(pattern)
            .Replace("\\*\\*", "@@DOUBLESTAR@@")  // Temp placeholder
            .Replace("\\*", "[^/]*")               // * matches anything except /
            .Replace("\\?", "[^/]")                // ? matches single char except /
            .Replace("@@DOUBLESTAR@@", ".*");      // ** matches everything including /

        // Apply leading/trailing double star
        if (hasLeadingDoubleStar)
            escaped = "(.*/)?" + escaped;
        if (hasTrailingDoubleStar)
            escaped = escaped + "(/.*)?";
        else if (!hasLeadingDoubleStar && !hasTrailingDoubleStar)
        {
            // No double stars - normal pattern
        }

        // Build the full pattern
        string regexPattern;

        if (anchoredToRoot)
        {
            // Anchored to root - must match from start
            if (matchDirOnly)
            {
                // Directory pattern at root - match the directory and anything under it
                regexPattern = $"^{escaped}(/.*)?$";
            }
            else
            {
                regexPattern = $"^{escaped}(/.*)?$";
            }
        }
        else if (matchDirOnly || !pattern.Contains("*"))
        {
            // Directory pattern or simple name - can match anywhere
            // Pattern like "node_modules/" should match "node_modules/..." or "src/node_modules/..."
            regexPattern = $"(^|/){escaped}(/.*)?$";
        }
        else
        {
            // Wildcard pattern like *.log
            regexPattern = $"(^|/){escaped}$";
        }

        return new Regex(regexPattern, RegexOptions.IgnoreCase | RegexOptions.Compiled);
    }

    /// <summary>
    /// Creates a filter with common development ignore patterns.
    /// </summary>
    public static IgnoreFilter CreateDefault()
    {
        return new IgnoreFilter(
        [
            // Version control
            ".git/",
            ".svn/",
            ".hg/",

            // IDE
            ".vs/",
            ".vscode/",
            ".idea/",
            "*.suo",
            "*.user",

            // Build outputs
            "bin/",
            "obj/",
            "node_modules/",
            "packages/",

            // Temp files
            "*.tmp",
            "*.temp",
            "*.log",
            "~$*",
            "Thumbs.db",
            ".DS_Store"
        ]);
    }
}
