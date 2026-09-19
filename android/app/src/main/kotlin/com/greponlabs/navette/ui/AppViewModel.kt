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
import com.greponlabs.navette.net.HttpImageFetcher
import com.greponlabs.navette.net.HttpWakeTransport
import com.greponlabs.navette.net.ImageCache
import com.greponlabs.navette.net.ImageFetcher
import com.greponlabs.navette.net.ImageRepository
import com.greponlabs.navette.net.NavetteApi
import com.greponlabs.navette.net.NavetteClient
import com.greponlabs.navette.net.Pairing
import com.greponlabs.navette.net.PairingRegistry
import com.greponlabs.navette.net.PairingStore
import com.greponlabs.navette.net.SavedPairing
import com.greponlabs.navette.net.WakeResult
import com.greponlabs.navette.net.WakeTarget
import com.greponlabs.navette.net.WakeTransport
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
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch

data class AppUiState(
    val pairing: Pairing? = null,
    val registry: PairingRegistry = PairingRegistry(),
    val connection: ConnectionState = ConnectionState.Disconnected,
    val apps: List<App> = emptyList(),
    val sessions: List<Session> = emptyList(),
    val isLoading: Boolean = false,
    val snackbarMessage: String? = null,
    /** The session the user is attached to, or `null` when the drawer is showing. */
    val activeSession: String? = null,
    /** Host management is explicit, so deleting an active host never auto-selects another. */
    val showingHosts: Boolean = false,
    val addingHost: Boolean = false,
    val wake: WakeUiState = WakeUiState.Idle,
    /**
     * Bumped once per completed refresh call -- a round-trip that got answers,
     * error responses included, but not one that threw -- so the drawer's
     * session thumbnails revalidate at exactly the cadence of the session list
     * they sit beside: no timer of their own, and nothing while the host is
     * not answering at all.
     */
    val refreshTick: Int = 0,
) {
    /**
     * The wake the user can send right now, resolved from [pairing] rather than
     * the registry's active id: the two diverge after a failed save, when the
     * pairing in use was never stored and the active host is a different
     * computer whose wake target must not be offered for this one. One
     * definition, shared by the Wake button and [AppEvent.WakeActive], so the
     * two cannot disagree about which relay is meant.
     */
    val wakeRoute: WakeRoute?
        get() {
            val current = pairing ?: return null
            val saved = registry.hosts.firstOrNull { it.pairing.host == current.host && it.pairing.port == current.port }
            val target = saved?.wake ?: return null
            val via = registry.hosts.firstOrNull { it.id == target.viaId } ?: return null
            return WakeRoute(target.mac, via)
        }
}

/** [mac] is the sleeping host's; [via] is the saved always-on daemon that broadcasts the packet for it. */
data class WakeRoute(val mac: String, val via: SavedPairing)

sealed interface WakeUiState {
    data object Idle : WakeUiState

    data object Sending : WakeUiState

    data class Sent(val viaLabel: String) : WakeUiState

    data class Failed(val message: String) : WakeUiState
}

/** The user-facing reading of a relay's answer; [viaLabel] is the relay's [SavedPairing.endpointLabel]. */
internal fun WakeResult.toUiState(viaLabel: String): WakeUiState =
    when (this) {
        WakeResult.Sent -> WakeUiState.Sent(viaLabel)
        WakeResult.Unreachable -> WakeUiState.Failed("Could not reach $viaLabel")
        is WakeResult.Rejected ->
            if (code == HTTP_UNAUTHORIZED) {
                WakeUiState.Failed("$viaLabel rejected the token")
            } else {
                WakeUiState.Failed("$viaLabel could not send (HTTP $code)")
            }
    }

private const val HTTP_UNAUTHORIZED = 401

sealed interface AppEvent {
    /** A pairing was just obtained -- by scanning a QR code or by manual entry. Saves and connects. */
    data class Paired(val pairing: Pairing) : AppEvent

