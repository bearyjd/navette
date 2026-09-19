package com.greponlabs.navette.ui.session

import java.io.ByteArrayInputStream
import java.io.InputStream
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.withContext
import kotlinx.coroutines.yield
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

    override suspend fun cancel(transferId: String): FileTransferCancelResult {
        cancelled += transferId
        return FileTransferCancelResult.Cancelled
    }
}

/**
 * Answers status/cancel calls from a script, repeating the last entry. The
 * upload never reports progress itself; the callback is captured for tests
 * that replay a late progress report.
 */
private class ScriptedTransport(
    private val uploadResult: FileTransferStatus? = status(FileTransferServerState.Queued),
    private val statuses: List<FileTransferStatus?> = listOf(null),
    private val cancels: List<FileTransferCancelResult> = listOf(FileTransferCancelResult.Cancelled),
) : FileTransferTransport {
    var statusCalls = 0
    var cancelCalls = 0
    var onProgress: ((Long) -> Unit)? = null

    override suspend fun preflight(source: FileTransferSource): FilePreflightResponse = preflight()

    override suspend fun upload(
        preflight: FilePreflightResponse,
        source: FileTransferSource,
        onProgress: (Long) -> Unit,
    ): FileTransferStatus? {
        this.onProgress = onProgress
        return uploadResult
    }

    override suspend fun status(transferId: String): FileTransferStatus? =
        statuses[minOf(statusCalls++, statuses.lastIndex)]

    override suspend fun cancel(transferId: String): FileTransferCancelResult =
        cancels[minOf(cancelCalls++, cancels.lastIndex)]
}

