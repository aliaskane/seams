package com.aliaskane.seams

import android.content.Context
import android.os.Build
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import com.aliaskane.seams.ui.devices.DiscoveredDevice
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import uniffi.seams_core.FolderInfo
import uniffi.seams_core.NodeEvent
import uniffi.seams_core.NodeEvents
import uniffi.seams_core.SeamsNode
import java.io.File

/**
 * Process-wide bridge between the Rust engine and Compose. The engine pushes events
 * from background threads; snapshot state is thread-safe, so no extra locking is
 * needed here.
 */
object SeamsRuntime {
    var node: SeamsNode? = null
        private set

    /** Devices announced on the LAN right now. */
    val discoveredDevices = mutableStateListOf<DiscoveredDevice>()

    /** Folders configured on this device. */
    val folders = mutableStateListOf<FolderInfo>()

    /** Folders the peers offer but this device does not have yet. */
    val offeredFolders = mutableStateListOf<String>()

    /** One-line status shown on the devices screen. */
    val status = mutableStateOf("Starting the sync engine…")

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    /** Idempotent: safe to call from every activity recreate. */
    fun init(appContext: Context) {
        if (node != null) return
        val dataDir = File(appContext.filesDir, "seams").apply { mkdirs() }
        val deviceName = listOfNotNull(Build.MANUFACTURER, Build.MODEL)
            .distinct()
            .joinToString(" ")
        val created = SeamsNode.create(dataDir.absolutePath, deviceName.ifBlank { "Android device" })
        node = created
        refreshFolders()
        scope.launch { created.start(0u.toUShort(), EngineEvents()) }
    }

    fun refreshFolders() {
        val current = node ?: return
        folders.clear()
        folders.addAll(current.listFolders())
    }

    fun addFolder(label: String, path: String): Result<String> {
        val current = node ?: return Result.failure(IllegalStateException("engine not started"))
        return runCatching { current.addFolder(label, path) }.onSuccess { refreshFolders() }
    }

    fun removeFolder(folderId: String): Boolean {
        val removed = node?.removeFolder(folderId) ?: false
        if (removed) refreshFolders()
        return removed
    }

    /** Pair with a discovered device: connects, syncs matching folders, records it. */
    fun syncWith(device: DiscoveredDevice) {
        val current = node ?: return
        status.value = "Connecting to ${device.name}…"
        scope.launch {
            runCatching {
                current.connectAndSync(device.deviceId, device.addr, device.port, device.name)
            }.onSuccess { reports ->
                val summary = reports.joinToString(", ") {
                    "${it.fetched} fetched, ${it.trashed} trashed, ${it.conflicts} conflicts"
                }
                status.value = if (reports.isEmpty()) {
                    "Connected to ${device.name} — no shared folders yet"
                } else {
                    "Synced with ${device.name}: $summary"
                }
            }.onFailure { error ->
                status.value = "Sync with ${device.name} failed: ${error.message}"
            }
        }
    }

    /** Sync with every paired device we have an address for. */
    fun syncAll() {
        val current = node ?: return
        status.value = "Syncing with known devices…"
        scope.launch {
            runCatching { current.syncKnownPeers() }
                .onSuccess { status.value = "Sync finished" }
                .onFailure { status.value = "Sync failed: ${it.message}" }
        }
    }

    private fun onEngineEvent(event: NodeEvent) {
        when (event) {
            is NodeEvent.DiscoveryObserved -> {
                val device = DiscoveredDevice(event.deviceId, event.name, event.addr, event.port)
                discoveredDevices.removeAll { it.deviceId == device.deviceId }
                discoveredDevices.add(device)
            }
            is NodeEvent.PeerConnected -> status.value = "Connected to ${event.name}"
            is NodeEvent.FolderSynced -> {
                val parts = mutableListOf<String>()
                if (event.fetched > 0u) parts += "${event.fetched} fetched"
                if (event.trashed > 0u) parts += "${event.trashed} trashed"
                if (event.conflicts > 0u) parts += "${event.conflicts} conflicts"
                status.value = if (parts.isEmpty()) "Everything is up to date" else "Sync: ${parts.joinToString(", ")}"
            }
            is NodeEvent.FolderOffered -> {
                val label = "\"${event.label}\""
                if (!offeredFolders.contains(label)) offeredFolders.add(label)
                status.value = "A device offers folder $label — add a folder with the same name to sync it"
            }
            is NodeEvent.SyncFailed -> status.value = "Sync failed: ${event.message}"
            is NodeEvent.Notice -> status.value = event.message
            else -> {}
        }
    }

    private inner class EngineEvents : NodeEvents {
        override fun onEvent(event: NodeEvent) = onEngineEvent(event)
    }
}
