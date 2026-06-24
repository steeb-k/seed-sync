package io.github.steeb_k.seedsync

import android.content.Context
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import io.github.steeb_k.seedsync.engine.EngineService
import io.github.steeb_k.seedsync.util.StoragePerms
import java.util.concurrent.TimeUnit

/**
 * Fallback resync: on aggressive OEM ROMs the foreground service can still be
 * killed. This periodic job (the OS schedules it within Doze windows) restarts
 * the service so sync recovers without user interaction. The foreground service
 * itself, not this worker, does the actual syncing.
 */
class ResyncWorker(context: Context, params: WorkerParameters) :
    CoroutineWorker(context, params) {

    override suspend fun doWork(): Result {
        if (StoragePerms.hasAllFilesAccess()) {
            EngineService.start(applicationContext)
        }
        return Result.success()
    }

    companion object {
        private const val WORK_NAME = "seedsync.resync"

        fun schedule(context: Context) {
            val req = PeriodicWorkRequestBuilder<ResyncWorker>(15, TimeUnit.MINUTES).build()
            WorkManager.getInstance(context).enqueueUniquePeriodicWork(
                WORK_NAME,
                ExistingPeriodicWorkPolicy.KEEP,
                req
            )
        }
    }
}
