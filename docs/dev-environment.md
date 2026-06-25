# Local cross-platform dev environment (one Windows box)

This documents a single **Windows 11** machine set up to build, run, and package
SEED Sync for **Windows, Linux, and Android** — without switching machines or
spending CI minutes. (macOS still needs a Mac; see `docs/macos-packaging.md`.)

- **Windows** — native MSVC build + GTK4 via gvsbuild + WiX 5 MSI.
- **Linux** — WSL2 (Ubuntu 24.04) with the GTK GUI rendered through WSLg, plus an
  optional KDE Plasma desktop for system-tray testing.
- **Android** — native Windows toolchain (JDK 17 + Android SDK/NDK + `cargo-ndk`),
  run on the emulator (WHPX) or a USB device.

Two helper scripts drive the run/test loops and **self-check the environment**,
pointing back here if something's missing:

| Script | Runs | From |
| --- | --- | --- |
| `scripts/run-android.ps1` | build + install + launch the APK (emulator or device) | Windows PowerShell |
| `scripts/run-linux.sh` | build + launch the GTK GUI | inside WSL (or any Linux) |

---

## ⚠️ Smart App Control must be OFF

Windows 11 **Smart App Control (SAC)** blocks execution of freshly-compiled,
unsigned executables — which is exactly what `cargo`/`cargo-ndk` produce when they
run build scripts. With SAC on, every build dies with:

```
error: failed to run custom build command for `libsqlite3-sys ...`
  An Application Control policy has blocked this file. (os error 4551)
```

SAC was turned **off** on this box (Windows Security → App & browser control →
Smart App Control → Off). This is **irreversible** without resetting Windows. If
native builds ever start failing with `os error 4551`, SAC got re-enabled.

Check it:
```powershell
(Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy").VerifiedAndReputablePolicyState
# 0 = OFF (good), 1 = ON (blocks builds), 2 = Evaluation
```

---

## 1. Windows (native)

