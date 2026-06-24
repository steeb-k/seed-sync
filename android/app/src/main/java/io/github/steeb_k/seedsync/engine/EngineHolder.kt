package io.github.steeb_k.seedsync.engine

import android.content.Context
import android.os.Environment
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.seed_mobile.CreatedShare
import uniffi.seed_mobile.EventListener
import uniffi.seed_mobile.MobileEngine
import uniffi.seed_mobile.PeerInfo
import uniffi.seed_mobile.ShareKeys
import uniffi.seed_mobile.ShareSummary
import java.io.File

/**
 * Process-wide owner of the one [MobileEngine]. The foreground [EngineService]
 * starts it; the Compose UI observes the [StateFlow]s here and issues mutations
 * through the suspend wrappers. Mirrors the desktop split where the GUI is a
 * thin client of the daemon — except here engine and UI share a process, so the
 * "IPC" is direct method calls plus the [EventListener] callback.
 */
object EngineHolder {
    private const val TAG = "EngineHolder"

    private val _shares = MutableStateFlow<List<ShareSummary>>(emptyList())
    val shares: StateFlow<List<ShareSummary>> = _shares.asStateFlow()

    private val _throughput = MutableStateFlow(Throughput(0u, 0u))
    val throughput: StateFlow<Throughput> = _throughput.asStateFlow()

    private val _deviceName = MutableStateFlow("")
    val deviceName: StateFlow<String> = _deviceName.asStateFlow()

    private val _running = MutableStateFlow(false)
    val running: StateFlow<Boolean> = _running.asStateFlow()

    /** True when the global user pause switch is set (drives the notification toggle). */
    private val _pausedAll = MutableStateFlow(false)
    val pausedAll: StateFlow<Boolean> = _pausedAll.asStateFlow()

    // Engine calls block the calling thread (they `block_on` the Rust runtime),
    // so funnel them through an IO scope and never touch them from the main thread.
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    @Volatile
    private var engine: MobileEngine? = null

    data class Throughput(val downBps: ULong, val upBps: ULong)

    /** Internal-storage dir for `state.db`, `node.key`, `docs/`. */
    fun dataDir(context: Context): File = File(context.filesDir, "engine").apply { mkdirs() }

    /**
     * Blob store dir on *shared* storage, co-located with the synced folders so
     * the engine's zero-copy reference export (rename/hardlink) stays on one
     * volume. Requires All-Files Access to create.
     */
    fun blobsDir(): File =
        File(Environment.getExternalStorageDirectory(), "SeedSync/blobs").apply { mkdirs() }

    /** Idempotent: boot the engine once and seed the initial UI state. */
    @Synchronized
    fun start(context: Context) {
        if (engine != null) return
        val data = dataDir(context).absolutePath
        val blobs = blobsDir().absolutePath
        Log.i(TAG, "starting engine data=$data blobs=$blobs")
        val listener = object : EventListener {
            override fun onShareListChanged() = refreshShares()
            override fun onThroughput(downBps: ULong, upBps: ULong) {
                _throughput.value = Throughput(downBps, upBps)
            }
            override fun onLastUpdated(shareId: String, ts: Long) = refreshShares()
        }
        engine = MobileEngine.init(data, blobs, listener)
        _running.value = true
        refreshShares()
        scope.launch { _deviceName.value = engine?.deviceName() ?: "" }
    }

    @Synchronized
    fun stop() {
        engine?.shutdown()
        engine?.close()
        engine = null
        _running.value = false
    }

    fun isStarted(): Boolean = engine != null

    private fun refreshShares() {
        scope.launch {
            engine?.let {
                _shares.value = it.listShares()
                _pausedAll.value = it.pausedAll()
            }
        }
    }

    // --- mutations (suspend; run off the main thread) ---

    suspend fun createShare(folder: String, ignore: List<String>): CreatedShare =
        withContext(Dispatchers.IO) { requireEngine().createShare(folder, ignore) }

    suspend fun addShare(key: String, folder: String, bootstrap: String?): String =
        withContext(Dispatchers.IO) { requireEngine().addShare(key, folder, bootstrap) }

    suspend fun revealKeys(shareId: String): ShareKeys =
        withContext(Dispatchers.IO) { requireEngine().revealKeys(shareId) }

    suspend fun peers(shareId: String): List<PeerInfo> =
        withContext(Dispatchers.IO) { requireEngine().getPeers(shareId) }

    suspend fun nodeAddr(): String =
        withContext(Dispatchers.IO) { requireEngine().nodeAddr() }

    suspend fun pause(shareId: String) = withContext(Dispatchers.IO) { requireEngine().pause(shareId) }
    suspend fun resume(shareId: String) = withContext(Dispatchers.IO) { requireEngine().resume(shareId) }
    suspend fun pauseAll() = withContext(Dispatchers.IO) { requireEngine().pauseAll() }
    suspend fun resumeAll() = withContext(Dispatchers.IO) { requireEngine().resumeAll() }

    suspend fun removeShare(shareId: String, deleteFiles: Boolean) =
        withContext(Dispatchers.IO) { requireEngine().removeShare(shareId, deleteFiles) }

    suspend fun setDeviceName(name: String) = withContext(Dispatchers.IO) {
        requireEngine().setDeviceName(name)
        _deviceName.value = name
    }

    /// Apply the host network/charging gate. Fire-and-forget on the IO scope so
    /// callbacks (network/charging changes) never block; no-op if not started yet.
    fun setSyncSuspended(suspended: Boolean) {
        scope.launch { engine?.setSyncSuspended(suspended) }
    }

    private fun requireEngine(): MobileEngine =
        engine ?: error("engine not started")
}
