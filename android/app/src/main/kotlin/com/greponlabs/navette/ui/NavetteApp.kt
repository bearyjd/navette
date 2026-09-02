package com.greponlabs.navette.ui

import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.lifecycle.viewmodel.compose.viewModel
import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.ui.connect.ConnectScreen
import com.greponlabs.navette.ui.drawer.DrawerScreen

/**
 * Two states, not a navigation graph: Connect (no session yet) and Drawer
 * (connected). Adding real navigation is worth it once there is a third
 * screen (the session screen) to route to.
 */
@Composable
fun NavetteApp(viewModel: AppViewModel = viewModel()) {
    val state by viewModel.state.collectAsState()

    if (state.connection is ConnectionState.Connected) {
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
    } else {
        ConnectScreen(
            host = state.host,
            connection = state.connection,
            onHostChanged = { host -> viewModel.onEvent(AppEvent.HostChanged(host)) },
            onConnect = { viewModel.onEvent(AppEvent.Connect) },
        )
    }
}
