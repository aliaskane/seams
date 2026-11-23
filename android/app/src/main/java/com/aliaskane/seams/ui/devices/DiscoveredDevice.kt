package com.aliaskane.seams.ui.devices

/** A device announced on the LAN, as shown in the devices list. */
data class DiscoveredDevice(
    val deviceId: String,
    val name: String,
    val addr: String,
    val port: UShort,
)
