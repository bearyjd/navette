package com.greponlabs.navette.ui

import androidx.activity.compose.BackHandler
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.lifecycle.viewmodel.compose.viewModel
import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.ViewScale
import com.greponlabs.navette.ui.connect.ConnectScreen
import com.greponlabs.navette.ui.drawer.DrawerScreen
import com.greponlabs.navette.ui.hosts.HostListScreen
import com.greponlabs.navette.ui.session.SessionScreen

/**
 * Four states, not a navigation graph: Connect/add host, host list, Drawer
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
            val saved = state.savedForPairing
            // The device default is resolved here and never stored: a saved
            // `null` means "whatever this device defaults to", which is what
            // lets a phone and a tablet share a host entry. LocalConfiguration,
            // not LocalContext.resources.configuration: only the former
            // recomposes when the Configuration changes (Compose's own
            // LocalContextConfigurationRead lint). So this does follow a
            // fold/unfold -- a cover screen is sw < 600 dp and the inner
            // screen may not be, so the default can flip mid-session on a
            // device with no saved preset; the guest re-lays out once, and
            // saving any preset pins it.
            val smallestWidthDp = LocalConfiguration.current.smallestScreenWidthDp
            SessionScreen(
                sessionName = activeSession,
                pairing = pairing,
                viewScale = saved?.viewScale ?: ViewScale.defaultFor(smallestWidthDp),
                // An unsaved pairing (failed keystore write) still gets the
                // control; the choice just lives for the session.
                onViewScaleChange = { scale -> saved?.let { viewModel.onEvent(AppEvent.SetViewScale(it.id, scale)) } },
                onLeave = { viewModel.onEvent(AppEvent.LeaveSession) },
            )
        }
        state.showingHosts ->
            HostListScreen(
                registry = state.registry,
                snackbarMessage = state.snackbarMessage,
                onSelect = { viewModel.onEvent(AppEvent.SelectHost(it)) },
                onDelete = { viewModel.onEvent(AppEvent.DeleteHost(it)) },
                onSetWake = { id, wake -> viewModel.onEvent(AppEvent.SetWake(id, wake)) },
                onAdd = { viewModel.onEvent(AppEvent.AddHost) },
                onBack = { viewModel.onEvent(AppEvent.HideHosts) },
                onSnackbarDismissed = { shown -> viewModel.onEvent(AppEvent.DismissSnackbar(shown)) },
            )
        state.connection is ConnectionState.Connected && !state.addingHost ->
            DrawerScreen(
                sessions = state.sessions,
                apps = state.apps,
                isLoading = state.isLoading,
                snackbarMessage = state.snackbarMessage,
                pairing = pairing,
                images = viewModel.imageRepository,
                refreshTick = state.refreshTick,
                onRefresh = { viewModel.onEvent(AppEvent.Refresh) },
                onRunApp = { appId -> viewModel.onEvent(AppEvent.RunApp(appId)) },
                onAttachSession = { session -> viewModel.onEvent(AppEvent.AttachSession(session)) },
                onManageHosts = { viewModel.onEvent(AppEvent.ShowHosts) },
                onSnackbarDismissed = { shown -> viewModel.onEvent(AppEvent.DismissSnackbar(shown)) },
            )
        // Unauthorized is not broken out here: ConnectScreen already handles it
        // itself, with a "pairing rejected" message and no Retry button, since
        // resending a token the daemon refused would just fail the same way.
        // A separate arm rendering the identical call was only a reminder that
        // it did not -- and now reads as a claim that it still doesn't.
        else ->
            ConnectScreen(
                connection = state.connection,
                onPaired = { pairing -> viewModel.onEvent(AppEvent.Paired(pairing)) },
                onRetry = { viewModel.onEvent(AppEvent.Reconnect) },
                onBack = if (state.addingHost) ({ viewModel.onEvent(AppEvent.CancelAddHost) }) else null,
                onManageHosts = if (state.registry.hosts.isNotEmpty()) ({ viewModel.onEvent(AppEvent.ShowHosts) }) else null,
                wake = state.wake,
                wakeViaLabel = state.wakeRoute?.via?.endpointLabel,
                onWake = { viewModel.onEvent(AppEvent.WakeActive) },
            )
    }
}
