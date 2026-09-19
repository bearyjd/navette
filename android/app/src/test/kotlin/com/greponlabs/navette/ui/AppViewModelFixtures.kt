package com.greponlabs.navette.ui

import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.NavetteApi
import com.greponlabs.navette.net.Pairing
import com.greponlabs.navette.net.PairingRegistry
import com.greponlabs.navette.net.PairingStore
import com.greponlabs.navette.net.SavedPairing
import com.greponlabs.navette.net.WakeResult
import com.greponlabs.navette.net.WakeTarget
import com.greponlabs.navette.net.WakeTransport
import com.greponlabs.navette.protocol.App
import com.greponlabs.navette.protocol.RequestCommand
import com.greponlabs.navette.protocol.Response
import com.greponlabs.navette.protocol.ResponseOutcome
import com.greponlabs.navette.protocol.ResponseResult
import com.greponlabs.navette.protocol.Session
import com.greponlabs.navette.protocol.SessionStatus
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/*
 * Fakes and fixtures shared by AppViewModelTest and AppViewModelWakeTest.
 * Hand-written, per this project's testing convention -- no mocking framework.
 */

/** Hand-written fake, per this project's testing convention -- no mocking framework. */
internal class FakeNavetteApi : NavetteApi {
    private val _connectionState = MutableStateFlow<ConnectionState>(ConnectionState.Disconnected)
    override val connectionState: StateFlow<ConnectionState> = _connectionState.asStateFlow()

    var closed = false
        private set
    val calls = mutableListOf<RequestCommand>()
    var responseFor: (RequestCommand) -> Response = { command ->
        Response(1, ResponseOutcome.Ok(ResponseResult.Ack))
    }

    fun emit(state: ConnectionState) {
        _connectionState.value = state
    }

    override fun connect() {
        // The test drives connection state directly via emit(); a real
        // connect() would start the actual WebSocket handshake.
    }

    override suspend fun call(command: RequestCommand): Response {
        calls.add(command)
        return responseFor(command)
    }

    // Unlike the real client's close(), this doesn't interrupt an in-flight
    // call() -- fine today since no test exercises that overlap, but worth
    // flagging so a future test doesn't assume this fake matches that
    // behavior.
    override fun close() {
        closed = true
    }
}

/** Hand-written fake, per this project's testing convention -- no mocking framework. */
internal class FakePairingStore(initial: Pairing? = null) : PairingStore {
    private var stored: Pairing? = initial
    private var registry = initial?.let { PairingRegistry(listOf(SavedPairing("initial", it)), "initial") } ?: PairingRegistry()

    // Models EncryptedPairingStore under a failed keystore. `by lazy` does not
    // memoize a thrown initializer, so the real store re-throws on every
    // touch rather than failing once -- hence a sticky flag, not a one-shot.
    var failOnSave = false
    var failOnLoad = false

    override fun load(): Pairing? {
        if (failOnLoad) throw IllegalStateException("keystore unavailable")
        return stored
    }

    override fun save(pairing: Pairing) {
        if (failOnSave) throw IllegalStateException("keystore unavailable")
        stored = pairing
    }

    override fun loadRegistry(): PairingRegistry {
        if (failOnLoad) throw IllegalStateException("keystore unavailable")
        return registry
    }

    override fun upsert(pairing: Pairing): PairingRegistry {
        if (failOnSave) throw IllegalStateException("keystore unavailable")
        val existing = registry.hosts.firstOrNull { it.pairing.host == pairing.host && it.pairing.port == pairing.port }
        // Like the real codec: re-pairing rotates the token and keeps the wake target.
        val saved = SavedPairing(existing?.id ?: "host-${registry.hosts.size + 1}", pairing, existing?.wake)
        registry = PairingRegistry(registry.hosts.filterNot { it.id == saved.id } + saved, saved.id)
        stored = pairing
        return registry
    }

    override fun select(id: String): PairingRegistry {
        registry = PairingRegistry(registry.hosts, id)
        stored = registry.active?.pairing
        return registry
    }

    override fun delete(id: String): PairingRegistry {
        val hosts = registry.hosts.filterNot { it.id == id }.map { if (it.wake?.viaId == id) it.copy(wake = null) else it }
        registry = PairingRegistry(hosts, registry.activeId?.takeIf { it != id })
        stored = registry.active?.pairing
        return registry
    }

    override fun setWake(hostId: String, wake: WakeTarget?): PairingRegistry {
        if (failOnSave) throw IllegalStateException("keystore unavailable")
        require(registry.hosts.any { it.id == hostId }) { "unknown host" }
        registry = registry.copy(hosts = registry.hosts.map { if (it.id == hostId) it.copy(wake = wake) else it })
        return registry
    }

    override fun clear() {
        stored = null
        registry = PairingRegistry()
    }
}

/** Hand-written fake, per this project's testing convention -- no mocking framework. */
internal class FakeWakeTransport(var result: WakeResult = WakeResult.Sent) : WakeTransport {
    val wakes = mutableListOf<Pair<Pairing, String>>()

    /**
     * When set, `wake` suspends on it and answers with its value instead of
     * [result] -- a relay that has not replied yet, so a test can move the
     * ViewModel on (switch host, delete the relay) while a wake is in flight.
     */
    var gate: CompletableDeferred<WakeResult>? = null

    override suspend fun wake(via: Pairing, mac: String): WakeResult {
        wakes.add(via to mac)
        return gate?.await() ?: result
    }
}

internal val testPairing = Pairing(host = "tower", port = 9417, token = "test-token")

internal val testSession =
    Session(
        name = "work",
        appId = "firefox.desktop",
        appPid = 10,
        daemonPid = 11,
        waylandDisplay = "navette-work",
        socketPath = "/run/user/1000/navette/work/wprs.sock",
        createdAtMs = 1_700_000_000_000,
        status = SessionStatus.RUNNING,
    )

internal val testApp = App(id = "firefox.desktop", name = "Firefox")
