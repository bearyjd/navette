package com.greponlabs.navette.ui.drawer

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.greponlabs.navette.protocol.App
import com.greponlabs.navette.protocol.Session
import com.greponlabs.navette.protocol.SessionStatus

private val WorkbenchInk = Color(0xFF1B1F3B)
private val WorkbenchTeal = Color(0xFF007D6A)
private val WorkbenchPaleTeal = Color(0xFFC8F5E9)

/**
 * The phone's remote-workbench: live sessions earn the most prominent space,
 * while the host's XDG applications remain quick to launch from a compact
 * library below. The data deliberately stays the same as the original drawer;
 * this is a presentation change, not a second navigation model.
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
    onManageHosts: () -> Unit,
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
        containerColor = MaterialTheme.colorScheme.background,
        topBar = {
            TopAppBar(
                title = {
                    Column {
                        Text("Navette", fontWeight = FontWeight.Bold)
                        Text(
                            text = "REMOTE WORKBENCH",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.primary,
                        )
                    }
                },
                actions = {
                    TextButton(onClick = onManageHosts) { Text("Computers") }
                    if (isLoading) {
                        CircularProgressIndicator(
                            modifier = Modifier.padding(horizontal = 20.dp).size(22.dp),
                            strokeWidth = 2.dp,
                        )
                    } else {
                        TextButton(onClick = onRefresh) { Text("Refresh") }
                    }
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
    ) { padding ->
        LazyVerticalGrid(
            // Two 140dp tiles, a 12dp gutter and 40dp of outer padding fit
            // within a 360dp handset; larger screens simply add columns.
            columns = GridCells.Adaptive(minSize = 140.dp),
            modifier = Modifier.fillMaxSize().padding(padding),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(20.dp),
            horizontalArrangement = Arrangement.spacedBy(12.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            item(span = { GridItemSpan(maxLineSpan) }) {
                LauncherMasthead(sessionCount = sessions.size, appCount = apps.size)
            }

            item(span = { GridItemSpan(maxLineSpan) }) {
                SectionLabel("LIVE SESSIONS", sessions.size)
            }
            if (sessions.isEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) { EmptySessionsCard() }
            } else {
                items(sessions, key = { it.name }, span = { GridItemSpan(maxLineSpan) }) { session ->
                    SessionWorkbenchCard(session = session, onClick = { onAttachSession(session.name) })
                }
            }

            item(span = { GridItemSpan(maxLineSpan) }) {
                SectionLabel("APP LIBRARY", apps.size)
            }
            if (apps.isEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) { EmptyAppsCard(isLoading) }
            } else {
                items(apps, key = { it.id }) { app ->
                    AppLaunchTile(app = app, onClick = { onRunApp(app.id) })
                }
            }
        }
    }
}

@Composable
private fun LauncherMasthead(sessionCount: Int, appCount: Int) {
    Card(
        colors = CardDefaults.cardColors(containerColor = WorkbenchInk, contentColor = Color.White),
        shape = RoundedCornerShape(24.dp),
    ) {
        Column(modifier = Modifier.fillMaxWidth().padding(20.dp)) {
            Text("Your Linux desk, in reach", style = MaterialTheme.typography.headlineSmall, fontWeight = FontWeight.Bold)
            Spacer(Modifier.height(6.dp))
            Text(
                text = "$sessionCount live ${if (sessionCount == 1) "session" else "sessions"} · $appCount launchable apps",
                style = MaterialTheme.typography.bodyMedium,
                color = Color(0xFFBFECE1),
            )
        }
    }
}

@Composable
private fun SectionLabel(title: String, count: Int) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Text(
            text = title,
            style = MaterialTheme.typography.labelLarge,
            color = workbenchLabelColor(),
            fontWeight = FontWeight.Bold,
        )
        Spacer(Modifier.width(8.dp))
        Text(text = count.toString(), style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}

@Composable
private fun SessionWorkbenchCard(session: Session, onClick: () -> Unit) {
    val statusColor = session.status.indicatorColor()
    Card(
        modifier =
            Modifier
                .fillMaxWidth()
                .semantics {
                    contentDescription = "Open ${session.name}, ${session.status.label()}"
                    role = Role.Button
                }
                .clickable(onClick = onClick),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHigh),
        elevation = CardDefaults.cardElevation(defaultElevation = 2.dp),
        shape = RoundedCornerShape(20.dp),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(18.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                modifier = Modifier.size(12.dp).clip(CircleShape).background(statusColor),
            )
            Spacer(Modifier.width(14.dp))
            Column(modifier = Modifier.weight(1f)) {
                Text(session.name, style = MaterialTheme.typography.titleLarge, fontWeight = FontWeight.SemiBold)
                Spacer(Modifier.height(3.dp))
                Text(
                    text = session.appId,
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            Text(
                text = session.status.label().uppercase(),
                style = MaterialTheme.typography.labelMedium,
                // The dot carries the visual status. Keep its adjacent text
                // at the scheme's guaranteed contrast rather than painting
                // small type in a decorative status colour.
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                fontWeight = FontWeight.Bold,
            )
        }
    }
}

@Composable
private fun AppLaunchTile(app: App, onClick: () -> Unit) {
    val category = app.categories.firstOrNull()?.replace('-', ' ') ?: "Application"
    Card(
        modifier =
            Modifier
                .fillMaxWidth()
                .semantics {
                    contentDescription = "Launch ${app.name}, $category"
                    role = Role.Button
                }
                .clickable(onClick = onClick),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainer),
        shape = RoundedCornerShape(20.dp),
    ) {
        Column(modifier = Modifier.fillMaxWidth().padding(16.dp)) {
            Box(
                modifier =
                    Modifier
                        .size(44.dp)
                        .clip(CircleShape)
                        .background(WorkbenchPaleTeal),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    text = appInitial(app.name),
                    style = MaterialTheme.typography.titleMedium,
                    color = WorkbenchInk,
                    fontWeight = FontWeight.Bold,
                )
            }
            Spacer(Modifier.height(18.dp))
            Text(
                text = app.name,
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.SemiBold,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
            )
            Spacer(Modifier.height(6.dp))
            Text(
                text = category.uppercase(),
                style = MaterialTheme.typography.labelSmall,
                color = workbenchLabelColor(),
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

@Composable
private fun EmptySessionsCard() {
    Card(colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerLow), shape = RoundedCornerShape(20.dp)) {
        Text(
            text = "No live sessions yet. Launch an app below to begin.",
            modifier = Modifier.padding(20.dp),
            style = MaterialTheme.typography.bodyLarge,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun EmptyAppsCard(isLoading: Boolean) {
    Card(colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerLow), shape = RoundedCornerShape(20.dp)) {
        Text(
            text = if (isLoading) "Reading the host app library…" else "No apps found. Check navetted's XDG index.",
            modifier = Modifier.padding(20.dp),
            style = MaterialTheme.typography.bodyLarge,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

internal fun appInitial(name: String): String = name.trim().firstOrNull()?.uppercase() ?: "?"

/** Small text needs more contrast than a decorative teal fill. */
@Composable
private fun workbenchLabelColor(): Color =
    if (isSystemInDarkTheme()) MaterialTheme.colorScheme.primary else WorkbenchTeal

private fun SessionStatus.label(): String =
    when (this) {
        SessionStatus.STARTING -> "starting"
        SessionStatus.RUNNING -> "running"
        SessionStatus.FAILED -> "failed"
        SessionStatus.STOPPED -> "stopped"
    }

@Composable
private fun SessionStatus.indicatorColor(): Color =
    when (this) {
        SessionStatus.RUNNING -> Color(0xFF52C78B)
        SessionStatus.STARTING -> Color(0xFFF1B95E)
        SessionStatus.FAILED -> MaterialTheme.colorScheme.error
        SessionStatus.STOPPED -> MaterialTheme.colorScheme.outline
    }
