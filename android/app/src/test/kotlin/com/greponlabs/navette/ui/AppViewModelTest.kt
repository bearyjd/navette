package com.greponlabs.navette.ui

import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.Pairing
import com.greponlabs.navette.protocol.ApiError
import com.greponlabs.navette.protocol.AttachInfo
import com.greponlabs.navette.protocol.ErrorCode
import com.greponlabs.navette.protocol.RequestCommand
import com.greponlabs.navette.protocol.Response
import com.greponlabs.navette.protocol.ResponseOutcome
import com.greponlabs.navette.protocol.ResponseResult
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
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
            assertTrue("the message must be about saving, got: $message", message!!.contains("not saved"))
            // The message is about the next launch and nothing else. It must
            // not claim the user has to pair again (a prior pairing survives a
            // failed save, so the next launch resumes that), nor say anything
            // about this session -- Retry now prefers the active pairing, so
            // reconnecting here reaches the right host.
            assertTrue("must not promise re-pairing is required, got: $message", !message.contains("will have to"))
            assertTrue("must scope the warning to the next launch, got: $message", message.contains("next launch"))
        }

    @Test
    fun `a failed save leaves a prior pairing intact, so the next launch resumes the old host`() =
        runTest {
            // This is the fact the notice's wording rests on. A failed save is
            // not a cleared store: the previous pairing survives, and a fresh
            // ViewModel (the next launch) auto-resumes it -- to the OLD host.
            // So "you will have to pair again" would be false, and the honest
            // warning is that the device may not come back to THIS host.
            val previous = Pairing(host = "old-tower", port = 9417, token = "old-token")
            val store = FakePairingStore(previous).apply { failOnSave = true }
            val vm = AppViewModel(pairingStore = store, clientFactory = { fake })

            vm.onEvent(AppEvent.Paired(testPairing))
            testScheduler.advanceUntilIdle()
            assertEquals(testPairing, vm.state.value.pairing)

            // The next launch, same store.
            store.failOnSave = false
            val relaunched = AppViewModel(pairingStore = store, clientFactory = { FakeNavetteApi() })
            testScheduler.advanceUntilIdle()
            assertEquals(
                "the store must still hold the pairing the failed save did not replace",
                previous,
                relaunched.state.value.pairing,
            )
        }

    @Test
    fun `the unsaved-pairing notice survives the connection succeeding`() =
        runTest {
            // Whether the notice can actually be READ depends on what happens
            // to snackbarMessage next. Connected does not touch it, so on the
            // ordinary path the user sees it.
            val store = FakePairingStore().apply { failOnSave = true }
            val vm = AppViewModel(pairingStore = store, clientFactory = { fake })

            vm.onEvent(AppEvent.Paired(testPairing))
            fake.emit(ConnectionState.Connected)
            testScheduler.advanceUntilIdle()

            val message = vm.state.value.snackbarMessage
            assertTrue("connecting must not clear the notice, got: $message", message != null)
            assertTrue(message!!.contains("not saved"))
        }

    @Test
    fun `a connection failure replaces the unsaved-pairing notice`() =
        runTest {
            // Documents the one case where the notice is lost: connectWithPairing's
            // collector overwrites snackbarMessage with the failure reason. Not
            // treated as a defect -- a connection that failed outright is the more
            // urgent thing to show, and the pairing was not saved either way. Pinned
            // so that a future change to snackbar handling has to decide about it
            // deliberately rather than by accident.
            val store = FakePairingStore().apply { failOnSave = true }
            val vm = AppViewModel(pairingStore = store, clientFactory = { fake })

            vm.onEvent(AppEvent.Paired(testPairing))
            fake.emit(ConnectionState.Failed("connection refused"))
            testScheduler.advanceUntilIdle()

            assertEquals("connection refused", vm.state.value.snackbarMessage)
        }

    @Test
    fun `a reconnect whose stored pairing cannot be read reports it instead of throwing`() =
        runTest {
            // Reaches the storage path only because there is no active pairing:
            // an empty store means init connected to nothing, so Retry has
            // nothing in state to prefer. init's load() is already guarded;
            // this is the second touch, on a user-initiated Retry, which was not.
            val store = FakePairingStore()
            val vm = AppViewModel(pairingStore = store, clientFactory = { fake })
            store.failOnLoad = true

            vm.onEvent(AppEvent.Reconnect)
            testScheduler.advanceUntilIdle()

            val message = vm.state.value.snackbarMessage
            assertTrue("a failed read must be surfaced, got: $message", message != null)
            assertTrue("the message must point at re-pairing, got: $message", message!!.contains("pairing code"))
        }

    @Test
    fun `retry after a failed save reconnects to the new host, not the stored one`() =
        runTest {
            // The trap this closes: pair() deliberately carries on with the new
            // pairing when save fails, but Retry used to reload from storage --
            // so it silently reconnected to the OLD host, with no indication
            // that it had gone somewhere other than where the user just paired.
            val previous = Pairing(host = "old-tower", port = 9417, token = "old-token")
            val store = FakePairingStore(previous)
            val clients = mutableListOf<Pairing>()
            val vm =
                AppViewModel(
                    pairingStore = store,
                    clientFactory = { pairing ->
                        clients.add(pairing)
                        FakeNavetteApi()
                    },
                )
            store.failOnSave = true

            vm.onEvent(AppEvent.Paired(testPairing))
            vm.onEvent(AppEvent.Reconnect)
            testScheduler.advanceUntilIdle()

            assertEquals(testPairing, vm.state.value.pairing)
            assertEquals(
                "Retry must redial the pairing in use, not the stale stored one",
                testPairing,
                clients.last(),
            )
            assertTrue(
                "the stale pairing must never be dialled after the new one",
                clients.indexOf(previous) < clients.indexOf(testPairing),
            )
        }

    @Test
    fun `retry with an empty store and no active pairing does nothing but say so`() =
        runTest {
            // The other half of the old trap: with nothing on disk, Retry was
            // a no-op button. It still cannot connect -- there is genuinely
            // nothing to connect to -- but it must not look broken.
            val store = FakePairingStore()
            val vm = AppViewModel(pairingStore = store, clientFactory = { fake })

            vm.onEvent(AppEvent.Reconnect)
            testScheduler.advanceUntilIdle()

            assertEquals(null, vm.state.value.pairing)
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

    @Test
    fun `switching hosts closes the old client and clears the active session`() =
        runTest {
            val first = FakeNavetteApi()
            val second = FakeNavetteApi()
            val store = FakePairingStore()
            val vm = AppViewModel(pairingStore = store, clientFactory = { if (it.host == "one") first else second })
            val one = Pairing("one", 9417, "one-token")
            val two = Pairing("two", 9417, "two-token")
            vm.onEvent(AppEvent.Paired(one))
            vm.onEvent(AppEvent.Paired(two))
            val firstId = vm.state.value.registry.hosts.first { it.pairing == one }.id
            vm.onEvent(AppEvent.SelectHost(firstId))
            testScheduler.advanceUntilIdle()
            assertTrue(second.closed)
            assertEquals(one, vm.state.value.pairing)
            assertEquals(null, vm.state.value.activeSession)
        }

    @Test
    fun `deleting inactive host preserves active connection`() =
        runTest {
            val store = FakePairingStore()
            val vm = AppViewModel(pairingStore = store, clientFactory = { fake })
            val one = Pairing("one", 9417, "one-token")
            val two = Pairing("two", 9417, "two-token")
            vm.onEvent(AppEvent.Paired(one))
            vm.onEvent(AppEvent.Paired(two))
            val inactiveId = vm.state.value.registry.hosts.first { it.pairing == one }.id
            vm.onEvent(AppEvent.DeleteHost(inactiveId))
            assertEquals(two, vm.state.value.pairing)
            assertEquals(1, vm.state.value.registry.hosts.size)
            assertTrue(!vm.state.value.showingHosts)
        }

    @Test
    fun `deleting active host disconnects and returns to host list without fallback`() =
        runTest {
            val store = FakePairingStore()
            val vm = AppViewModel(pairingStore = store, clientFactory = { fake })
            vm.onEvent(AppEvent.Paired(Pairing("one", 9417, "one-token")))
            val id = vm.state.value.registry.activeId!!
            vm.onEvent(AppEvent.DeleteHost(id))
            assertEquals(null, vm.state.value.pairing)
            assertEquals(ConnectionState.Disconnected, vm.state.value.connection)
            assertTrue(vm.state.value.showingHosts)
            assertTrue(fake.closed)
        }

    @Test
    fun `saved hosts remain reachable after the active host fails`() =
        runTest {
            val store = FakePairingStore()
            val vm = AppViewModel(pairingStore = store, clientFactory = { fake })
            vm.onEvent(AppEvent.Paired(Pairing("offline", 9417, "offline-token")))
            fake.emit(ConnectionState.Failed("offline"))
            testScheduler.advanceUntilIdle()
            vm.onEvent(AppEvent.ShowHosts)
            assertTrue(vm.state.value.showingHosts)
            assertEquals(1, vm.state.value.registry.hosts.size)
        }
}
