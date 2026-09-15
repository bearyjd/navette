package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.BlobDescriptor
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import kotlin.concurrent.thread

private class FakeBlobTransport : BlobTransport {
    val uploads = mutableListOf<CompletableDeferred<BlobDescriptor?>>()
    val downloads = mutableListOf<CompletableDeferred<ByteArray?>>()

    override suspend fun upload(mime: String, bytes: ByteArray): BlobDescriptor? =
        CompletableDeferred<BlobDescriptor?>().also(uploads::add).await()

    override suspend fun download(blob: BlobDescriptor): ByteArray? =
        CompletableDeferred<ByteArray?>().also(downloads::add).await()
}

@OptIn(ExperimentalCoroutinesApi::class)
class ClipboardBlobCoordinatorTest {
    private val first =
        BlobDescriptor("0123456789abcdef0123456789abcdef", "image/png", 1)
    private val second =
        BlobDescriptor("11111111111111111111111111111111", "image/png", 1)

    @Test
    fun newerUploadSupersedesOlderCompletedUpload() = runTest {
        val transport = FakeBlobTransport()
        val announced = mutableListOf<BlobDescriptor>()
        val coordinator = ClipboardBlobCoordinator(backgroundScope, transport, announced::add)

        coordinator.upload("image/png", byteArrayOf(1))
        runCurrent()
        coordinator.upload("image/png", byteArrayOf(2))
        runCurrent()
        transport.uploads[0].complete(first)
        transport.uploads[1].complete(second)
        runCurrent()

        assertEquals(listOf(second), announced)
    }

    @Test
    fun reconnectInvalidationDiscardsInFlightDownloadWithoutReplay() = runTest {
        val transport = FakeBlobTransport()
        val received = mutableListOf<ByteArray>()
        val coordinator = ClipboardBlobCoordinator(backgroundScope, transport, {})

        coordinator.download(first) { bytes, _ -> received += bytes }
        runCurrent()
        coordinator.invalidate()
        transport.downloads.single().complete(byteArrayOf(1))
        runCurrent()

        assertEquals(emptyList<ByteArray>(), received)
    }

    @Test
    fun crossThreadInvalidationMakesAnAlreadyStartedUploadStale() = runTest {
        val transport = FakeBlobTransport()
        val announced = mutableListOf<BlobDescriptor>()
        val coordinator = ClipboardBlobCoordinator(backgroundScope, transport, announced::add)

        coordinator.upload("image/png", byteArrayOf(1))
        runCurrent()
        thread { coordinator.invalidate() }.join()
        transport.uploads.single().complete(first)
        runCurrent()

        assertEquals(emptyList<BlobDescriptor>(), announced)
    }

    @Test
    fun invalidationCannotPassAnAcceptedAnnouncementEffect() {
        val effectStarted = CountDownLatch(1)
        val releaseEffect = CountDownLatch(1)
        val invalidationFinished = CountDownLatch(1)
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val coordinator =
                ClipboardBlobCoordinator(
                    scope,
                    object : BlobTransport {
                        override suspend fun upload(mime: String, bytes: ByteArray): BlobDescriptor? = first

                        override suspend fun download(blob: BlobDescriptor): ByteArray? = null
                    },
                ) {
                    effectStarted.countDown()
                    releaseEffect.await()
                }
            coordinator.upload("image/png", byteArrayOf(1))
            assertTrue(effectStarted.await(1, TimeUnit.SECONDS))
            thread {
                coordinator.invalidate()
                invalidationFinished.countDown()
            }

            assertFalse(
                "invalidation must serialize behind an accepted announcement effect",
                invalidationFinished.await(100, TimeUnit.MILLISECONDS),
            )
            releaseEffect.countDown()
            assertTrue(invalidationFinished.await(1, TimeUnit.SECONDS))
        } finally {
            scope.cancel()
        }
    }
}
