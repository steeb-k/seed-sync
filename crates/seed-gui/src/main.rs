//! SEED Sync GUI (GTK4 + Libadwaita): an unprivileged IPC client to the
//! `seed-daemon`. It shows the share list, drives the create/add/reveal/pause
//! flows, and reflects live status. All networking lives in the daemon; this
//! process only talks the IPC protocol.
//!
//! Threading: a Tokio runtime on a side thread owns the socket IO; results and
//! pushed events arrive on the GTK main thread via an `async-channel` consumed
//! by `glib::spawn_future_local`. GTK objects are only ever touched on the main
//! thread.

// Release builds are a GUI-subsystem app, so launching doesn't pop a console
// window — the first-run UI is clean. `--debug` allocates a console at runtime
// for the logs (see `main`). Debug builds keep the console for `cargo run`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod tray;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use seed_ipc::transport::{self, oneshot_request, read_frame, write_frame};
use seed_ipc::{
    Frame, IpcEvent, IpcRequest, IpcResponse, Message, PeerInfo, Role, ShareStatus, ShareSummary,
};
use tokio::runtime::Handle;

const APP_ID: &str = "io.github.steeb_k.SeedSync";

/// Messages from the IO side to the UI side.
enum UiMsg {
    Shares(Vec<ShareSummary>),
    /// A share finished being created. Keys are no longer shown automatically —
    /// the user reveals them on demand from the share's ⋮ menu.
    Created,
    Keys {
        master: Option<String>,
        viewer: String,
    },
    NodeAddr(String),
    Peers(Vec<PeerInfo>),
    /// This device's current display name (from the daemon), cached in the GUI.
    DeviceName(String),
    Throughput {
        down: u64,
        up: u64,
    },
    LastUpdated(i64),
    Refresh,
    /// The daemon could not be reached on the last request — the GUI shows the
    /// "Daemon Not Started" page instead of spamming error toasts.
    DaemonDown,
    Toast(String),
}

/// Send half plus the runtime handle and socket path — everything needed to
/// fire IPC requests off the GTK thread. Cheaply cloneable and `Send`.
#[derive(Clone)]
struct Net {
    handle: Handle,
    socket: PathBuf,
    tx: async_channel::Sender<UiMsg>,
}

impl Net {
    /// Run a request on the runtime and deliver a mapped [`UiMsg`] to the UI.
    fn send<F>(&self, req: IpcRequest, map: F)
    where
        F: FnOnce(anyhow::Result<IpcResponse>) -> Option<UiMsg> + Send + 'static,
    {
        let socket = self.socket.clone();
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            let res = oneshot_request(&socket, req).await.map_err(Into::into);
            if let Some(msg) = map(res) {
                let _ = tx.send(msg).await;
            }
        });
    }

    fn refresh(&self) {
        self.send(IpcRequest::ListShares, |res| match res {
            Ok(IpcResponse::Shares(s)) => Some(UiMsg::Shares(s)),
            Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(format!("list failed: {e}"))),
            // A connection failure means the daemon isn't running/reachable; the
            // UI surfaces a dedicated status page rather than a toast per refresh.
            Err(_) => Some(UiMsg::DaemonDown),
            _ => None,
        });
    }

    /// Maintain a long-lived subscription to daemon events (throughput, status,
    /// last-updated), reconnecting if the daemon restarts.
    fn subscribe_loop(&self) {
        let socket = self.socket.clone();
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            loop {
                let _ = stream_events(&socket, &tx).await;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
    }
}

/// Connect, subscribe, and forward events as [`UiMsg`]s until the connection drops.
async fn stream_events(
    socket: &std::path::Path,
    tx: &async_channel::Sender<UiMsg>,
) -> std::io::Result<()> {
    let stream = transport::connect(socket).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    write_frame(
        &mut writer,
        &Frame {
            id: 2,
            body: Message::Request(IpcRequest::Subscribe),
        },
    )
    .await?;
    while let Some(frame) = read_frame(&mut reader).await? {
        if let Message::Event(ev) = frame.body {
            let msg = match ev {
                IpcEvent::Throughput { down_bps, up_bps } => UiMsg::Throughput {
                    down: down_bps,
                    up: up_bps,
                },
                IpcEvent::LastUpdated { ts, .. } => UiMsg::LastUpdated(ts),
                IpcEvent::ShareListChanged
                | IpcEvent::Membership { .. }
                | IpcEvent::ShareStatus { .. } => UiMsg::Refresh,
            };
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    }
    Ok(())
}

fn default_socket() -> PathBuf {
    // Match the daemon's machine-wide socket on Windows so the unprivileged GUI
    // can reach a daemon that may be running as a LocalSystem service.
    #[cfg(windows)]
    {
        seed_ipc::machine_socket()
    }
    #[cfg(not(windows))]
    {
        directories::ProjectDirs::from("io.github", "steeb_k", "SeedSync")
            .map(|d| d.data_dir().join("seed.sock"))
            .unwrap_or_else(|| PathBuf::from(".seed-data/seed.sock"))
    }
}

/// On Windows, point GLib/GTK at the bundled runtime resources relative to this
/// exe, so a relocated install (Program Files, the portable tree, anywhere) finds
/// its GSettings schemas, pixbuf loaders, and icon themes. Without the schema dir
/// the file chooser aborts with "No GSettings schemas are installed on the system".
/// Must run before any GLib/GTK call. No-op on other platforms (system paths).
#[cfg(windows)]
fn setup_runtime_env() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // <prefix>\bin\seed-gui.exe -> prefix is the install root (holds share\, lib\).
    let Some(prefix) = exe.parent().and_then(|bin| bin.parent()) else {
        return;
    };
    let set_if = |var: &str, p: PathBuf| {
        if p.exists() && std::env::var_os(var).is_none() {
            std::env::set_var(var, &p);
        }
    };
    set_if(
        "GSETTINGS_SCHEMA_DIR",
        prefix.join(r"share\glib-2.0\schemas"),
    );
    set_if(
        "GDK_PIXBUF_MODULE_FILE",
        prefix.join(r"lib\gdk-pixbuf-2.0\2.10.0\loaders.cache"),
    );
    // Prepend our share\ so the icon theme is found.
    let share = prefix.join("share");
    if share.exists() {
        let val = match std::env::var_os("XDG_DATA_DIRS") {
            Some(cur) if !cur.is_empty() => {
                let mut s = std::ffi::OsString::from(&share);
                s.push(";");
                s.push(cur);
                s
            }
            _ => std::ffi::OsString::from(&share),
        };
        std::env::set_var("XDG_DATA_DIRS", val);
    }
}

