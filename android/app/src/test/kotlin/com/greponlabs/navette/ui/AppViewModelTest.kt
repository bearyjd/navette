package com.greponlabs.navette.ui

import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.NavetteApi
import com.greponlabs.navette.net.Pairing
import com.greponlabs.navette.net.PairingStore
import com.greponlabs.navette.protocol.ApiError
import com.greponlabs.navette.protocol.App
import com.greponlabs.navette.protocol.AttachInfo
import com.greponlabs.navette.protocol.ErrorCode
import com.greponlabs.navette.protocol.RequestCommand
import com.greponlabs.navette.protocol.Response
import com.greponlabs.navette.protocol.ResponseOutcome
import com.greponlabs.navette.protocol.ResponseResult
import com.greponlabs.navette.protocol.Session
import com.greponlabs.navette.protocol.SessionStatus
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.test.setMain
import org.junit.After
import org.junit.Before
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** Hand-written fake, per this project's testing convention -- no mocking framework. */
private class FakeNavetteApi : NavetteApi {
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
private class FakePairingStore(initial: Pairing? = null) : PairingStore {
    private var stored: Pairing? = initial

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

    override fun clear() {
        stored = null
    }
}

private val testPairing = Pairing(host = "tower", port = 9417, token = "test-token")

private val testSession =
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

private val testApp = App(id = "firefox.desktop", name = "Firefox")

@OptIn(ExperimentalCoroutinesApi::class)
class AppViewModelTest {
    private lateinit var fake: FakeNavetteApi
    private lateinit var viewModel: AppViewModel

    @Before
    fun setUp() {
        Dispatchers.setMain(StandardTestDispatcher())
        fake = FakeNavetteApi()
        viewModel = AppViewModel(pairingStore = FakePairingStore(), clientFactory = { fake })
    }

    @After
    fun tearDown() {
        Dispatchers.resetMain()
    }

    @Test
    fun `connecting and reaching Connected state triggers a refresh`() =
        runTest {
            fake.responseFor = { command ->
                when (command) {
                    RequestCommand.ListApps -> Response(1, ResponseOutcome.Ok(ResponseResult.Apps(listOf(testApp))))
                    RequestCommand.ListSessions ->
                        Response(2, ResponseOutcome.Ok(ResponseResult.Sessions(listOf(testSession))))
                    else -> error("unexpected command in this test: $command")
                }
            }

            viewModel.onEvent(AppEvent.Paired(testPairing))
            fake.emit(ConnectionState.Connected)
            testScheduler.advanceUntilIdle()

            val state = viewModel.state.value
            assertEquals(ConnectionState.Connected, state.connection)
            assertEquals(listOf(testApp), state.apps)
            assertEquals(listOf(testSession), state.sessions)
            assertTrue(fake.calls.contains(RequestCommand.ListApps))
            assertTrue(fake.calls.contains(RequestCommand.ListSessions))
        }

    @Test
    fun `reconnecting closes the previous client and stops listening to its state`() =
        runTest {
            // clientFactory is invoked once per pairing, on the SAME
            // ViewModel instance -- this is the actual regression shape
            // (a user re-pairing), not two independent ViewModels.
            val firstClient = FakeNavetteApi()
            val secondClient = FakeNavetteApi()
            val clients = ArrayDeque(listOf(firstClient, secondClient))
            val vm = AppViewModel(pairingStore = FakePairingStore(), clientFactory = { clients.removeFirst() })

            vm.onEvent(AppEvent.Paired(testPairing))
            firstClient.emit(ConnectionState.Connected)
            testScheduler.advanceUntilIdle()
            assertEquals(ConnectionState.Connected, vm.state.value.connection)

            vm.onEvent(AppEvent.Paired(testPairing))
            testScheduler.advanceUntilIdle()
            assertTrue("connect() must close the client it is replacing", firstClient.closed)

            // The regression this guards: if the first connect()'s
            // connectionState collector had leaked instead of being
            // cancelled, this late emission on the now-superseded client
            // would still reach state and flip it back to Failed.
            firstClient.emit(ConnectionState.Failed("stale, should be ignored"))
            testScheduler.advanceUntilIdle()
            assertEquals(ConnectionState.Disconnected, vm.state.value.connection)
        }

