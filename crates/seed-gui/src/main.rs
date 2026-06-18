//! Seed Sync GUI (GTK4 + Libadwaita): an unprivileged IPC client to the
//! `seed-daemon`. It shows the share list, drives the create/add/reveal/pause
//! flows, and reflects live status. All networking lives in the daemon; this
//! process only talks the IPC protocol.
//!
//! Threading: a Tokio runtime on a side thread owns the socket IO; results and
//! pushed events arrive on the GTK main thread via an `async-channel` consumed
//! by `glib::spawn_future_local`. GTK objects are only ever touched on the main
//! thread.

mod tray;

use std::path::PathBuf;
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
    Throughput {
        down: u64,
        up: u64,
    },
    LastUpdated(i64),
    Refresh,
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
            Err(e) => Some(UiMsg::Toast(format!("daemon unreachable: {e}"))),
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

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "seed_gui=info".into()),
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

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build_ui(app, handle.clone(), socket.clone()));
    // No CLI args of our own.
    app.run_with_args::<&str>(&[])
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

fn build_ui(app: &adw::Application, handle: Handle, socket: PathBuf) {
    load_css();
    let (tx, rx) = async_channel::unbounded::<UiMsg>();
    let net = Net { handle, socket, tx };

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Seed Sync")
        .default_width(580)
        .default_height(380)
        .build();

    // --- header bar ---
    let header = adw::HeaderBar::new();

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
    header.pack_start(&add_btn);

    // Gear menu: node address + quit.
    let gear_btn = gtk::MenuButton::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Settings")
        .build();
    let gear_popover = gtk::Popover::new();
    let gear_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let nodeaddr_btn = flat_button("Show this device's address…");
    let quit_btn = flat_button("Quit");
    gear_box.append(&nodeaddr_btn);
    gear_box.append(&quit_btn);
    gear_popover.set_child(Some(&gear_box));
    gear_btn.set_popover(Some(&gear_popover));
    header.pack_end(&gear_btn);

    // --- share list ---
    let listbox = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let placeholder = gtk::Label::builder()
        .label("No shares yet — use “+” to create or add one.")
        .css_classes(["dim-label"])
        .margin_top(24)
        .margin_bottom(24)
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

    // --- assemble ---
    let toast_overlay = adw::ToastOverlay::new();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&scroller);
    content.append(&status_bar);
    toast_overlay.set_child(Some(&content));
    window.set_content(Some(&toast_overlay));

    // --- wire actions ---
    {
        let net = net.clone();
        let window = window.clone();
        new_share_btn.connect_clicked(move |_| {
            add_popover.popdown();
            create_share_flow(&window, &net);
        });
    }
    {
        let net = net.clone();
        let window = window.clone();
        add_share_btn.connect_clicked(move |_| {
            show_add_dialog(&window, &net);
        });
    }
    {
        let net = net.clone();
        nodeaddr_btn.connect_clicked(move |_| {
            gear_popover.popdown();
            net.send(IpcRequest::NodeAddr, |res| match res {
                Ok(IpcResponse::NodeAddr(a)) => Some(UiMsg::NodeAddr(a)),
                _ => Some(UiMsg::Toast("could not get node address".into())),
            });
        });
    }
    {
        let app = app.clone();
        quit_btn.connect_clicked(move |_| app.quit());
    }

    // --- UI message pump (runs on the GTK main context) ---
    {
        let toast_overlay = toast_overlay.clone();
        let window = window.clone();
        let net = net.clone();
        let down_lbl = down_lbl.clone();
        let up_lbl = up_lbl.clone();
        let updated_lbl = updated_lbl.clone();
        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                match msg {
                    UiMsg::Shares(shares) => {
                        if let Some(ts) = shares.iter().map(|s| s.last_updated).max() {
                            if ts > 0 {
                                updated_lbl.set_text(&format!("Last updated: {}", fmt_time(ts)));
                            }
                        }
                        rebuild_list(&listbox, &shares, &net, &window);
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
                    UiMsg::Throughput { down, up } => {
                        down_lbl.set_text(&format!("↓ {}", fmt_speed(down)));
                        up_lbl.set_text(&format!("↑ {}", fmt_speed(up)));
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
    {
        let net = net.clone();
        net.refresh();
        glib::timeout_add_local(Duration::from_millis(2000), move || {
            net.refresh();
            glib::ControlFlow::Continue
        });
    }

    // --- system tray (best effort; ignored if no StatusNotifier host) ---
    tray::install(app);

    window.present();
}

/// Rebuild the share list rows from the latest summaries.
fn rebuild_list(
    listbox: &gtk::ListBox,
    shares: &[ShareSummary],
    net: &Net,
    window: &adw::ApplicationWindow,
) {
    while let Some(child) = listbox.first_child() {
        listbox.remove(&child);
    }
    for s in shares {
        listbox.append(&share_row(s, net, window));
    }
}

fn share_row(s: &ShareSummary, net: &Net, window: &adw::ApplicationWindow) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    let hbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();

    // pause/resume toggle
    let paused = s.paused;
    let pause_btn = gtk::Button::builder()
        .icon_name(if paused {
            "media-playback-start-symbolic"
        } else {
            "media-playback-pause-symbolic"
        })
        .tooltip_text(if paused { "Resume" } else { "Pause" })
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    {
        let net = net.clone();
        let id = s.share_id.clone();
        let name = s.name.clone();
        pause_btn.connect_clicked(move |_| {
            let (req, msg) = if paused {
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
    let role = match s.role {
        Role::Master => "master",
        Role::Viewer => "viewer",
    };
    let sub = gtk::Label::builder()
        .label(format!("{role} · {}", s.folder))
        .halign(gtk::Align::Start)
        .css_classes(["dim-label", "caption"])
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();
    name_box.append(&name);
    name_box.append(&sub);
    name_box.set_hexpand(true);
    hbox.append(&name_box);

    // status
    let status_txt = match s.status {
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
    };
    let status = gtk::Label::builder()
        .label(status_txt)
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

    // per-share actions menu (⋮): reveal keys (master only) + delete.
    let menu_btn = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Share actions")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    let menu_pop = gtk::Popover::new();
    let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 4);

    if matches!(s.role, Role::Master) {
        let keys_item = flat_button("Reveal keys…");
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
        menu_box.append(&keys_item);
    }

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
            let dialog = gtk::AlertDialog::builder()
                .modal(true)
                .message(format!("Remove “{name}”?"))
                .detail("The share stops syncing and leaves the list. Your local files are kept on disk.")
                .buttons(["Cancel", "Remove"])
                .cancel_button(0)
                .default_button(0)
                .build();
            let net = net.clone();
            let id = id.clone();
            let name = name.clone();
            dialog.choose(Some(&window), gio::Cancellable::NONE, move |res| {
                if matches!(res, Ok(1)) {
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
        });
    }
    menu_box.append(&del_item);

    menu_pop.set_child(Some(&menu_box));
    menu_btn.set_popover(Some(&menu_pop));
    hbox.append(&menu_btn);

    row.set_child(Some(&hbox));
    row
}

/// Default ignore patterns offered when creating a share.
const DEFAULT_IGNORE: &str = ".DS_Store\nThumbs.db\ndesktop.ini\n*.tmp\n*~";

/// Create flow: pick a folder, then show the ignore-list editor before creating.
fn create_share_flow(window: &adw::ApplicationWindow, net: &Net) {
    let dialog = gtk::FileDialog::builder()
        .title("Choose a folder to share")
        .build();
    let net = net.clone();
    let win = window.clone();
    dialog.select_folder(Some(window), gio::Cancellable::NONE, move |res| {
        if let Ok(folder) = res {
            if let Some(path) = folder.path() {
                show_create_dialog(&win, &net, path);
            }
        }
    });
}

/// Dialog to review the folder + edit ignore patterns, then create the share.
fn show_create_dialog(window: &adw::ApplicationWindow, net: &Net, folder: PathBuf) {
    let dialog = gtk::Window::builder()
        .title("Create share")
        .transient_for(window)
        .modal(true)
        .default_width(480)
        .default_height(360)
        .build();
    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_start(16)
        .margin_end(16)
        .margin_top(16)
        .margin_bottom(16)
        .build();

    vbox.append(
        &gtk::Label::builder()
            .label(format!("Folder: {}", folder.to_string_lossy()))
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["dim-label"])
            .build(),
    );
    vbox.append(
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
    vbox.append(&scroller);

    let create = gtk::Button::builder()
        .label("Create share")
        .css_classes(["suggested-action"])
        .build();
    {
        let net = net.clone();
        let dialog = dialog.clone();
        create.connect_clicked(move |_| {
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
            dialog.close();
        });
    }
    vbox.append(&create);
    dialog.set_child(Some(&vbox));
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
fn show_add_dialog(window: &adw::ApplicationWindow, net: &Net) {
    let dialog = gtk::Window::builder()
        .title("Add existing share")
        .transient_for(window)
        .modal(true)
        .default_width(460)
        .build();

    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_start(16)
        .margin_end(16)
        .margin_top(16)
        .margin_bottom(16)
        .build();

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
    let chosen: std::rc::Rc<std::cell::RefCell<Option<PathBuf>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    {
        let chosen = chosen.clone();
        let folder_lbl = folder_lbl.clone();
        let dialog = dialog.clone();
        folder_btn.connect_clicked(move |_| {
            let fd = gtk::FileDialog::builder()
                .title("Choose local folder")
                .build();
            let chosen = chosen.clone();
            let folder_lbl = folder_lbl.clone();
            fd.select_folder(Some(&dialog), gio::Cancellable::NONE, move |res| {
                if let Ok(f) = res {
                    if let Some(p) = f.path() {
                        folder_lbl.set_text(&p.to_string_lossy());
                        *chosen.borrow_mut() = Some(p);
                    }
                }
            });
        });
    }

    let add = gtk::Button::builder()
        .label("Add share")
        .css_classes(["suggested-action"])
        .build();
    {
        let net = net.clone();
        let key_entry = key_entry.clone();
        let boot_entry = boot_entry.clone();
        let chosen = chosen.clone();
        let dialog = dialog.clone();
        add.connect_clicked(move |_| {
            let key = key_entry.text().to_string();
            let Some(folder) = chosen.borrow().clone() else {
                return;
            };
            if key.is_empty() {
                return;
            }
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
            dialog.close();
        });
    }

    vbox.append(
        &gtk::Label::builder()
            .label("Key")
            .halign(gtk::Align::Start)
            .build(),
    );
    vbox.append(&key_entry);
    vbox.append(&boot_entry);
    let folder_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    folder_row.append(&folder_btn);
    folder_row.append(&folder_lbl);
    vbox.append(&folder_row);
    vbox.append(&add);

    dialog.set_child(Some(&vbox));
    dialog.present();
}

/// Show a dialog with the share's keys (selectable + copyable).
fn show_keys_dialog(
    window: &adw::ApplicationWindow,
    master: Option<&str>,
    viewer: &str,
    bootstrap: Option<&str>,
) {
    let dialog = gtk::Window::builder()
        .title("Share keys")
        .transient_for(window)
        .modal(true)
        .default_width(520)
        .build();
    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_start(16)
        .margin_end(16)
        .margin_top(16)
        .margin_bottom(16)
        .build();

    if let Some(m) = master {
        vbox.append(&key_field("Master key (write — keep secret)", m));
    }
    vbox.append(&key_field("Viewer key (read-only)", viewer));
    if let Some(b) = bootstrap {
        if !b.is_empty() {
            vbox.append(&key_field("Bootstrap address (this device)", b));
        }
    }
    let close = gtk::Button::with_label("Close");
    {
        let dialog = dialog.clone();
        close.connect_clicked(move |_| dialog.close());
    }
    vbox.append(&close);
    dialog.set_child(Some(&vbox));
    dialog.present();
}

fn show_text_dialog(window: &adw::ApplicationWindow, title: &str, subtitle: &str, text: &str) {
    let dialog = gtk::Window::builder()
        .title(title)
        .transient_for(window)
        .modal(true)
        .default_width(520)
        .build();
    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_start(16)
        .margin_end(16)
        .margin_top(16)
        .margin_bottom(16)
        .build();
    vbox.append(
        &gtk::Label::builder()
            .label(subtitle)
            .halign(gtk::Align::Start)
            .wrap(true)
            .css_classes(["dim-label"])
            .build(),
    );
    vbox.append(&key_field(title, text));
    let close = gtk::Button::with_label("Close");
    {
        let dialog = dialog.clone();
        close.connect_clicked(move |_| dialog.close());
    }
    vbox.append(&close);
    dialog.set_child(Some(&vbox));
    dialog.present();
}

/// Show the peers known for a share.
fn show_peers_dialog(window: &adw::ApplicationWindow, peers: &[PeerInfo]) {
    let dialog = gtk::Window::builder()
        .title("Peers")
        .transient_for(window)
        .modal(true)
        .default_width(420)
        .default_height(320)
        .build();
    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_start(16)
        .margin_end(16)
        .margin_top(16)
        .margin_bottom(16)
        .build();

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
            let dot = gtk::Label::new(Some(if p.online { "●" } else { "○" }));
            dot.set_tooltip_text(Some(if p.online { "online" } else { "offline" }));
            let id = gtk::Label::builder()
                .label(&p.node_id)
                .halign(gtk::Align::Start)
                .hexpand(true)
                .css_classes(["monospace"])
                .build();
            row.append(&dot);
            row.append(&id);
            list.append(&row);
        }
        vbox.append(&list);
    }

    let close = gtk::Button::with_label("Close");
    {
        let dialog = dialog.clone();
        close.connect_clicked(move |_| dialog.close());
    }
    vbox.append(&close);
    dialog.set_child(Some(&vbox));
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
fn fmt_speed(bytes_per_sec: u64) -> String {
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

fn flat_button(label: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(label)
        .css_classes(["flat"])
        .halign(gtk::Align::Fill)
        .build()
}
