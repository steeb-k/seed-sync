//! System tray integration.
//!
//! On Windows/macOS we use the `tray-icon` crate. On Linux its backend pulls in
//! GTK3 + libappindicator, which conflicts with this GTK4 application, so the
//! Linux tray is intentionally a no-op for now — to be revisited with a pure
//! StatusNotifier (`ksni`) implementation run on its own event loop.
//!
//! The tray lives for the whole process, and closing the window hides it rather
//! than quitting (see `main`), so the icon persists in the background. Double-
//! clicking it (or the "Open" menu item) re-shows the window; "Quit" exits.

#[cfg(any(windows, target_os = "macos"))]
pub fn install(app: &adw::Application, window: &adw::ApplicationWindow) {
    use adw::prelude::*;
    use gtk::glib;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{TrayIconBuilder, TrayIconEvent};

    let open = MenuItem::new("Open Seed Sync", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    let _ = menu.append(&open);
    let _ = menu.append(&quit);
    let open_id = open.id().clone();
    let quit_id = quit.id().clone();

    let icon = match TrayIconBuilder::new()
        .with_menu(Box::new(menu.clone()))
        .with_tooltip("Seed Sync")
        .build()
    {
        Ok(icon) => icon,
        Err(e) => {
            tracing::warn!("tray unavailable: {e}");
            return;
        }
    };

    // Match the tray context menu to the app's color scheme. Without this the
    // popup renders in the OS-default theme via muda's "Auto" (which can't detect
    // dark for the tray's hidden window), so it shows light even in dark mode.
    #[cfg(windows)]
    {
        let theme = if adw::StyleManager::default().is_dark() {
            tray_icon::menu::MenuTheme::Dark
        } else {
            tray_icon::menu::MenuTheme::Light
        };
        let hwnd = icon.window_handle();
        // SAFETY: `hwnd` is the live tray window from the icon we just built.
        unsafe {
            let _ = menu.set_theme_for_hwnd(hwnd as isize, theme);
        }
    }

    // Keep the icon alive for the process lifetime.
    Box::leak(Box::new(icon));
    tracing::info!("system tray installed");

    // tray-icon delivers clicks and menu picks on global channels; poll them on
    // the GTK main loop (where we can touch the window/app). 250 ms is plenty for
    // a tray and keeps the idle cost negligible.
    let app = app.clone();
    let window = window.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
        let mut open_window = false;
        let mut quit_app = false;
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if matches!(ev, TrayIconEvent::DoubleClick { .. }) {
                open_window = true;
            }
        }
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == open_id {
                open_window = true;
            } else if ev.id == quit_id {
                quit_app = true;
            }
        }
        if open_window {
            window.set_visible(true);
            window.present();
        }
        if quit_app {
            app.quit();
        }
        glib::ControlFlow::Continue
    });
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn install(_app: &adw::Application, _window: &adw::ApplicationWindow) {
    tracing::info!("tray not enabled on this platform build (Linux: planned via ksni)");
}
