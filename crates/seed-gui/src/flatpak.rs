//! In-sandbox daemon supervision for the Flatpak build (`packaging/flatpak/`).
//!
//! Outside Flatpak, `seed-daemon` is started by something the GUI doesn't
//! control and that's already running by the time the GUI opens its first IPC
//! connection: a `systemd --user` unit
//! (`packaging/linux/seed-daemon.service`) on a normal Linux install, or the
//! Windows service. Inside a Flatpak sandbox there is no `systemd --user`
//! reachable at all — no init of its own, and the finish-args this app
//! requests don't expose the host's session manager — so nothing starts the
//! daemon unless the GUI does it itself. [`ensure_daemon`] is that: called
//! once at startup and again from the "Daemon Not Started" page's retry
//! button (see `main.rs`), it spawns `seed-daemon run` as a child of the GUI
//! when we're sandboxed and nothing is listening on the socket yet.
//!
//! Autostart (so the daemon — and the tray — come back after a reboot without
//! the user opening the app by hand) can't use `systemd --user` either, and
//! `ashpd` (the Background portal's Rust binding) is not a dependency of this
//! workspace — `Cargo.lock` has no `ashpd` entry — so pulling it in would mean
//! a new dependency tree for a single call. Instead this writes a plain XDG
//! autostart `.desktop` entry to `~/.config/autostart`, exactly like the
//! non-Flatpak tarball installer already does for the tray
//! (`packaging/linux/seed-sync`'s `AUTOSTART_DIR="$HOME/.config/autostart"`,
//! and `main.rs`'s own Windows `ensure_autostart`) — same mechanism, same
//! directory, just naming `flatpak run` as the command instead of a path into
//! `~/.local/bin`.
//!
//! That directory is built from the literal `$HOME` env var, not
//! `directories::BaseDirs::config_dir()` / `$XDG_CONFIG_HOME`: Flatpak always
//! redirects `$XDG_CONFIG_HOME` to the sandboxed `~/.var/app/<id>/config`
//! (see `docs/linux-packaging.md`'s Flatpak section), which the host session
//! manager reading autostart entries never looks at — that redirect applies
//! regardless of `--filesystem=host`. `$HOME` itself is *not* redirected, and
//! `--filesystem=host` (already required so the daemon can mirror arbitrary
//! folders — see the manifest) makes the real `~/.config/autostart` directly
//! writable, so building the path from `$HOME` lands the entry where the
//! session manager will actually find it.

use std::path::{Path, PathBuf};

/// GTK/D-Bus/Flatpak application id. Duplicated from `crate::APP_ID` (a
/// private `const` in `main.rs`, not worth threading through) so this module
/// has no dependency on `main`'s internals beyond the one value it needs.
const APP_ID: &str = "io.github.steeb_k.SeedSync";

/// True when running inside a Flatpak sandbox. Flatpak bind-mounts
/// `/.flatpak-info` (an ini file naming the app) into every sandboxed
/// process; its presence is the standard, documented way to detect the
/// sandbox from inside — there is no dedicated API, and this is what
/// `flatpak-spawn` and most sandbox-aware apps check.
pub fn in_flatpak() -> bool {
    marker_present(Path::new("/.flatpak-info"))
}

/// Testable half of [`in_flatpak`]: does a marker file exist at this path.
fn marker_present(marker: &Path) -> bool {
    marker.exists()
}

/// If we're sandboxed and nothing is listening on `socket`, spawn
/// `seed-daemon run` as a child of the GUI and make sure it comes back at the
/// next login. A no-op outside Flatpak — every other platform already has
/// something else responsible for the daemon's lifecycle.
///
/// Best-effort throughout: every failure is logged and swallowed. Either way,
/// the caller's fallback is the "Daemon Not Started" page, which stays up (or
/// reappears) if this doesn't manage to bring the daemon up.
pub fn ensure_daemon(socket: &Path) {
    if !in_flatpak() {
        return;
    }
    if socket_reachable(socket) {
        return;
    }
    spawn_daemon();
    if let Err(e) = ensure_autostart_entry() {
        tracing::warn!("flatpak: could not write autostart entry: {e}");
    }
}

/// Whether a daemon is already listening on `socket`. A connect attempt
/// rather than `Path::exists()`: a stale socket file left behind by a daemon
/// that didn't shut down cleanly would otherwise read as "running".
#[cfg(unix)]
fn socket_reachable(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

/// Flatpak is Linux-only, so this branch is dead in practice (`in_flatpak`
/// is always false here) — it exists only so the module builds on every
/// platform this crate targets.
#[cfg(not(unix))]
fn socket_reachable(_socket: &Path) -> bool {
    false
}

/// Path to `seed-daemon`, resolved next to this executable — the Flatpak
/// module (`packaging/flatpak/io.github.steeb_k.SeedSync.yml`) installs both
/// binaries to the same `/app/bin`.
fn daemon_binary_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("seed-daemon"))
}