    @Test
    fun `a pairing that cannot be saved still connects, and says so`() =
        runTest {
            // The crash path this guards: keystore fails, init catches it and
            // shows ConnectScreen, the user scans a QR, and save() throws out
            // of the scanner's main-thread success callback. `by lazy` does not
            // memoize a thrown initializer, so the real store throws here even
            // though init already absorbed one failure.
            val store = FakePairingStore().apply { failOnSave = true }
            val vm = AppViewModel(pairingStore = store, clientFactory = { fake })

            vm.onEvent(AppEvent.Paired(testPairing))
            testScheduler.advanceUntilIdle()

            assertEquals(testPairing, vm.state.value.pairing)
            val message = vm.state.value.snackbarMessage
            assertTrue("a pairing that was not saved must say so, got: $message", message != null)
            assertTrue("the message must be about saving, got: $message", message!!.contains("could not be saved"))
        }

    @Test
    fun `a reconnect whose stored pairing cannot be read reports it instead of throwing`() =
        runTest {
            // init's load() is already guarded; this is the second touch, on a
            // user-initiated Retry, which was not.
            val store = FakePairingStore(testPairing)
            val vm = AppViewModel(pairingStore = store, clientFactory = { fake })
            store.failOnLoad = true

            vm.onEvent(AppEvent.Reconnect)
            testScheduler.advanceUntilIdle()

            val message = vm.state.value.snackbarMessage
            assertTrue("a failed read must be surfaced, got: $message", message != null)
            assertTrue("the message must point at re-pairing, got: $message", message!!.contains("pairing code"))
        }

    @Test
    fun `a store that fails on every touch does not crash construction or pairing`() =
        runTest {
            // Both guards together, in the order the broken-keystore device
            // actually hits them: construction, then a scan.
            val store = FakePairingStore().apply {
                failOnLoad = true
                failOnSave = true
            }
            val vm = AppViewModel(pairingStore = store, clientFactory = { fake })

            vm.onEvent(AppEvent.Paired(testPairing))
            vm.onEvent(AppEvent.Reconnect)
            testScheduler.advanceUntilIdle()

            assertEquals(testPairing, vm.state.value.pairing)
        }

    @Test
    fun `dismissing an older snackbar message does not clear a newer one`() =
        runTest {
            viewModel.onEvent(AppEvent.Paired(testPairing))
            fake.emit(ConnectionState.Failed("first error"))
            testScheduler.advanceUntilIdle()
            assertEquals("first error", viewModel.state.value.snackbarMessage)

            fake.emit(ConnectionState.Failed("second error"))
            testScheduler.advanceUntilIdle()
            assertEquals("second error", viewModel.state.value.snackbarMessage)

            // Dismissing the FIRST (stale) message must not clear the second.
            viewModel.onEvent(AppEvent.DismissSnackbar(shown = "first error"))
            assertEquals("second error", viewModel.state.value.snackbarMessage)

            viewModel.onEvent(AppEvent.DismissSnackbar(shown = "second error"))
            assertEquals(null, viewModel.state.value.snackbarMessage)
        }

    @Test
    fun `running an app that the server rejects surfaces the server's error message`() =
        runTest {
            viewModel.onEvent(AppEvent.Paired(testPairing))
            fake.responseFor = { Response(1, ResponseOutcome.Ok(ResponseResult.Apps(emptyList()))) }
            fake.emit(ConnectionState.Connected)
            testScheduler.advanceUntilIdle()

            fake.responseFor = { command ->
                when (command) {
                    is RequestCommand.Run ->
                        Response(
                            3,
                            ResponseOutcome.Error(ApiError(ErrorCode.NOT_FOUND, "no such app")),
                        )
                    else -> error("unexpected command: $command")
                }
            }
            viewModel.onEvent(AppEvent.RunApp("does-not-exist.desktop"))
            testScheduler.advanceUntilIdle()

            assertEquals("no such app", viewModel.state.value.snackbarMessage)
        }

