using System.Net.Http.Json;
using System.Text.Json;

namespace SeedSync.Cli;

/// <summary>
/// S.E.E.D. CLI - Command line interface for the sync daemon.
/// </summary>
public static class Program
{
    private static readonly HttpClient _client = new()
    {
        BaseAddress = new Uri("http://127.0.0.1:9876"),
        Timeout = TimeSpan.FromSeconds(30)
    };

    private static readonly JsonSerializerOptions _jsonOptions = new()
    {
        PropertyNameCaseInsensitive = true,
        WriteIndented = true
    };

    public static async Task<int> Main(string[] args)
    {
        if (args.Length == 0)
        {
            ShowHelp();
            return 0;
        }

        var command = args[0].ToLowerInvariant();

        try
        {
            return command switch
            {
                "create" => await CreateShareAsync(args[1..]),
                "add" => await AddShareAsync(args[1..]),
                "list" => await ListSharesAsync(),
                "status" => await StatusAsync(args.Length > 1 ? args[1] : null),
                "remove" => await RemoveShareAsync(args.Length > 1 ? args[1] : null),
                "help" or "--help" or "-h" => ShowHelp(),
                _ => ShowUnknownCommand(command)
            };
        }
        catch (HttpRequestException)
        {
            Console.WriteLine("Error: Could not connect to S.E.E.D. daemon.");
            Console.WriteLine("Make sure the daemon is running.");
            return 1;
        }
        catch (Exception ex)
        {
            Console.WriteLine($"Error: {ex.Message}");
            return 1;
        }
    }

    private static int ShowHelp()
    {
        Console.WriteLine("S.E.E.D. - Secure Environment Exchange Daemon CLI");
        Console.WriteLine();
        Console.WriteLine("Usage: seed <command> [arguments]");
        Console.WriteLine();
        Console.WriteLine("Commands:");
        Console.WriteLine("  create <path> [--name <name>]       Create a new share");
        Console.WriteLine("  add <key> <path> [--name <name>]    Add an existing share");
        Console.WriteLine("  list                                List all shares");
        Console.WriteLine("  status [share-id]                   Show status of share(s)");
        Console.WriteLine("  remove <share-id>                   Remove a share");
        Console.WriteLine("  help                                Show this help");
        Console.WriteLine();
        return 0;
    }

    private static int ShowUnknownCommand(string command)
    {
        Console.WriteLine($"Unknown command: {command}");
        Console.WriteLine("Use 'seed help' for available commands.");
        return 1;
    }

    private static async Task<int> CreateShareAsync(string[] args)
    {
        if (args.Length == 0)
        {
            Console.WriteLine("Usage: seed create <path> [--name <name>]");
            return 1;
        }

        var path = args[0];
        string? name = null;

        // Parse options
        for (int i = 1; i < args.Length - 1; i++)
        {
            if (args[i] == "--name" && i + 1 < args.Length)
            {
                name = args[i + 1];
                i++;
            }
        }

        var fullPath = Path.GetFullPath(path);
        if (!Directory.Exists(fullPath))
        {
            Console.WriteLine($"Error: Directory not found: {fullPath}");
            return 1;
        }

        var request = new
        {
            FolderPath = fullPath,
            Name = name
        };

        var response = await _client.PostAsJsonAsync("/api/shares", request);

        if (response.IsSuccessStatusCode)
        {
            var result = await response.Content.ReadFromJsonAsync<CreateShareResult>(_jsonOptions);
            if (result != null)
            {
                Console.WriteLine();
                Console.WriteLine("Share created successfully!");
                Console.WriteLine();
                Console.WriteLine($"  Share ID: {result.ShareId}");
                Console.WriteLine();
                Console.WriteLine("  Keys (save these!):");
                Console.WriteLine($"    Read/Write (SECRET): {result.ReadWriteKey}");
                Console.WriteLine($"    Read-Only:           {result.ReadOnlyKey}");
                Console.WriteLine();
                Console.WriteLine("  WARNING: The Read/Write key gives full access to modify files.");
                Console.WriteLine("           Only share it with trusted users!");
                Console.WriteLine();
                return 0;
            }
        }

        var error = await response.Content.ReadAsStringAsync();
        Console.WriteLine($"Error: {error}");
        return 1;
    }

