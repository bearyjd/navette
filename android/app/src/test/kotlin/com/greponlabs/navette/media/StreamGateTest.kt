package com.greponlabs.navette.media

import com.greponlabs.navette.net.MediaFlags
import com.greponlabs.navette.net.MediaHeader
import com.greponlabs.navette.net.MediaKind
import com.greponlabs.navette.net.MediaPacket
import com.greponlabs.navette.net.StreamConfig
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Scenario names are ported from `crates/navette-viewer/src/router.rs`'s own
 * test module wherever the case survives the single-stream simplification, so
 * the two sides stay readable against each other.
 */
class StreamGateTest {
    private val codecConfig = byteArrayOf(0, 0, 0, 1, 0x67)
    private val otherCodecConfig = byteArrayOf(0, 0, 0, 1, 0x67, 2)

    private fun packet(
        kind: MediaKind,
        streamId: Long,
        sequence: Long,
        payload: ByteArray,
        width: Long = 4,
        height: Long = 2,
    ) = MediaPacket.of(
        MediaHeader(
            kind = kind,
            flags = MediaFlags.of(keyframe = kind == MediaKind.VIDEO, discontinuity = false),
            streamId = streamId,
            sequence = sequence,
            timestampUs = sequence * 1000,
            payloadLen = 0,
            width = width,
            height = height,
        ),
        payload,
    )

    private fun streamConfig(
        streamId: Long,
        sequence: Long,
        clientId: Long = 11,
        surfaceId: Long = 12,
        codec: ByteArray = codecConfig,
        width: Long = 4,
        height: Long = 2,
    ) = packet(
        MediaKind.STREAM_CONFIG,
        streamId,
        sequence,
        StreamConfig(clientId, surfaceId, codec).encode(),
        width,
        height,
    )

    private fun video(streamId: Long, sequence: Long) =
        packet(MediaKind.VIDEO, streamId, sequence, byteArrayOf(0, 0, 0, 1, 0x65, sequence.toByte()))

    @Test
    fun `stream config bootstraps decoding and carries surface identity`() {
        val gate = StreamGate()

        val event = gate.handle(streamConfig(streamId = 7, sequence = 1))

        assertTrue(event is StreamGateEvent.Bootstrap)
        val stream = (event as StreamGateEvent.Bootstrap).stream
        assertEquals(7L, stream.streamId)
        assertEquals(11L, stream.clientId)
        assertEquals(12L, stream.surfaceId)
        assertEquals(4, stream.width)
        assertEquals(2, stream.height)
        assertEquals(stream, gate.primary)
    }

    @Test
    fun `video for the primary stream is forwarded with its timestamp`() {
        val gate = StreamGate()
        gate.handle(streamConfig(streamId = 7, sequence = 1))

        val event = gate.handle(video(streamId = 7, sequence = 2))

        assertTrue(event is StreamGateEvent.Video)
        assertEquals(2_000L, (event as StreamGateEvent.Video).timestampUs)
        assertEquals(6, event.accessUnit.size)
    }

    @Test
    fun `video without configuration is dropped`() {
        assertNull(StreamGate().handle(video(streamId = 7, sequence = 1)))
    }

    @Test
    fun `an empty video payload is dropped without disturbing the primary`() {
        val gate = StreamGate()
        gate.handle(streamConfig(streamId = 7, sequence = 1))

        assertNull(gate.handle(packet(MediaKind.VIDEO, 7, 2, ByteArray(0))))
        assertEquals(7L, gate.primary?.streamId)
    }

    /**
     * The single-stream form of `concurrent_streams_keep_independent_decoder_
     * state`: a second toplevel is silently ignored rather than fighting the
     * first for the one surface this screen has.
     */
    @Test
    fun `a second concurrent stream is ignored entirely`() {
        val gate = StreamGate()
        gate.handle(streamConfig(streamId = 1, sequence = 1))

        assertNull(gate.handle(streamConfig(streamId = 2, sequence = 1, clientId = 21, surfaceId = 22)))
        assertNull(gate.handle(video(streamId = 2, sequence = 2)))
        assertNull(gate.handle(packet(MediaKind.STREAM_END, 2, 3, ByteArray(0))))

        assertEquals(1L, gate.primary?.streamId)
        assertEquals(11L, gate.primary?.clientId)
        assertTrue(gate.handle(video(streamId = 1, sequence = 4)) is StreamGateEvent.Video)
    }