/// On macOS, point GLib/GTK at the bundled runtime resources relative to this
/// executable, so the self-contained tarball install (GTK dylibs relocated to
/// `../lib`) finds its GSettings schemas, gdk-pixbuf loaders, and Adwaita icon
/// theme without a system/Homebrew GTK. Mirrors the Windows function. Every set
/// is guarded by `exists()`, so a dev build run against Homebrew GTK (no bundled
/// `share/`+`lib/` next to the exe) is a no-op and keeps using the system paths.
/// Must run before any GLib/GTK call.
#[cfg(target_os = "macos")]
fn setup_runtime_env() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // Installed binaries are reached through ~/.local/bin symlinks into the real
    // prefix, and current_exe() can hand back the symlink path — canonicalize so
    // the prefix resolves to the install root, not the symlink's parent. Without
    // this, the bundled share/lib/etc aren't found and GTK silently falls back to
    // a system/Homebrew prefix (absent on a user's machine → file-chooser crash).
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    // <prefix>/bin/seed-gui -> prefix is the install root (holds share/, lib/).
    let Some(prefix) = exe.parent().and_then(|bin| bin.parent()) else {
        return;
    };
    let set_if = |var: &str, p: PathBuf| {
        if p.exists() && std::env::var_os(var).is_none() {
            std::env::set_var(var, &p);
        }
    };
    set_if(
        "GSETTINGS_SCHEMA_DIR",
        prefix.join("share/glib-2.0/schemas"),
    );
    set_if(
        "GDK_PIXBUF_MODULE_FILE",
        prefix.join("lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"),
    );
    set_if(
        "GDK_PIXBUF_MODULEDIR",
        prefix.join("lib/gdk-pixbuf-2.0/2.10.0/loaders"),
    );
    // fontconfig (pulled in by pango): the bundled libfontconfig has a compiled-in
    // config path under the Homebrew prefix, absent on a user's machine. Point it
    // at our bundled fonts.conf, which references the system macOS font dirs.
    set_if("FONTCONFIG_PATH", prefix.join("etc/fonts"));
    // Prepend our share/ so the bundled Adwaita icon theme is found by GTK.
    let share = prefix.join("share");
    if share.exists() {
        let val = match std::env::var_os("XDG_DATA_DIRS") {
            Some(cur) if !cur.is_empty() => {
                let mut s = std::ffi::OsString::from(&share);
                s.push(":");
                s.push(cur);
                s
            }
            _ => {
                let mut s = std::ffi::OsString::from(&share);
                s.push(":/usr/local/share:/usr/share");
                s
            }
        };
        std::env::set_var("XDG_DATA_DIRS", val);
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn setup_runtime_env() {}

/// macOS: present the window when the user reopens the app (Dock click, Finder
/// double-click, `open`), which GTK4 doesn't surface as a GApplication `activate`.
/// We observe `NSApplicationDidBecomeActiveNotification` and poke the reveal
/// channel; the GTK-side consumer (see `build_ui`) calls `window.present()`. The
/// observer block runs on the main (Cocoa == GTK) thread; `try_send` is enough.
#[cfg(target_os = "macos")]
fn install_reopen_handler(show_tx: async_channel::Sender<()>) {
    use block2::RcBlock;
    use objc2_app_kit::NSApplicationDidBecomeActiveNotification;
    use objc2_foundation::{NSNotification, NSNotificationCenter};
    use std::ptr::NonNull;

    let block = RcBlock::new(move |_n: NonNull<NSNotification>| {
        let _ = show_tx.try_send(());
    });
    let token = unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(NSApplicationDidBecomeActiveNotification),
            None,
            None,
            &block,
        )
    };
    // The observer + block live for the whole process.
    std::mem::forget(token);
    std::mem::forget(block);
}

/// macOS: resign the app's "active" status when hiding the window to the tray.
/// Otherwise the app stays the frontmost app with no visible window, and a later
/// reopen (Dock click / `open`) is not an inactive→active transition, so the
/// NSApplicationDidBecomeActive observer never fires and the window can't return.
#[cfg(target_os = "macos")]
fn macos_resign_active() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    if let Some(mtm) = MainThreadMarker::new() {
        // hide: (not the deprecated/no-op deactivate) genuinely makes the app
        // inactive + offscreen; the tray NSStatusItem is unaffected. A reopen then
        // un-hides + re-activates, firing the DidBecomeActive observer.
        NSApplication::sharedApplication(mtm).hide(None);
    }
}
#[cfg(not(target_os = "macos"))]
fn macos_resign_active() {}

fn main() -> glib::ExitCode {
    setup_runtime_env();

    // `--debug` reveals the log console (release builds are windowed by default,
    // so the first-run UI is clean) and bumps the default log verbosity.
    let debug = std::env::args().any(|a| a == "--debug");
    #[cfg(windows)]
    if debug {
        attach_console();
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                if debug {
                    "seed_gui=debug,seed_ipc=debug".into()
                } else {
                    "seed_gui=info".into()
                }
            }),
        )
        .init();

    // Socket path: SEED_SOCKET env override, else the platform default.
    let socket = std::env::var_os("SEED_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(default_socket);

    // A leaked multi-thread runtime lives for the process; we only need a Handle.
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = rt.handle().clone();
    Box::leak(Box::new(rt));

    // The autostart entry launches us with `--hidden`: start in the tray with no
    // window shown.
    let hidden = std::env::args().any(|a| a == "--hidden");

    // Single-instance: a second launch should reveal the running window, not spin
    // up a second process (and a second tray icon). `show_rx` carries those
    // "reveal" requests to the live window.
    let (show_tx, show_rx) = async_channel::unbounded::<()>();
    #[cfg(windows)]
    if single_instance::already_running(&show_tx) {
        // Another instance owns the lock; we signaled it to show and now exit.
        return glib::ExitCode::SUCCESS;
    }
    // macOS: GTK4 doesn't deliver the Dock/Finder "reopen" of a running app as a
    // GApplication `activate`, so a double-click wouldn't re-show a tray-hidden
    // window. Observe NSApplication becoming active and fire the same reveal
    // channel a second launch uses, which presents the window on the GTK loop.
    #[cfg(target_os = "macos")]
    install_reopen_handler(show_tx.clone());

    // Keep a sender alive for the whole process so the reveal-channel stays open
    // (on Windows the watcher thread also holds one; on Linux it's otherwise
    // unused — GApplication's own D-Bus uniqueness handles a second launch).
    let _show_tx = show_tx;

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| {
        // Re-activation (Linux GApplication uniqueness, macOS Dock/reopen, or our
        // reveal-signal) presents the existing window instead of building a second
        // one. After a close-to-tray the window is hidden (set_visible(false)), so
        // explicitly un-hide before presenting or present() may not re-show it.
        tracing::debug!("activate fired (windows={})", app.windows().len());
        if let Some(win) = app.windows().first() {
            win.set_visible(true);
            win.present();
            return;
        }
        build_ui(app, handle.clone(), socket.clone(), hidden, show_rx.clone());
    });
    // We parse our own args (above); don't hand them to GApplication.
    app.run_with_args::<&str>(&[])
}

/// Give a GUI-subsystem (windowed) build a console for `--debug` log output:
/// attach to the launching terminal if there is one, else allocate a dedicated
/// console window. Runs before the tracing subscriber initializes so its first
/// write lands on the new console. No-op-ish if a console already exists.
#[cfg(windows)]
fn attach_console() {
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
        fn AllocConsole() -> i32;
    }
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            AllocConsole();
        }
    }
}

/// Windows single-instance guard. GApplication's uniqueness is D-Bus-based and a
/// no-op on Windows, so each launch is its own primary (and spawns its own tray).
/// We enforce one instance with a named mutex, and let a second launch poke the
/// first (via a named event) to reveal its window before exiting.
#[cfg(windows)]
mod single_instance {
    use std::ffi::c_void;

    const ERROR_ALREADY_EXISTS: u32 = 183;
    const EVENT_MODIFY_STATE: u32 = 0x0002;
    const INFINITE: u32 = 0xFFFF_FFFF;
    const WAIT_OBJECT_0: u32 = 0;

