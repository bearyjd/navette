package com.greponlabs.navette.ui.session

import java.io.ByteArrayInputStream
import java.io.InputStream
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.withContext
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer

private class TestFileSource(
    override val name: String = "report.pdf",
    override val mime: String = "application/pdf",
    override val size: Long = 3,
) : FileTransferSource {
    var opens = 0
    override fun open() = ByteArrayInputStream(ByteArray(size.toInt())).also { opens += 1 }
}

private class FakeFileTransport : FileTransferTransport {
    val preflight = CompletableDeferred<FilePreflightResponse?>()
    var uploadResult: FileTransferStatus? = null
    var statusResult: FileTransferStatus? = null
    val cancelled = mutableListOf<String>()
    val progress = mutableListOf<Long>()
    var uploadCalls = 0

    override suspend fun preflight(source: FileTransferSource): FilePreflightResponse? = preflight.await()

    override suspend fun upload(
        preflight: FilePreflightResponse,
        source: FileTransferSource,
        onProgress: (Long) -> Unit,
    ): FileTransferStatus? {
        uploadCalls += 1
        onProgress(source.size)
        progress += source.size
        return uploadResult
    }

    override suspend fun status(transferId: String): FileTransferStatus? = statusResult

    override suspend fun cancel(transferId: String): Boolean {
        cancelled += transferId
        return true
    }
}

private fun preflight(id: String = "0123456789abcdef0123456789abcdef") =
    FilePreflightResponse(
        transfer_id = id,
        upload_url = "/v1/sessions/work/files/$id/content",
        expires_at = 1,
    )

private fun status(state: FileTransferServerState) =
    FileTransferStatus(
        transfer_id = "0123456789abcdef0123456789abcdef",
        name = "report.pdf",
        mime = "application/pdf",
        size = 3,
        bytes_received = 3,
        state = state,
    )

@OptIn(ExperimentalCoroutinesApi::class)
class FileTransferCoordinatorTest {
    @Test
    fun replacementAbortsAndCleansOnlyTheSupersededNonCancellableUpload() = runTest {
        val firstId = "11111111111111111111111111111111"
        val secondId = "22222222222222222222222222222222"
        val firstStarted = CompletableDeferred<Unit>()
        val releaseFirst = CompletableDeferred<Unit>()
        val transport =
            object : FileTransferTransport {
                val cancelled = mutableListOf<String>()
                var activeAbortCalls = 0

                override suspend fun preflight(source: FileTransferSource): FilePreflightResponse =
                    preflight(if (source.name == "first" ) firstId else secondId)

                override suspend fun upload(
                    preflight: FilePreflightResponse,
                    source: FileTransferSource,
                    onProgress: (Long) -> Unit,
                ): FileTransferStatus {
                    if (source.name == "first") {
                        firstStarted.complete(Unit)
                        withContext(NonCancellable) { releaseFirst.await() }
                    }
                    return status(FileTransferServerState.Delivered).copy(transfer_id = preflight.transfer_id)
                }

                override suspend fun status(transferId: String): FileTransferStatus? = null

                override suspend fun cancel(transferId: String): Boolean {
                    cancelled += transferId
                    return true
                }

                override fun cancelActiveUpload() {
                    activeAbortCalls += 1
                }
            }
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        coordinator.upload(TestFileSource(name = "first"))
        runCurrent()
        assertTrue(firstStarted.isCompleted)

        coordinator.upload(TestFileSource(name = "second"))
        runCurrent()
        assertEquals(1, transport.activeAbortCalls)
        assertEquals(listOf(firstId), transport.cancelled)

        releaseFirst.complete(Unit)
        runCurrent()
        assertEquals("a stale completion cannot cancel the replacement", listOf(firstId), transport.cancelled)
        assertEquals(FileTransferUiState.Delivered("second", 3), coordinator.state.value)
    }

    @Test
    fun closeAbortsAndCleansANonCancellableActiveUpload() = runTest {
        val id = "33333333333333333333333333333333"
        val started = CompletableDeferred<Unit>()
        val release = CompletableDeferred<Unit>()
        val transport =
            object : FileTransferTransport {
                val cancelled = mutableListOf<String>()
                var activeAbortCalls = 0

                override suspend fun preflight(source: FileTransferSource): FilePreflightResponse = preflight(id)

                override suspend fun upload(
                    preflight: FilePreflightResponse,
                    source: FileTransferSource,
                    onProgress: (Long) -> Unit,
                ): FileTransferStatus {
                    started.complete(Unit)
                    withContext(NonCancellable) { release.await() }
                    return status(FileTransferServerState.Delivered)
                }

                override suspend fun status(transferId: String): FileTransferStatus? = null

                override suspend fun cancel(transferId: String): Boolean {
                    cancelled += transferId
                    return true
                }

                override fun cancelActiveUpload() {
                    activeAbortCalls += 1
                }
            }
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        coordinator.upload(TestFileSource())
        runCurrent()
        assertTrue(started.isCompleted)

        coordinator.close()
        runCurrent()
        assertEquals(1, transport.activeAbortCalls)
        assertEquals(listOf(id), transport.cancelled)

        release.complete(Unit)
        runCurrent()
        assertEquals("late work must not issue a second deletion", listOf(id), transport.cancelled)
    }

