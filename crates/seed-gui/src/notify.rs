//! OS-level notifications (Action Center / notification daemon), used for the
//! peer-health alerts: the GUI is tray-resident, so with the window hidden an
//! in-app toast is invisible — the OS notification is what the user actually
//! sees. Best-effort by design: a broken notification daemon must never take
//! the GUI down or block the GTK loop, so delivery runs on its own thread and
//! failures are only logged.
//!
//! Known cosmetic caveat on Windows: until the MSI registers an AUMID +
//! shortcut for the app, toasts attribute to the PowerShell fallback AUMID
//! rather than "SEED Sync" (tracked as a packaging task).

/// Fire-and-forget an OS notification.
pub fn os_notify(summary: &str, body: &str) {
    let summary = summary.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let result = notify_rust::Notification::new()
            .appname("SEED Sync")
            .summary(&summary)
            .body(&body)
            .show();
        if let Err(e) = result {
            tracing::debug!("OS notification failed (non-fatal): {e}");
        }
    });
}

/// Human duration for alert copy: minutes under 90 min, else hours.
pub fn fmt_duration_secs(secs: i64) -> String {
    let mins = secs / 60;
    if mins < 90 {
        format!("{} min", mins.max(1))
    } else {
        format!("{:.0} h", secs as f64 / 3600.0)
    }
}
