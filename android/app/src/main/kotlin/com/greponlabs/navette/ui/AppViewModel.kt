package com.greponlabs.navette.ui

import android.content.Context
import android.util.Log
import androidx.lifecycle.ViewModel
import androidx.lifecycle.ViewModelProvider
import androidx.lifecycle.viewModelScope
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.EncryptedPairingStore
import com.greponlabs.navette.net.NavetteApi
import com.greponlabs.navette.net.NavetteClient
import com.greponlabs.navette.net.Pairing
import com.greponlabs.navette.net.PairingStore
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
    val pairing: Pairing? = null,
    val connection: ConnectionState = ConnectionState.Disconnected,
    val apps: List<App> = emptyList(),
    val sessions: List<Session> = emptyList(),
    val isLoading: Boolean = false,
    val snackbarMessage: String? = null,
    /** The session the user is attached to, or `null` when the drawer is showing. */
    val activeSession: String? = null,
)

sealed interface AppEvent {
    /** A pairing was just obtained -- by scanning a QR code or by manual entry. Saves and connects. */
    data class Paired(val pairing: Pairing) : AppEvent

    /**
     * Retries the current pairing without asking the user to scan or type it
     * again. Only meaningful after a transient [ConnectionState.Failed] --
     * [ConnectionState.Unauthorized] means the stored token was rejected, and
     * resending it would just fail the same way.
     */
    data object Reconnect : AppEvent

    data object Refresh : AppEvent

    data class RunApp(val appId: String) : AppEvent

    data class AttachSession(val session: String) : AppEvent

