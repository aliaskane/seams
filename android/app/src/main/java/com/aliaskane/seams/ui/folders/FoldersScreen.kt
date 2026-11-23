package com.aliaskane.seams.ui.folders

import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.dp
import com.aliaskane.seams.R
import com.aliaskane.seams.SeamsRuntime
import com.aliaskane.seams.ui.components.EmptyState
import com.aliaskane.seams.ui.storage.StorageAccess

@Composable
fun FoldersScreen(modifier: Modifier = Modifier) {
    var showAddDialog by rememberSaveable { mutableStateOf(false) }

    Scaffold(
        modifier = modifier,
        floatingActionButton = {
            ExtendedFloatingActionButton(
                onClick = { showAddDialog = true },
                icon = { Icon(painterResource(R.drawable.ic_add), contentDescription = null) },
                text = { Text("Add folder") },
            )
        },
    ) { innerPadding ->
        Box(
            modifier = Modifier
                .fillMaxSize()
                .padding(innerPadding)
        ) {
            val folders = SeamsRuntime.folders
            if (folders.isEmpty()) {
                EmptyState(
                    icon = R.drawable.ic_folders,
                    title = "No folders are being synced",
                    body = "Add a folder to share it with your devices. Photos, music, " +
                        "documents — anything on this device.",
                )
            } else {
                LazyColumn {
                    items(folders, key = { it.id }) { folder ->
                        ListItem(
                            headlineContent = { Text(folder.label) },
                            supportingContent = { Text(folder.path) },
                            trailingContent = {
                                IconButton(onClick = { SeamsRuntime.removeFolder(folder.id) }) {
                                    Icon(
                                        painterResource(R.drawable.ic_delete),
                                        contentDescription = "Remove folder",
                                    )
                                }
                            },
                        )
                        HorizontalDivider()
                    }
                }
            }
        }
    }

    if (showAddDialog) {
        AddFolderDialog(onDismiss = { showAddDialog = false })
    }
}

@Composable
private fun AddFolderDialog(onDismiss: () -> Unit) {
    val context = LocalContext.current
    var label by rememberSaveable { mutableStateOf("") }
    var chosenPath by rememberSaveable { mutableStateOf<String?>(null) }
    var needsPermission by rememberSaveable { mutableStateOf(!StorageAccess.isAllFilesGranted()) }

    val folderPicker = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenDocumentTree()
    ) { uri ->
        chosenPath = uri?.let { StorageAccess.resolveTreeUriPath(it) }
    }
    val permissionLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.StartActivityForResult()
    ) {
        needsPermission = !StorageAccess.isAllFilesGranted()
        if (!needsPermission) {
            folderPicker.launch(null)
        }
    }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Add folder") },
        text = {
            Column {
                OutlinedTextField(
                    value = label,
                    onValueChange = { label = it },
                    label = { Text("Folder name") },
                    singleLine = true,
                )
                TextButton(
                    onClick = {
                        if (!StorageAccess.isAllFilesGranted()) {
                            permissionLauncher.launch(
                                StorageAccess.allFilesSettingsIntent(context)
                            )
                        } else {
                            folderPicker.launch(null)
                        }
                    },
                    modifier = Modifier.padding(top = 8.dp),
                ) {
                    Text(chosenPath ?: "Choose folder…")
                }
                if (needsPermission) {
                    Text(
                        text = "One-time setup: grant \"All files access\" so the engine " +
                            "can sync any folder you pick.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        },
        confirmButton = {
            TextButton(
                onClick = {
                    val path = chosenPath
                    if (path != null) {
                        SeamsRuntime.addFolder(label.trim(), path)
                    }
                    onDismiss()
                },
                enabled = label.isNotBlank() && chosenPath != null,
            ) {
                Text("Add")
            }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Cancel") }
        },
    )
}
