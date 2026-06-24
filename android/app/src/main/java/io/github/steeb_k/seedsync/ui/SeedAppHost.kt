package io.github.steeb_k.seedsync.ui

import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import io.github.steeb_k.seedsync.engine.EngineHolder
import kotlinx.coroutines.launch
import uniffi.seed_mobile.CreatedShare
import uniffi.seed_mobile.PeerInfo
import uniffi.seed_mobile.ShareKeys
import uniffi.seed_mobile.ShareSummary

/**
 * Owns all dialog state and bridges the [AppScreen]'s [ScreenActions] to the
 * engine. Share creation/adding needs a folder, so it defers to the host
 * Activity's SAF picker via [pickFolder], then resolves to a real path before
 * calling the engine — the engine works on real paths, not content URIs.
 *
 * @param pickFolder launches ACTION_OPEN_DOCUMENT_TREE and calls back the
 *   resolved absolute path (or null if the user cancelled / the location is
 *   unreachable shared storage).
 */
@Composable
fun SeedAppHost(pickFolder: (onResolved: (String?) -> Unit) -> Unit) {
    val scope = rememberCoroutineScope()
    val snackbar = remember { SnackbarHostState() }

    var dialog by remember { mutableStateOf<Dialog?>(null) }

    fun toast(msg: String) = scope.launch { snackbar.showSnackbar(msg) }

    val actions = ScreenActions(
        onCreateShare = {
            pickFolder { path ->
                if (path == null) return@pickFolder
                scope.launch {
                    runCatching { EngineHolder.createShare(path, emptyList()) }
                        .onSuccess { created: CreatedShare ->
                            dialog = Dialog.Keys(created.masterKey, created.viewerKey, isMaster = true)
                        }
                        .onFailure { e -> toast("Create failed: ${e.message}") }
                }
            }
        },
        onAddExisting = { dialog = Dialog.AddExisting },
        onOpenSettings = { dialog = Dialog.Settings },
        onEditDeviceName = { current -> dialog = Dialog.DeviceName(current) },
        onShowNodeAddr = {
            scope.launch {
                runCatching { EngineHolder.nodeAddr() }
                    .onSuccess { dialog = Dialog.NodeAddr(it) }
                    .onFailure { toast("No address: ${it.message}") }
            }
        },
        onPauseAll = { scope.launch { runCatching { EngineHolder.pauseAll() } } },
        onResumeAll = { scope.launch { runCatching { EngineHolder.resumeAll() } } },
        onPause = { id -> scope.launch { runCatching { EngineHolder.pause(id) } } },
        onResume = { id -> scope.launch { runCatching { EngineHolder.resume(id) } } },
        onRevealKeys = { id ->
            scope.launch {
                runCatching { EngineHolder.revealKeys(id) }
                    .onSuccess { keys: ShareKeys ->
                        dialog = Dialog.Keys(keys.masterKey, keys.viewerKey, keys.masterKey != null)
                    }
                    .onFailure { e -> toast("Reveal failed: ${e.message}") }
            }
        },
        onShowPeers = { id ->
            scope.launch {
                runCatching { EngineHolder.peers(id) }
                    .onSuccess { p: List<PeerInfo> -> dialog = Dialog.Peers(p) }
                    .onFailure { e -> toast("Peers failed: ${e.message}") }
            }
        },
        onRemove = { share -> dialog = Dialog.Remove(share) },
    )

    AppScreenWithSnackbar(actions, snackbar)

    when (val d = dialog) {
        null -> {}
        is Dialog.Keys -> KeysDialog(d.masterKey, d.viewerKey, d.isMaster) { dialog = null }
        is Dialog.NodeAddr -> NodeAddrDialog(d.addr) { dialog = null }
        is Dialog.Peers -> PeersDialog(d.peers) { dialog = null }
        is Dialog.Settings -> SettingsDialog(onDismiss = { dialog = null })
        is Dialog.AddExisting -> AddExistingDialog(
            onDismiss = { dialog = null },
            onPickFolder = { onResolved -> pickFolder(onResolved) },
            onConfirm = { key, folder, bootstrap ->
                dialog = null
                scope.launch {
                    runCatching { EngineHolder.addShare(key, folder, bootstrap?.ifBlank { null }) }
                        .onFailure { toast("Add failed: ${it.message}") }
                }
            }
        )
        is Dialog.DeviceName -> DeviceNameDialog(d.current,
            onDismiss = { dialog = null },
            onConfirm = { name ->
                dialog = null
                scope.launch { runCatching { EngineHolder.setDeviceName(name) } }
            }
        )
        is Dialog.Remove -> RemoveDialog(d.share,
            onDismiss = { dialog = null },
            onConfirm = { deleteFiles ->
                dialog = null
                scope.launch {
                    runCatching { EngineHolder.removeShare(d.share.shareId, deleteFiles) }
                        .onFailure { toast("Remove failed: ${it.message}") }
                }
            }
        )
    }
}

@Composable
private fun AppScreenWithSnackbar(actions: ScreenActions, snackbar: SnackbarHostState) {
    // AppScreen owns its own Scaffold; render the snackbar host as an overlay.
    AppScreen(actions)
    SnackbarHost(hostState = snackbar)
}

/** The dialog currently open, if any. */
sealed interface Dialog {
    data class Keys(val masterKey: String?, val viewerKey: String, val isMaster: Boolean) : Dialog
    data class NodeAddr(val addr: String) : Dialog
    data class Peers(val peers: List<PeerInfo>) : Dialog
    data object AddExisting : Dialog
    data object Settings : Dialog
    data class DeviceName(val current: String) : Dialog
    data class Remove(val share: ShareSummary) : Dialog
}
