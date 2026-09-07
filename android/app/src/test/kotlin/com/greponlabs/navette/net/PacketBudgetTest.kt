package com.greponlabs.navette.net

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The socket queue's byte-budget arithmetic, in isolation.
 *
 * This exists because the budget itself cannot be driven end-to-end from a
 * test: `MockWebServer`'s outgoing queue tops out at 16 MiB, so it cannot
 * produce the oversized frames the interesting cases need. The Rust side pins
 * the same properties through its own transport
 * (`crates/navette-viewer/src/client.rs:402-419`,
 * `a_packet_larger_than_the_budget_is_still_queued`); this pins the
 * arithmetic those properties rest on.
 */
class PacketBudgetTest {
    private val budgetKib = 8 * 1024

    private fun packet(payloadBytes: Int): MediaPacket =
        MediaPacket.of(
            MediaHeader(
                kind = MediaKind.VIDEO,
                flags = MediaFlags.of(keyframe = false, discontinuity = false),
                streamId = 1,
                sequence = 1,
                timestampUs = 0,
                payloadLen = 0,
                width = 1280,
                height = 720,
            ),
            ByteArray(payloadBytes),
        )

    /**
     * A flood of empty packets would otherwise cost nothing at all, leaving
     * the queue bounded only by its object capacity.
     */
    @Test
    fun `every packet costs at least one KiB`() {
        assertEquals(1, packetBudgetKib(packet(0), budgetKib))
        assertEquals(1, packetBudgetKib(packet(1), budgetKib))
        assertEquals(1, packetBudgetKib(packet(1024), budgetKib))
    }

    @Test
    fun `the charge rounds up to whole KiB`() {
        assertEquals(2, packetBudgetKib(packet(1025), budgetKib))
        assertEquals(2, packetBudgetKib(packet(2048), budgetKib))
        assertEquals(3, packetBudgetKib(packet(2049), budgetKib))
        assertEquals(16, packetBudgetKib(packet(16 * 1024), budgetKib))
    }

    /**
     * The protocol permits a 16 MiB payload against an 8 MiB budget. Without
     * the clamp that charge could never be acquired, and a packet nothing else
     * holds a copy of would stall the reader thread forever rather than merely
     * slowing it -- a hang, not a slow path. Clamping instead makes it cost
     * the whole budget, which is what "one oversized packet at a time" means.
     */
    @Test
    fun `a payload larger than the whole budget is clamped, not made unacquirable`() {
        val oversized = packetBudgetKib(packet((budgetKib + 1) * 1024), budgetKib)

        assertEquals(budgetKib, oversized)
        assertTrue("an oversized packet must still be admissible", oversized <= budgetKib)
    }

    @Test
    fun `the maximum protocol payload is still admissible`() {
        assertEquals(budgetKib, packetBudgetKib(packet(MAX_MEDIA_PAYLOAD.toInt()), budgetKib))
    }

    @Test
    fun `a payload exactly at the budget charges the budget`() {
        assertEquals(budgetKib, packetBudgetKib(packet(budgetKib * 1024), budgetKib))
    }
}
