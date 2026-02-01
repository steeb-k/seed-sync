using System.Collections.ObjectModel;
using System.Net.Http.Json;
using System.Text.Json;
using Windows.ApplicationModel.DataTransfer;
using Microsoft.UI.Xaml.Controls;

namespace SeedSync.App.Views;

/// <summary>
/// Main page showing the list of shares.
/// </summary>
public partial class MainPage : Page
{
    private static readonly HttpClient _client = new()
    {
        BaseAddress = new Uri("http://127.0.0.1:9876"),
        Timeout = TimeSpan.FromSeconds(10)
    };

    private static readonly JsonSerializerOptions _jsonOptions = new()
    {
        PropertyNameCaseInsensitive = true
    };

    public ObservableCollection<ShareViewModel> Shares { get; } = [];

    public MainPage()
    {
        this.InitializeComponent();
        SharesList.ItemsSource = Shares;
        _ = LoadSharesAsync();
    }

    private void MainPage_Loaded(object sender, RoutedEventArgs e)
    {
        if (App.MainWindow is not null)
        {
            App.MainWindow.SetTitleBar(AppTitleBar);
        }
    }

    private async Task LoadSharesAsync()
    {
        try
        {
            var response = await _client.GetAsync("/api/shares");
            if (response.IsSuccessStatusCode)
            {
                var shares = await response.Content.ReadFromJsonAsync<List<ShareDto>>(_jsonOptions);
                Shares.Clear();

                if (shares != null && shares.Count > 0)
                {
                    foreach (var share in shares)
                    {
                        Shares.Add(new ShareViewModel(share));
                    }
                    EmptyState.Visibility = Visibility.Collapsed;
                }
                else
                {
                    EmptyState.Visibility = Visibility.Visible;
                }
            }
            else
            {
                ShowError("Failed to load shares from daemon.");
            }
        }
        catch (HttpRequestException)
        {
            ShowError("Cannot connect to S.E.E.D. daemon. Make sure it's running.");
            EmptyState.Visibility = Visibility.Visible;
        }
        catch (Exception ex)
        {
            ShowError($"Error: {ex.Message}");
        }
    }

    private async void CreateShare_Click(object sender, RoutedEventArgs e)
    {
        var dialog = new ContentDialog
        {
            Title = "Create New Share",
            PrimaryButtonText = "Create",
            CloseButtonText = "Cancel",
            XamlRoot = this.XamlRoot
        };

        var panel = new StackPanel { Spacing = 12 };

        var pathBox = new TextBox
        {
            Header = "Folder Path",
            PlaceholderText = @"C:\path\to\folder"
        };
        panel.Children.Add(pathBox);

        var browseButton = new Button { Content = "Browse..." };
        browseButton.Click += async (s, args) =>
        {
            var picker = new Windows.Storage.Pickers.FolderPicker();
            picker.SuggestedStartLocation = Windows.Storage.Pickers.PickerLocationId.DocumentsLibrary;
            picker.FileTypeFilter.Add("*");

            // Get the window handle for the picker
            var hwnd = WinRT.Interop.WindowNative.GetWindowHandle(App.MainWindow);
            WinRT.Interop.InitializeWithWindow.Initialize(picker, hwnd);

            var folder = await picker.PickSingleFolderAsync();
            if (folder != null)
            {
                pathBox.Text = folder.Path;
            }
        };
        panel.Children.Add(browseButton);

        var nameBox = new TextBox
        {
            Header = "Display Name (optional)",
            PlaceholderText = "My Shared Folder"
        };
        panel.Children.Add(nameBox);

        dialog.Content = panel;

        var result = await dialog.ShowAsync();
        if (result == ContentDialogResult.Primary && !string.IsNullOrWhiteSpace(pathBox.Text))
        {
            await CreateShareAsync(pathBox.Text, nameBox.Text);
        }
    }