    private static async Task<int> AddShareAsync(string[] args)
    {
        if (args.Length < 2)
        {
            Console.WriteLine("Usage: seed add <key> <path> [--name <name>]");
            return 1;
        }

        var key = args[0];
        var path = args[1];
        string? name = null;

        // Parse options
        for (int i = 2; i < args.Length - 1; i++)
        {
            if (args[i] == "--name" && i + 1 < args.Length)
            {
                name = args[i + 1];
                i++;
            }
        }

        var fullPath = Path.GetFullPath(path);

        // Check if key is RW and warn
        if (key.StartsWith("SEEDRW", StringComparison.OrdinalIgnoreCase))
        {
            Console.WriteLine();
            Console.WriteLine("  *** WARNING ***");
            Console.WriteLine("  You are adding this share with a READ/WRITE key.");
            Console.WriteLine("  Any changes you make will affect ALL users of this share!");
            Console.WriteLine();
        }

        var request = new
        {
            Key = key,
            LocalPath = fullPath,
            Name = name
        };

        var response = await _client.PostAsJsonAsync("/api/shares/add", request);

        if (response.IsSuccessStatusCode)
        {
            var result = await response.Content.ReadFromJsonAsync<AddShareResult>(_jsonOptions);
            if (result != null)
            {
                Console.WriteLine();
                Console.WriteLine("Share added successfully!");
                Console.WriteLine();
                Console.WriteLine($"  Share ID:    {result.ShareId}");
                Console.WriteLine($"  Access:      {result.AccessLevel}");
                Console.WriteLine($"  Local Path:  {fullPath}");

                if (result.IsReadWrite)
                {
                    Console.WriteLine();
                    Console.WriteLine("  You have READ/WRITE access. Changes will sync to all peers.");
                }
                Console.WriteLine();
                return 0;
            }
        }

        var error = await response.Content.ReadAsStringAsync();
        Console.WriteLine($"Error: {error}");
        return 1;
    }

    private static async Task<int> ListSharesAsync()
    {
        var response = await _client.GetAsync("/api/shares");

        if (response.IsSuccessStatusCode)
        {
            var shares = await response.Content.ReadFromJsonAsync<List<ShareInfo>>(_jsonOptions);
            if (shares == null || shares.Count == 0)
            {
                Console.WriteLine("No shares configured.");
                return 0;
            }

            Console.WriteLine();
            Console.WriteLine($"{"ID",-36} {"Name",-20} {"Access",-10} {"Status",-12} {"Peers",-6}");
            Console.WriteLine(new string('-', 90));

            foreach (var share in shares)
            {
                Console.WriteLine($"{share.Id,-36} {Truncate(share.Name, 20),-20} {share.AccessLevel,-10} {share.Status,-12} {share.ConnectedPeers,-6}");
            }
            Console.WriteLine();
            return 0;
        }

        Console.WriteLine("Error fetching shares.");
        return 1;
    }

    private static async Task<int> StatusAsync(string? shareId)
    {
        if (string.IsNullOrEmpty(shareId))
        {
            return await ListSharesAsync();
        }

        var response = await _client.GetAsync($"/api/shares/{shareId}");

        if (response.IsSuccessStatusCode)
        {
            var share = await response.Content.ReadFromJsonAsync<ShareInfo>(_jsonOptions);
            if (share != null)
            {
                Console.WriteLine();
                Console.WriteLine($"Share: {share.Name}");
                Console.WriteLine($"  ID:          {share.Id}");
                Console.WriteLine($"  Path:        {share.LocalPath}");
                Console.WriteLine($"  Access:      {share.AccessLevel}");
                Console.WriteLine($"  Status:      {share.Status}");
                Console.WriteLine($"  Progress:    {share.Progress:P0}");
                Console.WriteLine($"  Peers:       {share.ConnectedPeers}");

                if (!string.IsNullOrEmpty(share.ReadOnlyKey))
                {
                    Console.WriteLine();
                    Console.WriteLine($"  RO Key:      {share.ReadOnlyKey}");
                }
                Console.WriteLine();
                return 0;
            }
        }
        else if (response.StatusCode == System.Net.HttpStatusCode.NotFound)
        {
            Console.WriteLine($"Share not found: {shareId}");
            return 1;
        }

        Console.WriteLine("Error fetching share status.");
        return 1;
    }

    private static async Task<int> RemoveShareAsync(string? shareId)
    {
        if (string.IsNullOrEmpty(shareId))
        {
            Console.WriteLine("Usage: seed remove <share-id>");
            return 1;
        }

        var response = await _client.DeleteAsync($"/api/shares/{shareId}");

        if (response.IsSuccessStatusCode)
        {
            Console.WriteLine($"Share {shareId} removed.");
            return 0;
        }
        else if (response.StatusCode == System.Net.HttpStatusCode.NotFound)
        {
            Console.WriteLine($"Share not found: {shareId}");
            return 1;
        }

        Console.WriteLine("Error removing share.");
        return 1;
    }

    private static string Truncate(string value, int maxLength)
    {
        if (string.IsNullOrEmpty(value)) return "";
        return value.Length <= maxLength ? value : value[..(maxLength - 3)] + "...";
    }
}

// DTOs matching the daemon API
internal sealed class CreateShareResult
{
    public string ShareId { get; set; } = "";
    public string ReadWriteKey { get; set; } = "";
    public string ReadOnlyKey { get; set; } = "";
}

internal sealed class AddShareResult
{
    public string ShareId { get; set; } = "";
    public string AccessLevel { get; set; } = "";
    public bool IsReadWrite { get; set; }
}

internal sealed class ShareInfo
{
    public string Id { get; set; } = "";
    public string Name { get; set; } = "";
    public string LocalPath { get; set; } = "";
    public string AccessLevel { get; set; } = "";
    public string Status { get; set; } = "";
    public double Progress { get; set; }
    public int ConnectedPeers { get; set; }
    public string? ReadWriteKey { get; set; }
    public string? ReadOnlyKey { get; set; }
}