    // Session-local (per user session) so different logon sessions are independent.
    const MUTEX_NAME: &str = "Local\\com.seedsync.SeedSync.instance";
    const EVENT_NAME: &str = "Local\\com.seedsync.SeedSync.show";

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(attrs: *const c_void, owner: i32, name: *const u16) -> *mut c_void;
        fn CreateEventW(
            attrs: *const c_void,
            manual_reset: i32,
            initial: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
        fn SetEvent(handle: *mut c_void) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
        fn GetLastError() -> u32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Returns `true` if another instance already holds the lock — after signaling
    /// it to reveal its window. Otherwise claims the lock as the primary instance,
    /// spawns a thread that forwards reveal-signals onto `show_tx`, and returns
    /// `false`. The mutex/event handles are intentionally never closed: they live
    /// for the process.
    pub fn already_running(show_tx: &async_channel::Sender<()>) -> bool {
        let mutex_name = wide(MUTEX_NAME);
        let event_name = wide(EVENT_NAME);
        unsafe {
            let mutex = CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr());
            if !mutex.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
                // Secondary: poke the primary to show itself, then bow out.
                let ev = OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr());
                if !ev.is_null() {
                    SetEvent(ev);
                    CloseHandle(ev);
                }
                CloseHandle(mutex);
                return true;
            }

            // Primary: hold the mutex (never closed) and watch for reveal-signals.
            let event = CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr());
            if !event.is_null() {
                let event_addr = event as usize; // raw HANDLE isn't Send; move as usize
                let show_tx = show_tx.clone();
                std::thread::spawn(move || {
                    let event = event_addr as *mut c_void;
                    loop {
                        if WaitForSingleObject(event, INFINITE) != WAIT_OBJECT_0 {
                            break;
                        }
                        if show_tx.send_blocking(()).is_err() {
                            break;
                        }
                    }
                });
            }
            false
        }
    }
}

