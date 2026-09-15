package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.BlobDescriptor
import com.greponlabs.navette.net.MAX_BLOB_BYTES
import com.greponlabs.navette.net.mediaJson
import com.greponlabs.navette.net.validate
import java.io.ByteArrayOutputStream
import java.net.HttpURLConnection
import java.net.URL
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * Coordinates asynchronous HTTP blob work with the media socket's ordered
 * clipboard state. A single generation covers uploads and downloads: a newer
 * local or remote clipboard operation makes every older completion stale.
 */
internal interface BlobTransport {
    suspend fun upload(mime: String, bytes: ByteArray): BlobDescriptor?

    suspend fun download(blob: BlobDescriptor): ByteArray?
}

internal class ClipboardBlobCoordinator(
    private val scope: CoroutineScope,
    private val transport: BlobTransport,
    private val announce: (BlobDescriptor) -> Unit,
) {
    private val lock = Any()
    private var generation = 0L

    fun upload(mime: String, bytes: ByteArray) {
        val claim = synchronized(lock) { ++generation }
        scope.launch {
            val descriptor = transport.upload(mime, bytes) ?: return@launch
            commitIfCurrent(claim) { announce(descriptor) }
        }
    }

    fun download(blob: BlobDescriptor, accept: (ByteArray, Long) -> Unit) {
        val claim = synchronized(lock) { ++generation }
        scope.launch {
            val bytes = transport.download(blob) ?: return@launch
            commitIfCurrent(claim) { accept(bytes, claim) }
        }
    }

    /** Runs the externally visible effect while invalidation is excluded. */
    fun commitIfCurrent(claim: Long, effect: () -> Unit) {
        synchronized(lock) {
            if (claim == generation) effect()
        }
    }

    /** Call whenever a media connection is rebuilt; clipboard blobs do not replay. */
    fun invalidate() {
        synchronized(lock) { ++generation }
    }
}

/** Authenticated HTTP implementation; media messages carry descriptors only. */
internal class HttpBlobTransport(
    mediaUrl: String,
    private val token: String,
) : BlobTransport {
    private val collectionUrl: String =
        mediaUrl
            .replaceFirst("ws://", "http://")
            .replaceFirst("wss://", "https://")
            .removeSuffix("/media") + "/blobs"

    override suspend fun upload(mime: String, bytes: ByteArray): BlobDescriptor? =
        withContext(Dispatchers.IO) {
            if (bytes.isEmpty() || bytes.size.toLong() > MAX_BLOB_BYTES) return@withContext null
            runCatching {
                val connection = (URL(collectionUrl).openConnection() as HttpURLConnection)
                connection.requestMethod = "POST"
                connection.setRequestProperty("Authorization", "Bearer $token")
                connection.setRequestProperty("Content-Type", mime)
                connection.doOutput = true
                connection.outputStream.use { it.write(bytes) }
                if (connection.responseCode != HttpURLConnection.HTTP_CREATED) return@runCatching null
                connection.inputStream.bufferedReader().use {
                    mediaJson.decodeFromString(BlobDescriptor.serializer(), it.readText())
                }?.takeIf { it.validate() == null }
            }.getOrNull()
        }

    override suspend fun download(blob: BlobDescriptor): ByteArray? =
        withContext(Dispatchers.IO) {
            if (blob.validate() != null) return@withContext null
            runCatching {
                val connection =
                    (URL(collectionUrl + "/" + blob.id).openConnection() as HttpURLConnection)
                connection.setRequestProperty("Authorization", "Bearer $token")
                if (connection.responseCode != HttpURLConnection.HTTP_OK ||
                    connection.contentType != blob.mime
                ) {
                    return@runCatching null
                }
                connection.inputStream.use { input ->
                    val output = ByteArrayOutputStream(blob.size.toInt())
                    val buffer = ByteArray(DEFAULT_BUFFER_SIZE)
                    while (true) {
                        val count = input.read(buffer)
                        if (count < 0) break
                        if (output.size().toLong() + count > MAX_BLOB_BYTES) return@runCatching null
                        output.write(buffer, 0, count)
                    }
                    output.toByteArray().takeIf { it.size.toLong() == blob.size }
                }
            }.getOrNull()
        }
}
