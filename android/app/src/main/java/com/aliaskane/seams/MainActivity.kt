package com.aliaskane.seams

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material3.Icon
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.adaptive.navigationsuite.NavigationSuiteScaffold
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.tooling.preview.PreviewScreenSizes
import androidx.compose.ui.unit.dp
import com.aliaskane.seams.ui.conflicts.ConflictsScreen
import com.aliaskane.seams.ui.devices.DevicesScreen
import com.aliaskane.seams.ui.folders.FoldersScreen
import com.aliaskane.seams.ui.theme.SeamsTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        SeamsRuntime.init(applicationContext)
        setContent {
            SeamsTheme {
                SeamsApp()
            }
        }
    }
}

private enum class AppDestinations(
    val label: String,
    val icon: Int,
) {
    DEVICES("Devices", R.drawable.ic_devices),
    FOLDERS("Folders", R.drawable.ic_folders),
    CONFLICTS("Conflicts", R.drawable.ic_conflicts),
}

@PreviewScreenSizes
@Composable
fun SeamsApp() {
    var currentDestination by rememberSaveable { mutableStateOf(AppDestinations.DEVICES) }

    NavigationSuiteScaffold(
        navigationSuiteItems = {
            AppDestinations.entries.forEach {
                item(
                    icon = {
                        Icon(
                            painterResource(it.icon),
                            contentDescription = it.label,
                            modifier = Modifier.size(24.dp),
                        )
                    },
                    label = { Text(it.label) },
                    selected = it == currentDestination,
                    onClick = { currentDestination = it },
                )
            }
        },
    ) {
        Scaffold(modifier = Modifier.fillMaxSize()) { innerPadding ->
            Box(modifier = Modifier.fillMaxSize().padding(innerPadding)) {
                when (currentDestination) {
                    AppDestinations.DEVICES -> DevicesScreen()
                    AppDestinations.FOLDERS -> FoldersScreen()
                    AppDestinations.CONFLICTS -> ConflictsScreen()
                }
            }
        }
    }
}
