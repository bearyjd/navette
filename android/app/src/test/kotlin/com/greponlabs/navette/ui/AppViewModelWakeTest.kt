package com.greponlabs.navette.ui

import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.Pairing
import com.greponlabs.navette.net.WakeResult
import com.greponlabs.navette.net.WakeTarget
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.test.setMain
import kotlinx.coroutines.launch
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

/**
 * Wake-on-LAN through [AppViewModel]: configuring a relay, sending through it,
 * and what happens to the verdict when the registry or connection moves on
 * underneath it. Fakes live in `AppViewModelFixtures.kt`.
 */
@OptIn(ExperimentalCoroutinesApi::class)
class AppViewModelWakeTest {
    private lateinit var fake: FakeNavetteApi

    @Before
    fun setUp() {
        Dispatchers.setMain(StandardTestDispatcher())
        fake = FakeNavetteApi()
    }

    @After
    fun tearDown() {
        Dispatchers.resetMain()
    }

    private val relayPairing = Pairing("nas", 9417, "nas-token")
    private val sleeperMac = "aa:bb:cc:dd:ee:ff"

    /**
     * Pairs the relay, then the sleeper (leaving the sleeper active), configures
     * the sleeper to wake via the relay, and fails the sleeper's connection --
     * the screen the Wake button lives on.
     */
    private fun TestScope.pairSleeperAndRelay(transport: FakeWakeTransport, first: List<Pairing> = emptyList()): AppViewModel {
        val vm = AppViewModel(pairingStore = FakePairingStore(), clientFactory = { fake }, wakeTransport = transport)
        first.forEach { vm.onEvent(AppEvent.Paired(it)) }
        vm.onEvent(AppEvent.Paired(relayPairing))
        vm.onEvent(AppEvent.Paired(testPairing))
        val relayId = vm.state.value.registry.hosts.first { it.pairing == relayPairing }.id
        val sleeperId = vm.state.value.registry.hosts.first { it.pairing == testPairing }.id
        vm.onEvent(AppEvent.SetWake(sleeperId, WakeTarget(sleeperMac, relayId)))
        fake.emit(ConnectionState.Failed("connection refused"))
        testScheduler.advanceUntilIdle()
        return vm
    }

    @Test
    fun `setting a wake target exposes the relay for the failed host`() =
        runTest {
            val vm = pairSleeperAndRelay(FakeWakeTransport())
            val route = vm.state.value.wakeRoute
            assertEquals(sleeperMac, route?.mac)
            assertEquals(relayPairing, route?.via?.pairing)
            assertEquals("nas:9417", route?.via?.endpointLabel)
            assertEquals(WakeUiState.Idle, vm.state.value.wake)
        }

    @Test
    fun `waking sends the sleeper's mac through the relay and reports Sent with the relay's label`() =
        runTest {
            val transport = FakeWakeTransport(WakeResult.Sent)
            val vm = pairSleeperAndRelay(transport)

            vm.onEvent(AppEvent.WakeActive)
            assertEquals("the button must go quiet immediately", WakeUiState.Sending, vm.state.value.wake)
            testScheduler.advanceUntilIdle()

            assertEquals(listOf(relayPairing to sleeperMac), transport.wakes)
            assertEquals(WakeUiState.Sent("nas:9417"), vm.state.value.wake)
        }

    @Test
    fun `a relay that rejects the token says so`() =
        runTest {
            val vm = pairSleeperAndRelay(FakeWakeTransport(WakeResult.Rejected(401)))

            vm.onEvent(AppEvent.WakeActive)
            testScheduler.advanceUntilIdle()

            val wake = vm.state.value.wake
            assertTrue("expected Failed, got $wake", wake is WakeUiState.Failed)
            val message = (wake as WakeUiState.Failed).message
            assertTrue("must name the relay, got: $message", message.contains("nas:9417"))
            assertTrue("must be about the token, got: $message", message.contains("token"))
        }

    @Test
    fun `a relay that could not send reports the status`() =
        runTest {
            val vm = pairSleeperAndRelay(FakeWakeTransport(WakeResult.Rejected(503)))

            vm.onEvent(AppEvent.WakeActive)
            testScheduler.advanceUntilIdle()

            assertEquals(WakeUiState.Failed("nas:9417 could not send (HTTP 503)"), vm.state.value.wake)
        }

