package io.github.steeb_k.seedsync.engine

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.lifecycle.LifecycleService
import androidx.lifecycle.lifecycleScope
import io.github.steeb_k.seedsync.MainActivity
import io.github.steeb_k.seedsync.R
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.launch

/**
 * Foreground service that hosts the engine for the life of the process — the
 * Android equivalent of the desktop daemon. The persistent notification is the
 * system-tray replacement, showing live throughput.
 */
class EngineService : LifecycleService() {

    override fun onCreate() {
        super.onCreate()
        createChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        super.onStartCommand(intent, flags, startId)
        startForegroundCompat(buildNotification("Starting…"))
        EngineHolder.start(applicationContext)
        // Apply the Wi-Fi-only / charging-only policy for the engine's lifetime.
        SyncGate.bind(applicationContext)

        // Keep the notification's throughput line live.
        lifecycleScope.launch {
            EngineHolder.shares.combine(EngineHolder.throughput) { shares, tp -> shares to tp }
                .collect { (shares, tp) ->
                    val line = "↓ ${humanRate(tp.downBps)}  ↑ ${humanRate(tp.upBps)} · ${shares.size} share(s)"
                    notificationManager().notify(NOTIF_ID, buildNotification(line))
                }
        }
        // If the service is killed and restarted by the system, re-establish.
        return START_STICKY
    }

    override fun onDestroy() {
        SyncGate.unbind()
        super.onDestroy()
    }

    private fun startForegroundCompat(n: Notification) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIF_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            startForeground(NOTIF_ID, n)
        }
    }

    private fun buildNotification(text: String): Notification {
        val tap = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle(getString(R.string.notif_title))
            .setContentText(text)
            .setSmallIcon(android.R.drawable.stat_sys_upload_done)
            .setOngoing(true)
            .setContentIntent(tap)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .build()
    }

    private fun createChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val ch = NotificationChannel(
                CHANNEL_ID,
                getString(R.string.notif_channel_sync),
                NotificationManager.IMPORTANCE_LOW
            )
            notificationManager().createNotificationChannel(ch)
        }
    }

    private fun notificationManager() =
        getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

    companion object {
        private const val CHANNEL_ID = "seedsync.sync"
        private const val NOTIF_ID = 1

        fun start(context: Context) {
            val i = Intent(context, EngineService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(i)
            } else {
                context.startService(i)
            }
        }

        private fun humanRate(bps: ULong): String {
            val b = bps.toDouble()
            return when {
                b >= 1_000_000 -> String.format("%.1f MB/s", b / 1_000_000)
                b >= 1_000 -> String.format("%.0f KB/s", b / 1_000)
                else -> "$bps B/s"
            }
        }
    }
}
