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
    private fun packet(
        kind: MediaKind = MediaKind.VIDEO,
        sequence: Long,
        payload: Int = 0,
        discontinuity: Boolean = false,
    ) = MediaPacket(
        MediaHeader(
            kind = kind,
            flags = MediaFlags.of(keyframe = false, discontinuity = discontinuity),
            streamId = 1,
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
        assertTrue("expected ~10fps, got ${hud.sample(1900L).fps}", hud.sample(1900L).fps > 9.0)
        // Three seconds later every one has aged out.
        assertEquals(0.0, hud.sample(4900L).fps, 0.0)
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
        assertEquals(8000.0, hud.sample(2000L).bitrateBps, 1.0)
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
        assertEquals(0L, hud.sample(1002L).droppedPackets)
    }

    @Test
    fun `a sequence gap after the baseline counts as dropped packets`() {
        val hud = SessionHud()
        listOf(1L, 2L, 3L, 4L).forEach { hud.recordPacket(1000L, packet(sequence = it)) }
        hud.recordPacket(1001L, packet(sequence = 8))
        // 5, 6 and 7 never arrived.
        assertEquals(3L, hud.sample(1001L).droppedPackets)
    }

    @Test
    fun `a repeated sequence is a replay, not a drop and not a rewind`() {
        val hud = SessionHud()
        listOf(1L, 2L, 3L, 4L, 5L).forEach { hud.recordPacket(1000L, packet(sequence = it)) }
        hud.recordPacket(1001L, packet(sequence = 3))
        hud.recordPacket(1002L, packet(sequence = 6))
        assertEquals(0L, hud.sample(1002L).droppedPackets)
    }

    @Test
    fun `discontinuity flags accumulate`() {
        val hud = SessionHud()
        hud.recordPacket(1000L, packet(sequence = 1, discontinuity = true))
        hud.recordPacket(1001L, packet(sequence = 2))
        hud.recordPacket(1002L, packet(sequence = 3, discontinuity = true))
        assertEquals(2L, hud.sample(1002L).discontinuities)
    }

    @Test
    fun `decode time is feed to presentation of the same access unit`() {
        val hud = SessionHud()
        hud.recordFed(timestampUs = 500L, nowMs = 1000L)
        hud.recordPresented(timestampUs = 500L, nowMs = 1012L)
        assertEquals(12.0, hud.sample(1012L).decodeMs)
    }

    @Test
    fun `a presentation with no matching feed leaves decode time alone`() {
        val hud = SessionHud()
        hud.recordFed(timestampUs = 500L, nowMs = 1000L)
        hud.recordPresented(timestampUs = 500L, nowMs = 1012L)
        hud.recordPresented(timestampUs = 999L, nowMs = 1030L)
        assertEquals("an unmatched presentation must not overwrite a real reading", 12.0, hud.sample(1030L).decodeMs)
    }

    @Test
    fun `frame age grows until the next frame arrives`() {
        val hud = SessionHud()
        hud.recordPresented(timestampUs = 1L, nowMs = 1000L)
        assertEquals(500L, hud.sample(1500L).ageMs)
        hud.recordPresented(timestampUs = 2L, nowMs = 1600L)
        assertEquals(0L, hud.sample(1600L).ageMs)
    }

    @Test
    fun `age and decode time are null before anything has been presented`() {
        val hud = SessionHud()
        assertNull(hud.sample(1000L).ageMs)
        assertNull(hud.sample(1000L).decodeMs)
    }

    @Test
    fun `rtt is the round trip of a matched nonce`() {
        val hud = SessionHud()
        hud.recordPing(nonce = 1uL, nowMs = 1000L)
        hud.recordPong(nonce = 1uL, nowMs = 1043L)
        assertEquals(43L, hud.sample(1043L).rttMs)
    }

    @Test
    fun `a pong for a superseded nonce is discarded, not misattributed`() {
        val hud = SessionHud()
        hud.recordPing(nonce = 1uL, nowMs = 1000L)
        hud.recordPing(nonce = 2uL, nowMs = 2000L)
        // The first ping's answer finally turns up after its successor went out.
        hud.recordPong(nonce = 1uL, nowMs = 2100L)
        assertNull("a stale nonce must not be timed against the live ping", hud.sample(2100L).rttMs)
        hud.recordPong(nonce = 2uL, nowMs = 2110L)
        assertEquals(110L, hud.sample(2110L).rttMs)
    }

    @Test
    fun `an unanswered ping leaves rtt blank rather than growing without bound`() {
        val hud = SessionHud()
        hud.recordPing(nonce = 1uL, nowMs = 1000L)
        assertNull(hud.sample(60_000L).rttMs)
    }

    @Test
    fun `a stale rtt reading expires rather than being shown as live`() {
        val hud = SessionHud()
        hud.recordPing(nonce = 1uL, nowMs = 1000L)
        hud.recordPong(nonce = 1uL, nowMs = 1020L)
        assertEquals(20L, hud.sample(1020L).rttMs)
        // Five seconds on with nothing answered, the last reading is history.
        assertNull(hud.sample(6100L).rttMs)
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
