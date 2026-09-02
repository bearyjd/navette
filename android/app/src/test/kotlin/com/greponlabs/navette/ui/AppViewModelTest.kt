package com.greponlabs.navette.ui

import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.NavetteApi
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
        viewModel = AppViewModel(clientFactory = { fake })
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

            viewModel.onEvent(AppEvent.HostChanged("tower"))
            viewModel.onEvent(AppEvent.Connect)
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
            // clientFactory is invoked once per connect() call, on the SAME
            // ViewModel instance -- this is the actual regression shape
            // (a user retrying Connect), not two independent ViewModels.
            val firstClient = FakeNavetteApi()
            val secondClient = FakeNavetteApi()
            val clients = ArrayDeque(listOf(firstClient, secondClient))
            val vm = AppViewModel(clientFactory = { clients.removeFirst() })

            vm.onEvent(AppEvent.HostChanged("tower"))
            vm.onEvent(AppEvent.Connect)
            firstClient.emit(ConnectionState.Connected)
            testScheduler.advanceUntilIdle()
            assertEquals(ConnectionState.Connected, vm.state.value.connection)

            vm.onEvent(AppEvent.Connect)
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
    fun `dismissing an older snackbar message does not clear a newer one`() =
        runTest {
            viewModel.onEvent(AppEvent.HostChanged("tower"))
            viewModel.onEvent(AppEvent.Connect)
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
            viewModel.onEvent(AppEvent.HostChanged("tower"))
            viewModel.onEvent(AppEvent.Connect)
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

    @Test
    fun `attaching a running session confirms success without a session screen yet`() =
        runTest {
            viewModel.onEvent(AppEvent.HostChanged("tower"))
            viewModel.onEvent(AppEvent.Connect)
            fake.responseFor = { Response(1, ResponseOutcome.Ok(ResponseResult.Sessions(emptyList()))) }
            fake.emit(ConnectionState.Connected)
            testScheduler.advanceUntilIdle()

            fake.responseFor = { command ->
                when (command) {
                    is RequestCommand.Attach ->
                        Response(
                            4,
                            ResponseOutcome.Ok(
                                ResponseResult.AttachResult(AttachInfo("work", "/run/user/1000/x.sock")),
                            ),
                        )
                    else -> error("unexpected command: $command")
                }
            }
            viewModel.onEvent(AppEvent.AttachSession("work"))
            testScheduler.advanceUntilIdle()

            assertTrue(
                viewModel.state.value.snackbarMessage.orEmpty().contains("work"),
            )
        }
}
