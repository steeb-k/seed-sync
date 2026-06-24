# Android packaging & signing

The Android app lives in `android/` (Gradle) and wraps the shared `seed-core`
engine through the `crates/seed-mobile` UniFFI facade. This doc covers building
and **signing** a release APK locally. For the engine/UniFFI design see
[`android-app.md`](../android-app.md).

## Toolchain (one-time)

Local builds need, in addition to the Rust workspace:

- **Android SDK** (platform-tools, `platforms;android-35`, `build-tools;35.0.0`).
- **Android NDK r27c** — used by `cargo-ndk` to cross-compile `seed-mobile`.
- **JDK 17** — for Gradle and `keytool`/`apksigner`.
- **Gradle 8.9** wrapper (committed under `android/gradle/`).
- **cargo-ndk** (`cargo install cargo-ndk`) and the Rust Android targets:
  `rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android`.

On the maintainer's Windows box these live at:

| Tool | Path |
| --- | --- |
| NDK r27c | `C:\Android\android-ndk-r27c` |
| SDK | `C:\Android\Sdk` |
| JDK 17 | `C:\Android\jdk-17.0.13+11` |

Point the build at them via env vars before invoking Gradle:

```pwsh
$env:JAVA_HOME       = "C:\Android\jdk-17.0.13+11"
$env:ANDROID_HOME    = "C:\Android\Sdk"
$env:ANDROID_NDK_HOME = "C:\Android\android-ndk-r27c"
```

The Gradle build runs `cargo-ndk` (per-ABI `.so` into `app/src/main/jniLibs/`)
and generates the UniFFI Kotlin bindings from the **host** library into
`app/src/main/java/uniffi/` — see the `cargoNdkBuild` / `cargoHostBuild` /
`uniffiBindgen` tasks in `app/build.gradle.kts`. Both dirs are gitignored and
regenerated every build.

> **Gotcha:** regenerating the UniFFI bindings can confuse Kotlin's incremental
> compiler ("conflicting declarations"). If you see that, run `gradlew clean`.

## Signing (Option 1: self-signed release keystore)

Android does **not** use Authenticode/Azure Trusted Signing (that's Windows
only). APKs are signed with a Java keystore key, and Android trusts updates by
**signature continuity** (the same key must sign every update — there's no CA
chain to validate). A self-signed key is the norm for sideload/F-Droid.

### The key

A 4096-bit RSA key, alias `seedsync`, 10000-day validity, was generated with:

```pwsh
keytool -genkeypair -v -keystore android\keystore\seedsync-release.jks `
  -alias seedsync -keyalg RSA -keysize 4096 -validity 10000 `
  -dname "CN=SEED Sync, O=kznjk LLC, C=US"
```

Build credentials are read from **`android/keystore.properties`** (gitignored):

```properties
storeFile=keystore/seedsync-release.jks
storePassword=<store password>
keyAlias=seedsync
keyPassword=<key password>
```

`app/build.gradle.kts` loads this into the `release` `signingConfig`. If the
file is absent (fresh checkout, or CI without the secret), the release build is
produced **unsigned** — it won't install, by design; debug builds are unaffected.

> ### ⚠️ Back up the key — this is irreplaceable
> Both `android/keystore/seedsync-release.jks` **and** its password must be
> stored offline (password manager + an encrypted backup). **If the signing key
> is lost, the app can never be updated in place again** — every existing
> install would have to be uninstalled (losing its data) and reinstalled with a
> new key. Neither the keystore nor `keystore.properties` is in git.

## Building a release APK

```pwsh
$env:JAVA_HOME="C:\Android\jdk-17.0.13+11"; $env:ANDROID_HOME="C:\Android\Sdk"; $env:ANDROID_NDK_HOME="C:\Android\android-ndk-r27c"
cd android
.\gradlew.bat clean :app:assembleRelease
```

Output: `android/app/build/outputs/apk/release/app-release.apk` — a **single
universal APK** bundling all three ABIs (arm64-v8a, armeabi-v7a, x86_64).

Verify the signature:

```pwsh
& "C:\Android\Sdk\build-tools\35.0.0\apksigner.bat" verify --print-certs `
  android\app\build\outputs\apk\release\app-release.apk
```

Install/sideload:

```pwsh
adb install -r android\app\build\outputs\apk\release\app-release.apk
```

## Versioning

`versionName` tracks the workspace version (`Cargo.toml`). `versionCode` uses
`MAJOR*10000 + MINOR*100 + PATCH` (so **1.2.0 → 10200**) — monotonic and
decodable. Bump both in `android/app/build.gradle.kts` alongside the workspace
version bump (see [`releasing.md`](releasing.md)).

## Notes

- Distribution is sideload / GitHub releases (and later F-Droid). All-Files
  Access rules out Google Play, so there's no Play App Signing / AAB upload key.
- `minify`/R8 is **off** for now (the UniFFI + JNA bindings use reflection;
  `proguard-rules.pro` has keeps if you enable it later). The release APK is
  large (~100 MB) but installs/runs fine; shrinking it is a future optimization.
