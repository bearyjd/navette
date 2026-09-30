package com.greponlabs.navette.ui

import com.greponlabs.navette.net.Pairing
import com.greponlabs.navette.net.ViewScale
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.test.setMain
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Before
import org.junit.Test

/**
 * The per-host logical scale through [AppViewModel]: remembering it, clearing
 * it, and what a failed save does (and does not do). Fakes live in
 * `AppViewModelFixtures.kt`.
 */
@OptIn(ExperimentalCoroutinesApi::class)
class AppViewModelViewScaleTest {
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

    private val otherPairing = Pairing("nas", 9417, "nas-token")

    /** Pairs [otherPairing] then [testPairing] (leaving the latter active) and returns the ViewModel. */
    private fun pairTwo(store: FakePairingStore = FakePairingStore()): AppViewModel {
        val vm = AppViewModel(pairingStore = store, clientFactory = { fake }, wakeTransport = FakeWakeTransport())
        vm.onEvent(AppEvent.Paired(otherPairing))
        vm.onEvent(AppEvent.Paired(testPairing))
        return vm
    }

    private fun AppViewModel.idOf(pairing: Pairing): String = state.value.registry.hosts.first { it.pairing == pairing }.id

    @Test
    fun `setting a view scale persists it on the named host only`() =
        runTest {
            val vm = pairTwo()
            val towerId = vm.idOf(testPairing)

            vm.onEvent(AppEvent.SetViewScale(towerId, ViewScale.X3))
            testScheduler.advanceUntilIdle()

            assertEquals(ViewScale.X3, vm.state.value.registry.hosts.first { it.id == towerId }.viewScale)
            assertEquals(null, vm.state.value.registry.hosts.first { it.pairing == otherPairing }.viewScale)
            assertEquals("the session screen reads the scale from here", ViewScale.X3, vm.state.value.savedForPairing?.viewScale)
        }

    @Test
    fun `a null scale clears back to the device default`() =
        runTest {
            val vm = pairTwo()
            val towerId = vm.idOf(testPairing)
            vm.onEvent(AppEvent.SetViewScale(towerId, ViewScale.X2))

            vm.onEvent(AppEvent.SetViewScale(towerId, null))
            testScheduler.advanceUntilIdle()

            assertEquals(null, vm.state.value.registry.hosts.first { it.id == towerId }.viewScale)
        }

    @Test
    fun `a scale that cannot be saved is logged, not surfaced, and leaves the registry as stored`() =
        runTest {
            // Unlike a wake target, the scale has already taken effect for this
            // session, and the session screen has no snackbar host anyway.
            val store = FakePairingStore()
            val vm = pairTwo(store)
            val towerId = vm.idOf(testPairing)
            val before = vm.state.value.registry
            store.failOnSave = true

            vm.onEvent(AppEvent.SetViewScale(towerId, ViewScale.X3))
            testScheduler.advanceUntilIdle()

            assertEquals(before, vm.state.value.registry)
            assertEquals(null, vm.state.value.registry.hosts.first { it.id == towerId }.viewScale)
            assertEquals("a failed scale save must not snackbar", null, vm.state.value.snackbarMessage)
            assertEquals("the session stays attached-to", testPairing, vm.state.value.pairing)
        }

    @Test
    fun `an unknown host id is refused by the store and changes nothing`() =
        runTest {
            val vm = pairTwo()
            val before = vm.state.value.registry

            vm.onEvent(AppEvent.SetViewScale("ghost", ViewScale.X2))
            testScheduler.advanceUntilIdle()

            assertEquals(before, vm.state.value.registry)
            assertEquals(null, vm.state.value.snackbarMessage)
        }

    @Test
    fun `savedForPairing follows the pairing in use, not the registry's active id`() =
        runTest {
            // After a failed save the pairing in use was never stored: there is
            // no entry to read a scale from or to address a SetViewScale to.
            val store = FakePairingStore()
            val vm = pairTwo(store)
            assertEquals(vm.idOf(testPairing), vm.state.value.savedForPairing?.id)
            store.failOnSave = true

            vm.onEvent(AppEvent.Paired(Pairing("unsaved", 9417, "unsaved-token")))
            testScheduler.advanceUntilIdle()

            assertEquals("unsaved", vm.state.value.pairing?.host)
            assertEquals(vm.idOf(testPairing), vm.state.value.registry.activeId)
            assertEquals(null, vm.state.value.savedForPairing)
        }

    @Test
    fun `re-pairing a host keeps the scale it was given`() =
        runTest {
            val vm = pairTwo()
            val towerId = vm.idOf(testPairing)
            vm.onEvent(AppEvent.SetViewScale(towerId, ViewScale.X1_5))

            vm.onEvent(AppEvent.Paired(testPairing.copy(token = "rotated-token")))
            testScheduler.advanceUntilIdle()

            val tower = vm.state.value.registry.hosts.first { it.id == towerId }
            assertEquals("rotated-token", tower.pairing.token)
            assertEquals(ViewScale.X1_5, tower.viewScale)
        }
}