    @Test
    fun `an unreachable relay is reported as such`() =
        runTest {
            val vm = pairSleeperAndRelay(FakeWakeTransport(WakeResult.Unreachable))

            vm.onEvent(AppEvent.WakeActive)
            testScheduler.advanceUntilIdle()

            assertEquals(WakeUiState.Failed("Could not reach nas:9417"), vm.state.value.wake)
        }

    @Test
    fun `waking with no wake target configured is a no-op`() =
        runTest {
            val transport = FakeWakeTransport()
            val vm = AppViewModel(pairingStore = FakePairingStore(), clientFactory = { fake }, wakeTransport = transport)
            vm.onEvent(AppEvent.Paired(testPairing))
            fake.emit(ConnectionState.Failed("connection refused"))
            testScheduler.advanceUntilIdle()
            assertEquals(null, vm.state.value.wakeRoute)

            vm.onEvent(AppEvent.WakeActive)
            testScheduler.advanceUntilIdle()

            assertTrue("nothing must be sent", transport.wakes.isEmpty())
            assertEquals(WakeUiState.Idle, vm.state.value.wake)
        }

    @Test
    fun `a second tap while a wake is in flight is dropped`() =
        runTest {
            val transport = FakeWakeTransport()
            val vm = pairSleeperAndRelay(transport)

            vm.onEvent(AppEvent.WakeActive)
            vm.onEvent(AppEvent.WakeActive)
            testScheduler.advanceUntilIdle()

            assertEquals("one packet, not two", 1, transport.wakes.size)
        }

    @Test
    fun `retrying the connection resets the wake verdict`() =
        runTest {
            val vm = pairSleeperAndRelay(FakeWakeTransport(WakeResult.Sent))
            vm.onEvent(AppEvent.WakeActive)
            testScheduler.advanceUntilIdle()
            assertEquals(WakeUiState.Sent("nas:9417"), vm.state.value.wake)

            vm.onEvent(AppEvent.Reconnect)
            testScheduler.advanceUntilIdle()

            // A "sent" from before this attempt is not advice about this one.
            assertEquals(WakeUiState.Idle, vm.state.value.wake)
        }

    @Test
    fun `clearing the wake target removes the relay from the failed host`() =
        runTest {
            val vm = pairSleeperAndRelay(FakeWakeTransport())
            val sleeperId = vm.state.value.registry.hosts.first { it.pairing == testPairing }.id

            vm.onEvent(AppEvent.SetWake(sleeperId, null))

            assertEquals(null, vm.state.value.wakeRoute)
            assertEquals(null, vm.state.value.registry.hosts.first { it.id == sleeperId }.wake)
        }

    @Test
    fun `deleting the relay clears the wake targets that used it, and any verdict it gave`() =
        runTest {
            val vm = pairSleeperAndRelay(FakeWakeTransport(WakeResult.Sent))
            val relayId = vm.state.value.registry.hosts.first { it.pairing == relayPairing }.id
            vm.onEvent(AppEvent.WakeActive)
            testScheduler.advanceUntilIdle()
            assertEquals(WakeUiState.Sent("nas:9417"), vm.state.value.wake)

            vm.onEvent(AppEvent.DeleteHost(relayId))

            assertEquals(null, vm.state.value.wakeRoute)
            assertEquals(1, vm.state.value.registry.hosts.size)
            assertEquals(null, vm.state.value.registry.hosts.single().wake)
            // The path this closes: fail, wake, open Saved computers, delete the
            // relay, Back -- the failed screen must not still say "sent via nas".
            assertEquals(WakeUiState.Idle, vm.state.value.wake)
            assertEquals("the sleeper stays connected-to, and its failure stays on screen", testPairing, vm.state.value.pairing)
        }

    @Test
    fun `a wake target that cannot be saved says so and leaves the registry as stored`() =
        runTest {
            val store = FakePairingStore()
            val vm = AppViewModel(pairingStore = store, clientFactory = { fake }, wakeTransport = FakeWakeTransport())
            vm.onEvent(AppEvent.Paired(relayPairing))
            vm.onEvent(AppEvent.Paired(testPairing))
            val relayId = vm.state.value.registry.hosts.first { it.pairing == relayPairing }.id
            val sleeperId = vm.state.value.registry.hosts.first { it.pairing == testPairing }.id
            store.failOnSave = true

            vm.onEvent(AppEvent.SetWake(sleeperId, WakeTarget(sleeperMac, relayId)))
            testScheduler.advanceUntilIdle()

            assertEquals(null, vm.state.value.registry.hosts.first { it.id == sleeperId }.wake)
            val message = vm.state.value.snackbarMessage
            assertTrue("a setting that did not stick must say so, got: $message", message != null && message.contains("not saved"))
        }

