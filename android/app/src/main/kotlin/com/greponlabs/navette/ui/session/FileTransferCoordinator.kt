package com.greponlabs.navette.ui.session

import android.content.ContentResolver
import android.database.Cursor
import android.net.Uri
import android.provider.OpenableColumns
import com.greponlabs.navette.net.MAX_BLOB_BYTES
import com.greponlabs.navette.net.fileTransferCollectionUrl
import com.greponlabs.navette.net.mediaJson
import java.io.InputStream
import java.net.HttpURLConnection
import java.net.URL
import java.nio.charset.StandardCharsets
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.serialization.Serializable
import kotlin.coroutines.coroutineContext

/** The largest payload accepted by the daemon's file-transfer endpoint. */
internal const val MAX_FILE_TRANSFER_BYTES: Long = MAX_BLOB_BYTES
internal const val FILE_TRANSFER_BUFFER_BYTES: Int = 64 * 1024

/**
 * Re-openable file data. Retrying opens a new provider stream; bytes are never
 * cached in memory or on an app-owned temporary file.
 */
internal interface FileTransferSource {
    val name: String
    val mime: String
    val size: Long

    fun open(): InputStream?
}

/** Android DocumentsProvider-backed [FileTransferSource]. */
internal class ContentResolverFileTransferSource private constructor(
    private val resolver: ContentResolver,
    private val uri: Uri,
    override val name: String,
    override val mime: String,
    override val size: Long,
) : FileTransferSource {
    override fun open(): InputStream? = resolver.openInputStream(uri)

    companion object {
        /**
         * Refuse unknown sizes rather than streaming an unbounded provider.
         * This mirrors the daemon validation before it reserves session quota.
         */
        fun from(resolver: ContentResolver, uri: Uri): ContentResolverFileTransferSource? {
            val row =
                runCatching {
                    resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)
                        ?.use(::readDocumentMetadata)
                }.getOrNull() ?: return null
            val name = row.first ?: return null
            val size = row.second ?: return null
            val mime = runCatching { resolver.getType(uri) }.getOrNull() ?: "application/octet-stream"
            if (!isSafeFileTransferMetadata(name, mime, size)) return null
            return ContentResolverFileTransferSource(resolver, uri, name, mime, size)
        }

        private fun readDocumentMetadata(cursor: Cursor): Pair<String?, Long?> {
            if (!cursor.moveToFirst()) return null to null
            val name = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                .takeIf { it >= 0 && !cursor.isNull(it) }
                ?.let(cursor::getString)
            val size = cursor.getColumnIndex(OpenableColumns.SIZE)
                .takeIf { it >= 0 && !cursor.isNull(it) }
                ?.let(cursor::getLong)
            return name to size
        }
    }
}

internal fun isSafeFileTransferMetadata(name: String, mime: String, size: Long): Boolean =
    name.isNotEmpty() &&
        name.toByteArray(StandardCharsets.UTF_8).size <= 255 &&
        name != "." &&
        name != ".." &&
        '/' !in name &&
        '\\' !in name &&
        name.none(Char::isISOControl) &&
        size in 1..MAX_FILE_TRANSFER_BYTES &&
        isSafeMime(mime)

/** Kotlin mirror of the daemon's conservative RFC 9110 media-type validation. */
internal fun isSafeMime(mime: String): Boolean {
    if (mime.isEmpty() || mime.toByteArray(StandardCharsets.UTF_8).size > 255) return false
    val slash = mime.indexOf('/')
    if (slash <= 0 || slash != mime.lastIndexOf('/') || slash == mime.lastIndex) return false
    return mime.all {
        it == '/' ||
            it in 'a'..'z' ||
            it in 'A'..'Z' ||
            it in '0'..'9' ||
            it in "!#$%&'*+-.^_`|~"
    }
}

@Serializable
internal data class FilePreflightRequest(val name: String, val mime: String, val size: Long)

@Serializable
internal data class FilePreflightResponse(
    val transfer_id: String,
    val upload_url: String,
    val expires_at: Long,
)

@Serializable
internal data class FileTransferStatus(
    val transfer_id: String,
    val name: String,
    val mime: String,
    val size: Long,
    val bytes_received: Long,
    val state: FileTransferServerState,
    val expires_at: Long? = null,
)