    @Test
    fun httpTransportStreamsAtMost64KiBPerProviderRead() = runTest {
        val server = MockWebServer()
        server.start()
        try {
            val source =
                object : FileTransferSource {
                    override val name = "large.bin"
                    override val mime = "application/octet-stream"
                    override val size = (FILE_TRANSFER_BUFFER_BYTES * 2L) + 1
                    var maxRequested = 0

                    override fun open(): InputStream =
                        object : InputStream() {
                            var remaining = size

                            override fun read(): Int =
                                if (remaining-- > 0) 0 else -1

                            override fun read(buffer: ByteArray, offset: Int, length: Int): Int {
                                maxRequested = maxOf(maxRequested, length)
                                if (remaining == 0L) return -1
                                val count = minOf(length.toLong(), remaining).toInt()
                                remaining -= count
                                return count
                            }
                        }
                }
            server.enqueue(
                MockResponse().setResponseCode(202).setBody(
                    """{"transfer_id":"0123456789abcdef0123456789abcdef","name":"large.bin","mime":"application/octet-stream","size":131073,"bytes_received":131073,"state":"queued"}""",
                ),
            )
            val mediaUrl = server.url("/v1/sessions/work/media").toString().replaceFirst("http://", "ws://")
            val transport = HttpFileTransferTransport(mediaUrl, "token")
            val result = transport.upload(preflight(), source) {}

            assertEquals(FileTransferServerState.Queued, result?.state)
            assertEquals(FILE_TRANSFER_BUFFER_BYTES, source.maxRequested)
            assertEquals(source.size, server.takeRequest().bodySize)
        } finally {
            server.shutdown()
        }
    }

    @Test
    fun uploadsWithProgressThenPublishesDeliveredState() = runTest {
        val transport = FakeFileTransport().apply { uploadResult = status(FileTransferServerState.Delivered) }
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        val source = TestFileSource()

        coordinator.upload(source)
        runCurrent()
        assertEquals(FileTransferUiState.Preparing(source.name, source.size), coordinator.state.value)
        transport.preflight.complete(preflight())
        runCurrent()

        assertEquals(listOf(3L), transport.progress)
        assertEquals(FileTransferUiState.Delivered(source.name, source.size), coordinator.state.value)
        assertEquals("the coordinator owns no file-byte buffering or reads", 0, source.opens)
    }

    @Test
    fun queuedUploadPollsDaemonUntilGuestDeliveryCompletes() = runTest {
        val transport =
            FakeFileTransport().apply {
                uploadResult = status(FileTransferServerState.Queued)
                statusResult = status(FileTransferServerState.Delivered)
            }
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        coordinator.upload(TestFileSource())
        runCurrent()
        transport.preflight.complete(preflight())
        runCurrent()

        assertTrue(coordinator.state.value is FileTransferUiState.WaitingForGuest)
        testScheduler.advanceTimeBy(250)
        runCurrent()
        assertTrue(coordinator.state.value is FileTransferUiState.Delivered)
    }

    @Test
    fun cancellationStopsInFlightWorkAndCancelsKnownServerTransfer() = runTest {
        val uploadStarted = CompletableDeferred<Unit>()
        val uploadCancelled = CompletableDeferred<Unit>()
        val transport =
            object : FileTransferTransport {
                override suspend fun preflight(source: FileTransferSource): FilePreflightResponse = preflight()

                override suspend fun upload(
                    preflight: FilePreflightResponse,
                    source: FileTransferSource,
                    onProgress: (Long) -> Unit,
                ): FileTransferStatus? {
                    uploadStarted.complete(Unit)
                    try {
                        return CompletableDeferred<FileTransferStatus?>().await()
                    } catch (cancelled: CancellationException) {
                        uploadCancelled.complete(Unit)
                        throw cancelled
                    }
                }

                override suspend fun status(transferId: String): FileTransferStatus? = null

                override suspend fun cancel(transferId: String): Boolean {
                    cancelled += transferId
                    return true
                }

                val cancelled = mutableListOf<String>()
            }
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        coordinator.upload(TestFileSource())
        runCurrent()
        runCurrent()
        assertTrue(uploadStarted.isCompleted)
        coordinator.cancel()
        runCurrent()

        assertEquals(listOf(preflight().transfer_id), transport.cancelled)
        assertTrue(coordinator.state.value is FileTransferUiState.Cancelled)
        assertTrue(uploadCancelled.isCompleted)
    }

    @Test
    fun retryKeepsOnlyReopenableSourceAndStartsFreshPreflight() = runTest {
        var attempts = 0
        val transport =
            object : FileTransferTransport {
                override suspend fun preflight(source: FileTransferSource): FilePreflightResponse? =
                    if (++attempts == 1) null else preflight()

                override suspend fun upload(
                    preflight: FilePreflightResponse,
                    source: FileTransferSource,
                    onProgress: (Long) -> Unit,
                ): FileTransferStatus {
                    onProgress(source.size)
                    return status(FileTransferServerState.Delivered)
                }

                override suspend fun status(transferId: String): FileTransferStatus? = null

                override suspend fun cancel(transferId: String): Boolean = true
            }
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        val source = TestFileSource()
        coordinator.upload(source)
        runCurrent()
        assertTrue(coordinator.state.value is FileTransferUiState.Failed)
        coordinator.retry()
        runCurrent()

        assertEquals(2, attempts)
        assertEquals(FileTransferUiState.Delivered(source.name, source.size), coordinator.state.value)
        // Source bytes are opened by the transport only, never by coordinator
        // state/retry bookkeeping.
        assertEquals(0, source.opens)
    }

    @Test
    fun metadataValidationRejectsUnsafeNamesMimesAndUnknownOrOversizeLengths() {
        assertTrue(isSafeFileTransferMetadata("report.pdf", "application/pdf", 1))
        assertFalse(isSafeFileTransferMetadata("../report.pdf", "application/pdf", 1))
        assertFalse(isSafeFileTransferMetadata("report.pdf", "text/plain; charset=utf-8", 1))
        assertFalse(isSafeFileTransferMetadata("report.pdf", "application/pdf", 0))
        assertFalse(isSafeFileTransferMetadata("report.pdf", "application/pdf", MAX_FILE_TRANSFER_BYTES + 1))
    }
}