    private async Task CreateShareAsync(string path, string? name)
    {
        try
        {
            var request = new { FolderPath = path, Name = name };
            var response = await _client.PostAsJsonAsync("/api/shares", request);

            if (response.IsSuccessStatusCode)
            {
                var result = await response.Content.ReadFromJsonAsync<CreateShareResult>(_jsonOptions);
                if (result != null)
                {
                    // Show keys dialog
                    await ShowKeysDialogAsync(result);
                    await LoadSharesAsync();
                }
            }
            else
            {
                var error = await response.Content.ReadAsStringAsync();
                ShowError($"Failed to create share: {error}");
            }
        }
        catch (Exception ex)
        {
            ShowError($"Error: {ex.Message}");
        }
    }

    private async Task ShowKeysDialogAsync(CreateShareResult result)
    {
        var dialog = new ContentDialog
        {
            Title = "Share Created!",
            CloseButtonText = "OK",
            XamlRoot = this.XamlRoot
        };

        var panel = new StackPanel { Spacing = 12 };

        panel.Children.Add(new TextBlock
        {
            Text = "Save these keys! You'll need them to share this folder.",
            TextWrapping = TextWrapping.Wrap
        });

        var rwPanel = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        var rwBox = new TextBox
        {
            Header = "Read/Write Key (SECRET)",
            Text = result.ReadWriteKey,
            IsReadOnly = true,
            Width = 300
        };
        rwPanel.Children.Add(rwBox);
        var rwCopy = new Button { Content = "Copy" };
        rwCopy.Click += (s, e) => CopyToClipboard(result.ReadWriteKey);
        rwPanel.Children.Add(rwCopy);
        panel.Children.Add(rwPanel);

        panel.Children.Add(new InfoBar
        {
            Severity = InfoBarSeverity.Warning,
            Title = "Warning",
            Message = "The Read/Write key gives full access. Only share with trusted users!",
            IsOpen = true,
            IsClosable = false
        });

        var roPanel = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        var roBox = new TextBox
        {
            Header = "Read-Only Key",
            Text = result.ReadOnlyKey,
            IsReadOnly = true,
            Width = 300
        };
        roPanel.Children.Add(roBox);
        var roCopy = new Button { Content = "Copy" };
        roCopy.Click += (s, e) => CopyToClipboard(result.ReadOnlyKey);
        roPanel.Children.Add(roCopy);
        panel.Children.Add(roPanel);

        dialog.Content = panel;
        await dialog.ShowAsync();
    }

    private async void AddShare_Click(object sender, RoutedEventArgs e)
    {
        var dialog = new ContentDialog
        {
            Title = "Add Existing Share",
            PrimaryButtonText = "Add",
            CloseButtonText = "Cancel",
            XamlRoot = this.XamlRoot
        };

        var panel = new StackPanel { Spacing = 12 };

        var keyBox = new TextBox
        {
            Header = "Share Key",
            PlaceholderText = "Paste the share key here"
        };
        panel.Children.Add(keyBox);

        var pathBox = new TextBox
        {
            Header = "Save Location",
            PlaceholderText = @"C:\path\to\save"
        };
        panel.Children.Add(pathBox);

        var browseButton = new Button { Content = "Browse..." };
        browseButton.Click += async (s, args) =>
        {
            var picker = new Windows.Storage.Pickers.FolderPicker();
            picker.SuggestedStartLocation = Windows.Storage.Pickers.PickerLocationId.DocumentsLibrary;
            picker.FileTypeFilter.Add("*");

            var hwnd = WinRT.Interop.WindowNative.GetWindowHandle(App.MainWindow);
            WinRT.Interop.InitializeWithWindow.Initialize(picker, hwnd);

            var folder = await picker.PickSingleFolderAsync();
            if (folder != null)
            {
                pathBox.Text = folder.Path;
            }
        };
        panel.Children.Add(browseButton);

        var warningBar = new InfoBar
        {
            Severity = InfoBarSeverity.Warning,
            IsOpen = false
        };
        panel.Children.Add(warningBar);

        keyBox.TextChanged += (s, args) =>
        {
            if (keyBox.Text.StartsWith("SEEDRW", StringComparison.OrdinalIgnoreCase))
            {
                warningBar.Title = "Read/Write Key Detected";
                warningBar.Message = "Any changes you make will affect all users of this share!";
                warningBar.IsOpen = true;
            }
            else
            {
                warningBar.IsOpen = false;
            }
        };

        dialog.Content = panel;

        var result = await dialog.ShowAsync();
        if (result == ContentDialogResult.Primary &&
            !string.IsNullOrWhiteSpace(keyBox.Text) &&
            !string.IsNullOrWhiteSpace(pathBox.Text))
        {
            await AddShareAsync(keyBox.Text, pathBox.Text);
        }
    }

