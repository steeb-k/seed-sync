package io.github.steeb_k.seedsync.engine

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.os.BatteryManager
import android.util.Log
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * Applies the user's "sync only on Wi-Fi" / "sync only while charging" policy to
 * the engine. These are two independent toggles; either, both, or neither may be
 * enabled. While a toggle's condition is unmet, the engine is told to suspend
 * sync (`MobileEngine.setSyncSuspended`) — distinct from the user's pause state,
 * so toggling network/charger resumes whatever the user hasn't explicitly paused.
 *
 * The [EngineService] [bind]s the gate for the engine's lifetime; it watches the
 * default network (Wi-Fi vs cellular) and the charger, and recomputes on any
 * change to a condition or a setting. Settings persist in SharedPreferences.
 */
object SyncGate {
    private const val TAG = "SyncGate"
    private const val PREFS = "sync_prefs"
    private const val KEY_WIFI_ONLY = "wifi_only"
    private const val KEY_CHARGING_ONLY = "charging_only"

    private val _wifiOnly = MutableStateFlow(false)
    val wifiOnly: StateFlow<Boolean> = _wifiOnly.asStateFlow()

    private val _chargingOnly = MutableStateFlow(false)
    val chargingOnly: StateFlow<Boolean> = _chargingOnly.asStateFlow()

    /** Null when sync is running; otherwise a short reason for the UI banner. */
    private val _suspendReason = MutableStateFlow<String?>(null)
    val suspendReason: StateFlow<String?> = _suspendReason.asStateFlow()

    // Live conditions, updated by the callbacks below.
    @Volatile private var onWifi = false
    @Volatile private var charging = false

    private var appContext: Context? = null
    private var connectivity: ConnectivityManager? = null
    private var netCallback: ConnectivityManager.NetworkCallback? = null
    private var powerReceiver: BroadcastReceiver? = null

    @Synchronized
    fun bind(context: Context) {
        if (appContext != null) return
        val ctx = context.applicationContext
        appContext = ctx
        val prefs = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        _wifiOnly.value = prefs.getBoolean(KEY_WIFI_ONLY, false)
        _chargingOnly.value = prefs.getBoolean(KEY_CHARGING_ONLY, false)

        val cm = ctx.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        connectivity = cm
        onWifi = isOnWifi(cm)
        val cb = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                onWifi = isOnWifi(cm); recompute()
            }
            override fun onLost(network: Network) {
                onWifi = isOnWifi(cm); recompute()
            }
            override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) {
                onWifi = caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) ||
                    caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET)
                recompute()
            }
        }
        netCallback = cb
        cm.registerDefaultNetworkCallback(cb)

        // Initial charging state from the sticky battery broadcast.
        val batteryStatus = ctx.registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
        charging = isCharging(batteryStatus?.getIntExtra(BatteryManager.EXTRA_STATUS, -1) ?: -1)
        val pr = object : BroadcastReceiver() {
            override fun onReceive(c: Context?, intent: Intent?) {
                charging = when (intent?.action) {
                    Intent.ACTION_POWER_CONNECTED -> true
                    Intent.ACTION_POWER_DISCONNECTED -> false
                    else -> charging
                }
                recompute()
            }
        }
        powerReceiver = pr
        ctx.registerReceiver(pr, IntentFilter().apply {
            addAction(Intent.ACTION_POWER_CONNECTED)
            addAction(Intent.ACTION_POWER_DISCONNECTED)
        })

        recompute()
    }

    @Synchronized
    fun unbind() {
        val ctx = appContext ?: return
        netCallback?.let { connectivity?.unregisterNetworkCallback(it) }
        powerReceiver?.let { runCatching { ctx.unregisterReceiver(it) } }
        netCallback = null
        powerReceiver = null
        connectivity = null
        appContext = null
    }

    fun setWifiOnly(value: Boolean) {
        _wifiOnly.value = value
        appContext?.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            ?.edit()?.putBoolean(KEY_WIFI_ONLY, value)?.apply()
        recompute()
    }

    fun setChargingOnly(value: Boolean) {
        _chargingOnly.value = value
        appContext?.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            ?.edit()?.putBoolean(KEY_CHARGING_ONLY, value)?.apply()
        recompute()
    }

    /** Combine settings + live conditions, push the result to the engine. */
    private fun recompute() {
        val needWifi = _wifiOnly.value && !onWifi
        val needCharger = _chargingOnly.value && !charging
        val suspend = needWifi || needCharger
        val reason = when {
            needWifi && needCharger -> "Sync paused — waiting for Wi-Fi and charger"
            needWifi -> "Sync paused — waiting for Wi-Fi"
            needCharger -> "Sync paused — waiting for charger"
            else -> null
        }
        _suspendReason.value = reason
        Log.i(TAG, "gate: onWifi=$onWifi charging=$charging -> suspend=$suspend")
        EngineHolder.setSyncSuspended(suspend)
    }

    private fun isOnWifi(cm: ConnectivityManager): Boolean {
        val caps = cm.getNetworkCapabilities(cm.activeNetwork) ?: return false
        return caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) ||
            caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET)
    }

    private fun isCharging(status: Int): Boolean =
        status == BatteryManager.BATTERY_STATUS_CHARGING ||
            status == BatteryManager.BATTERY_STATUS_FULL
}
