package com.aliaskane.seams.ui.storage

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.DocumentsContract
import android.provider.Settings
import java.io.File

/**
 * Storage access for real filesystem paths. The app uses "All files access" so the
 * engine can walk arbitrary folders fast; the user still picks folders through the
 * system picker, whose tree URI is resolved back to a plain path here.
 */
object StorageAccess {

    fun isAllFilesGranted(): Boolean =
        Build.VERSION.SDK_INT < Build.VERSION_CODES.R || Environment.isExternalStorageManager()

    /** Settings screen where the user flips "All files access" for this app. */
    fun allFilesSettingsIntent(context: Context): Intent {
        val specific = Intent(
            Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION,
            Uri.parse("package:${context.packageName}")
        )
        try {
            return specific
        } catch (_: ActivityNotFoundException) {
            return Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION)
        }
    }

    /**
     * Maps a tree URI from the system picker (`content://…/tree/primary:DCIM`) to a
     * real path on primary storage (`/storage/emulated/0/DCIM`). Returns null when the
     * volume is not primary storage or the path does not exist.
     */
    fun resolveTreeUriPath(treeUri: Uri): String? {
        val docId = try {
            DocumentsContract.getTreeDocumentId(treeUri)
        } catch (_: IllegalArgumentException) {
            return null
        }
        val parts = docId.split(":", limit = 2)
        if (parts.size < 2 || parts[0] != "primary") return null

        val root = Environment.getExternalStorageDirectory().absolutePath
        val relative = parts[1].trimEnd('/')
        val resolved = if (relative.isEmpty()) root else File(root, relative).absolutePath
        return if (File(resolved).isDirectory) resolved else null
    }
}