/// Install the app stylesheet: a base "frameless" look on every platform, plus a
/// Windows 11-leaning layer (Segoe UI, accent, rounding) on Windows.
fn load_css() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    #[allow(unused_mut)]
    let mut css = String::from(include_str!("style.css"));
    #[cfg(windows)]
    css.push_str(include_str!("windows.css"));
    provider.load_from_data(&css);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn build_ui(
    app: &adw::Application,
    handle: Handle,
    socket: PathBuf,
    hidden: bool,
    show_rx: async_channel::Receiver<()>,
) {
    load_css();
    let (tx, rx) = async_channel::unbounded::<UiMsg>();
    let net = Net { handle, socket, tx };
    // This device's display name, fetched from the daemon at startup and kept in
    // sync; used to prefill the create/add "Your name" field and the gear dialog.
    let device_name: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

    // State shared with the tray (which lives partly on other threads). The
    // message pump writes it from the daemon's summaries/throughput; the tray
    // reads it to label its menu item and fill its tooltip. `tray_refresh` nudges
    // the Linux tray to re-render when something changes; `tray_pause` carries a
    // tray click back to the GTK loop, where `net` is available to issue the IPC.
    let paused_state = Arc::new(AtomicBool::new(false));
    let tray_speeds = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
    let (tray_refresh_tx, tray_refresh_rx) = async_channel::unbounded::<()>();
    let (tray_pause_tx, tray_pause_rx) = async_channel::unbounded::<()>();

    // Toggle the global pause switch from its current known state, then refresh.
    let toggle_pause_all = {
        let net = net.clone();
        let paused_state = paused_state.clone();
        Rc::new(move || {
            let req = if paused_state.load(Ordering::Relaxed) {
                IpcRequest::ResumeAll
            } else {
                IpcRequest::PauseAll
            };
            net.send(req, |_| None);
            net.refresh();
        })
    };

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("S.E.E.D.")
        .default_width(580)
        .default_height(380)
        .build();

    // --- header bar ---
    let header = adw::HeaderBar::new();
    // Title + subtitle: "S.E.E.D." over "Secure Environment Exchange Daemon".
    header.set_title_widget(Some(&adw::WindowTitle::new(
        "S.E.E.D.",
        "Secure Environment Exchange Daemon",
    )));

    // "+" menu: create / add.
    let add_btn = gtk::MenuButton::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add a share")
        .build();
    let add_popover = gtk::Popover::new();
    let add_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let new_share_btn = flat_button("Create new share…");
    let add_share_btn = flat_button("Add existing share…");
    add_box.append(&new_share_btn);
    add_box.append(&add_share_btn);
    add_popover.set_child(Some(&add_box));
    add_btn.set_popover(Some(&add_popover));

    // Gear menu: node address + quit.
    let gear_btn = gtk::MenuButton::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Settings")
        .build();
    let gear_popover = gtk::Popover::new();
    let gear_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    // Pause/Resume all activity; its label tracks the global pause state, kept in
    // sync by the message pump below.
    let pause_all_btn = flat_button("Pause all syncing");
    let setname_btn = flat_button("Set device name…");
    let nodeaddr_btn = flat_button("Show this device's address…");
    let quit_btn = flat_button("Quit");
    gear_box.append(&pause_all_btn);
    gear_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    gear_box.append(&setname_btn);
    gear_box.append(&nodeaddr_btn);
    gear_box.append(&quit_btn);
    gear_popover.set_child(Some(&gear_box));
    gear_btn.set_popover(Some(&gear_popover));

    // Header actions: "+" (add) then gear (settings). Their side depends on where
    // the platform draws its window controls. On macOS the traffic-light controls
    // own the LEFT corner, so the actions go to the RIGHT to avoid crowding them.
    // On Windows the controls are on the right (with the close button's rounded
    // corner), so the actions stay on the LEFT; Linux follows the same layout.
    #[cfg(target_os = "macos")]
    {
        // pack_end packs right-to-left: pack gear first so it lands in the corner,
        // then "+" to its left — a [＋][gear] group tucked against the right edge.
        header.pack_end(&gear_btn);
        header.pack_end(&add_btn);
    }
    #[cfg(not(target_os = "macos"))]
    {
        header.pack_start(&add_btn);
        header.pack_start(&gear_btn);
    }

    // --- share list ---
    let listbox = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    // Empty state: the standard libadwaita status page. Its icon is a themed
    // symbolic from the Adwaita icon set bundled with GTK, so it recolors with
    // the light/dark theme automatically.
    let placeholder = adw::StatusPage::builder()
        .icon_name("folder-remote-symbolic")
        .title("No shares yet")
        .description("Use “+” to create or add one.")
        .css_classes(["empty-state"])
        .build();
    listbox.set_placeholder(Some(&placeholder));

    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&listbox)
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(12)
        .build();

    // --- bottom status bar ---
    let down_lbl = gtk::Label::new(Some("↓ 0.0 Mbps"));
    let up_lbl = gtk::Label::new(Some("↑ 0.0 Mbps"));
    let updated_lbl = gtk::Label::builder()
        .label("Last updated: —")
        .hexpand(true)
        .halign(gtk::Align::End)
        .css_classes(["dim-label"])
        .build();
    let status_bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(16)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    status_bar.append(&down_lbl);
    status_bar.append(&up_lbl);
    status_bar.append(&updated_lbl);

    // --- "Syncing Paused" status page (shown when every share is paused) ---
    let paused_page = adw::StatusPage::builder()
        .icon_name("media-playback-pause-symbolic")
        .title("Syncing Paused")
        .css_classes(["empty-state"])
        .build();
    let resume_btn = gtk::Button::builder()
        .label("Resume")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    paused_page.set_child(Some(&resume_btn));
    {
        let toggle = toggle_pause_all.clone();
        resume_btn.connect_clicked(move |_| toggle());
    }

    // --- "Daemon Not Started" status page (shown when the daemon is unreachable) ---
    // A plain error glyph, deliberately left in the default symbolic color (not
    // the destructive/red accent) so it reads as "needs attention", not "crashed".
    let daemon_page = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title("Daemon Not Started")
        .css_classes(["empty-state"])
        .build();
    let start_daemon_btn = gtk::Button::builder()
        .label("Start Daemon")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    daemon_page.set_child(Some(&start_daemon_btn));
    {
        let net = net.clone();
        start_daemon_btn.connect_clicked(move |_| {
            start_daemon();
            // Give the service a moment to come up, then re-query.
            let net = net.clone();
            glib::timeout_add_local_once(Duration::from_millis(1500), move || net.refresh());
        });
    }

    // --- view stack: the share list, or one of the two status pages ---
    let view_stack = gtk::Stack::new();
    view_stack.set_vexpand(true);
    view_stack.add_named(&scroller, Some("list"));
    view_stack.add_named(&paused_page, Some("paused"));
    view_stack.add_named(&daemon_page, Some("daemon"));
    view_stack.set_visible_child_name("list");

    // --- assemble ---
    let toast_overlay = adw::ToastOverlay::new();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&view_stack);
    content.append(&status_bar);
    toast_overlay.set_child(Some(&content));
    window.set_content(Some(&toast_overlay));

    // --- wire actions ---
    {
        let net = net.clone();
        let window = window.clone();
        let device_name = device_name.clone();
        let add_popover = add_popover.clone();
        new_share_btn.connect_clicked(move |_| {
            add_popover.popdown();
            create_share_flow(&window, &net, &device_name);
        });
    }
    {
        let net = net.clone();
        let window = window.clone();
        let device_name = device_name.clone();
        let add_popover = add_popover.clone();
        add_share_btn.connect_clicked(move |_| {
            add_popover.popdown();
            show_add_dialog(&window, &net, &device_name);
        });
    }
    {
        let net = net.clone();
        let window = window.clone();
        let device_name = device_name.clone();
        let pop = gear_popover.clone();
        setname_btn.connect_clicked(move |_| {
            pop.popdown();
            show_set_name_dialog(&window, &net, &device_name);
        });
    }
    {
        let net = net.clone();
        let pop = gear_popover.clone();
        nodeaddr_btn.connect_clicked(move |_| {
            pop.popdown();
            net.send(IpcRequest::NodeAddr, |res| match res {
                Ok(IpcResponse::NodeAddr(a)) => Some(UiMsg::NodeAddr(a)),
                _ => Some(UiMsg::Toast("could not get node address".into())),
            });
        });
    }
    {
        let toggle = toggle_pause_all.clone();
        let pop = gear_popover.clone();
        pause_all_btn.connect_clicked(move |_| {
            pop.popdown();
            toggle();
        });
    }
    {
        let app = app.clone();
        quit_btn.connect_clicked(move |_| app.quit());
    }
    // A tray "pause/resume" click is bridged onto the GTK loop here, where `net`
    // is available to issue the request.
    {
        let toggle = toggle_pause_all.clone();
        glib::spawn_future_local(async move {
            while tray_pause_rx.recv().await.is_ok() {
                toggle();
            }
        });
    }

    // --- UI message pump (runs on the GTK main context) ---
    {
        let toast_overlay = toast_overlay.clone();
        let window = window.clone();
        let net = net.clone();
        let down_lbl = down_lbl.clone();
        let up_lbl = up_lbl.clone();
        let updated_lbl = updated_lbl.clone();
        let device_name = device_name.clone();
        let view_stack = view_stack.clone();
        let pause_all_btn = pause_all_btn.clone();
        let paused_state = paused_state.clone();
        let tray_speeds = tray_speeds.clone();
        let rows: Rc<RefCell<HashMap<String, RowWidgets>>> = Rc::new(RefCell::new(HashMap::new()));
        glib::spawn_future_local(async move {
            // Which status page (if any) the main area should show. Recomputed on
            // every Shares/DaemonDown so the stack tracks daemon + pause state.
            // `daemon_up` is set by both arms before it's read; `all_paused`
            // persists across ticks so a DaemonDown keeps the last-known value.
            let mut daemon_up;
            let mut all_paused = false;
            // Last throughput pushed to the tray, so an idle stream of identical
            // samples doesn't re-render the tray every second.
            let mut last_tray_speeds = (u64::MAX, u64::MAX);
            let select_view = |stack: &gtk::Stack, daemon_up: bool, all_paused: bool| {
                stack.set_visible_child_name(if !daemon_up {
                    "daemon"
                } else if all_paused {
                    "paused"
                } else {
                    "list"
                });
            };
            while let Ok(msg) = rx.recv().await {
                match msg {
                    UiMsg::Shares(shares) => {
                        daemon_up = true;
                        if let Some(ts) = shares.iter().map(|s| s.last_updated).max() {
                            if ts > 0 {
                                updated_lbl.set_text(&format!("Last updated: {}", fmt_time(ts)));
                            }
                        }
                        update_list(&listbox, &shares, &net, &window, &rows);
                        // "All paused" drives both the status page and the tray/gear
                        // labels. Notify the tray only when it actually flips.
                        all_paused = !shares.is_empty() && shares.iter().all(|s| s.paused);
                        if paused_state.swap(all_paused, Ordering::Relaxed) != all_paused {
                            let _ = tray_refresh_tx.send(()).await;
                        }
                        if let Some(lbl) = pause_all_btn.child().and_downcast::<gtk::Label>() {
                            lbl.set_text(if all_paused {
                                "Resume all syncing"
                            } else {
                                "Pause all syncing"
                            });
                        }
                        select_view(&view_stack, daemon_up, all_paused);
                    }
                    UiMsg::DaemonDown => {
                        daemon_up = false;
                        select_view(&view_stack, daemon_up, all_paused);
                    }
                    UiMsg::Created => {
                        // Don't pop the keys dialog automatically — a large folder
                        // can index for a long time and this is jarring. The keys
                        // stay available on demand via each share's ⋮ menu.
                        toast_overlay.add_toast(adw::Toast::new(
                            "Share created — use its ⋮ menu to reveal the keys",
                        ));
                    }
                    UiMsg::Keys { master, viewer } => {
                        show_keys_dialog(&window, master.as_deref(), &viewer, None)
                    }
                    UiMsg::NodeAddr(a) => show_text_dialog(
                        &window,
                        "This device's address",
                        "Hand this to a peer as the bootstrap address when adding the share.",
                        &a,
                    ),
                    UiMsg::Peers(peers) => show_peers_dialog(&window, &peers),
                    UiMsg::DeviceName(name) => *device_name.borrow_mut() = name,
                    UiMsg::Throughput { down, up } => {
                        down_lbl.set_text(&format!("↓ {}", fmt_speed(down)));
                        up_lbl.set_text(&format!("↑ {}", fmt_speed(up)));
                        // Mirror into the tray tooltip and nudge it to re-render,
                        // but only when the rate actually changed.
                        if last_tray_speeds != (down, up) {
                            tray_speeds.0.store(down, Ordering::Relaxed);
                            tray_speeds.1.store(up, Ordering::Relaxed);
                            last_tray_speeds = (down, up);
                            let _ = tray_refresh_tx.send(()).await;
                        }
                    }
                    UiMsg::LastUpdated(ts) => {
                        updated_lbl.set_text(&format!("Last updated: {}", fmt_time(ts)));
                    }
                    UiMsg::Refresh => net.refresh(),
                    UiMsg::Toast(t) => toast_overlay.add_toast(adw::Toast::new(&t)),
                }
            }
        });
    }

    // --- live event subscription + periodic refresh fallback ---
    net.subscribe_loop();
    // Seed the device-name cache once at startup.
    net.send(IpcRequest::GetDeviceName, |res| match res {
        Ok(IpcResponse::DeviceName(n)) => Some(UiMsg::DeviceName(n)),
        _ => None,
    });
    {
        let net = net.clone();
        net.refresh();
        glib::timeout_add_local(Duration::from_millis(2000), move || {
            net.refresh();
            glib::ControlFlow::Continue
        });
    }

    // Closing the window hides it to the tray instead of quitting, so the app
    // (and its tray icon) keeps running in the background.
    window.connect_close_request(|w| {
        tracing::debug!("close-request -> hide to tray");
        w.set_visible(false);
        macos_resign_active();
        glib::Propagation::Stop
    });

    // macOS: Cmd+Q hides the window to the tray (the app keeps running in the
    // background); Quit stays available from the tray menu. We rebind the standard
    // quit accelerator to a hide action so the menu-bar/⌘Q shortcut closes the
    // window instead of terminating the process.
    #[cfg(target_os = "macos")]
    {
        let hide = gio::SimpleAction::new("hide-to-tray", None);
        let w = window.clone();
        hide.connect_activate(move |_, _| {
            w.set_visible(false);
            macos_resign_active();
        });
        app.add_action(&hide);
        app.set_accels_for_action("app.hide-to-tray", &["<Meta>q"]);
        app.set_accels_for_action("app.quit", &[]);
    }

    // --- system tray (best effort; ignored if no StatusNotifier host) ---
    tray::install(
        app,
        &window,
        tray::TrayWiring {
            pause_tx: tray_pause_tx,
            paused: paused_state.clone(),
            refresh_rx: tray_refresh_rx,
            speeds: tray_speeds.clone(),
        },
    );

    // A second launch (single-instance) signals us here to reveal the window.
    {
        let window = window.clone();
        glib::spawn_future_local(async move {
            while show_rx.recv().await.is_ok() {
                window.present();
            }
        });
    }

    // Be present in the tray from login (Windows): a `--hidden` autostart entry.
    ensure_autostart();

    // Autostart launches us hidden (tray only); otherwise show the window.
    if !hidden {
        window.present();
    }
}

