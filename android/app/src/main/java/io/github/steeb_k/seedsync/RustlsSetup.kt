package io.github.steeb_k.seedsync

import android.content.Context
import android.util.Log

/**
 * Initializes `rustls-platform-verifier` (used by iroh's relay TLS) with the
 * app's JVM Context, via a native method implemented in `libseed_mobile.so`.
 * Must run once, before the engine opens any TLS connection — otherwise the
 * first handshake fails with "android context was not initialized".
 *
 * The Kotlin `CertificateVerifier` support class that the native side calls back
 * into is provided by the `rustls:rustls-platform-verifier` AAR (sourced from
 * the rustls-platform-verifier-android crate's local Maven repo; see
 * settings.gradle.kts).
 */
object RustlsSetup {
    @Volatile
    private var initialized = false

    private external fun initRustlsPlatformVerifier(context: Context)

    @Synchronized
    fun ensureInitialized(context: Context) {
        if (initialized) return
        // libseed_mobile is also loaded lazily by the UniFFI bindings; loading it
        // here first guarantees the native symbol is resolvable before we call it.
        System.loadLibrary("seed_mobile")
        initRustlsPlatformVerifier(context.applicationContext)
        initialized = true
        Log.i("RustlsSetup", "android context registered (ndk-context + rustls verifier)")
    }
}