@Serializable
internal enum class FileTransferServerState {
    @kotlinx.serialization.SerialName("awaiting_upload") AwaitingUpload,
    @kotlinx.serialization.SerialName("queued") Queued,
    @kotlinx.serialization.SerialName("materializing") Materializing,
    @kotlinx.serialization.SerialName("delivered") Delivered,
    @kotlinx.serialization.SerialName("failed") Failed,
    @kotlinx.serialization.SerialName("cancelled") Cancelled,
}

/**
 * The daemon distinguishes a retryable transport problem from a 409. The 409
 * alone is ambiguous: the entry may still be tearing down a PUT, may be
 * committed to delivery, or may already be terminal. The coordinator resolves
 * it with a status read rather than acting on it directly.
 */
internal enum class FileTransferCancelResult {
    Cancelled,
    CannotCancel,

    /** 404: no live session incarnation can deliver this id any more. */
    Gone,
    Unavailable,
}

/** Injectable HTTP boundary; the coordinator is fully JVM-testable without Android networking. */
internal interface FileTransferTransport {
    suspend fun preflight(source: FileTransferSource): FilePreflightResponse?

    suspend fun upload(
        preflight: FilePreflightResponse,
        source: FileTransferSource,
        onProgress: (Long) -> Unit,
    ): FileTransferStatus?

    suspend fun status(transferId: String): FileTransferStatus?

    suspend fun cancel(transferId: String): FileTransferCancelResult

    /** Abort the live request before the coordinator asks the daemon to cancel it. */
    fun cancelActiveUpload() = Unit
}

/** Authenticated fixed-length, 64 KiB streaming HTTP transport. */
internal class HttpFileTransferTransport(
    mediaUrl: String,
    private val token: String,
) : FileTransferTransport {
    private val collectionUrl = fileTransferCollectionUrl(mediaUrl)
    @Volatile private var activeUpload: HttpURLConnection? = null

    override suspend fun preflight(source: FileTransferSource): FilePreflightResponse? =
        withContext(Dispatchers.IO) {
            if (!isSafeFileTransferMetadata(source.name, source.mime, source.size)) return@withContext null
            request(
                "POST",
                collectionUrl,
                body = mediaJson.encodeToString(
                    FilePreflightRequest.serializer(),
                    FilePreflightRequest(source.name, source.mime, source.size),
                ),
            ) {
                if (responseCode != HttpURLConnection.HTTP_CREATED) return@request null
                inputStream.bufferedReader().use { reader ->
                    mediaJson.decodeFromString(FilePreflightResponse.serializer(), reader.readText())
                }.takeIf(::isExpectedUploadPath)
            }
        }

    override suspend fun upload(
        preflight: FilePreflightResponse,
        source: FileTransferSource,
        onProgress: (Long) -> Unit,
    ): FileTransferStatus? =
        withContext(Dispatchers.IO) {
            if (!isSafeFileTransferMetadata(source.name, source.mime, source.size) ||
                !isExpectedUploadPath(preflight)
            ) {
                return@withContext null
            }
            val connection = (URL(originUrl(preflight.upload_url)).openConnection() as HttpURLConnection)
            activeUpload = connection
            try {
                connection.requestMethod = "PUT"
                connection.setRequestProperty("Authorization", "Bearer $token")
                connection.setRequestProperty("Content-Type", "application/octet-stream")
                connection.setFixedLengthStreamingMode(source.size)
                connection.connectTimeout = HTTP_CONNECT_TIMEOUT_MS
                connection.readTimeout = HTTP_READ_TIMEOUT_MS
                connection.doOutput = true
                source.open()?.use { input ->
                    connection.outputStream.use { output ->
                        val buffer = ByteArray(FILE_TRANSFER_BUFFER_BYTES)
                        var sent = 0L
                        while (true) {
                            val count = input.read(buffer)
                            if (count < 0) break
                            if (count == 0) continue
                            if (sent + count > source.size) return@withContext null
                            output.write(buffer, 0, count)
                            sent += count
                            onProgress(sent)
                        }
                        if (sent != source.size) return@withContext null
                    }
                } ?: return@withContext null
                if (connection.responseCode != HttpURLConnection.HTTP_ACCEPTED) return@withContext null
                connection.inputStream.bufferedReader().use { reader ->
                    mediaJson.decodeFromString(FileTransferStatus.serializer(), reader.readText())
                }
            } finally {
                if (activeUpload === connection) activeUpload = null
                connection.disconnect()
            }
        }

    override suspend fun status(transferId: String): FileTransferStatus? =
        withContext(Dispatchers.IO) {
            if (!isTransferId(transferId)) return@withContext null
            request("GET", "$collectionUrl/$transferId") {
                if (responseCode != HttpURLConnection.HTTP_OK) return@request null
                inputStream.bufferedReader().use { reader ->
                    mediaJson.decodeFromString(FileTransferStatus.serializer(), reader.readText())
                }
            }
        }

    override suspend fun cancel(transferId: String): FileTransferCancelResult =
        withContext(Dispatchers.IO) {
            if (!isTransferId(transferId)) return@withContext FileTransferCancelResult.Unavailable
            request("DELETE", "$collectionUrl/$transferId") {
                when (responseCode) {
                    HttpURLConnection.HTTP_NO_CONTENT -> FileTransferCancelResult.Cancelled
                    HttpURLConnection.HTTP_CONFLICT -> FileTransferCancelResult.CannotCancel
                    HttpURLConnection.HTTP_NOT_FOUND -> FileTransferCancelResult.Gone
                    else -> FileTransferCancelResult.Unavailable
                }
            } ?: FileTransferCancelResult.Unavailable
        }

    override fun cancelActiveUpload() {
        activeUpload?.disconnect()
    }

    private fun originUrl(path: String): String = collectionUrl.substringBefore("/v1/") + path

    private fun isExpectedUploadPath(preflight: FilePreflightResponse): Boolean =
        isTransferId(preflight.transfer_id) &&
            preflight.upload_url ==
                collectionUrl.substringAfter(collectionUrl.substringBefore("/v1/")) +
                    "/${preflight.transfer_id}/content"

    private inline fun <T> request(method: String, url: String, body: String? = null, block: HttpURLConnection.() -> T): T? =
        runCatching {
            val connection = (URL(url).openConnection() as HttpURLConnection)
            try {
                connection.requestMethod = method
                connection.setRequestProperty("Authorization", "Bearer $token")
                connection.setRequestProperty("Accept", "application/json")
                connection.connectTimeout = HTTP_CONNECT_TIMEOUT_MS
                connection.readTimeout = HTTP_READ_TIMEOUT_MS
                if (body != null) {
                    connection.setRequestProperty("Content-Type", "application/json")
                    connection.doOutput = true
                    connection.outputStream.bufferedWriter().use { it.write(body) }
                }
                connection.block()
            } finally {
                connection.disconnect()
            }
        }.getOrNull()

    companion object {
        private const val HTTP_CONNECT_TIMEOUT_MS = 10_000
        private const val HTTP_READ_TIMEOUT_MS = 65_000
    }
}