/// Register a per-user autostart entry so the tray is available from login,
/// launching the GUI with `--hidden` (tray only, no window). Idempotent; a
/// LocalSystem service can't own a tray (session 0), so the user-session GUI
/// carries it. No-op off Windows.
#[cfg(windows)]
fn ensure_autostart() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let value = format!("\"{}\" --hidden", exe.display());
    let _ = std::process::Command::new("reg")
        .args([
            "add",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "SeedSync",
            "/t",
            "REG_SZ",
            "/d",
            &value,
            "/f",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

#[cfg(not(windows))]
fn ensure_autostart() {}

/// Attempt to start the background daemon (the "Start Daemon" button). On Windows
/// the daemon is a LocalSystem service the unprivileged GUI can't start, so we
/// relaunch `seed-daemon start` elevated via UAC. On Linux it's a systemd *user*
/// service. Best effort: failures are logged and the GUI re-queries shortly after,
/// falling back to the "Daemon Not Started" page if it's still down.
fn start_daemon() {
    #[cfg(windows)]
    {
        // <prefix>\bin\seed-gui.exe -> sibling seed-daemon.exe.
        if let Some(daemon) = sibling_exe("seed-daemon.exe") {
            shell_execute_runas(&daemon, "start");
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Err(e) = std::process::Command::new("systemctl")
            .args(["--user", "start", "seed-daemon.service"])
            .spawn()
        {
            tracing::warn!("start daemon failed: {e}");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(daemon) = sibling_exe("seed-daemon") {
            let _ = std::process::Command::new(daemon).arg("run").spawn();
        }
    }
}

/// Path to a binary sitting next to this executable (the bundled daemon lives in
/// the same `bin/` dir as the GUI).
#[cfg(any(windows, target_os = "macos"))]
fn sibling_exe(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join(name))
}

/// Relaunch `exe args` elevated through the Windows shell ("runas" verb → UAC).
/// Hidden window so the short-lived `seed-daemon start` console doesn't flash.
#[cfg(windows)]
fn shell_execute_runas(exe: &std::path::Path, args: &str) {
    use std::os::raw::c_void;
    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_cmd: i32,
        ) -> *mut c_void;
    }
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    const SW_HIDE: i32 = 0;
    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let params = wide(args);
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_HIDE,
        );
    }
}

/// Per-row widgets we mutate in place on refresh. Keeping the rows alive (rather
/// than tearing down and rebuilding the whole list every update) is what lets an
/// open ⋮ popover survive the once-a-second refresh during indexing; it also
/// stops the list from flickering and reordering on each refresh.
struct RowWidgets {
    row: gtk::ListBoxRow,
    name: gtk::Label,
    sub: gtk::Label,
    status: gtk::Label,
    members: gtk::Button,
    pause_btn: gtk::Button,
    paused: Rc<Cell<bool>>,
    /// "View keys…" menu item, disabled while the share is indexing.
    keys_item: gtk::Button,
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::Master => "master",
        Role::Viewer => "viewer",
    }
}

fn status_text(s: &ShareSummary) -> String {
    match s.status {
        ShareStatus::Healthy => format!("Healthy {}%", s.percent),
        ShareStatus::Syncing => format!("Syncing {}%", s.percent),
        ShareStatus::Indexing => {
            // Show "Indexing 13.4/29.0 GB (46%)", picking GB vs MB off the total.
            let (div, unit) = if s.index_total >= 1 << 30 {
                (1u64 << 30, "GB")
            } else {
                (1u64 << 20, "MB")
            };
            let v = |b: u64| b as f64 / div as f64;
            format!(
                "Indexing {:.1}/{:.1} {} ({}%)",
                v(s.indexed_bytes),
                v(s.index_total),
                unit,
                s.percent
            )
        }
        ShareStatus::Paused => "Paused".into(),
        ShareStatus::Error => "Error".into(),
    }
}

/// Reconcile the list with the latest summaries *in place*, keyed by share id:
/// refresh existing rows' labels, add rows for new shares, drop removed ones.
fn update_list(
    listbox: &gtk::ListBox,
    shares: &[ShareSummary],
    net: &Net,
    window: &adw::ApplicationWindow,
    rows: &Rc<RefCell<HashMap<String, RowWidgets>>>,
) {
    let mut rows = rows.borrow_mut();
    rows.retain(|id, rw| {
        let keep = shares.iter().any(|s| &s.share_id == id);
        if !keep {
            listbox.remove(&rw.row);
        }
        keep
    });
    for s in shares {
        if let Some(rw) = rows.get(&s.share_id) {
            rw.name.set_label(&s.name);
            rw.sub
                .set_label(&format!("{} · {}", role_str(s.role), s.folder));
            rw.status.set_label(&status_text(s));
            rw.members
                .set_label(&format!("{} of {} ▸", s.online, s.total));
            rw.paused.set(s.paused);
            rw.pause_btn.set_icon_name(if s.paused {
                "media-playback-start-symbolic"
            } else {
                "media-playback-pause-symbolic"
            });
            rw.pause_btn
                .set_tooltip_text(Some(if s.paused { "Resume" } else { "Pause" }));
            rw.keys_item
                .set_sensitive(!matches!(s.status, ShareStatus::Indexing));
        } else {
            let rw = build_row(s, net, window);
            listbox.append(&rw.row);
            rows.insert(s.share_id.clone(), rw);
        }
    }
}

