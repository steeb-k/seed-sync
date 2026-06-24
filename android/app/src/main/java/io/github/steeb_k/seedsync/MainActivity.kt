package io.github.steeb_k.seedsync

import android.Manifest
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import io.github.steeb_k.seedsync.engine.EngineService
import io.github.steeb_k.seedsync.ui.SeedAppHost
import io.github.steeb_k.seedsync.ui.SeedTheme
import io.github.steeb_k.seedsync.util.StoragePerms

class MainActivity : ComponentActivity() {

    // Pending callback for the in-flight folder pick (single-shot).
    private var pendingFolderCallback: ((String?) -> Unit)? = null

    private val openTree =
        registerForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri: Uri? ->
            val cb = pendingFolderCallback
            pendingFolderCallback = null
            cb?.invoke(uri?.let { StoragePerms.resolveTreeUriToPath(it) })
        }

    private val requestNotifications =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { /* best effort */ }

    private val allFilesAccessSettings =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) {
            // Re-render: the gate below re-checks Environment.isExternalStorageManager().
            recreateState()
        }

    private var stateEpoch by mutableStateOf(0)
    private fun recreateState() { stateEpoch++ }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Draw edge-to-edge with system-bar icon colors that auto-adapt to
        // light/dark. Android 15 (targetSdk 35) forces edge-to-edge anyway;
        // without this the status-bar icons stay dark-on-dark and vanish.
        enableEdgeToEdge()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            requestNotifications.launch(Manifest.permission.POST_NOTIFICATIONS)
        }

        setContent {
            SeedTheme {
                Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
                    // Reading stateEpoch makes the gate recompose after returning
                    // from the All-Files Access settings screen.
                    stateEpoch
                    if (StoragePerms.hasAllFilesAccess()) {
                        ensureServiceStarted()
                        SeedAppHost(pickFolder = ::pickFolder)
                    } else {
                        NeedsPermission(onGrant = ::openAllFilesAccess)
                    }
                }
            }
        }
    }

    override fun onResume() {
        super.onResume()
        recreateState()
    }

    private fun pickFolder(onResolved: (String?) -> Unit) {
        pendingFolderCallback = onResolved
        openTree.launch(null)
    }

    private fun openAllFilesAccess() {
        allFilesAccessSettings.launch(StoragePerms.requestAllFilesAccessIntent(this))
    }

    private var serviceStarted = false
    private fun ensureServiceStarted() {
        if (serviceStarted) return
        serviceStarted = true
        EngineService.start(applicationContext)
        ResyncWorker.schedule(applicationContext)
        maybeRequestBatteryExemption()
    }

    private fun maybeRequestBatteryExemption() {
        // Ask once to be exempt from Doze so the foreground service is less likely
        // to be killed. Best-effort; ignored if the user declines.
        runCatching {
            val intent = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS)
                .setData(Uri.parse("package:$packageName"))
            startActivity(intent)
        }
    }
}

@androidx.compose.runtime.Composable
private fun NeedsPermission(onGrant: () -> Unit) {
    Column(
        Modifier.fillMaxSize().padding(24.dp),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally
    ) {
        Text("All-Files Access needed", style = MaterialTheme.typography.titleLarge)
        Text(
            "SEED Sync mirrors real folders on shared storage, so it needs " +
                "All-Files Access. Your files are never uploaded to a server — they " +
                "sync directly, end-to-end, to your other devices.",
            style = MaterialTheme.typography.bodyMedium,
            modifier = Modifier.padding(vertical = 16.dp)
        )
        Button(onClick = onGrant) { Text("Grant access") }
    }
}
