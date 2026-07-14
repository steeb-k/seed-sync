package io.github.steeb_k.seedsync.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Edit
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material3.Card
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.steeb_k.seedsync.engine.EngineHolder
import uniffi.seed_mobile.Role
import uniffi.seed_mobile.ShareStatus
import uniffi.seed_mobile.ShareSummary

/**
 * The single top-level screen. Mirrors the GTK GUI: a share list with status
 * badges and peer-health "N of M online" counts, a throughput footer, and the
 * create / add-existing / reveal-keys / device-name / node-address flows hung
 * off the app bar and each share's overflow menu.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun AppScreen(
    actions: ScreenActions,
) {
    val shares by EngineHolder.shares.collectAsStateSafe(emptyList())
    val throughput by EngineHolder.throughput.collectAsStateSafe(EngineHolder.Throughput(0u, 0u))
    val deviceName by EngineHolder.deviceName.collectAsStateSafe("")
    val gate by io.github.steeb_k.seedsync.engine.SyncGate.state
        .collectAsStateSafe(io.github.steeb_k.seedsync.engine.SyncGate.GateState())
    val suspendReason = when {
        gate.needWifi && gate.needCharger -> "Sync paused — waiting for Wi-Fi and charger"
        gate.needWifi -> "Sync paused — waiting for Wi-Fi"
        gate.needCharger -> "Sync paused — waiting for charger"
        else -> null
    }

    var overflowOpen by remember { mutableStateOf(false) }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("SEED Sync") },
                actions = {
                    IconButton(onClick = { overflowOpen = true }) {
                        Icon(Icons.Default.MoreVert, contentDescription = "Menu")
                    }
                    DropdownMenu(expanded = overflowOpen, onDismissRequest = { overflowOpen = false }) {
                        DropdownMenuItem(
                            text = { Text("Add existing share…") },
                            onClick = { overflowOpen = false; actions.onAddExisting() }
                        )
                        DropdownMenuItem(
                            text = { Text("Settings…") },
                            onClick = { overflowOpen = false; actions.onOpenSettings() }
                        )
                        DropdownMenuItem(
                            text = { Text("Show this device's address…") },
                            onClick = { overflowOpen = false; actions.onShowNodeAddr() }
                        )
                        DropdownMenuItem(
                            text = { Text("Pause all") },
                            onClick = { overflowOpen = false; actions.onPauseAll() }
                        )
                        DropdownMenuItem(
                            text = { Text("Resume all") },
                            onClick = { overflowOpen = false; actions.onResumeAll() }
                        )
                    }
                }
            )
        },
        floatingActionButton = {
            var fabMenuOpen by remember { mutableStateOf(false) }
            Box {
                FloatingActionButton(onClick = { fabMenuOpen = true }) {
                    Icon(Icons.Default.Add, contentDescription = "Add a share")
                }
                // Sub-menu mirroring the desktop "+" popover (create vs add).
                DropdownMenu(expanded = fabMenuOpen, onDismissRequest = { fabMenuOpen = false }) {
                    DropdownMenuItem(
                        text = { Text("Create new share…") },
                        onClick = { fabMenuOpen = false; actions.onCreateShare() }
                    )
                    DropdownMenuItem(
                        text = { Text("Add existing share…") },
                        onClick = { fabMenuOpen = false; actions.onAddExisting() }
                    )
                }
            }
        },
        bottomBar = {
            ThroughputFooter(throughput, deviceName, onEdit = { actions.onEditDeviceName(deviceName) })
        }
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            suspendReason?.let { SuspendBanner(it) }
            if (shares.isEmpty()) {
                EmptyState(Modifier.weight(1f))
            } else {
                LazyColumn(
                    modifier = Modifier.fillMaxSize().weight(1f),
                    contentPadding = androidx.compose.foundation.layout.PaddingValues(12.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp)
                ) {
                    items(shares, key = { it.shareId }) { share ->
                        ShareRow(share, actions)
                    }
                }
            }
        }
    }
}

@Composable
private fun SuspendBanner(reason: String) {
    Surface(
        color = MaterialTheme.colorScheme.tertiaryContainer,
        modifier = Modifier.fillMaxWidth()
    ) {
        Text(
            reason,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onTertiaryContainer,
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 10.dp)
        )
    }
}

@Composable
private fun EmptyState(modifier: Modifier = Modifier) {
    Box(modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
        Column(horizontalAlignment = Alignment.CenterHorizontally) {
            Text("No shares yet", style = MaterialTheme.typography.titleMedium)
            Spacer(Modifier.size(8.dp))
            Text(
                "Tap + to create a new share or add an existing one from a key.",
                style = MaterialTheme.typography.bodyMedium
            )
        }
    }
}

@Composable
private fun ShareRow(share: ShareSummary, actions: ScreenActions) {
    var menuOpen by remember { mutableStateOf(false) }
    Card(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(12.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                StatusDot(share)
                Spacer(Modifier.width(8.dp))
                Column(Modifier.weight(1f)) {
                    Text(
                        share.name.ifEmpty { share.shareId.take(12) },
                        style = MaterialTheme.typography.titleMedium,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis
                    )
                    Text(
                        share.folder,
                        style = MaterialTheme.typography.bodySmall,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis
                    )
                }
                Box {
                    IconButton(onClick = { menuOpen = true }) {
                        Icon(Icons.Default.MoreVert, contentDescription = "Share menu")
                    }
                    DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                        if (share.paused) {
                            DropdownMenuItem(text = { Text("Resume") },
                                onClick = { menuOpen = false; actions.onResume(share.shareId) })
                        } else {
                            DropdownMenuItem(text = { Text("Pause") },
                                onClick = { menuOpen = false; actions.onPause(share.shareId) })
                        }
                        if (share.role == Role.MASTER) {
                            DropdownMenuItem(text = { Text("Reveal keys…") },
                                onClick = { menuOpen = false; actions.onRevealKeys(share.shareId) })
                        } else {
                            DropdownMenuItem(text = { Text("Reveal viewer key…") },
                                onClick = { menuOpen = false; actions.onRevealKeys(share.shareId) })
                        }
                        DropdownMenuItem(text = { Text("Peers…") },
                            onClick = { menuOpen = false; actions.onShowPeers(share.shareId) })
                        DropdownMenuItem(text = { Text("Remove…") },
                            onClick = { menuOpen = false; actions.onRemove(share) })
                    }
                }
            }
            Spacer(Modifier.size(6.dp))
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(statusLabel(share), style = MaterialTheme.typography.labelMedium)
                Spacer(Modifier.weight(1f))
                Text(
                    "${share.online} of ${share.total} online",
                    style = MaterialTheme.typography.labelMedium
                )
            }
            if (share.status == ShareStatus.INDEXING || share.status == ShareStatus.SYNCING) {
                Spacer(Modifier.size(6.dp))
                LinearProgressIndicator(
                    progress = { (share.percent.toInt().coerceIn(0, 100)) / 100f },
                    modifier = Modifier.fillMaxWidth()
                )
            }
        }
    }
}

@Composable
private fun StatusDot(share: ShareSummary) {
    // NO_PEERS must be matched explicitly: the `else` arm below means "syncing", and
    // letting a share that can reach nobody fall through to it would paint a total
    // partition the same amber as ordinary progress — the Android echo of the
    // "Healthy 100% while talking to no one" bug (known-issues #17).
    val color = when {
        share.paused -> Color(0xFF9E9E9E)
        share.status == ShareStatus.ERROR -> Color(0xFFD32F2F)
        share.status == ShareStatus.OUT_OF_SYNC -> Color(0xFFD32F2F)
        share.status == ShareStatus.NO_PEERS -> Color(0xFFD32F2F)
        share.status == ShareStatus.HEALTHY -> Color(0xFF2E7D32)
        else -> Color(0xFFF9A825) // syncing / indexing
    }
    Box(Modifier.size(12.dp).clip(CircleShape).background(color))
}

private fun statusLabel(share: ShareSummary): String = when {
    share.paused -> "Paused"
    else -> when (share.status) {
        ShareStatus.HEALTHY -> "Up to date"
        ShareStatus.SYNCING -> "Syncing ${share.percent}%"
        ShareStatus.INDEXING -> "Indexing ${share.percent}%"
        ShareStatus.PAUSED -> "Paused"
        ShareStatus.ERROR -> "Error"
        ShareStatus.OUT_OF_SYNC -> "⚠ Out of sync"
        ShareStatus.NO_PEERS -> "⚠ No members reachable"
    } + roleSuffix(share.role)
}

private fun roleSuffix(role: Role) = when (role) {
    Role.MASTER -> " · master"
    Role.VIEWER -> " · viewer"
}

@Composable
private fun ThroughputFooter(
    tp: EngineHolder.Throughput,
    deviceName: String,
    onEdit: () -> Unit,
) {
    // The Surface fills to the bottom screen edge (tonal background behind the
    // nav bar), while the content row is padded above the navigation/gesture
    // area via navigationBarsPadding() so text isn't clipped on gesture-nav or
    // curved-edge devices. Extra vertical padding gives it more height.
    Surface(tonalElevation = 2.dp) {
        Row(
            Modifier.fillMaxWidth()
                .navigationBarsPadding()
                .padding(start = 16.dp, end = 16.dp, top = 6.dp, bottom = 10.dp),
            verticalAlignment = Alignment.CenterVertically
        ) {
            Text(deviceName, style = MaterialTheme.typography.labelMedium)
            IconButton(onClick = onEdit, modifier = Modifier.size(34.dp)) {
                Icon(
                    Icons.Default.Edit,
                    contentDescription = "Rename this device",
                    modifier = Modifier.size(17.dp)
                )
            }
            Spacer(Modifier.weight(1f))
            Text("↓ ${humanRate(tp.downBps)}   ↑ ${humanRate(tp.upBps)}",
                style = MaterialTheme.typography.labelMedium)
        }
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