/// Where the spawned daemon's stdout/stderr go. On Linux `seed-daemon run`
/// only ever logs to stdout (see `init_logging` in
/// `crates/seed-daemon/src/main.rs` — the `<data_dir>/daemon.log` file it can
/// write is a Windows-service-only path); outside Flatpak that stdout lands
/// in the `systemd --user` journal for the unit. Spawned as a plain child of
/// the GUI instead, it would otherwise go nowhere useful, so it's redirected
/// into the same data directory the daemon already owns
/// (`$XDG_DATA_HOME/seedsync`, matching `default_data_dir()` in
/// `seed-daemon/src/main.rs`), alongside its socket and database.
fn daemon_log_path() -> PathBuf {
    let data_dir = directories::ProjectDirs::from("io.github", "steeb_k", "SeedSync")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".seed-data"));
    data_dir.join("daemon.log")
}

/// Spawn `seed-daemon run`, detached from the GUI's own stdio, with its
/// output appended to [`daemon_log_path`]. Reaps the child on a background
/// thread so it doesn't linger as a zombie once it (eventually) exits.
fn spawn_daemon() {
    let Some(bin) = daemon_binary_path() else {
        tracing::warn!("flatpak: could not resolve seed-daemon's path");
        return;
    };
    if !bin.exists() {
        tracing::warn!("flatpak: seed-daemon not found at {}", bin.display());
        return;
    }
    let log_path = daemon_log_path();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("run").stdin(std::process::Stdio::null());
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(out) => match out.try_clone() {
            Ok(err) => {
                cmd.stdout(std::process::Stdio::from(out));
                cmd.stderr(std::process::Stdio::from(err));
            }
            Err(_) => {
                cmd.stdout(std::process::Stdio::from(out));
                cmd.stderr(std::process::Stdio::null());
            }
        },
        Err(e) => {
            tracing::warn!("flatpak: could not open {}: {e}", log_path.display());
        }
    }
    match cmd.spawn() {
        Ok(mut child) => {
            tracing::info!("flatpak: spawned seed-daemon (sandbox has no systemd --user to do it)");
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!("flatpak: failed to spawn seed-daemon: {e}"),
    }
}

/// The autostart entry's content: an XDG desktop file that relaunches the GUI
/// (and, via [`ensure_daemon`], the daemon) hidden at login — the sandboxed
/// equivalent of `main.rs`'s Windows `ensure_autostart`. `flatpak run` rather
/// than a direct binary path, since `/app/bin` isn't on the host's `PATH`.
fn autostart_desktop_contents() -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=SEED Sync\n\
         Exec=flatpak run {APP_ID} --hidden\n\
         Terminal=false\n\
         NoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

/// Real (host, not sandbox-redirected) `~/.config/autostart` — see the module
/// doc for why this is built from `$HOME` rather than
/// `directories::BaseDirs::config_dir()`.
fn autostart_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("autostart"))
}

/// Write the autostart entry if it's missing or stale. Idempotent: a byte-
/// identical file already in place is left untouched (no needless write on
/// every launch).
fn ensure_autostart_entry() -> std::io::Result<()> {
    let Some(dir) = autostart_dir() else {
        return Ok(());
    };
    write_autostart_entry_in(&dir)
}

/// Testable half of [`ensure_autostart_entry`]: write into an injected
/// directory rather than the real `$HOME`.
fn write_autostart_entry_in(dir: &Path) -> std::io::Result<()> {
    use std::io::Write as _;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{APP_ID}.desktop"));
    let contents = autostart_desktop_contents();
    if std::fs::read_to_string(&path)
        .map(|s| s == contents)
        .unwrap_or(false)
    {
        return Ok(());
    }
    let mut f = std::fs::File::create(&path)?;
    f.write_all(contents.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test picks its own subdirectory under the OS temp dir (named with
    /// the PID and a per-call tag) so parallel test threads never collide,
    /// without adding a `tempfile` dependency just for this.
    fn scratch_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "seed-gui-flatpak-test-{}-{tag}",
            std::process::id()
        ))
    }

    #[test]
    fn marker_present_true_when_file_exists() {
        let dir = scratch_dir("marker-present");
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("flatpak-info");
        std::fs::write(&marker, b"[Application]\nname=io.github.steeb_k.SeedSync\n").unwrap();
        assert!(marker_present(&marker));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_present_false_when_absent() {
        let dir = scratch_dir("marker-absent");
        let marker = dir.join("flatpak-info");
        assert!(!marker_present(&marker));
    }

    #[test]
    fn autostart_contents_run_hidden_via_flatpak() {
        let s = autostart_desktop_contents();
        assert!(s.starts_with("[Desktop Entry]\n"));
        assert!(s.contains("Exec=flatpak run io.github.steeb_k.SeedSync --hidden"));
        assert!(s.contains("Type=Application\n"));
    }

    #[test]
    fn autostart_entry_written_and_idempotent() {
        let dir = scratch_dir("autostart-write");
        let _ = std::fs::remove_dir_all(&dir);
        write_autostart_entry_in(&dir).unwrap();
        let path = dir.join(format!("{APP_ID}.desktop"));
        let first = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first, autostart_desktop_contents());
        // A second call is a no-op write of the same content, not an error.
        write_autostart_entry_in(&dir).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first, second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn socket_reachable_false_for_missing_socket() {
        let dir = scratch_dir("socket-missing");
        assert!(!socket_reachable(&dir.join("seed.sock")));
    }
}