    /** Leaves the session screen: detaches server-side and returns to the drawer. */
    data object LeaveSession : AppEvent

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
 * Owns the one [NavetteApi] control connection this app uses, and which
 * session (if any) is currently attached. Single-host only, no
 * reconnect/backoff beyond [AppEvent.Reconnect].
 *
 * Media state is deliberately not here: `SessionScreen` owns its own
 * `MediaClient` and decoder, and this ViewModel stays the control-channel
 * owner it has always been.
 *
 * [pairingStore] is the single source of truth for host, port and token --
 * see [AppEvent.Paired] and the `init` block below, which resumes the last
 * pairing on every fresh launch of this ViewModel. [clientFactory] defaults
 * to a real [NavetteClient] but is overridable so tests can inject a fake
 * instead of standing up real networking -- see `AppViewModelTest`.
 */
class AppViewModel(
    private val pairingStore: PairingStore,
    private val clientFactory: (pairing: Pairing) -> NavetteApi = { pairing ->
        NavetteClient(controlWebSocketUrl(pairing.host, pairing.port), token = pairing.token)
    },
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

    init {
        // Resumes the last pairing on every fresh launch -- this is what
        // makes "kill the app, reopen it" retry the stored token rather than
        // sitting on an empty ConnectScreen until the user re-scans. Guarded:
        // EncryptedPairingStore's prefs are built lazily on first touch, and
        // this is the first call to reach it in the app's lifetime, so a
        // keystore failure here must yield "no pairing, show ConnectScreen"
        // rather than crashing startup.
        runCatching { pairingStore.load() }
            .onFailure { Log.w(TAG, "failed to load a stored pairing: ${it.message}") }
            .getOrNull()
            ?.let { connectWithPairing(it) }
    }

    fun onEvent(event: AppEvent) {
        when (event) {
            is AppEvent.Paired -> pair(event.pairing)
            AppEvent.Reconnect -> reconnect()
            AppEvent.Refresh -> refresh()
            is AppEvent.RunApp -> runApp(event.appId)
            is AppEvent.AttachSession -> attachSession(event.session)
            AppEvent.LeaveSession -> leaveSession()
            is AppEvent.DismissSnackbar ->
                _state.update {
                    if (it.snackbarMessage == event.shown) it.copy(snackbarMessage = null) else it
                }
        }
    }

    /**
     * Guarded for the same reason `init`'s [PairingStore.load] is, and it is
     * the more dangerous of the two: `by lazy` does not memoize a thrown
     * initializer, so [EncryptedPairingStore]'s prefs re-throw on every touch
     * once the keystore has failed. This call runs on the main thread inside
     * the QR scanner's success callback, where an escaping exception is a
     * crash immediately after a successful scan.
     *
     * A pairing that could not be stored must not read as one that succeeded,
     * so the failure is surfaced rather than logged and swallowed. The
     * connection still goes ahead: the scanned pairing is good for this
     * session, and refusing to use it would make a device with a broken
     * keystore unusable rather than merely forgetful.
     */
    private fun pair(pairing: Pairing) {
        val stored =
            runCatching { pairingStore.save(pairing) }
                .onFailure { Log.w(TAG, "failed to save the pairing: ${it.message}") }
                .isSuccess
        connectWithPairing(pairing)
        if (!stored) {
            _state.update {
                it.copy(
                    snackbarMessage =
                        "Paired, but this pairing could not be saved — you will have to pair again next launch.",
                )
            }
        }
    }

    /**
     * Guarded like [pair]: this is a user-initiated retry, so a keystore
     * failure here would crash on a button press. A reconnect with nothing to
     * reconnect *with* is a dead end the user has to be told about — silently
     * doing nothing would leave the Retry button looking broken.
     */
    private fun reconnect() {
        runCatching { pairingStore.load() }
            .onFailure { error ->
                Log.w(TAG, "failed to load a stored pairing: ${error.message}")
                _state.update {
                    it.copy(
                        snackbarMessage = "Could not read the saved pairing. Scan the pairing code again.",
                    )
                }
            }
            .getOrNull()
            ?.let { connectWithPairing(it) }
    }

    private fun connectWithPairing(pairing: Pairing) {
        connectionJob?.cancel()
        refreshJob?.cancel()
        client?.close()
        // A reconnect must not leave the user on a session screen belonging to
        // the connection being replaced.
        _state.update { it.copy(pairing = pairing, activeSession = null) }
        val newClient = clientFactory(pairing)
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

    /**
     * A successful attach navigates; a failed one stays on the drawer with the
     * server's own message. Navigation is the confirmation, so success adds no
     * snackbar of its own.
     */
    private fun attachSession(session: String) {
        val current = client ?: return
        viewModelScope.launch {
            try {
                val response = current.call(RequestCommand.Attach(session))
                val error = response.errorOrNull()
                if (error != null) {
                    _state.update { it.copy(snackbarMessage = error.message) }
                } else {
                    _state.update { it.copy(activeSession = session) }
                }
            } catch (error: CancellationException) {
                throw error
            } catch (error: Exception) {
                _state.update { it.copy(snackbarMessage = error.message ?: "attach failed") }
            }
        }
    }

    /**
     * The state flip happens first and synchronously: leaving the screen must
     * never be blocked by a `Detach` that fails or hangs, and a stale
     * [AppUiState.activeSession] would strand the user on a dead screen.
     *
     * `Detach` is not `Kill` -- the session keeps running server-side and
     * shows up on the drawer again.
     */
    private fun leaveSession() {
        val session = _state.value.activeSession ?: return
        _state.update { it.copy(activeSession = null) }
        val current = client ?: return
        viewModelScope.launch {
            try {
                current.call(RequestCommand.Detach(session))
            } catch (error: CancellationException) {
                throw error
            } catch (error: Exception) {
                // Best-effort: the user has already left, and the bridge drops
                // an attachment when its media socket closes regardless.
                Log.d(TAG, "detach from $session failed: ${error.message}")
            }
            refresh()
        }
    }

    override fun onCleared() {
        client?.close()
    }

    companion object {
        private const val TAG = "AppViewModel"

        /** Builds this ViewModel with a real [EncryptedPairingStore] backed by [context]. */
        fun factory(context: Context): ViewModelProvider.Factory =
            viewModelFactory {
                initializer { AppViewModel(pairingStore = EncryptedPairingStore(context.applicationContext)) }
            }
    }
}
