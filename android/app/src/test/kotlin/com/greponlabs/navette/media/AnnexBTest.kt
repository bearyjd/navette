package com.greponlabs.navette.media

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertNull
import org.junit.Test

class AnnexBTest {
    private val startCode4 = byteArrayOf(0, 0, 0, 1)
    private val startCode3 = byteArrayOf(0, 0, 1)

    // nal_unit_type lives in the low 5 bits; nal_ref_idc (0x60) rides above
    // it, so these are the byte values a real encoder emits, not bare 7/8.
    private val spsBody = byteArrayOf(0x67, 0x42, 0x00, 0x1E)
    private val ppsBody = byteArrayOf(0x68, 0xCE.toByte(), 0x3C, 0x80.toByte())
    private val sliceBody = byteArrayOf(0x65, 0x11, 0x22)

    @Test
    fun `sps then pps then slice is the typical case`() {
        val input = startCode4 + spsBody + startCode4 + ppsBody + startCode4 + sliceBody

        val (sps, pps) = AnnexB.splitSpsPps(input)

        assertArrayEquals(startCode4 + spsBody, sps)
        assertArrayEquals(startCode4 + ppsBody, pps)
    }

    @Test
    fun `pps before sps is handled -- order must not matter`() {
        val input = startCode4 + ppsBody + startCode4 + spsBody + startCode4 + sliceBody

        val (sps, pps) = AnnexB.splitSpsPps(input)

        assertArrayEquals(startCode4 + spsBody, sps)
        assertArrayEquals(startCode4 + ppsBody, pps)
    }

    /**
     * A parameter set sitting after a slice still has to be found: the plan's
     * scan runs to the end of the buffer rather than stopping at the first
     * non-parameter-set NAL.
     */
    @Test
    fun `a parameter set after a slice is still found`() {
        val input = startCode4 + spsBody + startCode4 + sliceBody + startCode4 + ppsBody

        val (sps, pps) = AnnexB.splitSpsPps(input)

        assertArrayEquals(startCode4 + spsBody, sps)
        assertArrayEquals(startCode4 + ppsBody, pps)
    }

    @Test
    fun `mixed three and four byte start codes in one buffer both parse`() {
        val input = startCode3 + spsBody + startCode4 + ppsBody + startCode3 + sliceBody

        val (sps, pps) = AnnexB.splitSpsPps(input)

        // Whatever the source used, both come back with a four-byte prefix.
        assertArrayEquals(startCode4 + spsBody, sps)
        assertArrayEquals(startCode4 + ppsBody, pps)
    }

    @Test
    fun `a missing pps yields only the sps`() {
        val (sps, pps) = AnnexB.splitSpsPps(startCode4 + spsBody + startCode4 + sliceBody)

        assertArrayEquals(startCode4 + spsBody, sps)
        assertNull(pps)
    }

    @Test
    fun `a missing sps yields only the pps`() {
        val (sps, pps) = AnnexB.splitSpsPps(startCode4 + ppsBody)

        assertNull(sps)
        assertArrayEquals(startCode4 + ppsBody, pps)
    }

    @Test
    fun `empty input yields neither`() {
        val (sps, pps) = AnnexB.splitSpsPps(ByteArray(0))

        assertNull(sps)
        assertNull(pps)
    }

    @Test
    fun `a buffer with no start code at all yields neither`() {
        val (sps, pps) = AnnexB.splitSpsPps(byteArrayOf(0x67, 0x42, 0x00, 0x1E))

        assertNull(sps)
        assertNull(pps)
    }

    @Test
    fun `a start code with no payload after it is skipped`() {
        val (sps, pps) = AnnexB.splitSpsPps(startCode4 + startCode4 + spsBody)

        assertArrayEquals(startCode4 + spsBody, sps)
        assertNull(pps)
    }

    @Test
    fun `a truncated start code prefix yields neither`() {
        val (sps, pps) = AnnexB.splitSpsPps(byteArrayOf(0, 0))

        assertNull(sps)
        assertNull(pps)
    }

    /**
     * The four-byte start code contains the three-byte one at an offset of
     * one. If the scanner attributed that extra zero to the previous NAL's
     * payload instead of to the start code, the extracted SPS would carry a
     * trailing zero byte no encoder put there.
     */
    @Test
    fun `the extra zero of a four-byte start code is not left on the previous NAL`() {
        val input = startCode4 + spsBody + startCode4 + ppsBody

        val (sps, _) = AnnexB.splitSpsPps(input)

        assertArrayEquals(startCode4 + spsBody, sps)
    }

    @Test
    fun `only the first sps and pps are taken when a buffer repeats them`() {
        val secondSps = byteArrayOf(0x67, 0x64, 0x00, 0x28)
        val input = startCode4 + spsBody + startCode4 + secondSps + startCode4 + ppsBody

        val (sps, pps) = AnnexB.splitSpsPps(input)

        assertArrayEquals(startCode4 + spsBody, sps)
        assertArrayEquals(startCode4 + ppsBody, pps)
    }
}