internal sealed interface FileTransferUiState {
    data object Idle : FileTransferUiState
    data class Preparing(val name: String, val size: Long) : FileTransferUiState
    data class Uploading(val name: String, val sent: Long, val size: Long) : FileTransferUiState
    data class WaitingForGuest(val transferId: String, val name: String, val size: Long) : FileTransferUiState
    data class Delivered(val name: String, val size: Long) : FileTransferUiState
    data class Cancelled(val name: String) : FileTransferUiState

    /**
     * The daemon may hold the file but never confirmed what became of it:
     * a cancellation it did not acknowledge, or a failure seen after it had
     * already refused to release the file. Deliberately not a retryable
     * failure: re-sending a possibly delivered file would duplicate it.
     */
    data class Unconfirmed(val name: String, val message: String) : FileTransferUiState
    data class Failed(val name: String, val message: String) : FileTransferUiState
}

/**
 * A session-keyed owner for one selected document. It deliberately does not
 * depend on SessionController: recreating a media WebSocket cannot cancel a
 * live HTTP upload or discard its retryable DocumentsProvider URI.
 */
internal class FileTransferCoordinator(
    private val scope: CoroutineScope,
    private val transport: FileTransferTransport,
) {
    private val _state = MutableStateFlow<FileTransferUiState>(FileTransferUiState.Idle)
    val state: StateFlow<FileTransferUiState> = _state.asStateFlow()
    private var retrySource: FileTransferSource? = null

    /** Read from the transport's IO thread by the upload progress callback. */
    @Volatile private var active: ActiveTransfer? = null

    /**
     * The newest attempt ever started, kept after it ends. [active] alone
     * cannot tell "no attempt running" from "a newer attempt already
     * finished": a slow verdict for an old attempt must not overwrite the
     * state its replacement published.
     */
    private var latest: ActiveTransfer? = null

    /**
     * The id belongs to this attempt, not the coordinator. A prior,
     * non-cancellable provider/HTTP call can finish after replacement; it
     * must only ever clean up its own daemon reservation.
     */
    private class ActiveTransfer(val source: FileTransferSource) {
        var transferId: String? = null
        var work: Job? = null
        var remoteCancellationStarted = false

        /**
         * The daemon holds the complete file: the PUT was accepted, or a
         * release was refused. Any failure seen afterwards can no longer
         * offer Retry: the guest may still receive the file, and a second
         * upload would duplicate it.
         */
        var committed = false

        /** A terminal state was published; the attempt can be neither resumed nor republished. */
        var finished = false
    }

    /**
     * What the daemon actually did with a cancellation. A 409 alone is
     * ambiguous: it covers a PUT still being torn down, a delivery that has
     * begun, and an entry that is already terminal. Only a status read tells
     * those apart, so a raw [FileTransferCancelResult] never leaves
     * [cancelRemote].
     */
    private sealed interface RemoteCancellation {
        /** The server says the transfer is gone, or ended without delivery. */
        data object Confirmed : RemoteCancellation

        /**
         * Delivery could not be stopped. Carries the status that proved it so
         * the caller can resume polling without another GET.
         */
        data class Committed(val status: FileTransferStatus) : RemoteCancellation

        /** Neither the DELETE nor a status read established what the server did. */
        data object Unconfirmed : RemoteCancellation
    }

    fun upload(next: FileTransferSource) {
        if (!isSafeFileTransferMetadata(next.name, next.mime, next.size)) {
            _state.value = FileTransferUiState.Failed(next.name, "This file cannot be sent")
            return
        }
        releaseActive()
        retrySource = next
        ActiveTransfer(next).also { transfer ->
            active = transfer
            latest = transfer
            transfer.work = scope.launch { uploadCurrent(transfer) }
        }
    }

    fun retry() {
        val retained = retrySource ?: return
        upload(retained)
    }

    fun cancel() {
        val transfer = active ?: return
        val name = transfer.source.name
        active = null
        stop(transfer)
        // Only the DELETE is shielded, for the same reason as releaseActive():
        // a Back tap right after Cancel would otherwise cancel this coroutine
        // with the scope and hold the reservation until server expiry. The
        // verdict handling stays cancellable so a later cancel()/close()/
        // upload() can end the polling it may resume.
        scope.launch(start = CoroutineStart.UNDISPATCHED) {
            val outcome =
                withContext(NonCancellable) {
                    withTimeoutOrNull(RELEASE_TIMEOUT_MS) { settleCancellation(transfer) }
                } ?: RemoteCancellation.Unconfirmed
            when (outcome) {
                RemoteCancellation.Confirmed ->
                    publishUnlessReplaced(transfer, FileTransferUiState.Cancelled(name))
                RemoteCancellation.Unconfirmed ->
                    publishUnlessReplaced(
                        transfer,
                        FileTransferUiState.Unconfirmed(name, "Could not confirm cancelling $name"),
                    )
                is RemoteCancellation.Committed -> resumeDeliveryAfterRejectedCancellation(transfer, outcome.status)
            }
        }
    }

    /**
     * A Cancel tap can find [cancelRemote]'s latch already set: [fail] is
     * following a file the daemon refused to release, or its release loop
     * was still retrying when stop() cancelled it. Another blind DELETE
     * budget would teach nothing in the first case, so one status read
     * decides: a delivery in progress resumes polling (which resets the
     * latch, so the next Cancel sends a fresh DELETE); an entry that is
     * still cancellable gets the DELETE the user asked for; no answer is
     * reported as unconfirmed.
     */
    private suspend fun settleCancellation(transfer: ActiveTransfer): RemoteCancellation {
        if (!transfer.remoteCancellationStarted) return cancelRemote(transfer)
        val id = transfer.transferId ?: return RemoteCancellation.Confirmed
        val status = readStatus(id) ?: return RemoteCancellation.Unconfirmed
        return when (status.state) {
            // Resetting the latch is safe: the only other release loop on
            // this attempt was the one stop() just cancelled, and its
            // continuation unwinds without touching the latch. A DELETE it
            // may still have in flight is idempotent: 204 here and 409
            // there classify as Cancelled, which is Confirmed either way.
            FileTransferServerState.AwaitingUpload,
            FileTransferServerState.Queued -> {
                transfer.remoteCancellationStarted = false
                cancelRemote(transfer)
            }
            FileTransferServerState.Materializing,
            FileTransferServerState.Delivered -> RemoteCancellation.Committed(status)
            FileTransferServerState.Cancelled,
            FileTransferServerState.Failed -> RemoteCancellation.Confirmed
        }
    }

    /**
     * A replacement may have started while the daemon cancellation was in
     * flight; its state must win over this old attempt, even once the
     * replacement has itself finished and cleared [active]. A finished
     * attempt is never republished either.
     */
    private fun publishUnlessReplaced(transfer: ActiveTransfer, state: FileTransferUiState) {
        if (active == null && latest === transfer && !transfer.finished) _state.value = state
    }

    fun close() {
        releaseActive()
    }

    /**
     * Ends the current attempt and releases its daemon reservation even when
     * [scope] is cancelled in the same tick. A Compose disposal pass runs
     * `onDispose { close() }` and cancels the screen's scope together, so a
     * plainly launched release would be cancelled before it was ever
     * dispatched and the reservation would be held until server expiry.
     * UNDISPATCHED runs the release to its first suspension right here, and
     * NonCancellable (same dispatcher, only the Job changes, so it does not
     * suspend first) lets the time-boxed remainder finish.
     */
    private fun releaseActive() {
        val transfer = active ?: return
        active = null
        stop(transfer)
        scope.launch(start = CoroutineStart.UNDISPATCHED) {
            withContext(NonCancellable) {
                withTimeoutOrNull(RELEASE_TIMEOUT_MS) { cancelRemote(transfer) }
            }
        }
    }

    private fun stop(transfer: ActiveTransfer) {
        // HttpURLConnection is not coroutine-cancellable while it is blocked
        // in a provider/socket read. Disconnect before cancelling its Job;
        // this lets the server drop its writer before the DELETE below.
        transport.cancelActiveUpload()
        transfer.work?.cancel()
    }

    private suspend fun cancelRemote(transfer: ActiveTransfer): RemoteCancellation {
        val id = transfer.transferId ?: return RemoteCancellation.Confirmed
        if (transfer.remoteCancellationStarted) return RemoteCancellation.Unconfirmed
        transfer.remoteCancellationStarted = true
        repeat(MAX_CANCEL_ATTEMPTS) { attempt ->
            when (deleteRemote(id)) {
                // Gone: the session incarnation that could deliver it is dead.
                FileTransferCancelResult.Cancelled,
                FileTransferCancelResult.Gone -> return RemoteCancellation.Confirmed
                FileTransferCancelResult.CannotCancel -> classifyConflict(id)?.let { return it }
                FileTransferCancelResult.Unavailable -> Unit
            }
            if (attempt + 1 < MAX_CANCEL_ATTEMPTS) delay(CANCEL_RETRY_DELAY_MS)
        }
        // Server expiry remains the backstop if the network is down.
        return verdictAfterExhaustedRetries(id)
    }

    /** A transport exception is retried exactly like an `Unavailable` reply. */
    private suspend fun deleteRemote(id: String): FileTransferCancelResult =
        try {
            transport.cancel(id)
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (_: Exception) {
            // The next short retry covers a close racing server teardown.
            FileTransferCancelResult.Unavailable
        }

    private suspend fun readStatus(id: String): FileTransferStatus? =
        try {
            transport.status(id)
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (_: Exception) {
            null
        }

    /**
     * Resolves a 409. `null` asks for another DELETE: the client's disconnect
     * frequently races the server's PUT teardown, and once the server's
     * writer drops the entry is left AwaitingUpload/Queued and cancellable
     * again. A failed status read is retried the same way; the post-budget
     * read in [verdictAfterExhaustedRetries] has the last word.
     */
    private suspend fun classifyConflict(id: String): RemoteCancellation? {
        val status = readStatus(id) ?: return null
        return when (status.state) {
            FileTransferServerState.AwaitingUpload,
            FileTransferServerState.Queued -> null
            FileTransferServerState.Materializing,
            FileTransferServerState.Delivered -> RemoteCancellation.Committed(status)
            // Failed is terminal without delivery: the user asked for the file
            // not to arrive and it did not, so it reads as cancelled.
            FileTransferServerState.Cancelled,
            FileTransferServerState.Failed -> RemoteCancellation.Confirmed
        }
    }

    /**
     * Every DELETE went unanswered or was refused without a verdict. One
     * status read decides what the UI may claim. No answer at all is
     * reported as unconfirmed rather than cancelled, because the file may
     * still reach the guest; an entry that is still cancellable means the
     * server is reachable again, so it earns one last DELETE.
     */
    private suspend fun verdictAfterExhaustedRetries(id: String): RemoteCancellation {
        val status = readStatus(id) ?: return RemoteCancellation.Unconfirmed
        return when (status.state) {
            FileTransferServerState.AwaitingUpload,
            FileTransferServerState.Queued -> verdictAfterFinalDelete(id, status)
            FileTransferServerState.Materializing,
            FileTransferServerState.Delivered -> RemoteCancellation.Committed(status)
            FileTransferServerState.Cancelled,
            FileTransferServerState.Failed -> RemoteCancellation.Confirmed
        }
    }

    /**
     * The post-budget read found the entry still cancellable. If this DELETE
     * does not land either, the verdict is honest about what happens next:
     * an unreleased reservation is unconfirmed, a queued file is committed
     * because the daemon will deliver it.
     */
    private suspend fun verdictAfterFinalDelete(id: String, status: FileTransferStatus): RemoteCancellation {
        val unreleased =
            if (status.state == FileTransferServerState.Queued) {
                RemoteCancellation.Committed(status)
            } else {
                RemoteCancellation.Unconfirmed
            }
        return when (deleteRemote(id)) {
            FileTransferCancelResult.Cancelled,
            FileTransferCancelResult.Gone -> RemoteCancellation.Confirmed
            FileTransferCancelResult.CannotCancel -> classifyConflict(id) ?: unreleased
            FileTransferCancelResult.Unavailable -> unreleased
        }
    }

    /**
     * The daemon refused to cancel, so this attempt is live again: it owns
     * the UI, a later cancel()/close()/upload() must issue a fresh DELETE,
     * and stop() must be able to end the polling that starts here.
     */
    private suspend fun resumeDeliveryAfterRejectedCancellation(transfer: ActiveTransfer, status: FileTransferStatus) {
        transfer.committed = true
        if (active != null || latest !== transfer || transfer.finished) return
        active = transfer
        transfer.remoteCancellationStarted = false
        transfer.work = coroutineContext[Job]
        try {
            waitForDelivery(transfer, status)
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (_: Exception) {
            fail(transfer, "Transfer failed")
        }
    }

    private fun isCurrent(transfer: ActiveTransfer): Boolean = active === transfer

    private suspend fun uploadCurrent(transfer: ActiveTransfer) {
        val current = transfer.source
        try {
            if (!isCurrent(transfer)) return
            _state.value = FileTransferUiState.Preparing(current.name, current.size)
            val preflight = transport.preflight(current) ?: return fail(transfer, "Could not prepare transfer")
            if (!isCurrent(transfer)) {
                transfer.transferId = preflight.transfer_id
                cancelRemote(transfer)
                return
            }
            transfer.transferId = preflight.transfer_id
            // The write loop reports from the IO thread and can outlive
            // stop(): disconnect() before connect() is a no-op, so a stopped
            // upload may still be streaming when a rejected cancellation
            // makes this attempt current again. Progress is shown only while
            // this upload's own Job is live; stop() cancels it before any
            // resume re-points [ActiveTransfer.work] at the polling Job.
            val uploadJob = coroutineContext[Job]
            val accepted = transport.upload(preflight, current) { sent ->
                if (uploadJob?.isActive == true && isCurrent(transfer)) {
                    _state.value = FileTransferUiState.Uploading(current.name, sent, current.size)
                }
            } ?: return fail(transfer, "Upload failed")
            // 202 means the daemon holds the complete file and will deliver
            // it; from here on a Retry would duplicate it on the guest.
            transfer.committed = true
            if (!isCurrent(transfer)) {
                cancelRemote(transfer)
                return
            }
            waitForDelivery(transfer, accepted)
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (_: Exception) {
            fail(transfer, "Transfer failed")
        }
    }

    private suspend fun waitForDelivery(transfer: ActiveTransfer, initial: FileTransferStatus) {
        val current = transfer.source
        var status = initial
        var droppedPolls = 0
        repeat(MAX_STATUS_POLLS) {
            if (!isCurrent(transfer)) {
                cancelRemote(transfer)
                return
            }
            when (status.state) {
                FileTransferServerState.Delivered ->
                    return publishTerminal(transfer, FileTransferUiState.Delivered(current.name, current.size))
                FileTransferServerState.Cancelled ->
                    return publishTerminal(transfer, FileTransferUiState.Cancelled(current.name))
                FileTransferServerState.Failed -> return fail(transfer, "Guest could not receive file")
                else -> if (isCurrent(transfer)) {
                    _state.value = FileTransferUiState.WaitingForGuest(status.transfer_id, current.name, current.size)
                }
            }
            delay(STATUS_POLL_DELAY_MS)
            // One dropped poll is not a verdict on a file the daemon may
            // already hold; the loop keeps its last known status and spends
            // one of its iterations on the retry.
            val polled = transport.status(status.transfer_id)
            if (polled == null) {
                droppedPolls += 1
                if (droppedPolls >= MAX_CONSECUTIVE_STATUS_FAILURES) return fail(transfer, "Could not confirm transfer")
            } else {
                droppedPolls = 0
                status = polled
            }
        }
        fail(transfer, "Guest delivery timed out")
    }

    /**
     * A failed PUT can leave a daemon-side AwaitingUpload reservation.
     * Release it before showing Retry so a retry does not consume a second
     * file/object slot for the same selected document, and let what the
     * daemon answered decide whether Retry is safe at all:
     *
     * - Confirmed: the reservation is gone; a retry sends the file once.
     * - Committed: the daemon refused to release a file it already has (a
     *   dropped status poll or a lost PUT response hid the outcome). A retry
     *   would deliver it twice, so this attempt follows the file to its end
     *   instead. That polling may call fail() again; [cancelRemote]'s latch
     *   then answers Unconfirmed at once and [ActiveTransfer.committed] turns
     *   it into a non-retryable [FileTransferUiState.Unconfirmed]. The
     *   re-entry is bounded to that one extra level by design.
     * - Unconfirmed: retryable unless the daemon is known to hold the file,
     *   which is the case from the moment the PUT was accepted.
     */
    private suspend fun fail(transfer: ActiveTransfer, message: String) {
        val name = transfer.source.name
        when (val outcome = cancelRemote(transfer)) {
            RemoteCancellation.Confirmed -> publishTerminal(transfer, FileTransferUiState.Failed(name, message))
            is RemoteCancellation.Committed -> {
                transfer.committed = true
                if (isCurrent(transfer)) waitForDelivery(transfer, outcome.status)
            }
            RemoteCancellation.Unconfirmed ->
                if (transfer.committed) {
                    publishTerminal(transfer, FileTransferUiState.Unconfirmed(name, message))
                } else {
                    publishTerminal(transfer, FileTransferUiState.Failed(name, message))
                }
        }
    }

    /**
     * A terminal state ends the attempt. Clearing [active] keeps the next
     * upload()/close() from issuing DELETE+GET on a finished id, so this must
     * be the last thing a function does for that attempt.
     */
    private fun publishTerminal(transfer: ActiveTransfer, state: FileTransferUiState) {
        if (!isCurrent(transfer)) return
        transfer.finished = true
        active = null
        _state.value = state
    }

    internal companion object {
        const val STATUS_POLL_DELAY_MS = 250L
        const val MAX_STATUS_POLLS = 120
        const val MAX_CONSECUTIVE_STATUS_FAILURES = 3
        const val CANCEL_RETRY_DELAY_MS = 50L
        const val MAX_CANCEL_ATTEMPTS = 20

        /**
         * Not a hard bound: withTimeoutOrNull cannot interrupt a blocking
         * HttpURLConnection connect on the IO thread, so a release can run
         * on for up to one more HTTP connect timeout. Harmless: nothing
         * waits on a release.
         */
        const val RELEASE_TIMEOUT_MS = 5_000L
    }
}

private fun isTransferId(id: String): Boolean =
    id.length == 32 && id.all { it.isDigit() || it in 'a'..'f' }
