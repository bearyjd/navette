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

/**
 * A successfully uploaded descriptor that has not reached the media socket.
 * This is owned by SessionScreen (rather than a reconnect-scoped controller)
 * so a WebSocket rebuild cannot orphan the HTTP object before it is announced.
 */
internal class PendingClipboardBlobAnnouncement {
    private val lock = Any()
    private var descriptor: BlobDescriptor? = null

    fun remember(blob: BlobDescriptor) {
        synchronized(lock) { descriptor = blob }
    }

    fun retry(announce: (BlobDescriptor) -> Boolean) {
        synchronized(lock) {
            val pending = descriptor ?: return
            if (announce(pending)) descriptor = null
        }
    }

    fun clear() {
        synchronized(lock) { descriptor = null }
    }
}

internal class ClipboardBlobCoordinator(
    private val scope: CoroutineScope,
    private val transport: BlobTransport,
    private val pendingAnnouncement: PendingClipboardBlobAnnouncement = PendingClipboardBlobAnnouncement(),
    private val announce: (BlobDescriptor) -> Boolean,
) {
    private val lock = Any()
    private var generation = 0L

    fun upload(mime: String, bytes: ByteArray) {
        uploadIfCurrent(claim(), mime, bytes)
    }

    /** Claims the ordering slot before a caller begins slow local I/O. */
    fun claim(): Long = synchronized(lock) {
        pendingAnnouncement.clear()
        ++generation
    }

    /** Starts an upload only when its pre-I/O claim is still the newest value. */
    fun uploadIfCurrent(claim: Long, mime: String, bytes: ByteArray) {
        if (!isCurrent(claim)) return
        scope.launch {
            val descriptor = transport.upload(mime, bytes) ?: return@launch
            commitIfCurrent(claim) {
                if (!announce(descriptor)) pendingAnnouncement.remember(descriptor)
            }
        }
    }

    fun download(blob: BlobDescriptor, accept: (ByteArray, Long) -> Unit) {
        // A remote image is newer clipboard state than any locally uploaded
        // descriptor waiting for a reconnect. Claim through the same path as
        // local work so that stale descriptor cannot be announced later.
        val claim = claim()
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

    /**
     * Re-sends an already-uploaded descriptor after the media socket returns.
     * The HTTP object remains valid until a newer clipboard claim supersedes
     * it, so retrying only this lightweight ordered notification is enough.
     */
    fun retryPendingAnnouncement() {
        synchronized(lock) {
            pendingAnnouncement.retry(announce)
        }
    }

    private fun isCurrent(claim: Long): Boolean = synchronized(lock) { claim == generation }

    /** Call when this controller is discarded; no old clipboard state may cross into its replacement. */
    fun invalidate(clearPendingAnnouncement: Boolean = true) {
        synchronized(lock) {
            ++generation
            if (clearPendingAnnouncement) pendingAnnouncement.clear()
        }
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
