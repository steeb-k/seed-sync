package io.github.steeb_k.seedsync.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Checkbox
import androidx.compose.material.icons.filled.Edit
import androidx.compose.material.icons.filled.QrCodeScanner
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.unit.dp
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import uniffi.seed_mobile.PeerInfo

@Composable
fun KeysDialog(masterKey: String?, viewerKey: String, isMaster: Boolean, onDismiss: () -> Unit) {
    val clipboard = LocalClipboardManager.current
    AlertDialog(
        onDismissRequest = onDismiss,
        confirmButton = { TextButton(onClick = onDismiss) { Text("Close") } },
        title = { Text("Share keys") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                if (isMaster && masterKey != null) {
                    Text("Master key (full read/write — keep secret):")
                    SelectableMono(masterKey) { clipboard.setText(AnnotatedString(masterKey)) }
                    Spacer(Modifier.size(12.dp))
                }
                Text("Viewer key (read-only):")
                SelectableMono(viewerKey) { clipboard.setText(AnnotatedString(viewerKey)) }
            }
        }
    )
}

@Composable
fun NodeAddrDialog(addr: String, onDismiss: () -> Unit) {
    val clipboard = LocalClipboardManager.current
    AlertDialog(
        onDismissRequest = onDismiss,
        confirmButton = { TextButton(onClick = onDismiss) { Text("Close") } },
        title = { Text("This device's address") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                Text("Hand this to a peer as the bootstrap when adding a share, if DNS discovery is unavailable.")
                Spacer(Modifier.size(8.dp))
                SelectableMono(addr) { clipboard.setText(AnnotatedString(addr)) }
            }
        }
    )
}

@Composable
fun PeersDialog(peers: List<PeerInfo>, onDismiss: () -> Unit) {
    AlertDialog(
        onDismissRequest = onDismiss,
        confirmButton = { TextButton(onClick = onDismiss) { Text("Close") } },
        title = { Text("Peers") },
        text = {
            if (peers.isEmpty()) {
                Text("No peers seen yet.")
            } else {
                Column(Modifier.verticalScroll(rememberScrollState()).heightIn(max = 360.dp)) {
                    peers.forEach { p ->
                        val role = if (p.role == uniffi.seed_mobile.Role.MASTER) "master" else "viewer"
                        val dot = if (p.online) "●" else "○"
                        Text("$dot ${p.name ?: p.nodeId}  ·  $role  ·  ${p.percent}%")
                        Spacer(Modifier.size(4.dp))
                    }
                }
            }
        }
    )
}

