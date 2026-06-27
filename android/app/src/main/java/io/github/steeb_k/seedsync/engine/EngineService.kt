package io.github.steeb_k.seedsync.engine

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.lifecycle.LifecycleService
import androidx.lifecycle.lifecycleScope
import io.github.steeb_k.seedsync.MainActivity
import io.github.steeb_k.seedsync.R
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.launch
import uniffi.seed_mobile.ShareSummary

/**
 * Foreground service that hosts the engine for the life of the process — the
 * Android equivalent of the desktop daemon. The persistent notification is the
 * system-tray replacement: it shows a live sync-state subtitle and a single
 * context-aware Pause/Resume action that drives the global user pause.
 */
class EngineService : LifecycleService() {

    private var started = false

    /** Held while the engine runs so Android delivers inbound mDNS multicast,
     * which the iroh endpoint's local-network discovery needs to find LAN peers.
     * Without it the device can advertise but never receives others' responses. */
    private var multicastLock: WifiManager.MulticastLock? = null

    override fun onCreate() {
        super.onCreate()
        createChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        super.onStartCommand(intent, flags, startId)
        // Action intents come from the notification's Pause/Resume button (the
        // service is already running/foreground when they fire). Everything else
        // is a normal start. The notification's subtitle/label refresh via the
        // flow below once the engine's pause state changes.
        when (intent?.action) {
            ACTION_PAUSE -> {
                ensureStarted()
                lifecycleScope.launch { runCatching { EngineHolder.pauseAll() } }
            }
            ACTION_RESUME -> {
                ensureStarted()
                lifecycleScope.launch { runCatching { EngineHolder.resumeAll() } }
            }
            else -> ensureStarted()
        }
        // If the service is killed and restarted by the system, re-establish.
        return START_STICKY
    }

    /** One-time engine startup + notification refresh loop (guarded so re-entry
     * from action intents or a sticky restart doesn't double-start). */
    private fun ensureStarted() {
        if (started) return
        started = true
        startForegroundCompat(buildNotification("Starting…", paused = false))
        acquireMulticastLock()
        EngineHolder.start(applicationContext)
        // Apply the Wi-Fi-only / charging-only policy for the engine's lifetime.
        SyncGate.bind(applicationContext)

        // Keep the notification's subtitle + Pause/Resume label live. Combines the
        // share list, throughput, the global pause switch, and the network/charging
        // gate into one sync-state line.
        lifecycleScope.launch {
            combine(
                EngineHolder.shares,
                EngineHolder.throughput,
                EngineHolder.pausedAll,
                SyncGate.state,
            ) { shares, tp, pausedAll, gate ->
                NotifModel(syncStateText(shares, pausedAll, gate, tp), pausedAll)
            }.collect { m ->
                notificationManager().notify(NOTIF_ID, buildNotification(m.text, m.paused))
            }
        }
    }

    override fun onDestroy() {
        SyncGate.unbind()
        releaseMulticastLock()
        super.onDestroy()
    }

    /** Acquire the Wi-Fi multicast lock so inbound mDNS reaches the engine.
     * Best-effort: a device with no Wi-Fi service, or a network that blocks
     * multicast, just means no LAN discovery — never a fatal error. */
    private fun acquireMulticastLock() {
        if (multicastLock != null) return
        runCatching {
            val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
            wifi.createMulticastLock("seedsync.mdns").apply {
                setReferenceCounted(false)
                acquire()
                multicastLock = this
            }
        }.onFailure { Log.w(TAG, "could not acquire multicast lock for LAN discovery", it) }
    }

    private fun releaseMulticastLock() {
        multicastLock?.let { lock ->
            runCatching { if (lock.isHeld) lock.release() }
        }
        multicastLock = null
    }

    private fun startForegroundCompat(n: Notification) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIF_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            startForeground(NOTIF_ID, n)
        }
    }

    private data class NotifModel(val text: String, val paused: Boolean)

    /** The sync-state subtitle, in priority order. */
    private fun syncStateText(
        shares: List<ShareSummary>,
        pausedAll: Boolean,
        gate: SyncGate.GateState,
        tp: EngineHolder.Throughput,
    ): String = when {
        pausedAll -> "All shares paused"
        gate.needWifi && gate.needCharger -> "Waiting for Wi-Fi and charger"
        gate.needWifi -> "Waiting for Wi-Fi"
        gate.needCharger -> "Waiting for charger"
        shares.isEmpty() -> "No shares yet"
        shares.any { it.paused } -> "One or more shares paused"
        else -> "Syncing all shares · ↓ ${humanRate(tp.downBps)}  ↑ ${humanRate(tp.upBps)}"
    }

    private fun buildNotification(stateText: String, paused: Boolean): Notification {
        val tap = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        val toggle = if (paused) {
            toggleAction("Resume", ACTION_RESUME, android.R.drawable.ic_media_play)
        } else {
            toggleAction("Pause", ACTION_PAUSE, android.R.drawable.ic_media_pause)
        }
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(stateText)
            .setSmallIcon(android.R.drawable.stat_sys_upload_done)
            .setOngoing(true)
            .setContentIntent(tap)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .addAction(toggle)
            .build()
    }

    private fun toggleAction(label: String, action: String, icon: Int): NotificationCompat.Action {
        val pi = PendingIntent.getService(
            this,
            action.hashCode(),
            Intent(this, EngineService::class.java).setAction(action),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        return NotificationCompat.Action.Builder(icon, label, pi).build()
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

    private fun humanRate(bps: ULong): String {
        val b = bps.toDouble()
        return when {
            b >= 1_000_000 -> String.format("%.1f MB/s", b / 1_000_000)
            b >= 1_000 -> String.format("%.0f KB/s", b / 1_000)
            else -> "$bps B/s"
        }
    }

    companion object {
        private const val TAG = "EngineService"
        private const val CHANNEL_ID = "seedsync.sync"
        private const val NOTIF_ID = 1
        private const val ACTION_PAUSE = "io.github.steeb_k.seedsync.action.PAUSE"
        private const val ACTION_RESUME = "io.github.steeb_k.seedsync.action.RESUME"

        fun start(context: Context) {
            val i = Intent(context, EngineService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(i)
            } else {
                context.startService(i)
            }
        }
    }
}
