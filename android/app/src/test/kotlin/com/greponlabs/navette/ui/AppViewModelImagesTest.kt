package com.greponlabs.navette.ui

import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.ImageFetch
import com.greponlabs.navette.net.ImageFetcher
import com.greponlabs.navette.net.Pairing
import com.greponlabs.navette.net.appIconPath
import com.greponlabs.navette.net.stubBitmap
import com.greponlabs.navette.protocol.ApiError
import com.greponlabs.navette.protocol.ErrorCode
import com.greponlabs.navette.protocol.RequestCommand
import com.greponlabs.navette.protocol.Response
import com.greponlabs.navette.protocol.ResponseOutcome
import com.greponlabs.navette.protocol.ResponseResult
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.test.setMain
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Before
import org.junit.Test

/**
 * The ViewModel's part of the drawer's thumbnails: the refresh tick they
 * revalidate on, and the cache they must not carry from one host to the next.
 * The images themselves are `ImageRepositoryTest`'s business.
 */
@OptIn(ExperimentalCoroutinesApi::class)
class AppViewModelImagesTest {
    private lateinit var fake: FakeNavetteApi
    private lateinit var fetcher: FakeImageFetcher

    /** Answers every request with one stub bitmap and counts them. */
    private class FakeImageFetcher : ImageFetcher {
        var fetches = 0

        override suspend fun fetch(pairing: Pairing, path: String, etag: String?): ImageFetch {
            fetches += 1
            return ImageFetch.Loaded(stubBitmap(), etag = null)
        }
    }

    @Before
    fun setUp() {
        Dispatchers.setMain(StandardTestDispatcher())
        fake = FakeNavetteApi()
        fetcher = FakeImageFetcher()
        fake.responseFor = { command ->
            when (command) {
                RequestCommand.ListApps -> Response(1, ResponseOutcome.Ok(ResponseResult.Apps(listOf(testApp))))
                RequestCommand.ListSessions -> Response(2, ResponseOutcome.Ok(ResponseResult.Sessions(listOf(testSession))))
                else -> error("unexpected command in this test: $command")
            }
        }
    }

    @After
    fun tearDown() {
        Dispatchers.resetMain()
    }

    private fun viewModel() = AppViewModel(pairingStore = FakePairingStore(), clientFactory = { fake }, imageFetcher = fetcher)

    @Test
    fun `the refresh tick advances once per completed refresh, not before`() =
        runTest {
            val vm = viewModel()
            assertEquals(0, vm.state.value.refreshTick)

            vm.onEvent(AppEvent.Paired(testPairing))
            fake.emit(ConnectionState.Connected)
            testScheduler.advanceUntilIdle()
            assertEquals("the post-connect refresh", 1, vm.state.value.refreshTick)

            vm.onEvent(AppEvent.Refresh)
            assertEquals("not until the session list has landed", 1, vm.state.value.refreshTick)
            testScheduler.advanceUntilIdle()
            assertEquals(2, vm.state.value.refreshTick)
        }

    @Test
    fun `a refresh that throws leaves the tick alone`() =
        runTest {
            val vm = viewModel()
            vm.onEvent(AppEvent.Paired(testPairing))
            fake.emit(ConnectionState.Connected)
            testScheduler.advanceUntilIdle()

            fake.responseFor = { throw IllegalStateException("not connected") }
            vm.onEvent(AppEvent.Refresh)
            testScheduler.advanceUntilIdle()

            assertEquals("no answer, so nothing to revalidate against", 1, vm.state.value.refreshTick)
        }

    @Test
    fun `a refresh answered with an error response still counts as completed`() =
        runTest {
            val vm = viewModel()
            vm.onEvent(AppEvent.Paired(testPairing))
            fake.emit(ConnectionState.Connected)
            testScheduler.advanceUntilIdle()

            fake.responseFor = { command ->
                when (command) {
                    RequestCommand.ListApps -> Response(1, ResponseOutcome.Ok(ResponseResult.Apps(listOf(testApp))))
                    RequestCommand.ListSessions -> Response(2, ResponseOutcome.Error(ApiError(ErrorCode.INTERNAL, "registry busy")))
                    else -> error("unexpected command in this test: $command")
                }
            }
            vm.onEvent(AppEvent.Refresh)
            testScheduler.advanceUntilIdle()

            assertEquals("the host answered, so the list beside the thumbnails is as fresh as it gets", 2, vm.state.value.refreshTick)
            assertEquals("registry busy", vm.state.value.snackbarMessage)
        }

    @Test
    fun `switching hosts forgets the previous host's images`() =
        runTest {
            val vm = viewModel()
            vm.onEvent(AppEvent.Paired(testPairing))
            fake.emit(ConnectionState.Connected)
            testScheduler.advanceUntilIdle()
            val path = appIconPath(testApp.id)
            assertNotNull(vm.imageRepository.load(testPairing, path, revalidate = false))
            assertNotNull(vm.imageRepository.cached(testPairing, path))
            assertEquals(1, fetcher.fetches)

            vm.onEvent(AppEvent.Paired(Pairing(host = "nas", port = 9417, token = "other-token")))

            assertNull("host A's icon must not be served on host B's drawer", vm.imageRepository.cached(testPairing, path))
        }
}
