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

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use std::sync::Arc;

/// Everything the tray backends need from the GTK side. Bundled so the
/// platform-specific `install`s share one signature.
pub struct TrayWiring {
    /// A pause/resume menu click, delivered back to the GTK loop (where `net`
    /// lives to issue the IPC).
    pub pause_tx: async_channel::Sender<()>,
    /// Current global pause state, written by the GTK side; drives the menu label.
    pub paused: Arc<AtomicBool>,
    /// Fires when the tray should re-render — a pause flip, fresh throughput, or a
    /// change in `stranded`. Used by the ksni backend (the `tray-icon` backend polls
    /// instead).
    pub refresh_rx: async_channel::Receiver<()>,
    /// Latest throughput in bytes/sec as `(down, up)`, shown in the tooltip.
    pub speeds: Arc<(AtomicU64, AtomicU64)>,
    /// How many shares can currently reach no member at all
    /// ([`seed_ipc::ShareStatus::NoPeers`]). Surfaced in the tooltip because the
    /// window is usually closed — this app lives in the tray, so a partition that is
    /// only visible once you open the window is a partition nobody notices, which is
    /// how known-issues #16 went unseen for a week.
    pub stranded: Arc<AtomicUsize>,
}

/// The tray tooltip: the live down/up rate line (mirroring the window's bottom
/// status bar, e.g. "↓ 1.2 Mbps   ↑ 0.3 Mbps"), plus a warning line whenever some
/// share can reach nobody.
fn tooltip(speeds: &(AtomicU64, AtomicU64), stranded: &AtomicUsize) -> String {
    use std::sync::atomic::Ordering;
    let down = speeds.0.load(Ordering::Relaxed);
    let up = speeds.1.load(Ordering::Relaxed);
    let rate = format!("↓ {}   ↑ {}", crate::fmt_speed(down), crate::fmt_speed(up));
    match stranded.load(Ordering::Relaxed) {
        0 => rate,
        1 => format!("{rate}\n⚠ 1 share has no members reachable"),
        n => format!("{rate}\n⚠ {n} shares have no members reachable"),
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub fn install(app: &adw::Application, window: &adw::ApplicationWindow, wiring: TrayWiring) {
    use adw::prelude::*;
    use gtk::glib;
    use std::sync::atomic::Ordering;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{TrayIconBuilder, TrayIconEvent};

    // The poll loop re-reads `paused`/`speeds` each tick, so the ksni re-render
    // signal isn't needed on this backend.
    let TrayWiring {
        pause_tx,
        paused,
        refresh_rx: _refresh_rx,
        speeds,
        stranded,
    } = wiring;

    // Opt the process into dark mode so native popups (the tray's context menu,
    // a TrackPopupMenu) honor the app's color scheme. muda only dark-themes menu
    // *bars*, not popups, so its set_theme has no effect on the tray menu — the
    // popup follows the process app-mode set here.
    #[cfg(windows)]
    set_preferred_app_mode(adw::StyleManager::default().is_dark());

    let open = MenuItem::new("Open S.E.E.D.", true, None);
    // Label reflects the current state; refreshed in the poll loop below.
    let pause = MenuItem::new(
        if paused.load(Ordering::Relaxed) {
            "Resume syncing"
        } else {
            "Pause syncing"
        },
        true,
        None,
    );
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    let _ = menu.append(&open);
    let _ = menu.append(&pause);
    let _ = menu.append(&quit);
    let open_id = open.id().clone();
    let pause_id = pause.id().clone();
    let quit_id = quit.id().clone();

    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("S.E.E.D.");
    // The glyph ships in two authored variants — a standard one tuned for a dark
    // panel and a "light" one (with dark rings) tuned for a light panel. Pick the
    // one matching the current panel background (`tray_bg_is_light` reads the
    // Windows taskbar theme / the macOS system appearance) and keep it in step in
    // the poll loop below. Note we no longer hand macOS a *template* image: the
    // art is colored now, and template mode keeps only the alpha (recoloring it to
    // the menu-bar tint), which would throw the color away.
    match load_tray_icon(tray_bg_is_light()) {
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

    tracing::info!("system tray installed");

    // tray-icon delivers clicks and menu picks on global channels; poll them on
    // the GTK main loop (where we can touch the window/app). 250 ms is plenty for
    // a tray and keeps the idle cost negligible. The `icon` is moved into this
    // closure (rather than leaked) so it stays alive *and* its tooltip can be
    // updated with the live throughput.
    let app = app.clone();
    let window = window.clone();
    let mut last_paused = paused.load(Ordering::Relaxed);
    let mut last_speeds = (u64::MAX, u64::MAX, usize::MAX);
    // Track the panel background theme so the glyph variant can be re-picked when
    // the user switches light/dark (taskbar on Windows, system appearance on
    // macOS) while the app is running.
    let mut last_light = tray_bg_is_light();
    glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
        let now_light = tray_bg_is_light();
        if now_light != last_light {
            if let Some(new_icon) = load_tray_icon(now_light) {
                let _ = icon.set_icon(Some(new_icon));
            }
            last_light = now_light;
        }
        // Keep the menu label in step with the (externally driven) pause state.
        let now_paused = paused.load(Ordering::Relaxed);
        if now_paused != last_paused {
            pause.set_text(if now_paused {
                "Resume syncing"
            } else {
                "Pause syncing"
            });
            last_paused = now_paused;
        }
        // Refresh the tooltip when the throughput — or the count of shares that can
        // reach nobody — changes.
        let now_speeds = (
            speeds.0.load(Ordering::Relaxed),
            speeds.1.load(Ordering::Relaxed),
            stranded.load(Ordering::Relaxed),
        );
        if now_speeds != last_speeds {
            let _ = icon.set_tooltip(Some(format!("S.E.E.D.\n{}", tooltip(&speeds, &stranded))));
            last_speeds = now_speeds;
        }
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
            } else if ev.id == pause_id {
                let _ = pause_tx.try_send(());
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

/// Whether the tray/panel background the icon sits on is *light*, which selects
/// the light (dark-ringed) glyph variant over the standard one. Windows reads the
/// taskbar theme; macOS follows the system appearance (the menu bar is dark in
/// Dark Mode). Re-evaluated in the poll loop so a live theme switch re-picks the
/// glyph.
#[cfg(windows)]
fn tray_bg_is_light() -> bool {
    taskbar_is_light()
}
#[cfg(target_os = "macos")]
fn tray_bg_is_light() -> bool {
    use adw::prelude::*;
    !adw::StyleManager::default().is_dark()
}

/// Decode the embedded tray PNG into an RGBA tray icon for the `tray-icon`
/// backend (Windows/macOS). Returns `None` if decoding fails, in which case the
/// tray falls back to the system default icon. Mirrors the Linux ksni
/// `load_icons` decode, but emits RGBA (R,G,B,A) as `tray-icon` expects.
///
/// Two authored variants are embedded: the standard glyph (tuned for a dark
/// panel) and the light glyph (with dark rings, tuned for a light panel). Pick by
/// `light_bg` — no runtime recoloring, since each variant is hand-drawn for its
/// background.
#[cfg(any(windows, target_os = "macos"))]
fn load_tray_icon(light_bg: bool) -> Option<tray_icon::Icon> {
    use gtk::gdk_pixbuf::{InterpType, Pixbuf};
    const DARK: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../icon/appTrayDark.png"
    ));
    const LIGHT: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../icon/appTrayLight.png"
    ));
    let png = if light_bg { LIGHT } else { DARK };
    let src = Pixbuf::from_read(std::io::Cursor::new(png)).ok()?;
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

/// Whether the Windows taskbar/notification area is using the *light* theme
/// (`SystemUsesLightTheme` = 1), against which the white tray glyph would be
/// nearly invisible. Note this is the system/shell theme, distinct from the
/// per-app theme (`AppsUseLightTheme`). Missing key ⇒ treat as dark (the
/// default), i.e. keep the white glyph.
#[cfg(windows)]
fn taskbar_is_light() -> bool {
    use std::os::raw::c_void;
    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(
            hkey: *mut c_void,
            subkey: *const u16,
            value: *const u16,
            flags: u32,
            ty: *mut u32,
            data: *mut c_void,
            data_len: *mut u32,
        ) -> i32;
    }
    const HKEY_CURRENT_USER: *mut c_void = 0x8000_0001u32 as usize as *mut c_void;
    const RRF_RT_REG_DWORD: u32 = 0x0000_0018; // restrict to DWORD values
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let subkey = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let value = wide("SystemUsesLightTheme");
    let mut data: u32 = 0;
    let mut len: u32 = 4;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut c_void,
            &mut len,
        )
    };
    rc == 0 && data == 1
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
        /// Current global pause state, written by the GTK side; drives the
        /// Pause/Resume menu label.
        paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
        /// A pause/resume menu click, bridged straight to the GTK loop.
        pause_tx: async_channel::Sender<()>,
        /// Live throughput `(down, up)` in bytes/sec, shown in the tooltip.
        speeds: std::sync::Arc<(std::sync::atomic::AtomicU64, std::sync::atomic::AtomicU64)>,
        /// Shares that can reach no member at all; warned about in the tooltip.
        stranded: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ksni::Tray for SeedTray {
        fn id(&self) -> String {
            "io.github.steeb_k.SeedSync".into()
        }
        fn title(&self) -> String {
            "S.E.E.D.".into()
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
                title: "S.E.E.D.".into(),
                description: super::tooltip(&self.speeds, &self.stranded),
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
            use std::sync::atomic::Ordering;
            let pause_label = if self.paused.load(Ordering::Relaxed) {
                "Resume syncing"
            } else {
                "Pause syncing"
            };
            vec![
                StandardItem {
                    label: "Open S.E.E.D.".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.try_send(TrayCmd::Open);
                    }),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: pause_label.into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.pause_tx.try_send(());
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
    ///
    /// The standard (dark-panel) glyph is used unconditionally here: a
    /// StatusNotifier host's panel color isn't reliably discoverable across
    /// desktops (and most panels are dark), so — unlike Windows/macOS — Linux
    /// doesn't switch to the light-panel variant.
    fn load_icons() -> Vec<ksni::Icon> {
        use gtk::gdk_pixbuf::{InterpType, Pixbuf};
        const PNG: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../icon/appTrayDark.png"
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

    pub fn install(
        app: &adw::Application,
        window: &adw::ApplicationWindow,
        wiring: super::TrayWiring,
    ) {
        let super::TrayWiring {
            pause_tx,
            paused,
            refresh_rx,
            speeds,
            stranded,
        } = wiring;
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
        let tray = SeedTray {
            icons,
            tx,
            paused,
            pause_tx,
            speeds,
            stranded,
        };
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
                        Ok(handle) => {
                            tracing::info!("system tray installed (ksni/StatusNotifier)");
                            // Re-render (menu label + tooltip) whenever the GTK side
                            // signals a change — a pause flip or fresh throughput.
                            while refresh_rx.recv().await.is_ok() {
                                let _ = handle.update(|_| {}).await;
                            }
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
pub fn install(_app: &adw::Application, _window: &adw::ApplicationWindow, _wiring: TrayWiring) {
    tracing::info!("tray not enabled on this platform build");
}