    data object ShowHosts : AppEvent
    data object HideHosts : AppEvent
    data object AddHost : AppEvent
    data object CancelAddHost : AppEvent
    data class SelectHost(val id: String) : AppEvent
    data class DeleteHost(val id: String) : AppEvent

    /** Sets (or, with `null`, clears) how the saved host [hostId] is woken. */
    data class SetWake(val hostId: String, val wake: WakeTarget?) : AppEvent

    /**
     * Asks the relay in [AppUiState.wakeRoute] to send a magic packet to the
     * host the current pairing failed to reach. A no-op when no wake is
     * configured or one is already in flight.
     */
    data object WakeActive : AppEvent

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
 * Owns the active [NavetteApi] control connection, the encrypted host
 * registry, and which session (if any) is currently attached. Selecting a
 * host persists that active choice, cancels collectors for the old client,
 * closes it, and clears the old session before the replacement connects. No
 * reconnect/backoff beyond [AppEvent.Reconnect].
 *
 * Media state is deliberately not here: `SessionScreen` owns its own
 * `MediaClient` and decoder, and this ViewModel stays the control-channel
 * owner it has always been.
 *
 * [pairingStore] is the single source of truth for the encrypted host
 * registry and active host -- see [AppEvent.Paired] and the `init` block
 * below, which resumes that active host on every fresh launch. [clientFactory]
 * [wakeTransport] and [imageFetcher] default to the real [NavetteClient],
 * [HttpWakeTransport] and [HttpImageFetcher] but are overridable so tests can
 * inject fakes instead of standing up real networking -- see `AppViewModelTest`.
 */
class AppViewModel(
    private val pairingStore: PairingStore,
    private val clientFactory: (pairing: Pairing) -> NavetteApi = { pairing ->
        NavetteClient(controlWebSocketUrl(pairing.host, pairing.port), token = pairing.token)
    },
    private val wakeTransport: WakeTransport = HttpWakeTransport(),
    imageFetcher: ImageFetcher = HttpImageFetcher(),
) : ViewModel() {
    private val _state = MutableStateFlow(AppUiState())
    val state: StateFlow<AppUiState> = _state.asStateFlow()

    /**
     * The drawer's thumbnails and icons. Lives here, not in the screen, so the
     * cache survives the drawer leaving composition for a session and coming
     * back; cleared wherever the pairing changes so one host's images are
     * never shown for another -- see [connectWithPairing] and [deleteHost].
     */
    val imageRepository = ImageRepository(imageFetcher, ImageCache(), viewModelScope)

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

    // The wake in flight, if any. Cancelled wherever the verdict is reset, so
    // a relay's late answer cannot land on a host it was never about -- see
    // wakeActive() for the race.
    private var wakeJob: Job? = null

    init {
        // Resumes the last pairing on every fresh launch -- this is what
        // makes "kill the app, reopen it" retry the stored token rather than
        // sitting on an empty ConnectScreen until the user re-scans. Guarded:
        // EncryptedPairingStore's prefs are built lazily on first touch, and
        // this is the first call to reach it in the app's lifetime, so a
        // keystore failure here must yield "no pairing, show ConnectScreen"
        // rather than crashing startup.
        runCatching { pairingStore.loadRegistry() }
            .onFailure { Log.w(TAG, "failed to load a stored pairing: ${it.message}") }
            .getOrNull()
            ?.also { registry -> _state.update { it.copy(registry = registry) } }
            ?.active
            ?.pairing
            ?.let(::connectWithPairing)
    }

    fun onEvent(event: AppEvent) {
        when (event) {
            is AppEvent.Paired -> pair(event.pairing)
            AppEvent.ShowHosts -> _state.update { it.copy(showingHosts = true, addingHost = false) }
            AppEvent.HideHosts -> _state.update { it.copy(showingHosts = false, addingHost = false) }
            AppEvent.AddHost -> _state.update { it.copy(showingHosts = false, addingHost = true) }
            AppEvent.CancelAddHost -> _state.update { it.copy(showingHosts = it.registry.hosts.isNotEmpty(), addingHost = false) }
            is AppEvent.SelectHost -> selectHost(event.id)
            is AppEvent.DeleteHost -> deleteHost(event.id)
            is AppEvent.SetWake -> setWake(event.hostId, event.wake)
            AppEvent.WakeActive -> wakeActive()
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
     *
     * The message is about the *next launch* and nothing else, which is now the
     * whole of what goes wrong: [reconnect] prefers the active pairing, so
     * Retry within this session reaches the host the user just paired with
     * rather than the stale stored one. What a failed `save` still costs is
     * persistence — a failed save leaves whatever was stored before intact, so
     * the next launch resumes that (or shows ConnectScreen if there was
     * nothing). Either way this pairing is not remembered, which is what the
     * message says and all it says.
     */
    private fun pair(pairing: Pairing) {
        val registry =
            runCatching { pairingStore.upsert(pairing) }
                .onFailure { Log.w(TAG, "failed to save the pairing: ${it.message}") }
                .getOrNull()
        connectWithPairing(pairing)
        _state.update { it.copy(registry = registry ?: it.registry, showingHosts = false, addingHost = false) }
        if (registry == null) {
            _state.update {
                it.copy(
                    snackbarMessage = "Pairing not saved — this device won't remember it next launch.",
                )
            }
        }
    }

    /**
     * Retries the pairing currently in use, falling back to storage only when
     * there is none.
     *
     * The order matters. The active pairing and the stored one diverge exactly
     * when a `save` failed — [pair] deliberately carries on with the new
     * pairing — so reloading from storage first would silently reconnect to the
     * *old* host, or, with an empty store, do nothing at all and leave Retry
     * looking broken. Retrying what the user is actually connected with is both
     * the obvious reading of the button and the only one that is right in that
     * case.
     *
     * The storage path stays for the launch where `init`'s load failed or found
     * nothing: a keystore that recovers between then and the button press is
     * worth a second attempt. It is guarded like [pair] — a user-initiated
     * retry must not crash on a button press — and a failure there is surfaced,
     * since a Retry that silently does nothing is indistinguishable from a
     * broken button.
     */
    private fun reconnect() {
        _state.value.pairing?.let {
            connectWithPairing(it)
            return
        }
        runCatching { pairingStore.loadRegistry() }
            .onFailure { error ->
                Log.w(TAG, "failed to load a stored pairing: ${error.message}")
                _state.update {
                    it.copy(
                        snackbarMessage = "Could not read the saved pairing. Scan the pairing code again.",
                    )
                }
            }
            .getOrNull()
            ?.also { registry -> _state.update { it.copy(registry = registry) } }
            ?.active
            ?.pairing
            ?.let(::connectWithPairing)
    }

    private fun selectHost(id: String) {
        val selected = _state.value.registry.hosts.firstOrNull { it.id == id } ?: return
        val persisted = runCatching { pairingStore.select(id) }
            .onFailure { Log.w(TAG, "failed to select a saved host: ${it.message}") }
            .getOrNull() ?: return
        _state.update { it.copy(registry = persisted, showingHosts = false, addingHost = false) }
        connectWithPairing(selected.pairing)
    }

    private fun deleteHost(id: String) {
        val prior = _state.value.registry
        val deleted = prior.hosts.firstOrNull { it.id == id } ?: return
        val updated = runCatching { pairingStore.delete(id) }
            .onFailure { Log.w(TAG, "failed to delete a saved host: ${it.message}") }
            .getOrNull() ?: return
        if (prior.activeId != deleted.id) {
            // Only the relay's deletion touches the verdict: "sent via nas"
            // stops being advice once nas is gone, and a wake still asking it
            // has nothing to wait for. Any other host has no bearing on it, and
            // resetting anyway would drop a real verdict or re-enable the
            // button mid-send.
            val relayDeleted = deleted.id == _state.value.wakeRoute?.via?.id
            if (relayDeleted) wakeJob?.cancel()
            _state.update { it.copy(registry = updated, wake = if (relayDeleted) WakeUiState.Idle else it.wake) }
            return
        }
        connectionJob?.cancel()
        refreshJob?.cancel()
        wakeJob?.cancel()
        client?.close()
        client = null
        imageRepository.clear()
        _state.update {
            it.copy(
                pairing = null, registry = updated, connection = ConnectionState.Disconnected,
                apps = emptyList(), sessions = emptyList(), isLoading = false, activeSession = null,
                showingHosts = true, addingHost = false, wake = WakeUiState.Idle,
            )
        }
    }

    /**
     * Guarded like [deleteHost]: the dialog already validated the MAC and the
     * relay, so what can still fail here is the keystore, and a setting that
     * silently did not stick would only be discovered the next time the host
     * is asleep. The registry in state is left as it was, so the list keeps
     * showing what is actually stored. A verdict from the previous target is
     * dropped along with the target: "sent via nas" is no longer advice once
     * the relay is something else.
     */
    private fun setWake(hostId: String, wake: WakeTarget?) {
        val updated = runCatching { pairingStore.setWake(hostId, wake) }
            .onFailure { Log.w(TAG, "failed to save the wake target: ${it.message}") }
            .getOrNull()
        if (updated == null) {
            _state.update { it.copy(snackbarMessage = "Wake-up settings not saved.") }
            return
        }
        wakeJob?.cancel()
        _state.update { it.copy(registry = updated, wake = WakeUiState.Idle) }
    }

    /**
     * One wake at a time: a second tap while the first is in flight is dropped
     * rather than queued, since two magic packets do nothing one does not.
     *
     * The verdict is only ever about the host the wake was sent for. Every
     * site that resets [AppUiState.wake] -- a Retry or host switch
     * ([connectWithPairing]), the relay going away ([deleteHost]), the target
     * changing ([setWake]) -- also cancels [wakeJob], so a relay that answers
     * after the user has moved on cannot publish "sent via nas" over a host
     * that has nothing to do with nas, hiding that host's own Wake button.
     */
    private fun wakeActive() {
        if (_state.value.wake is WakeUiState.Sending) return
        val route = _state.value.wakeRoute ?: return
        val viaLabel = route.via.endpointLabel
        // Synchronously, so the guard above sees it: the coroutine below does
        // not run until the dispatcher gets to it, and two taps can land first.
        _state.update { it.copy(wake = WakeUiState.Sending) }
        wakeJob =
            viewModelScope.launch {
                val result =
                    try {
                        wakeTransport.wake(route.via.pairing, route.mac)
                    } catch (error: CancellationException) {
                        throw error
                    } catch (error: Exception) {
                        // HttpWakeTransport already maps IOException; this is the
                        // net for anything else, so a wake never crashes the app.
                        Log.w(TAG, "wake via $viaLabel failed: ${error.message}")
                        WakeResult.Unreachable
                    }
                // A transport that returned normally after this job was
                // cancelled (a blocking HTTP call that finished anyway) must
                // still not publish: cancellation is only guaranteed to be
                // observed at a suspension point, and this is the last one.
                ensureActive()
                _state.update { it.copy(wake = result.toUiState(viaLabel)) }
            }
    }

    private fun connectWithPairing(pairing: Pairing) {
        connectionJob?.cancel()
        refreshJob?.cancel()
        wakeJob?.cancel()
        client?.close()
        // Every pairing change funnels through here (init, pair, reconnect,
        // selectHost), so this is the one place the image cache needs clearing
        // for host A's thumbnails never to appear on host B's drawer.
        imageRepository.clear()
        // A reconnect must not leave the user on a session screen belonging to
        // the connection being replaced, nor carry a wake verdict that was
        // advice for the previous attempt.
        _state.update { it.copy(pairing = pairing, activeSession = null, showingHosts = false, addingHost = false, wake = WakeUiState.Idle) }
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
                            refreshTick = it.refreshTick + 1,
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