    @Test
    fun `an unchanged configuration replay is a no-op`() {
        val gate = StreamGate()
        gate.handle(streamConfig(streamId = 1, sequence = 1))

        // The hub replays the latest configuration on every attach; treating
        // that as a reconfigure would tear the decoder down for no change.
        assertNull(gate.handle(streamConfig(streamId = 1, sequence = 2)))
    }

    @Test
    fun `a new codec configuration reconfigures the primary stream`() {
        val gate = StreamGate()
        gate.handle(streamConfig(streamId = 1, sequence = 1))

        val event = gate.handle(streamConfig(streamId = 1, sequence = 2, codec = otherCodecConfig))

        assertTrue(event is StreamGateEvent.Reconfigure)
        assertTrue((event as StreamGateEvent.Reconfigure).stream.config.codecConfig.contentEquals(otherCodecConfig))
    }

    /**
     * The identical `codec_config` is reused deliberately: only the coded
     * dimensions differ, so this fails unless the gate compares width/height
     * as well -- the Kotlin form of `router.rs`'s
     * `a_resized_stream_configuration_resets_that_stream_decoder`.
     */
    @Test
    fun `a resized stream configuration reconfigures the primary stream`() {
        val gate = StreamGate()
        gate.handle(streamConfig(streamId = 1, sequence = 1, width = 4, height = 2))

        val event = gate.handle(streamConfig(streamId = 1, sequence = 2, width = 4, height = 6))

        assertTrue("a resize-only change must not be mistaken for a replay", event is StreamGateEvent.Reconfigure)
        assertEquals(4, (event as StreamGateEvent.Reconfigure).stream.width)
        assertEquals(6, event.stream.height)
    }

    @Test
    fun `a new surface identity reconfigures the primary stream`() {
        val gate = StreamGate()
        gate.handle(streamConfig(streamId = 1, sequence = 1, clientId = 11, surfaceId = 12))

        val event = gate.handle(streamConfig(streamId = 1, sequence = 2, clientId = 11, surfaceId = 99))

        assertTrue(event is StreamGateEvent.Reconfigure)
        assertEquals(99L, (event as StreamGateEvent.Reconfigure).stream.surfaceId)
    }

    @Test
    fun `stream end clears the primary and later video is dropped`() {
        val gate = StreamGate()
        gate.handle(streamConfig(streamId = 1, sequence = 1))

        assertEquals(StreamGateEvent.Ended, gate.handle(packet(MediaKind.STREAM_END, 1, 2, ByteArray(0))))
        assertNull(gate.primary)
        assertNull(gate.handle(video(streamId = 1, sequence = 3)))
        // A second end for a stream that is already gone is not an event.
        assertNull(gate.handle(packet(MediaKind.STREAM_END, 1, 4, ByteArray(0))))
    }

    @Test
    fun `a malformed stream configuration leaves the gate unconfigured`() {
        val gate = StreamGate()

        assertNull(gate.handle(packet(MediaKind.STREAM_CONFIG, 1, 1, byteArrayOf(9, 9, 9))))

        assertNull(gate.primary)
        assertNull(gate.handle(video(streamId = 1, sequence = 2)))
    }

    @Test
    fun `a stream configuration with an unusable coded size is discarded`() {
        val gate = StreamGate()

        assertNull(gate.handle(streamConfig(streamId = 1, sequence = 1, width = 0, height = 720)))
        // 0xFFFFFFFF: what an unsigned header field looks like at its extreme.
        assertNull(gate.handle(streamConfig(streamId = 1, sequence = 2, width = 0xFFFFFFFFL, height = 720)))

        assertNull(gate.primary)
    }

    @Test
    fun `metrics packets are ignored`() {
        val gate = StreamGate()
        gate.handle(streamConfig(streamId = 1, sequence = 1))

        assertNull(gate.handle(packet(MediaKind.METRICS, 1, 2, byteArrayOf(1, 2, 3))))
        assertEquals(1L, gate.primary?.streamId)
    }

    @Test
    fun `a fresh stream can be adopted after the first one ends`() {
        val gate = StreamGate()
        gate.handle(streamConfig(streamId = 1, sequence = 1))
        gate.handle(packet(MediaKind.STREAM_END, 1, 2, ByteArray(0)))

        val event = gate.handle(streamConfig(streamId = 5, sequence = 3, clientId = 51, surfaceId = 52))

        assertTrue(event is StreamGateEvent.Bootstrap)
        assertEquals(5L, gate.primary?.streamId)
    }
}
