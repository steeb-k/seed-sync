package io.github.steeb_k.seedsync

import android.app.Application

class SeedApp : Application() {
    override fun onCreate() {
        super.onCreate()
        // Wire up the Android trust store for iroh's relay TLS before any engine
        // component (service / activity) can start a connection.
        RustlsSetup.ensureInitialized(this)
    }
}
