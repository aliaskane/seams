package com.aliaskane.seams.ui.devices

import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import com.aliaskane.seams.R
import com.aliaskane.seams.ui.components.EmptyState

@Composable
fun DevicesScreen(modifier: Modifier = Modifier) {
    // The sync engine (seams-core) feeds real peers here in phase 1.
    EmptyState(
        icon = R.drawable.ic_devices,
        title = "No devices on this network yet",
        body = "Install Seams on your other device and connect to the same Wi-Fi. " +
            "Nearby devices will show up here with their names.",
        modifier = modifier,
    )
}
