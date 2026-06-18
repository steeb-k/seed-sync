//! Seed Sync GUI entry point (GTK4 + Libadwaita).
//!
//! The full window (share list, create/add flows, peers dialog, settings) and
//! the system tray land in M3. This skeleton just brings up a minimal Adwaita
//! window so the toolchain + bindings are wired end-to-end.

use adw::prelude::*;
use gtk::glib;

const APP_ID: &str = "io.github.steeb_k.SeedSync";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Seed Sync")
        .default_width(560)
        .default_height(360)
        .build();

    let header = adw::HeaderBar::new();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&gtk::Label::new(Some("Seed Sync — GUI skeleton (M3)")));

    window.set_content(Some(&content));
    window.present();
}