    private async Task AddShareAsync(string key, string path)
    {
        try
        {
            var request = new { Key = key, LocalPath = path };
            var response = await _client.PostAsJsonAsync("/api/shares/add", request);

            if (response.IsSuccessStatusCode)
            {
                await LoadSharesAsync();
            }
            else
            {
                var error = await response.Content.ReadAsStringAsync();
                ShowError($"Failed to add share: {error}");
            }
        }
        catch (Exception ex)
        {
            ShowError($"Error: {ex.Message}");
        }
    }

    private void CopyKey_Click(object sender, RoutedEventArgs e)
    {
        if (sender is Button button && button.Tag is string key)
        {
            CopyToClipboard(key);
        }
    }

    private async void RemoveShare_Click(object sender, RoutedEventArgs e)
    {
        if (sender is Button button && button.Tag is string shareId)
        {
            var dialog = new ContentDialog
            {
                Title = "Remove Share?",
                Content = "This will stop syncing this folder. Your files will not be deleted.",
                PrimaryButtonText = "Remove",
                CloseButtonText = "Cancel",
                XamlRoot = this.XamlRoot
            };

            var result = await dialog.ShowAsync();
            if (result == ContentDialogResult.Primary)
            {
                await RemoveShareAsync(shareId);
            }
        }
    }

    private async Task RemoveShareAsync(string shareId)
    {
        try
        {
            var response = await _client.DeleteAsync($"/api/shares/{shareId}");
            if (response.IsSuccessStatusCode)
            {
                await LoadSharesAsync();
            }
            else
            {
                ShowError("Failed to remove share.");
            }
        }
        catch (Exception ex)
        {
            ShowError($"Error: {ex.Message}");
        }
    }

    private void CopyToClipboard(string text)
    {
        var package = new DataPackage();
        package.SetText(text);
        Clipboard.SetContent(package);
    }

    private void ShowError(string message)
    {
        ErrorBar.Message = message;
        ErrorBar.IsOpen = true;
    }
}

// View model for shares
public class ShareViewModel
{
    public string Id { get; }
    public string Name { get; }
    public string LocalPath { get; }
    public string Status { get; }
    public int ConnectedPeers { get; }
    public string? ReadOnlyKey { get; }
    public Visibility IsReadWrite { get; }
    public Visibility IsReadOnly { get; }

    public ShareViewModel(ShareDto dto)
    {
        Id = dto.Id;
        Name = dto.Name;
        LocalPath = dto.LocalPath;
        Status = dto.Status;
        ConnectedPeers = dto.ConnectedPeers;
        ReadOnlyKey = dto.ReadOnlyKey;
        IsReadWrite = dto.AccessLevel == "ReadWrite" ? Visibility.Visible : Visibility.Collapsed;
        IsReadOnly = dto.AccessLevel == "ReadOnly" ? Visibility.Visible : Visibility.Collapsed;
    }
}

// DTOs
public class ShareDto
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

public class CreateShareResult
{
    public string ShareId { get; set; } = "";
    public string ReadWriteKey { get; set; } = "";
    public string ReadOnlyKey { get; set; } = "";
}
