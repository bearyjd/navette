package com.greponlabs.navette.ui.drawer

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Card
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.greponlabs.navette.protocol.App
import com.greponlabs.navette.protocol.Session
import com.greponlabs.navette.protocol.SessionStatus

/**
 * Two sections -- Running (live sessions) and Apps (the remote XDG menu) --
 * per docs/prp/PRP-plan.md §4.3. Tap Running to attach; tap an app to
 * run-and-attach. Attach currently only confirms success (see
 * [com.greponlabs.navette.ui.AppViewModel]'s AttachSession handler) since
 * the MediaCodec session screen is the next slice of M3, not this one.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun DrawerScreen(
    sessions: List<Session>,
    apps: List<App>,
    isLoading: Boolean,
    snackbarMessage: String?,
    onRefresh: () -> Unit,
    onRunApp: (String) -> Unit,
    onAttachSession: (String) -> Unit,
    onSnackbarDismissed: (shown: String) -> Unit,
) {
    val snackbarHostState = remember { SnackbarHostState() }
    LaunchedEffect(snackbarMessage) {
        if (snackbarMessage != null) {
            snackbarHostState.showSnackbar(snackbarMessage)
            onSnackbarDismissed(snackbarMessage)
        }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Navette") },
                actions = {
                    if (isLoading) {
                        CircularProgressIndicator(modifier = Modifier.padding(8.dp))
                    } else {
                        TextButton(onClick = onRefresh) { Text("Refresh") }
                    }
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
    ) { padding ->
        LazyColumn(modifier = Modifier.fillMaxSize().padding(padding)) {
            item { SectionHeader("Running") }
            if (sessions.isEmpty()) {
                item { EmptyRow("No sessions running") }
            } else {
                items(sessions, key = { it.name }) { session ->
                    SessionRow(session, onClick = { onAttachSession(session.name) })
                }
            }

            item { SectionHeader("Apps") }
            if (apps.isEmpty()) {
                item { EmptyRow("No apps found -- check navetted's XDG index") }
            } else {
                items(apps, key = { it.id }) { app ->
                    AppRow(app, onClick = { onRunApp(app.id) })
                }
            }
        }
    }
}

@Composable
private fun SectionHeader(title: String) {
    Text(
        text = title,
        style = MaterialTheme.typography.titleMedium,
        modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp),
    )
}

@Composable
private fun EmptyRow(message: String) {
    Text(
        text = message,
        style = MaterialTheme.typography.bodyMedium,
        modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp),
    )
}

@Composable
private fun SessionRow(session: Session, onClick: () -> Unit) {
    Card(modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp)) {
        ListItem(
            headlineContent = { Text(session.name) },
            supportingContent = { Text("${session.appId} -- ${session.status.label()}") },
            modifier = Modifier.clickable(onClick = onClick),
        )
    }
}

@Composable
private fun AppRow(app: App, onClick: () -> Unit) {
    Card(modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp)) {
        ListItem(
            headlineContent = { Text(app.name) },
            supportingContent = { app.categories.firstOrNull()?.let { Text(it) } },
            modifier = Modifier.clickable(onClick = onClick),
        )
    }
}

private fun SessionStatus.label(): String =
    when (this) {
        SessionStatus.STARTING -> "starting"
        SessionStatus.RUNNING -> "running"
        SessionStatus.FAILED -> "failed"
        SessionStatus.STOPPED -> "stopped"
    }
