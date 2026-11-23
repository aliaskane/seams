package com.aliaskane.seams.ui.devices

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.aliaskane.seams.R
import com.aliaskane.seams.SeamsRuntime
import com.aliaskane.seams.ui.components.EmptyState

@Composable
fun DevicesScreen(modifier: Modifier = Modifier) {
    val devices = SeamsRuntime.discoveredDevices

    Column(modifier = modifier.fillMaxSize()) {
        if (devices.isEmpty()) {
            EmptyState(
                icon = R.drawable.ic_devices,
                title = "No devices on this network yet",
                body = "Install Seams on your other device and connect to the same Wi-Fi. " +
                    "Nearby devices will show up here with their names.",
                modifier = Modifier.weight(1f),
            )
        } else {
            LazyColumn(modifier = Modifier.weight(1f)) {
                items(devices, key = { it.deviceId }) { device ->
                    ListItem(
                        headlineContent = { Text(device.name) },
                        supportingContent = { Text(device.addr) },
                        trailingContent = {
                            TextButton(onClick = { SeamsRuntime.syncWith(device) }) {
                                Text("Sync")
                            }
                        },
                    )
                    HorizontalDivider()
                }
            }
        }
        Text(
            text = SeamsRuntime.status.value,
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(16.dp),
        )
    }
}