    /** Connects and drains the post-Connect refresh, leaving the drawer showing. */
    private fun TestScope.connectAndSettle() {
        viewModel.onEvent(AppEvent.Paired(testPairing))
        fake.responseFor = { Response(1, ResponseOutcome.Ok(ResponseResult.Sessions(emptyList()))) }
        fake.emit(ConnectionState.Connected)
        testScheduler.advanceUntilIdle()
    }

    private fun attachSucceeds() {
        fake.responseFor = { command ->
            when (command) {
                is RequestCommand.Attach ->
                    Response(
                        4,
                        ResponseOutcome.Ok(
                            ResponseResult.AttachResult(AttachInfo("work", "/run/user/1000/x.sock")),
                        ),
                    )
                else -> Response(5, ResponseOutcome.Ok(ResponseResult.Ack))
            }
        }
    }

    @Test
    fun `attaching a running session navigates to it`() =
        runTest {
            connectAndSettle()
            attachSucceeds()

            viewModel.onEvent(AppEvent.AttachSession("work"))
            testScheduler.advanceUntilIdle()

            assertEquals("work", viewModel.state.value.activeSession)
            // Navigation is the confirmation; there is no snackbar for success.
            assertEquals(null, viewModel.state.value.snackbarMessage)
        }

    @Test
    fun `an attach the server rejects stays on the drawer and shows why`() =
        runTest {
            connectAndSettle()
            fake.responseFor = { command ->
                when (command) {
                    is RequestCommand.Attach ->
                        Response(4, ResponseOutcome.Error(ApiError(ErrorCode.NOT_FOUND, "no such session")))
                    else -> error("unexpected command: $command")
                }
            }

            viewModel.onEvent(AppEvent.AttachSession("ghost"))
            testScheduler.advanceUntilIdle()

            assertEquals(null, viewModel.state.value.activeSession)
            assertEquals("no such session", viewModel.state.value.snackbarMessage)
        }

    @Test
    fun `leaving a session clears it and detaches rather than killing it`() =
        runTest {
            connectAndSettle()
            attachSucceeds()
            viewModel.onEvent(AppEvent.AttachSession("work"))
            testScheduler.advanceUntilIdle()
            fake.calls.clear()

            viewModel.onEvent(AppEvent.LeaveSession)
            // The state flip is synchronous: leaving must not wait on the call.
            assertEquals(null, viewModel.state.value.activeSession)
            testScheduler.advanceUntilIdle()

            assertTrue(
                "expected a Detach, got ${fake.calls}",
                fake.calls.contains(RequestCommand.Detach("work")),
            )
            assertTrue(
                "Detach must never escalate to Kill",
                fake.calls.none { it is RequestCommand.Kill },
            )
        }

    @Test
    fun `a failing detach still leaves the session screen`() =
        runTest {
            connectAndSettle()
            attachSucceeds()
            viewModel.onEvent(AppEvent.AttachSession("work"))
            testScheduler.advanceUntilIdle()

            fake.responseFor = { error("navetted went away mid-detach") }
            viewModel.onEvent(AppEvent.LeaveSession)
            testScheduler.advanceUntilIdle()

            assertEquals(
                "a failed detach must not strand the user on a dead session screen",
                null,
                viewModel.state.value.activeSession,
            )
        }

    @Test
    fun `leaving when no session is active is a no-op`() =
        runTest {
            connectAndSettle()
            fake.calls.clear()

            viewModel.onEvent(AppEvent.LeaveSession)
            testScheduler.advanceUntilIdle()

            assertEquals(null, viewModel.state.value.activeSession)
            assertTrue("nothing should have been sent", fake.calls.isEmpty())
        }

    @Test
    fun `reconnecting leaves any active session behind`() =
        runTest {
            connectAndSettle()
            attachSucceeds()
            viewModel.onEvent(AppEvent.AttachSession("work"))
            testScheduler.advanceUntilIdle()
            assertEquals("work", viewModel.state.value.activeSession)

            viewModel.onEvent(AppEvent.Paired(testPairing))
            testScheduler.advanceUntilIdle()

            assertEquals(
                "a new connection must not keep the previous one's session screen up",
                null,
                viewModel.state.value.activeSession,
            )
        }
}