fn build_row(s: &ShareSummary, net: &Net, window: &adw::ApplicationWindow) -> RowWidgets {
    let row = gtk::ListBoxRow::new();
    let hbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();

    // pause/resume toggle — reads its state from a cell so the row can be updated
    // in place (no need to recreate the click handler when paused flips).
    let paused = Rc::new(Cell::new(s.paused));
    let pause_btn = gtk::Button::builder()
        .icon_name(if s.paused {
            "media-playback-start-symbolic"
        } else {
            "media-playback-pause-symbolic"
        })
        .tooltip_text(if s.paused { "Resume" } else { "Pause" })
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    {
        let net = net.clone();
        let id = s.share_id.clone();
        let name = s.name.clone();
        let paused = paused.clone();
        pause_btn.connect_clicked(move |_| {
            let (req, msg) = if paused.get() {
                (
                    IpcRequest::Resume {
                        share_id: id.clone(),
                    },
                    format!("Resumed “{name}”"),
                )
            } else {
                (
                    IpcRequest::Pause {
                        share_id: id.clone(),
                    },
                    format!("Paused “{name}”"),
                )
            };
            net.send(req, move |_| Some(UiMsg::Toast(msg.clone())));
            net.refresh();
        });
    }
    hbox.append(&pause_btn);

    // name + folder
    let name_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let name = gtk::Label::builder()
        .label(&s.name)
        .halign(gtk::Align::Start)
        .css_classes(["heading"])
        .build();
    let sub = gtk::Label::builder()
        .label(format!("{} · {}", role_str(s.role), s.folder))
        .halign(gtk::Align::Start)
        .css_classes(["dim-label", "caption"])
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();
    name_box.append(&name);
    name_box.append(&sub);
    name_box.set_hexpand(true);
    hbox.append(&name_box);

    // status
    let status = gtk::Label::builder()
        .label(status_text(s))
        .halign(gtk::Align::End)
        .build();
    hbox.append(&status);

    // members (clickable -> peers dialog)
    let members = gtk::Button::builder()
        .label(format!("{} of {} ▸", s.online, s.total))
        .tooltip_text("View peers")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    {
        let net = net.clone();
        let id = s.share_id.clone();
        members.connect_clicked(move |_| {
            net.send(
                IpcRequest::GetPeers {
                    share_id: id.clone(),
                },
                |res| match res {
                    Ok(IpcResponse::Peers(p)) => Some(UiMsg::Peers(p)),
                    _ => Some(UiMsg::Toast("could not load peers".into())),
                },
            );
        });
    }
    hbox.append(&members);

    // per-share actions menu (⋮): open folder, view keys, delete.
    let menu_btn = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Share actions")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    let menu_pop = gtk::Popover::new();
    let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 4);

    // Open the share's folder in the system file manager.
    let open_item = flat_button("Open folder");
    {
        let folder = s.folder.clone();
        let pop = menu_pop.clone();
        open_item.connect_clicked(move |_| {
            pop.popdown();
            open_in_file_manager(&folder);
        });
    }
    menu_box.append(&open_item);

    // View keys — for every share, not just masters: a master sees both its
    // master (write) and viewer (read) keys; a viewer sees the viewer key it can
    // hand to further peers (which is the only way to share it from the UI).
    // Disabled while the share is still indexing — it isn't ready to hand out
    // yet; the refresh keeps this in sync as the status changes.
    let keys_item = flat_button("View keys…");
    keys_item.set_sensitive(!matches!(s.status, ShareStatus::Indexing));
    {
        let net = net.clone();
        let id = s.share_id.clone();
        let pop = menu_pop.clone();
        keys_item.connect_clicked(move |_| {
            pop.popdown();
            net.send(
                IpcRequest::RevealKeys {
                    share_id: id.clone(),
                },
                |res| match res {
                    Ok(IpcResponse::Keys {
                        master_key,
                        viewer_key,
                    }) => Some(UiMsg::Keys {
                        master: master_key,
                        viewer: viewer_key,
                    }),
                    _ => Some(UiMsg::Toast("could not reveal keys".into())),
                },
            );
        });
    }
    menu_box.append(&keys_item);

    let del_item = flat_button("Delete share…");
    del_item.add_css_class("destructive-action");
    {
        let net = net.clone();
        let id = s.share_id.clone();
        let name = s.name.clone();
        let window = window.clone();
        let pop = menu_pop.clone();
        del_item.connect_clicked(move |_| {
            pop.popdown();
            // Frameless, modal confirmation (libadwaita message dialog — no
            // titlebar or window controls), centered over the main window.
            let dialog = adw::MessageDialog::new(
                Some(&window),
                Some(&format!("Remove “{name}”?")),
                Some("The share stops syncing and leaves the list. Your local files are kept on disk."),
            );
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("remove", "Remove");
            dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            let net = net.clone();
            let id = id.clone();
            let name = name.clone();
            dialog.connect_response(None, move |_, resp| {
                if resp == "remove" {
                    let name = name.clone();
                    net.send(
                        IpcRequest::RemoveShare {
                            share_id: id.clone(),
                            delete_files: false,
                        },
                        move |r| match r {
                            Ok(IpcResponse::Ok) => Some(UiMsg::Toast(format!("Removed “{name}”"))),
                            _ => Some(UiMsg::Toast("could not remove share".into())),
                        },
                    );
                    net.refresh();
                }
            });
            dialog.present();
        });
    }
    menu_box.append(&del_item);

    menu_pop.set_child(Some(&menu_box));
    menu_btn.set_popover(Some(&menu_pop));
    hbox.append(&menu_btn);

    row.set_child(Some(&hbox));
    RowWidgets {
        row,
        name,
        sub,
        status,
        members,
        pause_btn,
        paused,
        keys_item,
    }
}

/// Default ignore patterns offered when creating a share.
const DEFAULT_IGNORE: &str = ".DS_Store\nThumbs.db\ndesktop.ini\n*.tmp\n*~";

/// Create flow: pick a folder, then show the ignore-list editor before creating.
fn create_share_flow(
    window: &adw::ApplicationWindow,
    net: &Net,
    device_name: &Rc<RefCell<String>>,
) {
    let dialog = gtk::FileDialog::builder()
        .title("Choose a folder to share")
        .build();
    let net = net.clone();
    let win = window.clone();
    let device_name = device_name.clone();
    dialog.select_folder(Some(window), gio::Cancellable::NONE, move |res| {
        if let Ok(folder) = res {
            if let Some(path) = folder.path() {
                show_create_dialog(&win, &net, path, &device_name);
            }
        }
    });
}

/// Apply a (possibly changed) device name from a form entry: persist it to the
/// daemon and update the GUI cache. No-op when blank or unchanged.
fn apply_device_name(net: &Net, device_name: &Rc<RefCell<String>>, entry: &gtk::Entry) {
    let name = entry.text().trim().to_string();
    if name.is_empty() || name == *device_name.borrow() {
        return;
    }
    *device_name.borrow_mut() = name.clone();
    net.send(IpcRequest::SetDeviceName { name }, |res| match res {
        Ok(_) => None,
        Err(_) => Some(UiMsg::Toast("could not set device name".into())),
    });
}

/// A labeled "Your name" entry prefilled with the current device name, for the
/// create/add forms.
fn name_field(device_name: &Rc<RefCell<String>>) -> (gtk::Box, gtk::Entry) {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
    b.append(
        &gtk::Label::builder()
            .label("Your name (shown to other members)")
            .halign(gtk::Align::Start)
            .css_classes(["caption-heading"])
            .build(),
    );
    let entry = gtk::Entry::builder()
        .text(&*device_name.borrow())
        .placeholder_text("This device")
        .build();
    b.append(&entry);
    (b, entry)
}

