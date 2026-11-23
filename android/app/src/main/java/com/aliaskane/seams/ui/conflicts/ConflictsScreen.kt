package com.aliaskane.seams.ui.conflicts

import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import com.aliaskane.seams.R
import com.aliaskane.seams.ui.components.EmptyState

@Composable
fun ConflictsScreen(modifier: Modifier = Modifier) {
    EmptyState(
        icon = R.drawable.ic_conflicts,
        title = "No conflicts",
        body = "When both devices change the same file before syncing, Seams keeps " +
            "both versions and shows them here, so nothing is ever lost.",
        modifier = modifier,
    )
}