    @Test
    fun `after a failed save the active host's wake target is not offered for the unsaved pairing`() =
        runTest {
            // registry.active is the OLD host here; the pairing in use was never
            // stored. Offering the old host's relay would wake the wrong machine.
            val store = FakePairingStore()
            val vm = AppViewModel(pairingStore = store, clientFactory = { fake }, wakeTransport = FakeWakeTransport())
            vm.onEvent(AppEvent.Paired(relayPairing))
            vm.onEvent(AppEvent.Paired(Pairing("old-tower", 9417, "old-token")))
            val relayId = vm.state.value.registry.hosts.first { it.pairing == relayPairing }.id
            val oldId = vm.state.value.registry.hosts.first { it.pairing.host == "old-tower" }.id
            vm.onEvent(AppEvent.SetWake(oldId, WakeTarget(sleeperMac, relayId)))
            store.failOnSave = true

            vm.onEvent(AppEvent.Paired(testPairing))
            fake.emit(ConnectionState.Failed("connection refused"))
            testScheduler.advanceUntilIdle()

            assertEquals(testPairing, vm.state.value.pairing)
            assertEquals(oldId, vm.state.value.registry.activeId)
            assertEquals(null, vm.state.value.wakeRoute)
        }

    @Test
    fun `a wake still in flight when the user switches host never publishes its verdict`() =
        runTest {
            // The race: the relay is slow, the user gives up and picks another
            // computer, and then the relay's 204 arrives. That verdict was for
            // the previous host; landing it here would hide this host's Wake
            // button behind a "sent via nas" that is about something else.
            val transport = FakeWakeTransport().apply { gate = CompletableDeferred() }
            val vm = pairSleeperAndRelay(transport)
            val seen = mutableListOf<WakeUiState>()
            backgroundScope.launch { vm.state.collect { seen.add(it.wake) } }

            vm.onEvent(AppEvent.WakeActive)
            testScheduler.advanceUntilIdle()
            assertEquals(WakeUiState.Sending, vm.state.value.wake)
            assertEquals(1, transport.wakes.size)

            val relayId = vm.state.value.registry.hosts.first { it.pairing == relayPairing }.id
            vm.onEvent(AppEvent.SelectHost(relayId))
            testScheduler.advanceUntilIdle()
            assertEquals(WakeUiState.Idle, vm.state.value.wake)

            transport.gate?.complete(WakeResult.Sent)
            testScheduler.advanceUntilIdle()

            assertEquals(WakeUiState.Idle, vm.state.value.wake)
            assertTrue("no Sent may ever have been published, saw $seen", seen.none { it is WakeUiState.Sent })
        }

    @Test
    fun `deleting an unrelated host leaves a wake verdict alone`() =
        runTest {
            val other = Pairing("other", 9417, "other-token")
            val vm = pairSleeperAndRelay(FakeWakeTransport(WakeResult.Sent), first = listOf(other))
            vm.onEvent(AppEvent.WakeActive)
            testScheduler.advanceUntilIdle()
            assertEquals(WakeUiState.Sent("nas:9417"), vm.state.value.wake)

            val otherId = vm.state.value.registry.hosts.first { it.pairing == other }.id
            vm.onEvent(AppEvent.DeleteHost(otherId))

            assertEquals(2, vm.state.value.registry.hosts.size)
            assertEquals("a host that is neither the sleeper nor its relay has no bearing on the verdict", WakeUiState.Sent("nas:9417"), vm.state.value.wake)
        }

    @Test
    fun `deleting an unrelated host does not interrupt a wake in flight`() =
        runTest {
            val other = Pairing("other", 9417, "other-token")
            val transport = FakeWakeTransport().apply { gate = CompletableDeferred() }
            val vm = pairSleeperAndRelay(transport, first = listOf(other))
            vm.onEvent(AppEvent.WakeActive)
            testScheduler.advanceUntilIdle()

            val otherId = vm.state.value.registry.hosts.first { it.pairing == other }.id
            vm.onEvent(AppEvent.DeleteHost(otherId))
            assertEquals("the button must stay disabled while the relay is still answering", WakeUiState.Sending, vm.state.value.wake)

            transport.gate?.complete(WakeResult.Sent)
            testScheduler.advanceUntilIdle()
            assertEquals(WakeUiState.Sent("nas:9417"), vm.state.value.wake)
        }
}
