using System.Runtime.InteropServices;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Navigation;
using Microsoft.UI.Xaml;
using H.NotifyIcon;
using SeedSync.App.Views;
using WinRT.Interop;
using Windows.Graphics;

namespace SeedSync.App;

// Dark mode support for native Win32 menus
internal static class DarkModeHelper
{
    private enum PreferredAppMode { Default = 0, AllowDark = 1, ForceDark = 2, ForceLight = 3 }

    [DllImport("uxtheme.dll", EntryPoint = "#135", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern int SetPreferredAppMode(PreferredAppMode mode);

    [DllImport("uxtheme.dll", EntryPoint = "#136", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern void FlushMenuThemes();

    public static void EnableDarkModeForApp()
    {
        try
        {
            // Allow dark mode based on system settings (AllowDark = 1)
            // Use ForceDark = 2 to always force dark mode
            SetPreferredAppMode(PreferredAppMode.AllowDark);
            FlushMenuThemes();
        }
        catch
        {
            // Ignore - these are undocumented APIs that may not exist on older Windows
        }
    }
}

/// <summary>
/// S.E.E.D. WinUI 3 Application with system tray support.
/// </summary>
public partial class App : Application
{
    private Window? _window;
    private TaskbarIcon? _trayIcon;
    private AppWindow? _appWindow;
    private bool _isExiting;

    /// <summary>
    /// Gets the main window.
    /// </summary>
    public static Window? MainWindow { get; private set; }

    /// <summary>
    /// Initializes the singleton application object.
    /// </summary>
    public App()
    {
        this.InitializeComponent();
        
        // Enable dark mode for native Win32 menus (like tray context menu)
        DarkModeHelper.EnableDarkModeForApp();
    }

    /// <summary>
    /// Invoked when the application is launched.
    /// </summary>
    protected override void OnLaunched(LaunchActivatedEventArgs e)
    {
        _window = new Window
        {
            Title = "S.E.E.D. - Secure Environment Exchange Daemon"
        };
        MainWindow = _window;

        // Modern decorations: extend content into title bar, Mica backdrop
        _window.ExtendsContentIntoTitleBar = true;
        _window.SystemBackdrop = new MicaBackdrop();

        // Get AppWindow so we can hide/show instead of close
        var hwnd = WindowNative.GetWindowHandle(_window);
        var windowId = Microsoft.UI.Win32Interop.GetWindowIdFromWindow(hwnd);
        _appWindow = AppWindow.GetFromWindowId(windowId);

        // Fixed size 640x480 (DPI-aware) and disable maximize
        if (_appWindow.Presenter is OverlappedPresenter overlapped)
        {
            overlapped.IsResizable = false;
            overlapped.IsMaximizable = false;
        }

        // When user clicks X: cancel close and hide to tray
        _appWindow.Closing += (sender, args) =>
        {
            if (_isExiting)
                return;
            args.Cancel = true;
            _appWindow?.Hide();
        };

        // Set up the frame and navigate to main page
        var rootFrame = new Frame();
        rootFrame.NavigationFailed += OnNavigationFailed;
        rootFrame.RequestedTheme = ElementTheme.Default; // follow system (dark/light) for tray menu and UI
        _window.Content = rootFrame;

        _ = rootFrame.Navigate(typeof(MainPage), e.Arguments);

        // Create tray icon
        SetupTrayIcon();

        _window.Activate();
        ResizeWindowDpiAware(hwnd, 640, 480);
    }

    [DllImport("user32.dll", ExactSpelling = true)]
    private static extern int GetDpiForWindow(IntPtr hwnd);

    private void ResizeWindowDpiAware(IntPtr hwnd, int widthLogical, int heightLogical)
    {
        if (_appWindow == null) return;
        int dpi = GetDpiForWindow(hwnd);
        if (dpi <= 0) dpi = 96;
        int widthPx = widthLogical * dpi / 96;
        int heightPx = heightLogical * dpi / 96;
        _appWindow.Resize(new SizeInt32(widthPx, heightPx));
    }

    private void SetupTrayIcon()
    {
        _trayIcon = new TaskbarIcon
        {
            ToolTipText = "S.E.E.D. Sync",
            // Default PopupMenu mode - native Win32 menu, should follow system theme on Win11
            ContextMenuMode = ContextMenuMode.PopupMenu
        };

        var contextMenu = new MenuFlyout();

        var openItem = new MenuFlyoutItem 
        { 
            Text = "Open S.E.E.D.",
            Command = new RelayCommand(ShowWindow)
        };
        contextMenu.Items.Add(openItem);

        contextMenu.Items.Add(new MenuFlyoutSeparator());

        var exitItem = new MenuFlyoutItem 
        { 
            Text = "Quit",
            Command = new RelayCommand(ExitApplication)
        };
        contextMenu.Items.Add(exitItem);

        _trayIcon.ContextFlyout = contextMenu;
        _trayIcon.DoubleClickCommand = new RelayCommand(ShowWindow);

        _trayIcon.ForceCreate();
    }

    private void ShowWindow()
    {
        if (_appWindow != null)
        {
            _appWindow.Show();
            _window?.Activate();
        }
    }

    private void ExitApplication()
    {
        _isExiting = true;
        _trayIcon?.Dispose();
        
        // Try graceful exit, then force if needed
        try
        {
            _window?.Close();
        }
        catch { /* ignore */ }

        // Force terminate - Environment.Exit doesn't always work from tray context
        System.Diagnostics.Process.GetCurrentProcess().Kill();
    }

    void OnNavigationFailed(object sender, NavigationFailedEventArgs e)
    {
        throw new Exception("Failed to load Page " + e.SourcePageType.FullName);
    }
}