@Composable
fun AddExistingDialog(
    onDismiss: () -> Unit,
    onPickFolder: ((String?) -> Unit) -> Unit,
    onConfirm: (key: String, folder: String, bootstrap: String?) -> Unit,
) {
    var key by remember { mutableStateOf("") }
    var folder by remember { mutableStateOf("") }
    var bootstrap by remember { mutableStateOf("") }
    // Scan a key QR from the desktop app (ZXing handles the camera-permission
    // prompt). The scanned contents are the key string.
    val scanLauncher = rememberLauncherForActivityResult(ScanContract()) { result ->
        result.contents?.let { key = it.trim() }
    }
    AlertDialog(
        onDismissRequest = onDismiss,
        confirmButton = {
            TextButton(
                enabled = key.isNotBlank() && folder.isNotBlank(),
                onClick = { onConfirm(key.trim(), folder, bootstrap.trim()) }
            ) { Text("Add") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
        title = { Text("Add existing share") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                OutlinedTextField(
                    value = key, onValueChange = { key = it },
                    label = { Text("Share key (master or viewer)") },
                    singleLine = true, modifier = Modifier.fillMaxWidth()
                )
                TextButton(onClick = {
                    scanLauncher.launch(
                        ScanOptions().apply {
                            setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                            setPrompt("Scan a key QR from the SEED Sync desktop app")
                            setBeepEnabled(false)
                            // Portrait scanner instead of ZXing's landscape default.
                            setCaptureActivity(io.github.steeb_k.seedsync.PortraitCaptureActivity::class.java)
                            setOrientationLocked(false)
                        }
                    )
                }) {
                    Icon(
                        androidx.compose.material.icons.Icons.Default.QrCodeScanner,
                        contentDescription = null,
                        modifier = Modifier.size(18.dp)
                    )
                    Spacer(Modifier.size(6.dp))
                    Text("Scan QR code")
                }
                Spacer(Modifier.size(8.dp))
                Row {
                    OutlinedTextField(
                        value = folder, onValueChange = { folder = it },
                        label = { Text("Destination folder") },
                        singleLine = true, modifier = Modifier.fillMaxWidth().padding(end = 8.dp)
                    )
                }
                TextButton(onClick = { onPickFolder { p -> if (p != null) folder = p } }) {
                    Text("Pick folder…")
                }
                OutlinedTextField(
                    value = bootstrap, onValueChange = { bootstrap = it },
                    label = { Text("Bootstrap address (optional)") },
                    singleLine = true, modifier = Modifier.fillMaxWidth()
                )
            }
        }
    )
}

@Composable
fun SettingsDialog(onDismiss: () -> Unit, onEditDeviceName: () -> Unit) {
    val gate = io.github.steeb_k.seedsync.engine.SyncGate
    val wifiOnly by gate.wifiOnly.collectAsStateSafe(false)
    val chargingOnly by gate.chargingOnly.collectAsStateSafe(false)
    val deviceName by io.github.steeb_k.seedsync.engine.EngineHolder.deviceName.collectAsStateSafe("")
    AlertDialog(
        onDismissRequest = onDismiss,
        confirmButton = { TextButton(onClick = onDismiss) { Text("Done") } },
        title = { Text("Settings") },
        text = {
            Column {
                // Device name (moved here from the overflow menu); tapping opens
                // the rename dialog. The footer pencil is the quick shortcut.
                Row(
                    modifier = Modifier.fillMaxWidth()
                        .clickable { onEditDeviceName() }
                        .padding(vertical = 6.dp),
                    verticalAlignment = androidx.compose.ui.Alignment.CenterVertically
                ) {
                    Column(Modifier.weight(1f)) {
                        Text("Device name", style = androidx.compose.material3.MaterialTheme.typography.bodyLarge)
                        Text(
                            deviceName.ifEmpty { "—" },
                            style = androidx.compose.material3.MaterialTheme.typography.bodySmall
                        )
                    }
                    Icon(
                        androidx.compose.material.icons.Icons.Default.Edit,
                        contentDescription = "Rename this device"
                    )
                }
                HorizontalDivider(Modifier.padding(vertical = 8.dp))
                SettingSwitch(
                    title = "Sync only on Wi-Fi",
                    subtitle = "Pause syncing on cellular data",
                    checked = wifiOnly,
                    onChange = { gate.setWifiOnly(it) }
                )
                Spacer(Modifier.size(8.dp))
                SettingSwitch(
                    title = "Sync only while charging",
                    subtitle = "Pause syncing on battery power",
                    checked = chargingOnly,
                    onChange = { gate.setChargingOnly(it) }
                )
            }
        }
    )
}

@Composable
private fun SettingSwitch(
    title: String,
    subtitle: String,
    checked: Boolean,
    onChange: (Boolean) -> Unit,
) {
    Row(
        modifier = Modifier.fillMaxWidth().padding(vertical = 4.dp),
        verticalAlignment = androidx.compose.ui.Alignment.CenterVertically
    ) {
        Column(Modifier.weight(1f)) {
            Text(title, style = androidx.compose.material3.MaterialTheme.typography.bodyLarge)
            Text(subtitle, style = androidx.compose.material3.MaterialTheme.typography.bodySmall)
        }
        Spacer(Modifier.size(12.dp))
        androidx.compose.material3.Switch(checked = checked, onCheckedChange = onChange)
    }
}

@Composable
fun DeviceNameDialog(current: String, onDismiss: () -> Unit, onConfirm: (String) -> Unit) {
    var name by remember { mutableStateOf(current) }
    AlertDialog(
        onDismissRequest = onDismiss,
        confirmButton = {
            TextButton(enabled = name.isNotBlank(), onClick = { onConfirm(name.trim()) }) { Text("Save") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
        title = { Text("Device name") },
        text = {
            OutlinedTextField(
                value = name, onValueChange = { name = it },
                label = { Text("Shown to peers") }, singleLine = true,
                modifier = Modifier.fillMaxWidth()
            )
        }
    )
}

@Composable
fun RemoveDialog(
    share: uniffi.seed_mobile.ShareSummary,
    onDismiss: () -> Unit,
    onConfirm: (deleteFiles: Boolean) -> Unit,
) {
    var deleteFiles by remember { mutableStateOf(false) }
    AlertDialog(
        onDismissRequest = onDismiss,
        confirmButton = { TextButton(onClick = { onConfirm(deleteFiles) }) { Text("Remove") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
        title = { Text("Remove share") },
        text = {
            Column {
                Text("Stop syncing \"${share.name.ifEmpty { share.shareId.take(12) }}\"?")
                Spacer(Modifier.size(8.dp))
                Row {
                    Checkbox(checked = deleteFiles, onCheckedChange = { deleteFiles = it })
                    Text("Also delete the local folder contents")
                }
            }
        }
    )
}

@Composable
private fun SelectableMono(text: String, onCopy: () -> Unit) {
    Column {
        Text(text, style = androidx.compose.material3.MaterialTheme.typography.bodySmall)
        TextButton(onClick = onCopy) { Text("Copy") }
    }
}