/// Gear-menu dialog to set this device's display name (shown to other members).
fn show_set_name_dialog(
    window: &adw::ApplicationWindow,
    net: &Net,
    device_name: &Rc<RefCell<String>>,
) {
    let dialog = adw::MessageDialog::new(
        Some(window),
        Some("Set device name"),
        Some("How this device is shown to the other members of your shares."),
    );
    let entry = gtk::Entry::builder()
        .text(&*device_name.borrow())
        .activates_default(true)
        .build();
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("save", "Save");
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");
    let net = net.clone();
    let device_name = device_name.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp == "save" {
            apply_device_name(&net, &device_name, &entry);
            net.refresh();
        }
    });
    dialog.present();
}

/// Dialog to review the folder + edit ignore patterns, then create the share.
fn show_create_dialog(
    window: &adw::ApplicationWindow,
    net: &Net,
    folder: PathBuf,
    device_name: &Rc<RefCell<String>>,
) {
    let dialog = adw::MessageDialog::new(Some(window), Some("Create share"), None);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.append(
        &gtk::Label::builder()
            .label(format!("Folder: {}", folder.to_string_lossy()))
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["dim-label"])
            .build(),
    );
    let (name_box, name_entry) = name_field(device_name);
    content.append(&name_box);
    content.append(
        &gtk::Label::builder()
            .label("Ignore patterns (one glob per line)")
            .halign(gtk::Align::Start)
            .css_classes(["caption-heading"])
            .build(),
    );

    let text = gtk::TextView::builder().monospace(true).build();
    text.buffer().set_text(DEFAULT_IGNORE);
    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(140)
        .child(&text)
        .build();
    content.append(&scroller);

    dialog.set_extra_child(Some(&content));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("create", "Create share");
    dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("create"));
    dialog.set_close_response("cancel");

    {
        let net = net.clone();
        let device_name = device_name.clone();
        dialog.connect_response(None, move |_, resp| {
            if resp != "create" {
                return;
            }
            apply_device_name(&net, &device_name, &name_entry);
            let buf = text.buffer();
            let body = buf
                .text(&buf.start_iter(), &buf.end_iter(), false)
                .to_string();
            let ignore: Vec<String> = body
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            submit_create(&net, folder.clone(), ignore);
        });
    }

    dialog.present();
}

/// Send the CreateShare request and, on success, fetch the bootstrap address to
/// show alongside the generated keys.
fn submit_create(net: &Net, folder: PathBuf, ignore: Vec<String>) {
    net.send(
        IpcRequest::CreateShare {
            folder: folder.to_string_lossy().into_owned(),
            generate_ignore: false,
            ignore,
        },
        |res| match res {
            Ok(IpcResponse::ShareCreated { .. }) => Some(UiMsg::Created),
            Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(format!("create failed: {e}"))),
            _ => Some(UiMsg::Toast("create failed".into())),
        },
    );
}

/// Add flow: enter a key (+ optional bootstrap), pick a folder, add the share.
fn show_add_dialog(window: &adw::ApplicationWindow, net: &Net, device_name: &Rc<RefCell<String>>) {
    let dialog = adw::MessageDialog::new(Some(window), Some("Add existing share"), None);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);

    let key_entry = gtk::Entry::builder()
        .placeholder_text("Paste master or viewer key (seedm… / seedv…)")
        .build();
    let boot_entry = gtk::Entry::builder()
        .placeholder_text("Bootstrap address (optional)")
        .build();
    let folder_lbl = gtk::Label::builder()
        .label("No folder chosen")
        .halign(gtk::Align::Start)
        .css_classes(["dim-label"])
        .build();
    let folder_btn = gtk::Button::with_label("Choose folder…");
    let chosen: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
    let (name_box, name_entry) = name_field(device_name);

    // The "Add" response stays disabled until there's a key and a chosen folder —
    // a MessageDialog response always closes, so we gate it rather than no-op on
    // submit.
    let update_valid: Rc<dyn Fn()> = {
        let dialog = dialog.clone();
        let key_entry = key_entry.clone();
        let chosen = chosen.clone();
        Rc::new(move || {
            let ok = !key_entry.text().trim().is_empty() && chosen.borrow().is_some();
            dialog.set_response_enabled("add", ok);
        })
    };

    {
        let chosen = chosen.clone();
        let folder_lbl = folder_lbl.clone();
        let dialog = dialog.clone();
        let update_valid = update_valid.clone();
        folder_btn.connect_clicked(move |_| {
            let fd = gtk::FileDialog::builder()
                .title("Choose local folder")
                .build();
            let chosen = chosen.clone();
            let folder_lbl = folder_lbl.clone();
            let update_valid = update_valid.clone();
            fd.select_folder(Some(&dialog), gio::Cancellable::NONE, move |res| {
                if let Ok(f) = res {
                    if let Some(p) = f.path() {
                        folder_lbl.set_text(&p.to_string_lossy());
                        *chosen.borrow_mut() = Some(p);
                        update_valid();
                    }
                }
            });
        });
    }
    {
        let update_valid = update_valid.clone();
        key_entry.connect_changed(move |_| update_valid());
    }

    content.append(
        &gtk::Label::builder()
            .label("Key")
            .halign(gtk::Align::Start)
            .build(),
    );
    content.append(&key_entry);
    content.append(&boot_entry);
    let folder_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    folder_row.append(&folder_btn);
    folder_row.append(&folder_lbl);
    content.append(&folder_row);
    content.append(&name_box);

    dialog.set_extra_child(Some(&content));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("add", "Add");
    dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("add"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("add", false);

    {
        let net = net.clone();
        let device_name = device_name.clone();
        dialog.connect_response(None, move |_, resp| {
            if resp != "add" {
                return;
            }
            apply_device_name(&net, &device_name, &name_entry);
            let key = key_entry.text().to_string();
            let Some(folder) = chosen.borrow().clone() else {
                return;
            };
            let bootstrap = {
                let b = boot_entry.text().to_string();
                if b.trim().is_empty() {
                    None
                } else {
                    Some(b)
                }
            };
            net.send(
                IpcRequest::AddShare {
                    key,
                    folder: folder.to_string_lossy().into_owned(),
                    bootstrap,
                },
                |res| match res {
                    Ok(IpcResponse::ShareAdded { .. }) => Some(UiMsg::Toast("share added".into())),
                    Ok(IpcResponse::Err(e)) => Some(UiMsg::Toast(format!("add failed: {e}"))),
                    _ => Some(UiMsg::Toast("add failed".into())),
                },
            );
        });
    }

    dialog.present();
}

/// Show a dialog with the share's keys (selectable + copyable).
fn show_keys_dialog(
    window: &adw::ApplicationWindow,
    master: Option<&str>,
    viewer: &str,
    bootstrap: Option<&str>,
) {
    let dialog = adw::MessageDialog::new(Some(window), Some("Share keys"), None);
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    // Each key row has a "QR" button that pops out a scannable code for the phone
    // app — no copying a giant string, and the dialog stays compact (the QR is in
    // a popover, not inline). Master + viewer keys carry a QR; the bootstrap
    // address stays text (rarely scanned; the key already embeds discovery info).
    if let Some(m) = master {
        vbox.append(&key_field_qr("Master key (write — keep secret)", m));
    }
    vbox.append(&key_field_qr("Viewer key (read-only)", viewer));
    if let Some(b) = bootstrap {
        if !b.is_empty() {
            vbox.append(&key_field("Bootstrap address (this device)", b));
        }
    }
    dialog.set_extra_child(Some(&vbox));
    dialog.add_response("close", "Close");
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");
    dialog.present();
}

