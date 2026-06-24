package io.github.steeb_k.seedsync.util

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Environment
import android.provider.DocumentsContract
import android.provider.Settings
import java.io.File

/**
 * All-Files Access (MANAGE_EXTERNAL_STORAGE) helpers plus the SAF→real-path
 * bridge. The engine works on real `std::fs` paths, not `content://` URIs, so
 * we use the document-tree picker only for UX and then resolve the chosen tree
 * to a `/storage/emulated/0/...` path to hand to the engine.
 */
object StoragePerms {

    /** True once the user has granted All-Files Access in system settings. */
    fun hasAllFilesAccess(): Boolean = Environment.isExternalStorageManager()

    /** Intent that opens the per-app All-Files Access settings page. */
    fun requestAllFilesAccessIntent(context: Context): Intent =
        Intent(
            Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION,
            Uri.parse("package:${context.packageName}")
        )

    /** Intent that launches the folder picker (UX only; we resolve to a real path). */
    fun openDocumentTreeIntent(): Intent =
        Intent(Intent.ACTION_OPEN_DOCUMENT_TREE).apply {
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
        }

    /**
     * Resolve a document-tree [Uri] from [openDocumentTreeIntent] to an absolute
     * filesystem path, or `null` if it is not on shared storage (e.g. an SD card
     * or another app's private dir, which All-Files Access cannot reach).
     *
     * Tree URIs from the storage provider encode the location as
     * `primary:<relative/path>`; "primary" maps to the shared-storage root.
     */
    fun resolveTreeUriToPath(treeUri: Uri): String? {
        val docId = DocumentsContract.getTreeDocumentId(treeUri) ?: return null
        val parts = docId.split(":", limit = 2)
        if (parts.size != 2) return null
        val (volume, relative) = parts
        if (volume != "primary") return null // only primary shared storage is reachable
        val root = Environment.getExternalStorageDirectory()
        return File(root, relative).absolutePath
    }
}
