package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.MediaFlags
import com.greponlabs.navette.net.MediaHeader
import com.greponlabs.navette.net.MediaKind
import com.greponlabs.navette.net.MediaPacket
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class SessionHudTest {
    /** The two streams a guest with two toplevel windows puts on one socket. */
    private val A = 1L
    private val B = 2L

    private fun packet(
        kind: MediaKind = MediaKind.VIDEO,
        sequence: Long,
        payload: Int = 0,
        discontinuity: Boolean = false,
        streamId: Long = 1,
    ) = MediaPacket(
        MediaHeader(
            kind = kind,
            flags = MediaFlags.of(keyframe = false, discontinuity = discontinuity),
            streamId = streamId,
            sequence = sequence,
            timestampUs = 0,
            payloadLen = payload.toLong(),
            width = 1920,
            height = 1080,
        ),
        ByteArray(payload),
    )

    @Test
    fun `frames outside the one-second window stop counting`() {
        val hud = SessionHud()
        repeat(10) { hud.recordPresented(timestampUs = it.toLong(), nowMs = 1000L + it * 100L) }
        // Ten frames spread over 900ms, sampled at the last one.
        val fps = hud.sample(1900L, streamId = 1).fps
        assertTrue("expected ~10fps, got $fps", fps > 9.0)
        // Three seconds later every one has aged out.
        assertEquals(0.0, hud.sample(4900L, streamId = 1).fps, 0.0)
    }

    @Test
    fun `a frame presented and sampled in the same millisecond reports no rate rather than a huge one`() {
        val hud = SessionHud()
        hud.recordPresented(timestampUs = 1L, nowMs = 1000L)
        // Zero elapsed: there is no span to divide by. hud.rs returns 0.0 here
        // (hud.rs:174-176) and a real clock hits this constantly, so dividing by
        // a floored 1ms would put "FPS 1000.0" on the overlay.
        assertEquals(0.0, hud.sample(1000L, streamId = 1).fps, 0.0)
    }

    @Test
    fun `bitrate counts only video payload inside the window`() {
        val hud = SessionHud()
        hud.recordPacket(1000L, packet(sequence = 1, payload = 1000))
        hud.recordPacket(1500L, packet(kind = MediaKind.METRICS, sequence = 2, payload = 9_000_000))
        // 1000 bytes = 8000 bits. Sampled at exactly 1000ms after the only
        // video packet, so the rate is over a full second and the expected
        // value is exact: rates divide by elapsed-since-oldest, as hud.rs's
        // own rate() does, not by the nominal window.
        assertEquals(8000.0, hud.sample(2000L, streamId = 1).bitrateBps, 1.0)
    }

    @Test
    fun `the first three packets only establish a sequence baseline`() {
        val hud = SessionHud()
        // Mirrors the attach replay: a config and a keyframe at their original
        // sequence numbers, then live traffic resuming much later. That jump is
        // history this client was never sent, not a drop.
        hud.recordPacket(1000L, packet(kind = MediaKind.STREAM_CONFIG, sequence = 2))
        hud.recordPacket(1001L, packet(sequence = 3))
        hud.recordPacket(1002L, packet(sequence = 900))
        assertEquals(0L, hud.sample(1002L, streamId = 1).droppedPackets)
    }

    @Test
    fun `a sequence gap after the baseline counts as dropped packets`() {
        val hud = SessionHud()
        listOf(1L, 2L, 3L, 4L).forEach { hud.recordPacket(1000L, packet(sequence = it)) }
        hud.recordPacket(1001L, packet(sequence = 8))
        // 5, 6 and 7 never arrived.
        assertEquals(3L, hud.sample(1001L, streamId = 1).droppedPackets)
    }

    @Test
    fun `a repeated sequence is a replay, not a drop and not a rewind`() {
        val hud = SessionHud()
        listOf(1L, 2L, 3L, 4L, 5L).forEach { hud.recordPacket(1000L, packet(sequence = it)) }
        hud.recordPacket(1001L, packet(sequence = 3))
        hud.recordPacket(1002L, packet(sequence = 6))
        assertEquals(0L, hud.sample(1002L, streamId = 1).droppedPackets)
    }

    @Test
    fun `discontinuity flags accumulate`() {
        val hud = SessionHud()
        hud.recordPacket(1000L, packet(sequence = 1, discontinuity = true))
        hud.recordPacket(1001L, packet(sequence = 2))
        hud.recordPacket(1002L, packet(sequence = 3, discontinuity = true))
        assertEquals(2L, hud.sample(1002L, streamId = 1).discontinuities)
    }

    @Test
    fun `decode time is feed to presentation of the same access unit`() {
        val hud = SessionHud()
        hud.recordFed(timestampUs = 500L, nowMs = 1000L)
        hud.recordPresented(timestampUs = 500L, nowMs = 1012L)
        assertEquals(12.0, hud.sample(1012L, streamId = 1).decodeMs)
    }

    @Test
    fun `a presentation with no matching feed leaves decode time alone`() {
        val hud = SessionHud()
        hud.recordFed(timestampUs = 500L, nowMs = 1000L)
        hud.recordPresented(timestampUs = 500L, nowMs = 1012L)
        hud.recordPresented(timestampUs = 999L, nowMs = 1030L)
        assertEquals(
            "an unmatched presentation must not overwrite a real reading",
            12.0,
            hud.sample(1030L, streamId = 1).decodeMs,
        )
    }

    @Test
    fun `frame age grows until the next frame arrives`() {
        val hud = SessionHud()
        hud.recordPresented(timestampUs = 1L, nowMs = 1000L)
        assertEquals(500L, hud.sample(1500L, streamId = 1).ageMs)
        hud.recordPresented(timestampUs = 2L, nowMs = 1600L)
        assertEquals(0L, hud.sample(1600L, streamId = 1).ageMs)
    }

    @Test
    fun `age and decode time are null before anything has been presented`() {
        val hud = SessionHud()
        assertNull(hud.sample(1000L, streamId = 1).ageMs)
        assertNull(hud.sample(1000L, streamId = 1).decodeMs)
    }

    @Test
    fun `rtt is the round trip of a matched nonce`() {
        val hud = SessionHud()
        hud.recordPing(nonce = 1uL, nowMs = 1000L)
        hud.recordPong(nonce = 1uL, nowMs = 1043L)
        assertEquals(43L, hud.sample(1043L, streamId = 1).rttMs)
    }

    @Test
    fun `a pong for a superseded nonce is discarded, not misattributed`() {
        val hud = SessionHud()
        hud.recordPing(nonce = 1uL, nowMs = 1000L)
        hud.recordPing(nonce = 2uL, nowMs = 2000L)
        // The first ping's answer finally turns up after its successor went out.
        hud.recordPong(nonce = 1uL, nowMs = 2100L)
        assertNull("a stale nonce must not be timed against the live ping", hud.sample(2100L, streamId = 1).rttMs)
        hud.recordPong(nonce = 2uL, nowMs = 2110L)
        assertEquals(110L, hud.sample(2110L, streamId = 1).rttMs)
    }

    @Test
    fun `an unanswered ping leaves rtt blank rather than growing without bound`() {
        val hud = SessionHud()
        hud.recordPing(nonce = 1uL, nowMs = 1000L)
        assertNull(hud.sample(60_000L, streamId = 1).rttMs)
    }

    @Test
    fun `a stale rtt reading expires rather than being shown as live`() {
        val hud = SessionHud()
        hud.recordPing(nonce = 1uL, nowMs = 1000L)
        hud.recordPong(nonce = 1uL, nowMs = 1020L)
        assertEquals(20L, hud.sample(1020L, streamId = 1).rttMs)
        // Five seconds on with nothing answered, the last reading is history.
        assertNull(hud.sample(6100L, streamId = 1).rttMs)
    }

    /**
     * Drives both streams past their own baselines and leaves them
     * interleaved, at the divergent sequence positions a guest with two
     * toplevel windows really produces.
     */
    private fun twoInterleavedStreams(hud: SessionHud) {
        // B is replayed first and sits at sequence ~10; A joins at ~4000.
        listOf(10L, 11L, 12L).forEach { hud.recordPacket(1000L, packet(sequence = it, streamId = B)) }
        listOf(4000L, 4001L, 4002L).forEach { hud.recordPacket(1000L, packet(sequence = it, streamId = A)) }
        hud.recordPacket(1001L, packet(sequence = 13, streamId = B))
        hud.recordPacket(1001L, packet(sequence = 4003, streamId = A))
        hud.recordPacket(1002L, packet(sequence = 14, streamId = B))
        hud.recordPacket(1002L, packet(sequence = 4004, streamId = A))
    }

    @Test
    fun `interleaved streams at divergent sequence positions manufacture no drops`() {
        val hud = SessionHud()
        twoInterleavedStreams(hud)
        // Sequence numbering is per-stream (bridge.rs:1177-1181), so A's
        // position says nothing about B's and the distance between them is
        // history, not loss. One shared cursor read every hop between the two
        // as a gap of thousands.
        assertEquals(0L, hud.sample(1002L, streamId = A).droppedPackets)
        assertEquals(0L, hud.sample(1002L, streamId = B).droppedPackets)
    }

    @Test
    fun `each stream establishes its own baseline`() {
        val hud = SessionHud()
        // Each gets the attach replay -- config and keyframe at their original
        // sequence numbers -- plus one live packet resuming much later.
        hud.recordPacket(1000L, packet(kind = MediaKind.STREAM_CONFIG, sequence = 2, streamId = A))
        hud.recordPacket(1000L, packet(kind = MediaKind.STREAM_CONFIG, sequence = 7, streamId = B))
        hud.recordPacket(1001L, packet(sequence = 3, streamId = A))
        hud.recordPacket(1001L, packet(sequence = 8, streamId = B))
        hud.recordPacket(1002L, packet(sequence = 900, streamId = A))
        hud.recordPacket(1002L, packet(sequence = 640, streamId = B))
        // BASELINE_PACKETS = 3 was always right per stream; it only failed
        // because the count that consumed it was shared.
        assertEquals(0L, hud.sample(1002L, streamId = A).droppedPackets)
        assertEquals(0L, hud.sample(1002L, streamId = B).droppedPackets)
    }

    @Test
    fun `a genuine gap is still counted, against the stream that dropped it`() {
        val hud = SessionHud()
        twoInterleavedStreams(hud)
        // 4005 and 4006 never arrived on A. B carries on cleanly.
        hud.recordPacket(1003L, packet(sequence = 4007, streamId = A))
        hud.recordPacket(1003L, packet(sequence = 15, streamId = B))
        assertEquals(2L, hud.sample(1003L, streamId = A).droppedPackets)
        assertEquals(0L, hud.sample(1003L, streamId = B).droppedPackets)
    }

    @Test
    fun `bitrate counts only the sampled stream's payload`() {
        val hud = SessionHud()
        hud.recordPacket(1000L, packet(sequence = 1, payload = 1000, streamId = A))
        hud.recordPacket(1000L, packet(sequence = 1, payload = 500_000, streamId = B))
        // 1000 bytes = 8000 bits over a full second. B's much larger payload
        // is on the same socket but not on the surface.
        assertEquals(8000.0, hud.sample(2000L, streamId = A).bitrateBps, 1.0)
        assertEquals(4_000_000.0, hud.sample(2000L, streamId = B).bitrateBps, 1.0)
    }

    @Test
    fun `discontinuity flags count only against the stream that carried them`() {
        val hud = SessionHud()
        hud.recordPacket(1000L, packet(sequence = 1, streamId = A))
        hud.recordPacket(1001L, packet(sequence = 1, discontinuity = true, streamId = B))
        hud.recordPacket(1002L, packet(sequence = 2, discontinuity = true, streamId = B))
        assertEquals(0L, hud.sample(1002L, streamId = A).discontinuities)
        assertEquals(2L, hud.sample(1002L, streamId = B).discontinuities)
    }

    @Test
    fun `a stream ending evicts its counters and leaves the others alone`() {
        val hud = SessionHud()
        twoInterleavedStreams(hud)
        // Payload, so the post-eviction bitrate below can only be zero because
        // the counters went away -- rate() reports 0.0 for a zero total too.
        hud.recordPacket(1003L, packet(sequence = 4007, payload = 1000, streamId = A))
        assertEquals(2L, hud.sample(1003L, streamId = A).droppedPackets)
        assertTrue(hud.sample(1003L, streamId = A).bitrateBps > 0.0)
        // The guest window closed. Mirrors the eviction at session.rs:258.
        hud.recordPacket(1004L, packet(kind = MediaKind.STREAM_END, sequence = 4008, streamId = A))
        assertEquals(0L, hud.sample(1004L, streamId = A).droppedPackets)
        assertEquals(0.0, hud.sample(1004L, streamId = A).bitrateBps, 0.0)
        assertEquals(0L, hud.sample(1004L, streamId = B).droppedPackets)
        // B's own state survived: it is still past its baseline, so a real gap
        // on B is still audited rather than swallowed as a fresh baseline.
        hud.recordPacket(1005L, packet(sequence = 17, streamId = B))
        assertEquals(2L, hud.sample(1005L, streamId = B).droppedPackets)
    }

    @Test
    fun `no rendered stream reports blanks rather than another stream's figures`() {
        val hud = SessionHud()
        twoInterleavedStreams(hud)
        hud.recordPacket(1003L, packet(sequence = 4007, payload = 1000, streamId = A))
        // Before bootstrap and after Ended, gate.primary is null. The overlay
        // must not then be handed whatever the socket last carried.
        val blank = hud.sample(1003L, streamId = null)
        assertEquals(0L, blank.droppedPackets)
        assertEquals(0L, blank.discontinuities)
        assertEquals(0.0, blank.bitrateBps, 0.0)
        // An unknown stream is the same case.
        assertEquals(0L, hud.sample(1003L, streamId = 99).droppedPackets)
    }

    @Test
    fun `the formatted line shows every field and blanks what it lacks`() {
        val sample = HudSample(
            fps = 29.94,
            bitrateBps = 2_500_000.0,
            decodeMs = 4.2,
            ageMs = 33L,
            rttMs = null,
            droppedPackets = 3,
            discontinuities = 1,
        )
        assertEquals("FPS 29.9  KBPS 2500  DEC 4.2MS  AGE 33MS  RTT --  DROP 3  DISC 1", sample.format())
    }
}
