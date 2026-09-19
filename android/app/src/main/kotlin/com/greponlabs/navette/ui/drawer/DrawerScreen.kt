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
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import com.greponlabs.navette.net.ImageRepository
import com.greponlabs.navette.net.Pairing
import com.greponlabs.navette.net.appIconPath
import com.greponlabs.navette.net.sessionThumbnailPath
import com.greponlabs.navette.protocol.App
import com.greponlabs.navette.protocol.Session
import com.greponlabs.navette.protocol.SessionStatus

private val WorkbenchInk = Color(0xFF1B1F3B)
private val WorkbenchTeal = Color(0xFF007D6A)
private val WorkbenchPaleTeal = Color(0xFFC8F5E9)

// A 16:9 tile that sits inside the session card's text block, so it adds
// width, not height. Sized for the name to stay readable on a 360dp handset:
// 360 - 2x20 grid padding = 320 card; - 2x18 card padding = 284 content;
// - 80 tile - 14 gap = 190; - 8 gap - 12 dot - 6 gap - ~64 "STARTING" at
// bold labelMedium = ~100dp for the name column, which is ~11 characters of
// titleMedium -- "firefox-1" whole, where the 96dp tile and titleLarge left
// room for about seven.
private val THUMBNAIL_WIDTH = 80.dp
private val THUMBNAIL_HEIGHT = 45.dp

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
    pairing: Pairing?,
    images: ImageRepository,
    refreshTick: Int,
    onRefresh: () -> Unit,
    onRunApp: (String) -> Unit,
    onAttachSession: (String) -> Unit,
    onManageHosts: () -> Unit,
    onSnackbarDismissed: (shown: String) -> Unit,
) {
    val snackbarHostState = remember { SnackbarHostState() }
    // A session knows its app by id; the letter and icon label want the name.
    val appNames = remember(apps) { apps.associate { it.id to it.name } }
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
                    SessionWorkbenchCard(
                        session = session,
                        appName = appNames[session.appId] ?: session.appId,
                        pairing = pairing,
                        images = images,
                        refreshTick = refreshTick,
                        onClick = { onAttachSession(session.name) },
                    )
                }
            }

            item(span = { GridItemSpan(maxLineSpan) }) {
                SectionLabel("APP LIBRARY", apps.size)
            }
            if (apps.isEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) { EmptyAppsCard(isLoading) }
            } else {
                items(apps, key = { it.id }) { app ->
                    AppLaunchTile(app = app, pairing = pairing, images = images, onClick = { onRunApp(app.id) })
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
private fun SessionWorkbenchCard(
    session: Session,
    appName: String,
    pairing: Pairing?,
    images: ImageRepository,
    refreshTick: Int,
    onClick: () -> Unit,
) {
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
            SessionThumbnail(session = session, appName = appName, pairing = pairing, images = images, refreshTick = refreshTick)
            Spacer(Modifier.width(14.dp))
            Column(modifier = Modifier.weight(1f)) {
                // One line: the thumbnail fixes the card's height, and a name
                // that wrapped would be the one thing that could still move it.
                Text(
                    session.name,
                    style = MaterialTheme.typography.titleMedium,
                    fontWeight = FontWeight.SemiBold,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Spacer(Modifier.height(3.dp))
                Text(
                    text = session.appId,
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            Spacer(Modifier.width(8.dp))
            Box(
                modifier = Modifier.size(12.dp).clip(CircleShape).background(statusColor),
            )
            Spacer(Modifier.width(6.dp))
            Text(
                text = session.status.label().uppercase(),
                style = MaterialTheme.typography.labelMedium,
                // The dot carries the visual status. Keep its adjacent text
                // at the scheme's guaranteed contrast rather than painting
                // small type in a decorative status colour.
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                fontWeight = FontWeight.Bold,
                maxLines = 1,
            )
        }
    }
}

/**
 * A 16:9 tile that is the same size whatever it ends up showing, so the list
 * never jumps as images arrive: the session's latest frame, else the app's
 * icon, else the app's initial. The frame revalidates on the drawer's refresh
 * tick; the icon and the letter are stable.
 */
@Composable
private fun SessionThumbnail(
    session: Session,
    appName: String,
    pairing: Pairing?,
    images: ImageRepository,
    refreshTick: Int,
) {
    Box(
        modifier =
            Modifier
                .size(width = THUMBNAIL_WIDTH, height = THUMBNAIL_HEIGHT)
                .clip(RoundedCornerShape(10.dp))
                .background(WorkbenchPaleTeal),
        contentAlignment = Alignment.Center,
    ) {
        HostImage(
            pairing = pairing,
            images = images,
            path = sessionThumbnailPath(session.name),
            refreshKey = refreshTick,
            modifier = Modifier.fillMaxSize(),
            contentScale = ContentScale.Crop,
        ) {
            AppIcon(appId = session.appId, appName = appName, pairing = pairing, images = images, size = 40.dp)
        }
    }
}

/** The app's icon from the host, else its initial in the workbench ink. */
@Composable
private fun AppIcon(appId: String, appName: String, pairing: Pairing?, images: ImageRepository, size: Dp) {
    HostImage(
        pairing = pairing,
        images = images,
        path = appIconPath(appId),
        refreshKey = null,
        modifier = Modifier.size(size),
        contentScale = ContentScale.Fit,
    ) {
        Text(
            text = appInitial(appName),
            style = MaterialTheme.typography.titleMedium,
            color = WorkbenchInk,
            fontWeight = FontWeight.Bold,
        )
    }
}

/**
 * [AuthenticatedImage] when there is a host to ask; the placeholder alone when
 * the drawer has none. Decorative either way: both cards already announce
 * themselves ("Open work, running", "Launch Firefox, Application") and
 * `clickable` merges descendants, so a described image would have TalkBack
 * read the same name twice.
 */
@Composable
private fun HostImage(
    pairing: Pairing?,
    images: ImageRepository,
    path: String,
    refreshKey: Any?,
    modifier: Modifier,
    contentScale: ContentScale,
    placeholder: @Composable () -> Unit,
) {
    if (pairing == null) {
        placeholder()
    } else {
        AuthenticatedImage(
            repository = images,
            pairing = pairing,
            path = path,
            refreshKey = refreshKey,
            contentDescription = null,
            modifier = modifier,
            contentScale = contentScale,
            placeholder = placeholder,
        )
    }
}

@Composable
private fun AppLaunchTile(app: App, pairing: Pairing?, images: ImageRepository, onClick: () -> Unit) {
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
                AppIcon(appId = app.id, appName = app.name, pairing = pairing, images = images, size = 30.dp)
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
