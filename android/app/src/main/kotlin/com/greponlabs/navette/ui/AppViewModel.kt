package com.greponlabs.navette.ui

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.NavetteClient
import com.greponlabs.navette.net.controlWebSocketUrl
import com.greponlabs.navette.protocol.App
import com.greponlabs.navette.protocol.RequestCommand
import com.greponlabs.navette.protocol.Session
import com.greponlabs.navette.protocol.appsOrNull
import com.greponlabs.navette.protocol.errorOrNull
import com.greponlabs.navette.protocol.sessionsOrNull
import kotlin.coroutines.cancellation.CancellationException
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

    data object DismissSnackbar : AppEvent
}

/**
 * Owns the one [NavetteClient] this screen uses. Single-host only, no
 * reconnect/backoff, no session screen wired up yet -- this is the drawer
 * slice of M3 (see docs/ROADMAP.md Phase 1), not the whole milestone.
 */
class AppViewModel : ViewModel() {
    private val _state = MutableStateFlow(AppUiState())
    val state: StateFlow<AppUiState> = _state.asStateFlow()

    private var client: NavetteClient? = null

    fun onEvent(event: AppEvent) {
        when (event) {
            is AppEvent.HostChanged -> _state.update { it.copy(host = event.host) }
            AppEvent.Connect -> connect()
            AppEvent.Refresh -> refresh()
            is AppEvent.RunApp -> runApp(event.appId)
            is AppEvent.AttachSession -> attachSession(event.session)
            AppEvent.DismissSnackbar -> _state.update { it.copy(snackbarMessage = null) }
        }
    }

    private fun connect() {
        val host = _state.value.host.trim()
        if (host.isEmpty()) return

        client?.close()
        val newClient = NavetteClient(controlWebSocketUrl(host))
        client = newClient

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
        viewModelScope.launch {
            _state.update { it.copy(isLoading = true) }
            try {
                val appsResponse = current.call(RequestCommand.ListApps)
                val sessionsResponse = current.call(RequestCommand.ListSessions)
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
