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

    // Opt the process into dark mode so native popups (the tray's context menu,
    // a TrackPopupMenu) honor the app's color scheme. muda only dark-themes menu
    // *bars*, not popups, so its set_theme has no effect on the tray menu — the
    // popup follows the process app-mode set here.
    #[cfg(windows)]
    set_preferred_app_mode(adw::StyleManager::default().is_dark());

    let open = MenuItem::new("Open Seed Sync", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    let _ = menu.append(&open);
    let _ = menu.append(&quit);
    let open_id = open.id().clone();
    let quit_id = quit.id().clone();

    let icon = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Seed Sync")
        .build()
    {
        Ok(icon) => icon,
        Err(e) => {
            tracing::warn!("tray unavailable: {e}");
            return;
        }
    };

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

/// Opt this process into Windows dark mode via the undocumented
/// `uxtheme.dll#135` (`SetPreferredAppMode`) so native popups — notably the tray
/// context menu (a `TrackPopupMenu`) — render to match the app: `ForceDark` when
/// the app is dark, `Default` (follow the OS) otherwise. GTK draws its own
/// windows, so it never sets this, which is why the tray menu was stuck light.
#[cfg(windows)]
fn set_preferred_app_mode(dark: bool) {
    use std::os::raw::c_void;
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *const c_void;
    }
    // PreferredAppMode: Default=0, AllowDark=1, ForceDark=2, ForceLight=3.
    let mode: i32 = if dark { 2 } else { 0 };
    let dll: Vec<u16> = "uxtheme.dll"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let lib = LoadLibraryW(dll.as_ptr());
        if lib.is_null() {
            return;
        }
        // Ordinal 135 (MAKEINTRESOURCEA: the value's high word must be zero).
        let proc = GetProcAddress(lib, 135 as *const u8);
        if proc.is_null() {
            return;
        }
        let set_preferred_app_mode: unsafe extern "system" fn(i32) -> i32 =
            std::mem::transmute(proc);
        set_preferred_app_mode(mode);
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn install(_app: &adw::Application, _window: &adw::ApplicationWindow) {
    tracing::info!("tray not enabled on this platform build (Linux: planned via ksni)");
}
