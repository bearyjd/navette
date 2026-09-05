package com.greponlabs.navette.ui

import androidx.activity.compose.BackHandler
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.lifecycle.viewmodel.compose.viewModel
import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.ui.connect.ConnectScreen
import com.greponlabs.navette.ui.drawer.DrawerScreen
import com.greponlabs.navette.ui.session.SessionScreen

/**
 * Three states, not a navigation graph: Connect (no host yet), Drawer
 * (connected, choosing), and Session (attached).
 *
 * Navigation Compose was considered and left out: with three screens and a
 * back-stack no deeper than session-to-drawer, a third branch here is simpler
 * than a nav graph and a new dependency. Worth revisiting if a fourth screen
 * or real history shows up.
 */
@Composable
fun NavetteApp(viewModel: AppViewModel = viewModel()) {
    val state by viewModel.state.collectAsState()
    val activeSession = state.activeSession

    when {
        activeSession != null -> {
            // System back leaves the session rather than the app, matching the
            // Back button the session screen shows on a dead connection.
            BackHandler { viewModel.onEvent(AppEvent.LeaveSession) }
            SessionScreen(
                sessionName = activeSession,
                host = state.host.trim(),
                onLeave = { viewModel.onEvent(AppEvent.LeaveSession) },
            )
        }
        state.connection is ConnectionState.Connected ->
            DrawerScreen(
                sessions = state.sessions,
                apps = state.apps,
                isLoading = state.isLoading,
                snackbarMessage = state.snackbarMessage,
                onRefresh = { viewModel.onEvent(AppEvent.Refresh) },
                onRunApp = { appId -> viewModel.onEvent(AppEvent.RunApp(appId)) },
                onAttachSession = { session -> viewModel.onEvent(AppEvent.AttachSession(session)) },
                onSnackbarDismissed = { shown -> viewModel.onEvent(AppEvent.DismissSnackbar(shown)) },
            )
        else ->
            ConnectScreen(
                host = state.host,
                connection = state.connection,
                onHostChanged = { host -> viewModel.onEvent(AppEvent.HostChanged(host)) },
                onConnect = { viewModel.onEvent(AppEvent.Connect) },
            )
    }
}
