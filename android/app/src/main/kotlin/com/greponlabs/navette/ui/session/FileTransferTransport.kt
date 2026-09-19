package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.fileTransferCollectionUrl
import com.greponlabs.navette.net.mediaJson
import java.net.HttpURLConnection
import java.net.URL
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.Serializable

internal const val FILE_TRANSFER_BUFFER_BYTES: Int = 64 * 1024

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

private fun isTransferId(id: String): Boolean =
    id.length == 32 && id.all { it.isDigit() || it in 'a'..'f' }
