//! System tray integration.
//!
//! On Windows/macOS we use the `tray-icon` crate. On Linux its backend pulls in
//! GTK3 + libappindicator, which conflicts with this GTK4 application, so the
//! Linux tray is intentionally a no-op for now — to be revisited with a pure
//! StatusNotifier (`ksni`) implementation run on its own event loop.

#[cfg(any(windows, target_os = "macos"))]
pub fn install(_app: &adw::Application) {
    use tray_icon::menu::{Menu, MenuItem};
    use tray_icon::TrayIconBuilder;

    let menu = Menu::new();
    let _ = menu.append(&MenuItem::new("Show Seed Sync", true, None));
    let _ = menu.append(&MenuItem::new("Quit", true, None));

    match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Seed Sync")
        .build()
    {
        Ok(icon) => {
            // Keep the icon alive for the process lifetime.
            Box::leak(Box::new(icon));
            tracing::info!("system tray installed");
        }
        Err(e) => tracing::warn!("tray unavailable: {e}"),
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn install(_app: &adw::Application) {
    tracing::info!("tray not enabled on this platform build (Linux: planned via ksni)");
}
