package com.greponlabs.navette.media

/**
 * Just enough Annex-B parsing to pull the SPS and PPS out of a
 * `StreamConfig.codec_config` blob so they can be handed to `MediaCodec` as
 * `csd-0`/`csd-1`.
 *
 * Deliberately not a NAL parser: nothing here reads a NAL's payload, only
 * where each one starts and what its type nibble is, so emulation-prevention
 * bytes never need removing. Anything more would be building a decoder this
 * client does not need -- `MediaCodec` is the decoder.
 */
object AnnexB {
    /** `nal_unit_type` values, per H.264 Annex A. */
    const val NAL_TYPE_SPS: Int = 7
    const val NAL_TYPE_PPS: Int = 8

    /**
     * The four-byte start code every extracted NAL is re-emitted with.
     * `MediaCodec` accepts either length, but a consistent four-byte prefix on
     * `csd-0`/`csd-1` is the conventional shape and costs one byte.
     */
    private val START_CODE = byteArrayOf(0, 0, 0, 1)

    /** One NAL's position within an Annex-B buffer. */
    private class Nal(val payloadStart: Int, val payloadEnd: Int, val type: Int)

    /**
     * Returns `(sps, pps)`, each as its own start-code-prefixed buffer, or
     * `null` for one that is absent. Order in the input does not matter: the
     * whole buffer is scanned until both are found or it runs out.
     */
    fun splitSpsPps(annexB: ByteArray): Pair<ByteArray?, ByteArray?> {
        var sps: ByteArray? = null
        var pps: ByteArray? = null
        for (nal in scan(annexB)) {
            when (nal.type) {
                NAL_TYPE_SPS -> if (sps == null) sps = extract(annexB, nal)
                NAL_TYPE_PPS -> if (pps == null) pps = extract(annexB, nal)
                else -> Unit
            }
            if (sps != null && pps != null) break
        }
        return sps to pps
    }

    private fun extract(annexB: ByteArray, nal: Nal): ByteArray =
        START_CODE + annexB.copyOfRange(nal.payloadStart, nal.payloadEnd)

    /**
     * Walks the buffer's start codes and yields each NAL's bounds.
     *
     * A three-byte `00 00 01` preceded by another zero is the tail of a
     * four-byte start code, not a NAL of its own -- so the extra zero is
     * attributed to the start code rather than to the previous NAL's payload.
     */
    private fun scan(annexB: ByteArray): List<Nal> {
        val boundaries = mutableListOf<Pair<Int, Int>>()
        var index = 0
        while (index + 2 < annexB.size) {
            if (annexB[index] == ZERO && annexB[index + 1] == ZERO && annexB[index + 2] == ONE) {
                if (index > 0 && annexB[index - 1] == ZERO) {
                    boundaries.add((index - 1) to 4)
                } else {
                    boundaries.add(index to 3)
                }
                index += 3
            } else {
                index++
            }
        }

        return boundaries.mapIndexedNotNull { position, (start, length) ->
            val payloadStart = start + length
            val payloadEnd = boundaries.getOrNull(position + 1)?.first ?: annexB.size
            // A start code immediately followed by another one carries no NAL.
            if (payloadStart >= payloadEnd) {
                null
            } else {
                Nal(payloadStart, payloadEnd, annexB[payloadStart].toInt() and 0x1F)
            }
        }
    }

    private const val ZERO: Byte = 0
    private const val ONE: Byte = 1
}
