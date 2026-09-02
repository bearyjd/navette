package com.greponlabs.navette.ui

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.NavetteApi
import com.greponlabs.navette.net.NavetteClient
import com.greponlabs.navette.net.controlWebSocketUrl
import com.greponlabs.navette.protocol.App
import com.greponlabs.navette.protocol.RequestCommand
import com.greponlabs.navette.protocol.Session
import com.greponlabs.navette.protocol.appsOrNull
import com.greponlabs.navette.protocol.errorOrNull
import com.greponlabs.navette.protocol.sessionsOrNull
import kotlin.coroutines.cancellation.CancellationException
import kotlinx.coroutines.Job
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch

data class AppUiState(
    val host: String = "",
    val connection: ConnectionState = ConnectionState.Disconnected,
    val apps: List<App> = emptyList(),
    val sessions: List<Session> = emptyList(),
    val isLoading: Boolean = false,
    val snackbarMessage: String? = null,
)

sealed interface AppEvent {
    data class HostChanged(val host: String) : AppEvent

    data object Connect : AppEvent

    data object Refresh : AppEvent

    data class RunApp(val appId: String) : AppEvent

    data class AttachSession(val session: String) : AppEvent

    /**
     * [shown] is the message the caller just finished displaying. Only
     * clears [AppUiState.snackbarMessage] if it still equals [shown] --
     * without this check, dismissing an older message could race a newer
     * one that arrived while the first was still showing and clobber it
     * before it ever got a chance to display.
     */
    data class DismissSnackbar(val shown: String) : AppEvent
}

/**
 * Owns the one [NavetteApi] connection this screen uses. Single-host only,
 * no reconnect/backoff, no session screen wired up yet -- this is the
 * drawer slice of M3 (see docs/ROADMAP.md Phase 1), not the whole milestone.
 *
 * [clientFactory] defaults to a real [NavetteClient] but is overridable so
 * tests can inject a fake instead of standing up real networking -- see
 * `AppViewModelTest`.
 */
class AppViewModel(
    private val clientFactory: (host: String) -> NavetteApi = { host -> NavetteClient(controlWebSocketUrl(host)) },
) : ViewModel() {
    private val _state = MutableStateFlow(AppUiState())
    val state: StateFlow<AppUiState> = _state.asStateFlow()

    private var client: NavetteApi? = null

    // The previous connection's connectionState collector must be cancelled
    // before starting a new one -- a StateFlow never completes on its own,
    // so without this, every reconnect attempt (e.g. retrying after a
    // Failed state) leaks a collector that sits idle for the rest of this
    // ViewModel's lifetime instead of stopping when its client is replaced.
    private var connectionJob: Job? = null

    // Guards against two refresh() calls overlapping -- e.g. a manual
    // Refresh while the post-Connect refresh is still in flight -- where
    // whichever response happened to land last would win regardless of
    // which request was actually newer.
    private var refreshJob: Job? = null

    fun onEvent(event: AppEvent) {
        when (event) {
            is AppEvent.HostChanged -> _state.update { it.copy(host = event.host) }
            AppEvent.Connect -> connect()
            AppEvent.Refresh -> refresh()
            is AppEvent.RunApp -> runApp(event.appId)
            is AppEvent.AttachSession -> attachSession(event.session)
            is AppEvent.DismissSnackbar ->
                _state.update {
                    if (it.snackbarMessage == event.shown) it.copy(snackbarMessage = null) else it
                }
        }
    }

    private fun connect() {
        val host = _state.value.host.trim()
        if (host.isEmpty()) return

        connectionJob?.cancel()
        refreshJob?.cancel()
        client?.close()
        val newClient = clientFactory(host)
        client = newClient

        connectionJob =
            viewModelScope.launch {
                newClient.connectionState.collect { connectionState ->
                    _state.update { it.copy(connection = connectionState) }
                    if (connectionState is ConnectionState.Connected) {
                        refresh()
                    }
                    if (connectionState is ConnectionState.Failed) {
                        _state.update { it.copy(snackbarMessage = connectionState.reason) }
                    }
                }
            }
        newClient.connect()
    }

    private fun refresh() {
        val current = client ?: return
        refreshJob?.cancel()
        refreshJob =
            viewModelScope.launch {
                _state.update { it.copy(isLoading = true) }
                try {
                    val (appsResponse, sessionsResponse) =
                        coroutineScope {
                            val apps = async { current.call(RequestCommand.ListApps) }
                            val sessions = async { current.call(RequestCommand.ListSessions) }
                            apps.await() to sessions.await()
                        }
                    val firstError = appsResponse.errorOrNull() ?: sessionsResponse.errorOrNull()
                    _state.update {
                        it.copy(
                            apps = appsResponse.appsOrNull() ?: it.apps,
                            sessions = sessionsResponse.sessionsOrNull() ?: it.sessions,
                            isLoading = false,
                            snackbarMessage = firstError?.message ?: it.snackbarMessage,
                        )
                    }
                } catch (error: CancellationException) {
                    throw error
                } catch (error: Exception) {
                    _state.update {
                        it.copy(isLoading = false, snackbarMessage = error.message ?: "refresh failed")
                    }
                }
            }
    }

    private fun runApp(appId: String) {
        val current = client ?: return
        viewModelScope.launch {
            try {
                val response = current.call(RequestCommand.Run(appId))
                val error = response.errorOrNull()
                if (error != null) {
                    _state.update { it.copy(snackbarMessage = error.message) }
                } else {
                    refresh()
                }
            } catch (error: CancellationException) {
                throw error
            } catch (error: Exception) {
                _state.update { it.copy(snackbarMessage = error.message ?: "run failed") }
            }
        }
    }

    private fun attachSession(session: String) {
        val current = client ?: return
        viewModelScope.launch {
            try {
                val response = current.call(RequestCommand.Attach(session))
                val error = response.errorOrNull()
                val message =
                    error?.message
                        ?: "Attached to $session -- the session screen isn't built yet (next slice of M3)."
                _state.update { it.copy(snackbarMessage = message) }
            } catch (error: CancellationException) {
                throw error
            } catch (error: Exception) {
                _state.update { it.copy(snackbarMessage = error.message ?: "attach failed") }
            }
        }
    }

    override fun onCleared() {
        client?.close()
    }
}