**Installed:** Rust (stable-msvc), Visual Studio 2026 C++ tools, .NET 8, Git, gh,
**PowerShell 7** (the repo's `.ps1` scripts need it), WiX 5 (`dotnet tool install
--global wix --version "5.*"`), and the GTK4 stack.

**GTK4 (gvsbuild prebuilt):** extracted to `C:\gtk`. Persisted user env vars:
```
PKG_CONFIG_PATH = C:\gtk\lib\pkgconfig
LIB             = C:\gtk\lib;<existing>
PATH           += C:\gtk\bin        (also needed at runtime so seed-gui.exe finds the GTK DLLs)
```

**Build + run:**
```powershell
cargo build --release
# smoke test (two terminals):
target\release\seed-daemon.exe run
target\release\seed-gui.exe
```

**MSI (WiX 5):**
```powershell
pwsh -File scripts\build-msi.ps1 -SkipBuild
#   -> target\wix\seed-sync-<ver>-windows-x86_64.msi
```
Local builds are **unsigned** (Azure Trusted Signing isn't set up here). The repo
ships `artifact-signing-metadata.json`, so `build-msi.ps1` will *try* to sign and
fail on the missing client tools — force an unsigned build by pointing the metadata
env var at a nonexistent path:
```powershell
$env:ARTIFACT_SIGNING_METADATA = "C:\__no_signing__\none.json"
pwsh -File scripts\build-msi.ps1 -SkipBuild
```

---

## 2. Linux (WSL2 + Ubuntu 24.04)

**Installed:** WSL2 with **Ubuntu 24.04** (ships GTK 4.14 / libadwaita 1.5 — clears
the `v4_10`/`v1_4` features; 22.04's GTK 4.6 does not). User `steeb` has passwordless
sudo. GTK dev packages: `libgtk-4-dev libadwaita-1-dev libdbus-1-dev` + `build-essential
pkg-config imagemagick`. A separate Linux `rustup` toolchain.

**Where the code lives:** clone into the **WSL filesystem** (`~/seed-sync-gtk`), not
`/mnt/c` — `/mnt/c` is slow for cargo and trips git's dubious-ownership guard. The
clone was made from the Windows working tree:
```bash
git config --global --add safe.directory /mnt/c/Users/steeb-ai/seed-sync-gtk
git clone /mnt/c/Users/steeb-ai/seed-sync-gtk ~/seed-sync-gtk
```

### Build + run the GTK GUI
From inside WSL, in `~/seed-sync-gtk`:
```bash
bash scripts/run-linux.sh            # checks env, builds, launches daemon + GUI
bash scripts/run-linux.sh --skip-build   # launch without rebuilding
```
The window renders on your Windows desktop through **WSLg**. To run it from a Windows
terminal in one shot:
```powershell
wsl -d Ubuntu-24.04 -- bash -lc "cd ~/seed-sync-gtk && bash scripts/run-linux.sh"
```

**Rendering:** WSL has no hardware GL via EGL (no DRM render node), and GTK4 only uses
EGL/Vulkan — so the GUI runs on the **llvmpipe software renderer** (the script sets
`GSK_RENDERER=cairo` under WSL for a clean window). The RTX 3080 *is* reachable via
GLX/d3d12, but GTK4 won't use GLX. For a 2D app this is plenty fast. True
GPU-accelerated Linux GUI testing needs bare metal / a physical Linux box.

### Linux tarball
```bash
bash scripts/package-linux.sh        # -> dist/seed-sync-<ver>-linux-x86_64.tar.gz
```

### Full desktop for tray testing (KDE Plasma)
WSLg renders individual windows but has **no system tray** (no `StatusNotifierWatcher`),
so the app's ksni tray / close-to-tray / single-instance-reveal can't be exercised under
plain WSLg. A KDE Plasma desktop + xrdp is installed for that:

- xrdp listens on **port 3390** (the Windows host owns 3389) and **auto-starts**
  (systemd units enabled). Session launches Plasma X11 via `~/.xsession`.
- **Mirrored networking** is enabled (`%USERPROFILE%\.wslconfig`:
  `[wsl2] networkingMode=mirrored`) so `localhost:3390` reaches WSL. A Hyper-V firewall
  allow-rule for inbound TCP 3390 was added (mirrored mode defaults to block).
- Connect with **Windows Remote Desktop** to `localhost:3390` as `steeb`. A saved
  profile is on the Desktop: **`SEED-KDE.rdp`** (check "Remember me" once for auto-login).
- WSL idle-shuts-down, which stops xrdp. Use **`Connect-SEED-KDE.cmd`** on the Desktop
  (boots WSL, waits, then opens the profile), or enable an always-on keep-alive task:
  ```powershell
  schtasks /Create /TN "WSL Keep-Alive (SEED KDE)" /TR "wscript.exe `"$env:LOCALAPPDATA\wsl-keepalive.vbs`"" /SC ONLOGON /RL LIMITED /F
  ```

Inside the KDE session, run the app and watch the panel tray:
```bash
bash ~/seed-sync-gtk/scripts/run-linux.sh
```

---

## 3. Android (native Windows toolchain)

**Installed:** Temurin **JDK 17** (`JAVA_HOME`), Android **SDK** at `C:\Android\Sdk`
(`ANDROID_HOME`) with platform-tools, `platforms;android-35`, `build-tools;35.0.0`,
the **emulator** + `system-images;android-35;google_apis;x86_64`, and **NDK r27c**
(`C:\Android\Sdk\ndk\27.2.12479018`, `ANDROID_NDK_HOME`). Plus `cargo-ndk` and the
three Rust targets:
```
aarch64-linux-android  armv7-linux-androideabi  x86_64-linux-android
```
An emulator AVD **`seed_api35`** (API 35, x86_64) was created; it accelerates via
**WHPX** (Windows Hypervisor Platform, enabled alongside WSL2's hypervisor).

### Build + run
```powershell
# emulator (default): build debug APK, boot seed_api35, install, launch
pwsh -File scripts\run-android.ps1

# physical device over USB (enable Developer Options + USB debugging first)
pwsh -File scripts\run-android.ps1 -Device

# signed release APK (needs android\keystore.properties — see below)
pwsh -File scripts\run-android.ps1 -Release

# install the existing APK without rebuilding
pwsh -File scripts\run-android.ps1 -SkipBuild
```
On first launch the app requests **All-files access** (a system Settings page opens) —
toggle it on, then back to the app.

Manual equivalents:
```powershell
cd android
.\gradlew.bat assembleDebug      # -> app\build\outputs\apk\debug\app-debug.apk
adb install -r app\build\outputs\apk\debug\app-debug.apk
adb shell am start -n io.github.steeb_k.seedsync/.MainActivity
```

### Signed release APK
The signing key is **irreplaceable** — reuse the existing one, never generate a new
key, or existing installs can't update. Place your key + create the props file (both
gitignored):
```
android\keystore\seedsync-release.jks
android\keystore.properties:
    storeFile=keystore/seedsync-release.jks
    storePassword=...
    keyAlias=seedsync
    keyPassword=...
```
Then `pwsh -File scripts\run-android.ps1 -Release`, or `cd android; .\gradlew.bat
assembleRelease`. Verify the signature:
```powershell
& "$env:ANDROID_HOME\build-tools\35.0.0\apksigner.bat" verify --print-certs `
  android\app\build\outputs\apk\release\app-release.apk
```

---

## Troubleshooting

- **`os error 4551` on any build** → Smart App Control got re-enabled (see top).
- **`.rdp` won't connect / `localhost:3390` refused** → WSL idle-shut-down; run
  `Connect-SEED-KDE.cmd` or `wsl -d Ubuntu-24.04 -- true` first. Confirm the WSL IP
  with `wsl hostname -I`.
- **GTK build can't find gtk4** (Windows) → open a fresh shell so the persisted
  `PKG_CONFIG_PATH`/`LIB`/`PATH` are loaded, or set them for the session.
- **Gradle "conflicting declarations" (UniFFI)** → `cd android; .\gradlew.bat clean`.
- **Emulator slow / `-accel-check` fails** → enable "Windows Hypervisor Platform"
  optional feature (needs a reboot).
- **`git clone` "dubious ownership"** in WSL → `git config --global --add
  safe.directory <path>`.
