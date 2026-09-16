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
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.serialization.Serializable

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

/** Injectable HTTP boundary; the coordinator is fully JVM-testable without Android networking. */
internal interface FileTransferTransport {
    suspend fun preflight(source: FileTransferSource): FilePreflightResponse?

    suspend fun upload(
        preflight: FilePreflightResponse,
        source: FileTransferSource,
        onProgress: (Long) -> Unit,
    ): FileTransferStatus?

    suspend fun status(transferId: String): FileTransferStatus?

    suspend fun cancel(transferId: String): Boolean

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

    override suspend fun cancel(transferId: String): Boolean =
        withContext(Dispatchers.IO) {
            if (!isTransferId(transferId)) return@withContext false
            request("DELETE", "$collectionUrl/$transferId") {
                responseCode == HttpURLConnection.HTTP_NO_CONTENT
            } ?: false
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
    private var active: ActiveTransfer? = null

    /**
     * The id belongs to this attempt, not the coordinator. A prior,
     * non-cancellable provider/HTTP call can finish after replacement; it
     * must only ever clean up its own daemon reservation.
     */
    private class ActiveTransfer(val source: FileTransferSource) {
        var transferId: String? = null
        var work: Job? = null
        var remoteCancellationStarted = false
    }

    fun upload(next: FileTransferSource) {
        if (!isSafeFileTransferMetadata(next.name, next.mime, next.size)) {
            _state.value = FileTransferUiState.Failed(next.name, "This file cannot be sent")
            return
        }
        stopActive()
        retrySource = next
        ActiveTransfer(next).also { transfer ->
            active = transfer
            transfer.work = scope.launch { uploadCurrent(transfer) }
        }
    }

    fun retry() {
        val retained = retrySource ?: return
        upload(retained)
    }

    fun cancel() {
        val transfer = active ?: return
        active = null
        stop(transfer)
        scope.launch {
            cancelRemote(transfer)
            // A replacement may have started while the daemon cancellation
            // was in flight; its state must win over this old attempt.
            if (active == null) _state.value = FileTransferUiState.Cancelled(transfer.source.name)
        }
    }

    fun close() {
        active?.let { transfer ->
            active = null
            stop(transfer)
            scope.launch { cancelRemote(transfer) }
        }
    }

    private fun stopActive() {
        active?.let { transfer ->
            active = null
            stop(transfer)
            scope.launch { cancelRemote(transfer) }
        }
    }

    private fun stop(transfer: ActiveTransfer) {
        // HttpURLConnection is not coroutine-cancellable while it is blocked
        // in a provider/socket read. Disconnect before cancelling its Job;
        // this lets the server drop its writer before the DELETE below.
        transport.cancelActiveUpload()
        transfer.work?.cancel()
    }

    private suspend fun cancelRemote(transfer: ActiveTransfer) {
        val id = transfer.transferId ?: return
        if (transfer.remoteCancellationStarted) return
        transfer.remoteCancellationStarted = true
        repeat(MAX_CANCEL_ATTEMPTS) { attempt ->
            try {
                if (transport.cancel(id)) return
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (_: Exception) {
                // The next short retry covers a close racing server teardown.
            }
            if (attempt + 1 < MAX_CANCEL_ATTEMPTS) delay(CANCEL_RETRY_DELAY_MS)
        }
        // Server expiry remains the backstop if the network is down.
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
            val accepted = transport.upload(preflight, current) { sent ->
                if (isCurrent(transfer)) {
                    _state.value = FileTransferUiState.Uploading(current.name, sent, current.size)
                }
            } ?: return fail(transfer, "Upload failed")
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
        repeat(MAX_STATUS_POLLS) {
            if (!isCurrent(transfer)) {
                cancelRemote(transfer)
                return
            }
            when (status.state) {
                FileTransferServerState.Delivered -> {
                    if (isCurrent(transfer)) _state.value = FileTransferUiState.Delivered(current.name, current.size)
                    return
                }
                FileTransferServerState.Cancelled -> {
                    if (isCurrent(transfer)) _state.value = FileTransferUiState.Cancelled(current.name)
                    return
                }
                FileTransferServerState.Failed -> return fail(transfer, "Guest could not receive file")
                else -> if (isCurrent(transfer)) {
                    _state.value = FileTransferUiState.WaitingForGuest(status.transfer_id, current.name, current.size)
                }
            }
            delay(STATUS_POLL_DELAY_MS)
            status = transport.status(status.transfer_id) ?: return fail(transfer, "Could not confirm transfer")
        }
        fail(transfer, "Guest delivery timed out")
    }

    private suspend fun fail(transfer: ActiveTransfer, message: String) {
        val current = transfer.source
        // A failed PUT can leave a daemon-side AwaitingUpload reservation.
        // Release it before showing Retry so a retry does not consume a second
        // file/object slot for the same selected document.
        cancelRemote(transfer)
        if (isCurrent(transfer)) _state.value = FileTransferUiState.Failed(current.name, message)
    }

    private companion object {
        const val STATUS_POLL_DELAY_MS = 250L
        const val MAX_STATUS_POLLS = 120
        const val CANCEL_RETRY_DELAY_MS = 50L
        const val MAX_CANCEL_ATTEMPTS = 20
    }
}

private fun isTransferId(id: String): Boolean =
    id.length == 32 && id.all { it.isDigit() || it in 'a'..'f' }
