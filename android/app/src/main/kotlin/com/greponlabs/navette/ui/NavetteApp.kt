package com.greponlabs.navette.ui

import androidx.activity.compose.BackHandler
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.platform.LocalContext
import androidx.lifecycle.viewmodel.compose.viewModel
import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.ui.connect.ConnectScreen
import com.greponlabs.navette.ui.drawer.DrawerScreen
import com.greponlabs.navette.ui.session.SessionScreen

/**
 * Three states, not a navigation graph: Connect (no pairing yet), Drawer
 * (connected, choosing), and Session (attached).
 *
 * Navigation Compose was considered and left out: with three screens and a
 * back-stack no deeper than session-to-drawer, a third branch here is simpler
 * than a nav graph and a new dependency. Worth revisiting if a fourth screen
 * or real history shows up.
 */
@Composable
fun NavetteApp(
    viewModel: AppViewModel = viewModel(factory = AppViewModel.factory(LocalContext.current)),
) {
    val state by viewModel.state.collectAsState()
    val activeSession = state.activeSession
    val pairing = state.pairing

    when {
        // Both must be non-null together: activeSession only ever gets set
        // from a successful Attach, which requires a connected client, which
        // requires a pairing. The `pairing != null` half is defence in
        // depth -- if it ever daylights, falling through to Drawer/Connect
        // below is the safe failure, not a crash on a forced-non-null.
        activeSession != null && pairing != null -> {
            // System back leaves the session rather than the app, matching the
            // Back button the session screen shows on a dead connection.
            BackHandler { viewModel.onEvent(AppEvent.LeaveSession) }
            SessionScreen(
                sessionName = activeSession,
                pairing = pairing,
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
        // Kept apart from the `else` below rather than folded into it: the
        // daemon refused our token, which reads the same as any other
        // connection failure on ConnectScreen today (it has no Unauthorized
        // case of its own yet), but this arm exists so that gap has to be
        // noticed and addressed explicitly the next time this state's
        // handling changes, instead of silently falling through an `else`.
        state.connection is ConnectionState.Unauthorized ->
            ConnectScreen(
                connection = state.connection,
                onPaired = { pairing -> viewModel.onEvent(AppEvent.Paired(pairing)) },
                onRetry = { viewModel.onEvent(AppEvent.Reconnect) },
            )
        else ->
            ConnectScreen(
                connection = state.connection,
                onPaired = { pairing -> viewModel.onEvent(AppEvent.Paired(pairing)) },
                onRetry = { viewModel.onEvent(AppEvent.Reconnect) },
            )
    }
}
