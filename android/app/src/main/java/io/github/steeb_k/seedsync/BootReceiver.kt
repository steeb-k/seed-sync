package io.github.steeb_k.seedsync

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import io.github.steeb_k.seedsync.engine.EngineService
import io.github.steeb_k.seedsync.util.StoragePerms

/**
 * Restart the sync service after a reboot, so an always-on device keeps syncing
 * without the user reopening the app. Only fires if All-Files Access is granted
 * (the engine needs real paths); otherwise the user must open the app to grant it.
 */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val action = intent.action ?: return
        if (action == Intent.ACTION_BOOT_COMPLETED ||
            action == Intent.ACTION_LOCKED_BOOT_COMPLETED
        ) {
            if (StoragePerms.hasAllFilesAccess()) {
                EngineService.start(context.applicationContext)
            }
        }
    }
}