/// A key field (label + read-only entry + copy) plus a "QR" button that pops out
/// a scannable QR of the value, so the dialog stays compact.
fn key_field_qr(label: &str, value: &str) -> gtk::Box {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 2);
    outer.append(
        &gtk::Label::builder()
            .label(label)
            .halign(gtk::Align::Start)
            .css_classes(["caption-heading"])
            .build(),
    );
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let entry = gtk::Entry::builder()
        .text(value)
        .editable(false)
        .hexpand(true)
        .build();
    let copy = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("Copy")
        .css_classes(["flat"])
        .build();
    {
        let value = value.to_string();
        copy.connect_clicked(move |btn| {
            btn.clipboard().set_text(&value);
        });
    }
    row.append(&entry);
    row.append(&copy);
    // QR pop-out: a MenuButton whose popover holds the scannable code.
    if let Some(pic) = qr_picture(value) {
        let qr_btn = gtk::MenuButton::builder()
            .label("QR")
            .tooltip_text("Show a scannable QR code")
            .css_classes(["flat"])
            .build();
        let popover = gtk::Popover::new();
        pic.set_margin_top(8);
        pic.set_margin_bottom(8);
        pic.set_margin_start(8);
        pic.set_margin_end(8);
        popover.set_child(Some(&pic));
        qr_btn.set_popover(Some(&popover));
        row.append(&qr_btn);
    }
    outer.append(&row);
    outer
}

/// Render `data` as a QR code into a GTK picture (black modules on white, with a
/// 4-module quiet zone). Returns `None` if the data is too large to encode.
fn qr_picture(data: &str) -> Option<gtk::Picture> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    let quiet = 4usize;
    let scale = 6usize; // ~6 px/module → comfortably scannable in the popover
    let modules = width + quiet * 2;
    let px = modules * scale;
    let mut buf = vec![255u8; px * px * 3]; // white background
    for y in 0..width {
        for x in 0..width {
            if colors[y * width + x] == qrcode::Color::Dark {
                let ox = (x + quiet) * scale;
                let oy = (y + quiet) * scale;
                for dy in 0..scale {
                    let row = (oy + dy) * px;
                    for dx in 0..scale {
                        let i = (row + ox + dx) * 3;
                        buf[i] = 0;
                        buf[i + 1] = 0;
                        buf[i + 2] = 0;
                    }
                }
            }
        }
    }
    let bytes = glib::Bytes::from(&buf);
    let texture = gtk::gdk::MemoryTexture::new(
        px as i32,
        px as i32,
        gtk::gdk::MemoryFormat::R8g8b8,
        &bytes,
        px * 3,
    );
    let pic = gtk::Picture::for_paintable(&texture);
    pic.set_size_request(px as i32, px as i32);
    pic.set_can_shrink(false);
    pic.set_halign(gtk::Align::Center);
    pic.set_margin_top(6);
    Some(pic)
}

fn show_text_dialog(window: &adw::ApplicationWindow, title: &str, subtitle: &str, text: &str) {
    let dialog = adw::MessageDialog::new(Some(window), Some(title), Some(subtitle));
    dialog.set_extra_child(Some(&key_field(title, text)));
    dialog.add_response("close", "Close");
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");
    dialog.present();
}

/// Show the peers known for a share.
fn show_peers_dialog(window: &adw::ApplicationWindow, peers: &[PeerInfo]) {
    let dialog = adw::MessageDialog::new(Some(window), Some("Members"), None);
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);

    if peers.is_empty() {
        vbox.append(
            &gtk::Label::builder()
                .label("No peers seen yet.")
                .css_classes(["dim-label"])
                .build(),
        );
    } else {
        let list = gtk::ListBox::builder().css_classes(["boxed-list"]).build();
        for p in peers {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.set_margin_start(8);
            row.set_margin_end(8);
            row.set_margin_top(6);
            row.set_margin_bottom(6);

            // One dead-simple health dot: green = fully synced, yellow = under
            // 100% (downloading or behind), gray = offline. Word label on hover.
            let (css, tip) = if !p.online {
                ("health-off", "Offline".to_string())
            } else if p.percent >= 100 {
                ("health-ok", "Synced".to_string())
            } else {
                ("health-sync", format!("{}% synced", p.percent))
            };
            let dot = gtk::Label::new(Some("●"));
            dot.add_css_class("health-dot");
            dot.add_css_class(css);
            dot.set_tooltip_text(Some(&tip));

            // Name (falls back to the short id); the local row is tagged.
            let display = p.name.clone().unwrap_or_else(|| p.node_id.clone());
            let label_text = if p.node_id == "This device" {
                format!("{display} (this device)")
            } else {
                display
            };
            let name_lbl = gtk::Label::builder()
                .label(&label_text)
                .halign(gtk::Align::Start)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();

            let role = gtk::Label::builder()
                .label(role_str(p.role))
                .halign(gtk::Align::End)
                .css_classes(["dim-label", "caption"])
                .build();

            row.append(&dot);
            row.append(&name_lbl);
            row.append(&role);
            list.append(&row);
        }
        vbox.append(&list);
    }

    let scroller = gtk::ScrolledWindow::builder()
        .propagate_natural_height(true)
        .max_content_height(360)
        .child(&vbox)
        .build();
    dialog.set_extra_child(Some(&scroller));
    dialog.add_response("close", "Close");
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");
    dialog.present();
}

/// A labeled, selectable key value with a copy button.
fn key_field(label: &str, value: &str) -> gtk::Box {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 2);
    outer.append(
        &gtk::Label::builder()
            .label(label)
            .halign(gtk::Align::Start)
            .css_classes(["caption-heading"])
            .build(),
    );
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let entry = gtk::Entry::builder()
        .text(value)
        .editable(false)
        .hexpand(true)
        .build();
    let copy = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("Copy")
        .css_classes(["flat"])
        .build();
    {
        let value = value.to_string();
        copy.connect_clicked(move |btn| {
            btn.clipboard().set_text(&value);
        });
    }
    row.append(&entry);
    row.append(&copy);
    outer.append(&row);
    outer
}

/// Format a byte/sec rate as a human bit-rate (Mbps/Kbps).
pub(crate) fn fmt_speed(bytes_per_sec: u64) -> String {
    let bits = bytes_per_sec as f64 * 8.0;
    if bits >= 1_000_000.0 {
        format!("{:.1} Mbps", bits / 1_000_000.0)
    } else {
        format!("{:.0} Kbps", bits / 1_000.0)
    }
}

/// Format a unix timestamp as a short local time-of-day (best effort).
fn fmt_time(ts: i64) -> String {
    let dt = glib::DateTime::from_unix_local(ts).ok();
    dt.and_then(|d| d.format("%H:%M:%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// Open a folder in the OS file manager. Shells out to the platform opener —
/// GIO's `launch_default_for_uri` silently fails for `file://` URIs in the
/// bundled Windows GTK runtime (no URI handler registered).
fn open_in_file_manager(path: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if let Err(e) = std::process::Command::new("explorer")
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
        {
            tracing::warn!("open folder failed: {e}");
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

fn flat_button(label: &str) -> gtk::Button {
    // Left-align the label within the full-width button so popover menu items read
    // as a left-aligned list rather than centered text.
    let lbl = gtk::Label::builder()
        .label(label)
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .build();
    gtk::Button::builder()
        .child(&lbl)
        .css_classes(["flat"])
        .halign(gtk::Align::Fill)
        .build()
}