private const val SETTLE_BUDGET_MS =
    2 * FileTransferCoordinator.MAX_STATUS_POLLS * FileTransferCoordinator.STATUS_POLL_DELAY_MS +
        2 * FileTransferCoordinator.MAX_CANCEL_ATTEMPTS * FileTransferCoordinator.CANCEL_RETRY_DELAY_MS

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
    /**
     * advanceUntilIdle() stops as soon as no foreground task is queued, and
     * the coordinator runs entirely in backgroundScope. Every scenario here
     * terminates well inside this virtual-time budget.
     */
    private fun TestScope.settle() {
        testScheduler.advanceTimeBy(SETTLE_BUDGET_MS)
        runCurrent()
    }

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

                override suspend fun cancel(transferId: String): FileTransferCancelResult {
                    cancelled += transferId
                    return FileTransferCancelResult.Cancelled
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

                override suspend fun cancel(transferId: String): FileTransferCancelResult {
                    cancelled += transferId
                    return FileTransferCancelResult.Cancelled
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

                override suspend fun cancel(transferId: String): FileTransferCancelResult {
                    cancelled += transferId
                    return FileTransferCancelResult.Cancelled
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
    fun cancellationConflictResumesDeliveryPollingInsteadOfShowingCancelled() = runTest {
        var statusCalls = 0
        val transport =
            object : FileTransferTransport {
                override suspend fun preflight(source: FileTransferSource): FilePreflightResponse = preflight()

                override suspend fun upload(
                    preflight: FilePreflightResponse,
                    source: FileTransferSource,
                    onProgress: (Long) -> Unit,
                ): FileTransferStatus = status(FileTransferServerState.Queued)

                override suspend fun status(transferId: String): FileTransferStatus =
                    if (++statusCalls == 1) {
                        status(FileTransferServerState.Materializing)
                    } else {
                        status(FileTransferServerState.Delivered)
                    }

                override suspend fun cancel(transferId: String): FileTransferCancelResult =
                    FileTransferCancelResult.CannotCancel
            }
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        val seen = mutableListOf<FileTransferUiState>()
        backgroundScope.launch { coordinator.state.collect { seen += it } }
        val source = TestFileSource()

        coordinator.upload(source)
        runCurrent()
        assertTrue(coordinator.state.value is FileTransferUiState.WaitingForGuest)
        // WaitingForGuest is already showing, and StateFlow drops the equal
        // value the resumed loop republishes, so the resume is proven by the
        // polling it does rather than by a second emission.
        assertEquals(0, statusCalls)
        coordinator.cancel()
        runCurrent()

        assertEquals("the 409 is resolved with exactly one status read", 1, statusCalls)
        assertTrue(
            "a 409 means materialization has begun, not that cancellation succeeded",
            seen.none { it is FileTransferUiState.Cancelled },
        )
        assertTrue(coordinator.state.value is FileTransferUiState.WaitingForGuest)
        testScheduler.advanceTimeBy(FileTransferCoordinator.STATUS_POLL_DELAY_MS)
        runCurrent()
        assertEquals("delivery polling resumed after the rejected cancellation", 2, statusCalls)
        assertEquals(FileTransferUiState.Delivered(source.name, source.size), coordinator.state.value)
        assertTrue(seen.none { it is FileTransferUiState.Cancelled })
    }

    @Test
    fun cancellationConflictDuringUploadTeardownRetriesUntilCancelled() = runTest {
        var cancelCalls = 0
        val transport =
            object : FileTransferTransport {
                override suspend fun preflight(source: FileTransferSource): FilePreflightResponse = preflight()

                override suspend fun upload(
                    preflight: FilePreflightResponse,
                    source: FileTransferSource,
                    onProgress: (Long) -> Unit,
                ): FileTransferStatus? = CompletableDeferred<FileTransferStatus?>().await()

                override suspend fun status(transferId: String): FileTransferStatus =
                    status(FileTransferServerState.AwaitingUpload)

                // The first DELETE races the server's PUT teardown; the retry lands.
                override suspend fun cancel(transferId: String): FileTransferCancelResult =
                    if (++cancelCalls == 1) FileTransferCancelResult.CannotCancel else FileTransferCancelResult.Cancelled
            }
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        val seen = mutableListOf<FileTransferUiState>()
        backgroundScope.launch { coordinator.state.collect { seen += it } }
        val source = TestFileSource()

        coordinator.upload(source)
        runCurrent()
        coordinator.cancel()
        runCurrent()
        assertFalse(
            "an AwaitingUpload 409 is a teardown race, not a delivery in progress",
            coordinator.state.value is FileTransferUiState.WaitingForGuest,
        )
        testScheduler.advanceTimeBy(FileTransferCoordinator.CANCEL_RETRY_DELAY_MS)
        runCurrent()

        assertEquals(FileTransferUiState.Cancelled(source.name), coordinator.state.value)
        assertTrue(cancelCalls >= 2)
        assertTrue(seen.none { it is FileTransferUiState.WaitingForGuest })
    }

    @Test
    fun cancellationWithoutServerConfirmationIsNotReportedAsCancelled() = runTest {
        var cancelCalls = 0
        var statusCalls = 0
        val transport =
            object : FileTransferTransport {
                override suspend fun preflight(source: FileTransferSource): FilePreflightResponse = preflight()

                override suspend fun upload(
                    preflight: FilePreflightResponse,
                    source: FileTransferSource,
                    onProgress: (Long) -> Unit,
                ): FileTransferStatus? = CompletableDeferred<FileTransferStatus?>().await()

                override suspend fun status(transferId: String): FileTransferStatus? {
                    statusCalls += 1
                    return null
                }

                override suspend fun cancel(transferId: String): FileTransferCancelResult {
                    cancelCalls += 1
                    return FileTransferCancelResult.Unavailable
                }
            }
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        val seen = mutableListOf<FileTransferUiState>()
        backgroundScope.launch { coordinator.state.collect { seen += it } }
        val source = TestFileSource()

        coordinator.upload(source)
        runCurrent()
        coordinator.cancel()
        runCurrent()
        testScheduler.advanceTimeBy(
            FileTransferCoordinator.MAX_CANCEL_ATTEMPTS * FileTransferCoordinator.CANCEL_RETRY_DELAY_MS,
        )
        runCurrent()

        assertEquals(
            FileTransferUiState.Unconfirmed(source.name, "Could not confirm cancelling ${source.name}"),
            coordinator.state.value,
        )
        assertEquals("no status fact means no extra DELETE", FileTransferCoordinator.MAX_CANCEL_ATTEMPTS, cancelCalls)
        assertEquals("one status read decides after the retry budget", 1, statusCalls)
        assertTrue(
            "no server fact supports a Cancelled claim",
            seen.none { it is FileTransferUiState.Cancelled },
        )
    }

    @Test
    fun cancellationUnavailableButServerDeliveredShowsDelivered() = runTest {
        var statusCalls = 0
        val transport =
            object : FileTransferTransport {
                override suspend fun preflight(source: FileTransferSource): FilePreflightResponse = preflight()

                override suspend fun upload(
                    preflight: FilePreflightResponse,
                    source: FileTransferSource,
                    onProgress: (Long) -> Unit,
                ): FileTransferStatus = status(FileTransferServerState.Queued)

                override suspend fun status(transferId: String): FileTransferStatus {
                    statusCalls += 1
                    return status(FileTransferServerState.Delivered)
                }

                override suspend fun cancel(transferId: String): FileTransferCancelResult =
                    FileTransferCancelResult.Unavailable
            }
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        val source = TestFileSource()

        coordinator.upload(source)
        runCurrent()
        assertTrue(coordinator.state.value is FileTransferUiState.WaitingForGuest)
        coordinator.cancel()
        runCurrent()
        testScheduler.advanceTimeBy(
            FileTransferCoordinator.MAX_CANCEL_ATTEMPTS * FileTransferCoordinator.CANCEL_RETRY_DELAY_MS,
        )
        runCurrent()

        assertEquals(FileTransferUiState.Delivered(source.name, source.size), coordinator.state.value)
        assertEquals(1, statusCalls)
    }

    @Test
    fun cancellationAfterResumedPollingIssuesAnotherDelete() = runTest {
        var cancelCalls = 0
        var statusCalls = 0
        val transport =
            object : FileTransferTransport {
                override suspend fun preflight(source: FileTransferSource): FilePreflightResponse = preflight()

                override suspend fun upload(
                    preflight: FilePreflightResponse,
                    source: FileTransferSource,
                    onProgress: (Long) -> Unit,
                ): FileTransferStatus = status(FileTransferServerState.Queued)

                override suspend fun status(transferId: String): FileTransferStatus {
                    statusCalls += 1
                    return status(FileTransferServerState.Materializing)
                }

                override suspend fun cancel(transferId: String): FileTransferCancelResult {
                    cancelCalls += 1
                    return FileTransferCancelResult.CannotCancel
                }
            }
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        coordinator.upload(TestFileSource())
        runCurrent()
        coordinator.cancel()
        runCurrent()
        assertEquals(1, cancelCalls)
        assertTrue(coordinator.state.value is FileTransferUiState.WaitingForGuest)

        coordinator.cancel()
        runCurrent()
        assertEquals("a rejected cancellation must not latch the attempt as already cancelled", 2, cancelCalls)
        assertTrue(coordinator.state.value is FileTransferUiState.WaitingForGuest)
        val statusCallsAfterSecondCancel = statusCalls
        testScheduler.advanceTimeBy(FileTransferCoordinator.STATUS_POLL_DELAY_MS)
        runCurrent()
        // Exactly one poll: the loop started by the first resume was stopped
        // by the second cancel, so only the second resume is still polling.
        assertEquals(statusCallsAfterSecondCancel + 1, statusCalls)
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

                override suspend fun cancel(transferId: String): FileTransferCancelResult = FileTransferCancelResult.Cancelled
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
    fun oneDroppedStatusPollDoesNotFailAQueuedTransfer() = runTest {
        val transport = ScriptedTransport(statuses = listOf(null, status(FileTransferServerState.Delivered)))
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        val seen = mutableListOf<FileTransferUiState>()
        backgroundScope.launch { coordinator.state.collect { seen += it } }
        val source = TestFileSource()

        coordinator.upload(source)
        settle()

        assertEquals(FileTransferUiState.Delivered(source.name, source.size), coordinator.state.value)
        assertTrue("a transient poll failure is not a verdict", seen.none { it is FileTransferUiState.Failed })
        assertEquals(0, transport.cancelCalls)
    }

    @Test
    fun failureAfterDroppedPollsFollowsAFileTheDaemonAlreadyHasInsteadOfOfferingRetry() = runTest {
        val transport =
            ScriptedTransport(
                statuses = listOf(null, null, null, status(FileTransferServerState.Delivered)),
                cancels = listOf(FileTransferCancelResult.CannotCancel),
            )
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        val seen = mutableListOf<FileTransferUiState>()
        backgroundScope.launch { coordinator.state.collect { seen += it } }
        val source = TestFileSource()

        coordinator.upload(source)
        settle()

        assertEquals(FileTransferUiState.Delivered(source.name, source.size), coordinator.state.value)
        assertTrue("Retry on a delivered file would duplicate it", seen.none { it is FileTransferUiState.Failed })
        assertEquals("the release DELETE was refused, which is what committed the attempt", 1, transport.cancelCalls)
    }

    @Test
    fun unconfirmedReleaseOfANeverCommittedTransferStaysRetryable() = runTest {
        // The PUT itself failed: the daemon never had the file, so Retry is safe
        // even though the reservation could not be released.
        val transport =
            ScriptedTransport(
                uploadResult = null,
                statuses = listOf(null),
                cancels = listOf(FileTransferCancelResult.Unavailable),
            )
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        val source = TestFileSource()

        coordinator.upload(source)
        settle()

        assertEquals(FileTransferUiState.Failed(source.name, "Upload failed"), coordinator.state.value)
        assertEquals(FileTransferCoordinator.MAX_CANCEL_ATTEMPTS, transport.cancelCalls)
    }

    @Test
    fun acceptedUploadThatGoesDarkIsUnconfirmedNotRetryable() = runTest {
        // 202, then the phone leaves the network: the daemon has the whole
        // file and will deliver it, so Retry would duplicate it.
        val transport =
            ScriptedTransport(statuses = listOf(null), cancels = listOf(FileTransferCancelResult.Unavailable))
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        val seen = mutableListOf<FileTransferUiState>()
        backgroundScope.launch { coordinator.state.collect { seen += it } }
        val source = TestFileSource()

        coordinator.upload(source)
        settle()

        assertEquals(
            FileTransferUiState.Unconfirmed(source.name, "Could not confirm transfer"),
            coordinator.state.value,
        )
        assertTrue("an accepted PUT commits the file; Retry must never be offered", seen.none { it is FileTransferUiState.Failed })
        assertEquals(FileTransferCoordinator.MAX_CANCEL_ATTEMPTS, transport.cancelCalls)
    }

    @Test
    fun failureAfterACommittedVerdictIsUnconfirmedNotRetryable() = runTest {
        val transport =
            ScriptedTransport(
                statuses = listOf(null, null, null, status(FileTransferServerState.Materializing), null),
                cancels = listOf(FileTransferCancelResult.CannotCancel),
            )
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        val seen = mutableListOf<FileTransferUiState>()
        backgroundScope.launch { coordinator.state.collect { seen += it } }
        val source = TestFileSource()

        coordinator.upload(source)
        settle()

        assertEquals(
            FileTransferUiState.Unconfirmed(source.name, "Could not confirm transfer"),
            coordinator.state.value,
        )
        assertTrue("the daemon holds the file; Retry must never be offered", seen.none { it is FileTransferUiState.Failed })
        assertEquals("the re-entrant failure is answered by the latch, not a second DELETE budget", 1, transport.cancelCalls)
    }

    /** A transport whose DELETE suspends once: a release cancelled with its scope never resumes past it. */
    private class SuspendingCancelTransport : FileTransferTransport {
        val cancelled = mutableListOf<String>()

        override suspend fun preflight(source: FileTransferSource): FilePreflightResponse = preflight()

        override suspend fun upload(
            preflight: FilePreflightResponse,
            source: FileTransferSource,
            onProgress: (Long) -> Unit,
        ): FileTransferStatus = status(FileTransferServerState.Queued)

        override suspend fun status(transferId: String): FileTransferStatus? = null

        override suspend fun cancel(transferId: String): FileTransferCancelResult {
            yield()
            cancelled += transferId
            return FileTransferCancelResult.Cancelled
        }
    }

    @Test
    fun closeReleasesTheReservationEvenWhenItsScopeIsCancelledInTheSameTick() = runTest {
        val transport = SuspendingCancelTransport()
        val scope = CoroutineScope(SupervisorJob() + StandardTestDispatcher(testScheduler))
        val coordinator = FileTransferCoordinator(scope, transport)
        coordinator.upload(TestFileSource())
        runCurrent()
        assertTrue(coordinator.state.value is FileTransferUiState.WaitingForGuest)

        // A Compose disposal pass runs onDispose { close() } and cancels the
        // screen's rememberCoroutineScope() in the same synchronous step.
        coordinator.close()
        scope.cancel()
        runCurrent()

        assertEquals(listOf(preflight().transfer_id), transport.cancelled)
    }

    @Test
    fun closeReleasesTheReservationEvenWhenItsScopeWasAlreadyCancelled() = runTest {
        val transport = SuspendingCancelTransport()
        val scope = CoroutineScope(SupervisorJob() + StandardTestDispatcher(testScheduler))
        val coordinator = FileTransferCoordinator(scope, transport)
        coordinator.upload(TestFileSource())
        runCurrent()
        assertTrue(coordinator.state.value is FileTransferUiState.WaitingForGuest)

        // Compose does not promise which RememberObserver is forgotten first.
        scope.cancel()
        coordinator.close()
        runCurrent()

        assertEquals(listOf(preflight().transfer_id), transport.cancelled)
    }

    @Test
    fun cancelReleasesTheReservationEvenWhenItsScopeIsCancelledInTheSameTick() = runTest {
        val transport = SuspendingCancelTransport()
        val scope = CoroutineScope(SupervisorJob() + StandardTestDispatcher(testScheduler))
        val coordinator = FileTransferCoordinator(scope, transport)
        coordinator.upload(TestFileSource())
        runCurrent()
        assertTrue(coordinator.state.value is FileTransferUiState.WaitingForGuest)

        // Cancel, then Back: close() finds nothing active, so the DELETE
        // started by cancel() is the only release there will be.
        coordinator.cancel()
        coordinator.close()
        scope.cancel()
        runCurrent()

        assertEquals(listOf(preflight().transfer_id), transport.cancelled)
    }

    @Test
    fun cancelWhileFollowingARefusedReleaseReadsStatusInsteadOfGivingUp() = runTest {
        val transport =
            ScriptedTransport(
                statuses =
                    listOf(
                        null, null, null,
                        status(FileTransferServerState.Materializing),
                        status(FileTransferServerState.Materializing),
                        status(FileTransferServerState.Delivered),
                    ),
                cancels = listOf(FileTransferCancelResult.CannotCancel),
            )
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        val seen = mutableListOf<FileTransferUiState>()
        backgroundScope.launch { coordinator.state.collect { seen += it } }
        val source = TestFileSource()

        coordinator.upload(source)
        testScheduler.advanceTimeBy(FileTransferCoordinator.MAX_CONSECUTIVE_STATUS_FAILURES * FileTransferCoordinator.STATUS_POLL_DELAY_MS)
        runCurrent()
        assertEquals("fail() took the Committed branch and is following delivery", 1, transport.cancelCalls)
        assertEquals(4, transport.statusCalls)
        assertTrue(coordinator.state.value is FileTransferUiState.WaitingForGuest)

        coordinator.cancel()
        runCurrent()

        assertEquals("the latch makes a DELETE pointless; one status read decides", 5, transport.statusCalls)
        assertEquals(1, transport.cancelCalls)
        assertTrue(
            "a live entry resumes polling rather than reporting an unconfirmed cancellation",
            coordinator.state.value is FileTransferUiState.WaitingForGuest,
        )
        testScheduler.advanceTimeBy(FileTransferCoordinator.STATUS_POLL_DELAY_MS)
        runCurrent()
        assertEquals(FileTransferUiState.Delivered(source.name, source.size), coordinator.state.value)
        assertTrue(seen.none { it is FileTransferUiState.Unconfirmed || it is FileTransferUiState.Failed })
    }

    @Test
    fun cancelDuringAFailedReleaseLoopStillSendsTheDelete() = runTest {
        // A failed PUT starts fail()'s release loop with the latch set while
        // the UI still offers Cancel; the first DELETE is lost to the network.
        val transport =
            ScriptedTransport(
                uploadResult = null,
                statuses = listOf(status(FileTransferServerState.AwaitingUpload)),
                cancels = listOf(FileTransferCancelResult.Unavailable, FileTransferCancelResult.Cancelled),
            )
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        val seen = mutableListOf<FileTransferUiState>()
        backgroundScope.launch { coordinator.state.collect { seen += it } }
        val source = TestFileSource()

        coordinator.upload(source)
        runCurrent()
        assertEquals("the release loop is mid-retry", 1, transport.cancelCalls)

        coordinator.cancel()
        runCurrent()

        assertEquals("a still-cancellable entry gets the DELETE the user asked for", 2, transport.cancelCalls)
        assertEquals(FileTransferUiState.Cancelled(source.name), coordinator.state.value)
        assertTrue(
            "a never-uploaded file must not be shown as delivering",
            seen.none { it is FileTransferUiState.WaitingForGuest },
        )
    }

    @Test
    fun staleUploadProgressCannotOverwriteAResumedDelivery() = runTest {
        val transport =
            ScriptedTransport(
                statuses = listOf(status(FileTransferServerState.Materializing)),
                cancels = listOf(FileTransferCancelResult.CannotCancel),
            )
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        coordinator.upload(TestFileSource())
        runCurrent()
        assertTrue(coordinator.state.value is FileTransferUiState.WaitingForGuest)
        coordinator.cancel()
        runCurrent()
        assertTrue("the 409 resumed delivery polling", coordinator.state.value is FileTransferUiState.WaitingForGuest)

        // The PUT write loop is not coroutine-cancellable and can report
        // after cancel() once the attempt is current again.
        checkNotNull(transport.onProgress)(123)

        assertTrue(
            "a write loop that outlived cancel() must not republish Uploading",
            coordinator.state.value is FileTransferUiState.WaitingForGuest,
        )
    }

    @Test
    fun goneOnCancelIsConfirmedWithoutAStatusRead() = runTest {
        val transport = ScriptedTransport(cancels = listOf(FileTransferCancelResult.Gone))
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        val source = TestFileSource()

        coordinator.upload(source)
        runCurrent()
        coordinator.cancel()
        runCurrent()

        assertEquals(FileTransferUiState.Cancelled(source.name), coordinator.state.value)
        assertEquals("a dead session cannot deliver; no retry is needed", 1, transport.cancelCalls)
        assertEquals(0, transport.statusCalls)
    }

    @Test
    fun exhaustedRetriesTryOneLastDeleteWhenTheEntryIsStillCancellable() = runTest {
        val transport =
            ScriptedTransport(
                statuses = listOf(status(FileTransferServerState.AwaitingUpload)),
                cancels =
                    List(FileTransferCoordinator.MAX_CANCEL_ATTEMPTS) { FileTransferCancelResult.Unavailable } +
                        FileTransferCancelResult.Cancelled,
            )
        val coordinator = FileTransferCoordinator(backgroundScope, transport)
        val source = TestFileSource()

        coordinator.upload(source)
        runCurrent()
        coordinator.cancel()
        settle()

        assertEquals(FileTransferUiState.Cancelled(source.name), coordinator.state.value)
        assertEquals(
            "a reachable server with a cancellable entry earns one last DELETE",
            FileTransferCoordinator.MAX_CANCEL_ATTEMPTS + 1,
            transport.cancelCalls,
        )
    }

    @Test
    fun aLateVerdictForASupersededAttemptCannotOverwriteItsReplacement() = runTest {
        val slowId = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        val fastId = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        val releaseSlow = CompletableDeferred<Unit>()
        val transport =
            object : FileTransferTransport {
                override suspend fun preflight(source: FileTransferSource): FilePreflightResponse =
                    preflight(if (source.name == "slow") slowId else fastId)

                override suspend fun upload(
                    preflight: FilePreflightResponse,
                    source: FileTransferSource,
                    onProgress: (Long) -> Unit,
                ): FileTransferStatus = status(FileTransferServerState.Queued).copy(transfer_id = preflight.transfer_id)

                override suspend fun status(transferId: String): FileTransferStatus? = null

                override suspend fun cancel(transferId: String): FileTransferCancelResult {
                    if (transferId == slowId) releaseSlow.await()
                    return FileTransferCancelResult.Cancelled
                }
            }
        val coordinator = FileTransferCoordinator(backgroundScope, transport)

        coordinator.upload(TestFileSource(name = "slow"))
        runCurrent()
        coordinator.cancel()
        runCurrent()
        coordinator.upload(TestFileSource(name = "fast"))
        runCurrent()
        coordinator.cancel()
        runCurrent()
        assertEquals(FileTransferUiState.Cancelled("fast"), coordinator.state.value)

        releaseSlow.complete(Unit)
        runCurrent()

        assertEquals(
            "a superseded attempt's verdict must not overwrite its replacement's",
            FileTransferUiState.Cancelled("fast"),
            coordinator.state.value,
        )
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
