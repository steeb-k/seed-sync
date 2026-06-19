//! System tray integration.
//!
//! On Windows/macOS we use the `tray-icon` crate. On Linux that crate's backend
//! pulls in GTK3 + libappindicator, which clashes with this GTK4 application, so
//! Linux instead uses a pure-Rust StatusNotifier implementation (`ksni`, no GTK3)
//! driven by its own tokio runtime on a dedicated thread; tray events are bridged
//! back to the GTK main loop over an `async-channel`.
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

    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Seed Sync");
    match load_tray_icon() {
        Some(icon) => builder = builder.with_icon(icon),
        None => tracing::warn!("tray icon failed to decode; using system default"),
    }
    let icon = match builder.build() {
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

/// Decode the embedded app PNG into an RGBA tray icon for the `tray-icon`
/// backend (Windows/macOS). Returns `None` if decoding fails, in which case the
/// tray falls back to the system default icon. Mirrors the Linux ksni
/// `load_icons` decode, but emits RGBA (R,G,B,A) as `tray-icon` expects.
#[cfg(any(windows, target_os = "macos"))]
fn load_tray_icon() -> Option<tray_icon::Icon> {
    use gtk::gdk_pixbuf::{InterpType, Pixbuf};
    // High-visibility variant, chosen specifically for the small tray size.
    const PNG: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../icon/appIconHiVis.png"
    ));
    let src = Pixbuf::from_read(std::io::Cursor::new(PNG)).ok()?;
    let pb = src.scale_simple(32, 32, InterpType::Bilinear)?;
    let pb = if pb.has_alpha() {
        pb
    } else {
        pb.add_alpha(false, 0, 0, 0).ok()?
    };
    let (w, h) = (pb.width(), pb.height());
    let rowstride = pb.rowstride() as usize;
    let nch = pb.n_channels() as usize;
    let bytes = pb.read_pixel_bytes();
    let bytes = bytes.as_ref();
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h as usize {
        let row = &bytes[y * rowstride..y * rowstride + w as usize * nch];
        for px in row.chunks_exact(nch) {
            let a = if nch == 4 { px[3] } else { 255 };
            rgba.extend_from_slice(&[px[0], px[1], px[2], a]);
        }
    }
    tray_icon::Icon::from_rgba(rgba, w as u32, h as u32).ok()
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

// ----------------------------------------------------------------------------
// Linux: StatusNotifier tray via `ksni` (no GTK3/appindicator dependency).
// ----------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod linux {
    use adw::prelude::*;
    use gtk::glib;

    /// What a tray interaction asks the GTK main loop to do.
    enum TrayCmd {
        Open,
        Quit,
    }

    /// The StatusNotifier item. Holds pre-rendered ARGB icons and a sender that
    /// hands interactions (run on the ksni thread) back to the GTK main loop.
    struct SeedTray {
        icons: Vec<ksni::Icon>,
        tx: async_channel::Sender<TrayCmd>,
    }

    impl ksni::Tray for SeedTray {
        fn id(&self) -> String {
            "io.github.steeb_k.SeedSync".into()
        }
        fn title(&self) -> String {
            "Seed Sync".into()
        }
        fn category(&self) -> ksni::Category {
            ksni::Category::ApplicationStatus
        }
        fn status(&self) -> ksni::Status {
            ksni::Status::Active
        }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            self.icons.clone()
        }
        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip {
                title: "Seed Sync".into(),
                description: String::new(),
                icon_name: String::new(),
                icon_pixmap: Vec::new(),
            }
        }
        // Left-click (activate) re-shows the window, matching the Windows tray.
        fn activate(&mut self, _x: i32, _y: i32) {
            let _ = self.tx.try_send(TrayCmd::Open);
        }
        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::StandardItem;
            vec![
                StandardItem {
                    label: "Open Seed Sync".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.try_send(TrayCmd::Open);
                    }),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Quit".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.try_send(TrayCmd::Quit);
                    }),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    /// Decode the embedded app PNG and render a few tray-sized ARGB32 icons
    /// (StatusNotifier wants pixmaps in network byte order: bytes are A,R,G,B).
    /// Returns empty if decoding fails, in which case the tray is skipped.
    fn load_icons() -> Vec<ksni::Icon> {
        use gtk::gdk_pixbuf::{InterpType, Pixbuf};
        const PNG: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../icon/appIcon.png"
        ));
        let Ok(src) = Pixbuf::from_read(std::io::Cursor::new(PNG)) else {
            return Vec::new();
        };
        [22, 32, 48, 64]
            .into_iter()
            .filter_map(|sz| {
                let pb = src.scale_simple(sz, sz, InterpType::Bilinear)?;
                let pb = if pb.has_alpha() {
                    pb
                } else {
                    pb.add_alpha(false, 0, 0, 0).ok()?
                };
                let (w, h) = (pb.width(), pb.height());
                let rowstride = pb.rowstride() as usize;
                let nch = pb.n_channels() as usize;
                let rgba = pb.read_pixel_bytes();
                let rgba = rgba.as_ref();
                let mut data = Vec::with_capacity((w * h * 4) as usize);
                for y in 0..h as usize {
                    let row = &rgba[y * rowstride..y * rowstride + w as usize * nch];
                    for px in row.chunks_exact(nch) {
                        let a = if nch == 4 { px[3] } else { 255 };
                        data.extend_from_slice(&[a, px[0], px[1], px[2]]);
                    }
                }
                Some(ksni::Icon {
                    width: w,
                    height: h,
                    data,
                })
            })
            .collect()
    }

    pub fn install(app: &adw::Application, window: &adw::ApplicationWindow) {
        let icons = load_icons();
        if icons.is_empty() {
            tracing::warn!("tray icon failed to decode; tray disabled");
            return;
        }
        let (tx, rx) = async_channel::unbounded::<TrayCmd>();

        // Bridge tray interactions (sent from the ksni thread) onto the GTK main
        // loop, where we can touch the window/app.
        let app = app.clone();
        let window = window.clone();
        glib::spawn_future_local(async move {
            while let Ok(cmd) = rx.recv().await {
                match cmd {
                    TrayCmd::Open => {
                        window.set_visible(true);
                        window.present();
                    }
                    TrayCmd::Quit => app.quit(),
                }
            }
        });

        // ksni's tokio service runs on its own thread; block_on keeps the runtime
        // (and the returned handle) alive for the process lifetime.
        let tray = SeedTray { icons, tx };
        let spawn = std::thread::Builder::new()
            .name("seed-tray".into())
            .spawn(move || {
                use ksni::TrayMethods;
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::warn!("tray runtime unavailable: {e}");
                        return;
                    }
                };
                rt.block_on(async move {
                    match tray.spawn().await {
                        Ok(_handle) => {
                            tracing::info!("system tray installed (ksni/StatusNotifier)");
                            std::future::pending::<()>().await;
                        }
                        Err(e) => tracing::warn!("tray unavailable: {e}"),
                    }
                });
            });
        if let Err(e) = spawn {
            tracing::warn!("could not start tray thread: {e}");
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::install;

// Other unixes (e.g. *BSD): no tray backend wired.
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub fn install(_app: &adw::Application, _window: &adw::ApplicationWindow) {
    tracing::info!("tray not enabled on this platform build");
}
