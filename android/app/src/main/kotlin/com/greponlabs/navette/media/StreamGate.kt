package com.greponlabs.navette.media

import android.util.Log
import com.greponlabs.navette.net.MediaDecodeException
import com.greponlabs.navette.net.MediaKind
import com.greponlabs.navette.net.MediaPacket
import com.greponlabs.navette.net.StreamConfig

/**
 * Largest coded dimension this client will hand to `MediaCodec`.
 *
 * The wire carries width/height as unsigned 32-bit, so a malformed or hostile
 * header can name a size no decoder could ever configure. H.264's highest
 * defined level tops out well below this, so anything above it is a malformed
 * header rather than a stream worth trying to decode.
 */
private const val MAX_CODED_DIMENSION = 16_384

/** The one stream this screen renders, plus the surface identity its input is addressed to. */
class PrimaryStream(
    val streamId: Long,
    val config: StreamConfig,
    val width: Int,
    val height: Int,
) {
    val clientId: Long get() = config.clientId
    val surfaceId: Long get() = config.surfaceId

    override fun equals(other: Any?): Boolean =
        this === other ||
            (
                other is PrimaryStream &&
                    streamId == other.streamId &&
                    config == other.config &&
                    width == other.width &&
                    height == other.height
            )

    override fun hashCode(): Int {
        var result = streamId.hashCode()
        result = 31 * result + config.hashCode()
        result = 31 * result + width
        result = 31 * result + height
        return result
    }

    override fun toString(): String =
        "PrimaryStream(streamId=$streamId, ${width}x$height, clientId=$clientId, surfaceId=$surfaceId)"
}

sealed interface StreamGateEvent {
    /** The first configuration for the adopted stream: build a decoder. */
    data class Bootstrap(val stream: PrimaryStream) : StreamGateEvent

    /** The adopted stream's configuration genuinely changed: rebuild the decoder. */
    data class Reconfigure(val stream: PrimaryStream) : StreamGateEvent

    /** An access unit for the adopted stream. */
    class Video(val accessUnit: ByteArray, val timestampUs: Long) : StreamGateEvent {
        override fun equals(other: Any?): Boolean =
            this === other ||
                (
                    other is Video &&
                        timestampUs == other.timestampUs &&
                        accessUnit.contentEquals(other.accessUnit)
                )

        override fun hashCode(): Int = 31 * accessUnit.contentHashCode() + timestampUs.hashCode()

        override fun toString(): String = "Video(${accessUnit.size} bytes, timestampUs=$timestampUs)"
    }

    /** The adopted stream's toplevel closed. */
    data object Ended : StreamGateEvent
}

/**
 * Picks one stream out of a session and ignores the rest.
 *
 * A deliberately simplified port of `crates/navette-viewer/src/router.rs`'s
 * `StreamRouter`: where that keeps a decoder per `stream_id` in a map, this
 * tracks at most one `stream_id` total -- the first one it sees a
 * `StreamConfig` for. Packets for any other stream are dropped. Multi-window
 * support is a later slice, not a gap here.
 *
 * Pure logic with no socket and no `MediaCodec`, exactly like the Rust
 * router, so the bootstrap/reconfigure/teardown behaviour can be driven from
 * fixture packets in a unit test.
 *
 * **One writer, many readers.** [handle] is called only from the coroutine
 * draining the media socket, in order, so nothing here needs mutual
 * exclusion. But [primary] is read from the main thread by every input path
 * -- touch, hardware key, IME -- so [current] is `@Volatile`: without it those
 * reads have no happens-before edge to the packet thread's write, and an
 * input event could see no adopted stream and silently do nothing (every
 * input path drops on a null primary rather than erroring, so the failure
 * would be invisible).
 */
class StreamGate {
    @Volatile
    private var current: PrimaryStream? = null

    /** Touched only from [handle], i.e. only ever on the one writer thread. */
    private var loggedForeignStream = false

    /** The adopted stream, or `null` before bootstrap and after [StreamGateEvent.Ended]. */
    val primary: PrimaryStream? get() = current

    fun handle(packet: MediaPacket): StreamGateEvent? =
        when (packet.header.kind) {
            MediaKind.STREAM_CONFIG -> configure(packet)
            MediaKind.VIDEO -> video(packet)
            MediaKind.STREAM_END -> end(packet)
            MediaKind.METRICS -> null
        }

    private fun configure(packet: MediaPacket): StreamGateEvent? {
        val streamId = packet.header.streamId
        val adopted = current
        if (adopted != null && adopted.streamId != streamId) {
            ignoreForeign(streamId)
            return null
        }

        val config =
            try {
                StreamConfig.decode(packet.payload)
            } catch (error: MediaDecodeException) {
                // Mirrors router.rs:135 -- a configuration this client cannot
                // parse leaves the stream exactly as it was.
                Log.w(TAG, "discarding malformed stream configuration for $streamId: ${error.error}")
                return null
            }
        val width = packet.header.width
        val height = packet.header.height
        if (width !in 1..MAX_CODED_DIMENSION || height !in 1..MAX_CODED_DIMENSION) {
            Log.w(TAG, "discarding stream configuration for $streamId with unusable size ${width}x$height")
            return null
        }

        val stream = PrimaryStream(streamId, config, width.toInt(), height.toInt())
        if (adopted == null) {
            current = stream
            return StreamGateEvent.Bootstrap(stream)
        }
        if (adopted == stream) {
            // The hub replays the latest configuration on every attach
            // (router.rs:151); rebuilding the decoder for it would cost a
            // black flash for no change at all.
            return null
        }
        current = stream
        return StreamGateEvent.Reconfigure(stream)
    }

    private fun video(packet: MediaPacket): StreamGateEvent? {
        val adopted = current ?: return null
        if (adopted.streamId != packet.header.streamId) {
            ignoreForeign(packet.header.streamId)
            return null
        }
        if (packet.payload.isEmpty()) {
            // Malformed rather than fatal, matching router.rs:193-197: an
            // empty access unit says nothing about the decoder.
            Log.w(TAG, "dropping video packet with an empty payload")
            return null
        }
        return StreamGateEvent.Video(packet.payload, packet.header.timestampUs)
    }

    private fun end(packet: MediaPacket): StreamGateEvent? {
        val adopted = current ?: return null
        if (adopted.streamId != packet.header.streamId) {
            ignoreForeign(packet.header.streamId)
            return null
        }
        current = null
        return StreamGateEvent.Ended
    }

    /** Logged once, not per packet: a busy second stream would otherwise flood the log. */
    private fun ignoreForeign(streamId: Long) {
        if (loggedForeignStream) return
        loggedForeignStream = true
        Log.i(TAG, "ignoring stream $streamId; this screen renders only the primary stream")
    }

    private companion object {
        const val TAG = "StreamGate"
    }
}
