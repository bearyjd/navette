package com.greponlabs.navette.net

import kotlinx.serialization.SerializationException
import kotlinx.serialization.json.Json
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

/**
 * Fixtures are the ones `crates/navette-protocol/src/media.rs`'s own
 * `#[cfg(test)] mod tests` asserts against, so a wire-shape drift on either
 * side breaks a test rather than passing silently.
 */
class MediaProtocolTest {
    private fun header() =
        MediaHeader(
            kind = MediaKind.VIDEO,
            flags = MediaFlags.of(keyframe = true, discontinuity = true),
            streamId = 7,
            sequence = 9,
            timestampUs = 11,
            payloadLen = 0,
            width = 1920,
            height = 1080,
        )

    private fun decodeError(bytes: ByteArray): MediaDecodeError =
        try {
            MediaPacket.decode(bytes)
            fail("expected a decode failure")
            error("unreachable")
        } catch (exception: MediaDecodeException) {
            exception.error
        }

    @Test
    fun `media packet round trips in network byte order`() {
        val packet = MediaPacket.of(header(), byteArrayOf(0, 0, 0, 1, 0x65))
        val encoded = packet.encode()

        assertArrayEquals("NVTM".toByteArray(), encoded.copyOfRange(0, 4))
        assertArrayEquals(byteArrayOf(0, 1), encoded.copyOfRange(4, 6))
        assertEquals(MediaKind.VIDEO.wire.toByte(), encoded[6])
        assertEquals(3.toByte(), encoded[7])
        assertEquals(MEDIA_HEADER_LEN + 5, encoded.size)
        assertEquals(packet, MediaPacket.decode(encoded))
    }

    @Test
    fun `decoded header carries every field back unchanged`() {
        val decoded = MediaPacket.decode(MediaPacket.of(header(), byteArrayOf(1, 2, 3)).encode()).header

        assertEquals(MediaKind.VIDEO, decoded.kind)
        assertTrue(decoded.flags.keyframe)
        assertTrue(decoded.flags.discontinuity)
        assertEquals(7L, decoded.streamId)
        assertEquals(9L, decoded.sequence)
        assertEquals(11L, decoded.timestampUs)
        assertEquals(3L, decoded.payloadLen)
        assertEquals(1920L, decoded.width)
        assertEquals(1080L, decoded.height)
    }

    @Test
    fun `stream config round trips with surface identity`() {
        val config = StreamConfig(clientId = 11, surfaceId = 12, codecConfig = byteArrayOf(0, 0, 0, 1, 0x67))

        assertEquals(config, StreamConfig.decode(config.encode()))
        assertEquals(STREAM_CONFIG_PREFIX_LEN + 5, config.encode().size)

        try {
            StreamConfig.decode(ByteArray(0))
            fail("an empty payload is not a stream configuration")
        } catch (exception: MediaDecodeException) {
            assertEquals(MediaDecodeError.TruncatedStreamConfig, exception.error)
        }
    }

    @Test
    fun `stream config rejects trailing bytes rather than ignoring them`() {
        val config = StreamConfig(clientId = 1, surfaceId = 2, codecConfig = byteArrayOf(7))
        try {
            StreamConfig.decode(config.encode() + byteArrayOf(0))
            fail("a length that disagrees with the buffer must not decode")
        } catch (exception: MediaDecodeException) {
            assertEquals(MediaDecodeError.LengthMismatch, exception.error)
        }
    }

    @Test
    fun `malformed or oversized packets are rejected before payload copy`() {
        assertEquals(MediaDecodeError.TruncatedHeader, decodeError("short".toByteArray()))

        val badMagic = MediaPacket.of(header(), byteArrayOf(1)).encode()
        badMagic[0] = 'X'.code.toByte()
        assertEquals(MediaDecodeError.InvalidMagic, decodeError(badMagic))

        val oversized = MediaPacket.of(header(), byteArrayOf(1)).encode()
        writeUnsignedInt(oversized, offset = 32, value = MAX_MEDIA_PAYLOAD + 1)
        assertTrue(decodeError(oversized) is MediaDecodeError.PayloadTooLarge)
    }

    /**
     * The Rust fixture uses `MAX_MEDIA_PAYLOAD + 1`, which still fits in a
     * positive Int and so passes even if the length were read as a signed
     * Int. `0xFFFFFFFF` is the value that separates the two: read signed it
     * is `-1`, which is *below* the bound and would sail through the
     * "reject before allocating" guard entirely.
     */
    @Test
    fun `a payload length with the high bit set is rejected, not read as negative`() {
        val hostile = MediaPacket.of(header(), byteArrayOf(1)).encode()
        writeUnsignedInt(hostile, offset = 32, value = 0xFFFFFFFFL)
        val error = decodeError(hostile)
        assertTrue("expected PayloadTooLarge, got $error", error is MediaDecodeError.PayloadTooLarge)
        assertEquals(0xFFFFFFFFL, (error as MediaDecodeError.PayloadTooLarge).payloadLen)
    }

    @Test
    fun `an unsupported version is rejected before the kind byte is read`() {
        val packet = MediaPacket.of(header(), byteArrayOf(1)).encode()
        packet[4] = 0
        packet[5] = 9
        // Kind is corrupted too: the version error must win, matching the
        // order media.rs:96-108 checks in.
        packet[6] = 99
        assertEquals(MediaDecodeError.UnsupportedVersion(9), decodeError(packet))
    }

    @Test
    fun `unknown flag bits and unknown kinds are rejected`() {
        val unknownFlags = MediaPacket.of(header(), byteArrayOf(1)).encode()
        unknownFlags[7] = 0xFF.toByte()
        assertEquals(MediaDecodeError.UnknownFlags(0xFF), decodeError(unknownFlags))

        val unknownKind = MediaPacket.of(header(), byteArrayOf(1)).encode()
        unknownKind[6] = 9
        assertEquals(MediaDecodeError.UnknownKind(9), decodeError(unknownKind))
    }

    @Test
    fun `a payload shorter than the header promises is a length mismatch`() {
        val packet = MediaPacket.of(header(), byteArrayOf(1, 2, 3)).encode()
        assertEquals(MediaDecodeError.LengthMismatch, decodeError(packet.copyOfRange(0, packet.size - 1)))
    }

    @Test
    fun `input validation rejects adversarial values`() {
        assertEquals(
            InputValidationError.NonFiniteCoordinate,
            MediaInput.PointerMotion(1uL, 2uL, Double.NaN, 0.0).validate(),
        )
        assertEquals(
            InputValidationError.NonFiniteCoordinate,
            MediaInput.PointerMotion(1uL, 2uL, 0.0, Double.POSITIVE_INFINITY).validate(),
        )
        assertEquals(InputValidationError.ViewportOutOfRange, MediaInput.ViewportResize(10, 10).validate())
        assertNull(MediaInput.ViewportResize(MIN_VIEWPORT_WIDTH, MIN_VIEWPORT_HEIGHT).validate())
        assertNull(MediaInput.ViewportResize(MAX_VIEWPORT_WIDTH, MAX_VIEWPORT_HEIGHT).validate())
        assertEquals(
            InputValidationError.ViewportOutOfRange,
            MediaInput.ViewportResize(MAX_VIEWPORT_WIDTH + 1, MAX_VIEWPORT_HEIGHT).validate(),
        )
    }

    @Test
    fun `input validation bounds buttons and keycodes`() {
        assertNull(MediaInput.PointerButton(1uL, 2uL, BTN_LEFT, true).validate())
        assertEquals(
            InputValidationError.ButtonOutOfRange(0x10f),
            MediaInput.PointerButton(1uL, 2uL, 0x10f, true).validate(),
        )
        assertEquals(
            InputValidationError.ButtonOutOfRange(0x120),
            MediaInput.PointerButton(1uL, 2uL, 0x120, true).validate(),
        )
        assertNull(MediaInput.KeyboardKey(1uL, 2uL, MAX_KEYCODE, true).validate())
        assertEquals(
            InputValidationError.KeyOutOfRange(MAX_KEYCODE + 1),
            MediaInput.KeyboardKey(1uL, 2uL, MAX_KEYCODE + 1, true).validate(),
        )
    }

    @Test
    fun `input validation bounds the keyboard layout index`() {
        fun modifiers(layoutIndex: Int) =
            MediaInput.KeyboardModifiers(
                clientId = 1uL,
                surfaceId = 2uL,
                ctrl = false,
                alt = false,
                shift = false,
                capsLock = false,
                logo = false,
                numLock = false,
                layoutIndex = layoutIndex,
            )

        assertNull(modifiers(0).validate())
        assertNull(modifiers(MAX_LAYOUTS - 1).validate())
        assertEquals(InputValidationError.LayoutOutOfRange(MAX_LAYOUTS), modifiers(MAX_LAYOUTS).validate())
        assertEquals(InputValidationError.LayoutOutOfRange(-1), modifiers(-1).validate())
    }

    /**
     * The JSON these produce is what `navette-bridge` deserializes with
     * serde's `#[serde(tag = "type", rename_all = "snake_case")]`, so the
     * exact strings matter more than the round-trip does.
     */
    @Test
    fun `media input serializes to the tagged snake_case shape serde expects`() {
        assertEquals(
            """{"type":"request_keyframe"}""",
            mediaJson.encodeToString(MediaInput.serializer(), MediaInput.RequestKeyframe),
        )
        assertEquals(
            """{"type":"pointer_motion","client_id":11,"surface_id":12,"x":3.5,"y":4.5}""",
            mediaJson.encodeToString(MediaInput.serializer(), MediaInput.PointerMotion(11uL, 12uL, 3.5, 4.5)),
        )
        assertEquals(
            """{"type":"pointer_button","client_id":11,"surface_id":12,"button":272,"pressed":true}""",
            mediaJson.encodeToString(MediaInput.serializer(), MediaInput.PointerButton(11uL, 12uL, BTN_LEFT, true)),
        )
        assertEquals(
            """{"type":"keyboard_key","client_id":11,"surface_id":12,"keycode":30,"pressed":false}""",
            mediaJson.encodeToString(MediaInput.serializer(), MediaInput.KeyboardKey(11uL, 12uL, 30, false)),
        )
        assertEquals(
            """{"type":"viewport_resize","width":1280,"height":720}""",
            mediaJson.encodeToString(MediaInput.serializer(), MediaInput.ViewportResize(1280, 720)),
        )
        assertEquals(
            """{"type":"keyboard_modifiers","client_id":11,"surface_id":12,"ctrl":true,"alt":false,""" +
                """"shift":true,"caps_lock":false,"logo":false,"num_lock":false,"layout_index":0}""",
            mediaJson.encodeToString(
                MediaInput.serializer(),
                MediaInput.KeyboardModifiers(11uL, 12uL, true, false, true, false, false, false, 0),
            ),
        )
    }

    /**
     * Regression test for a real on-device bug: a session's `client_id`
     * genuinely exceeds `Long.MAX_VALUE` (confirmed live against
     * `wprsd` -- e.g. `15272202610726850855`), and the bridge rejected
     * every pointer/keyboard event this client sent with
     * `invalid value: integer` `-3174541462982700761`, expected u64` --
     * a `Long`'s two's-complement bit pattern serialized as a negative
     * JSON literal, which `serde` refuses for a `u64` field. `ULong`
     * serializes the same bit pattern as its correct unsigned decimal
     * form, which is what this test pins.
     */
    @Test
    fun `a client_id above Long MAX_VALUE serializes as an unsigned decimal, not negative`() {
        val aboveLongMax = 15_272_202_610_726_850_855uL
        assertEquals(
            """{"type":"pointer_motion","client_id":15272202610726850855,"surface_id":16817429954436193089,""" +
                """"x":1.0,"y":2.0}""",
            mediaJson.encodeToString(
                MediaInput.serializer(),
                MediaInput.PointerMotion(aboveLongMax, 16_817_429_954_436_193_089uL, 1.0, 2.0),
            ),
        )
    }

    @Test
    fun `a server error frame decodes into its code and message`() {
        val decoded =
            mediaJson.decodeFromString(
                MediaServerMessage.serializer(),
                """{"type":"error","code":"invalid_input","message":"viewport out of range"}""",
            )
        assertEquals(MediaServerMessage.Error("invalid_input", "viewport out of range"), decoded)
    }

    @Test
    fun `ping serialises exactly as the rust protocol expects`() {
        val encoded = mediaJson.encodeToString(MediaInput.serializer(), MediaInput.Ping(42uL))
        assertEquals("""{"type":"ping","nonce":42}""", encoded)
    }

    @Test
    fun `a ping is always valid whatever its nonce`() {
        assertNull(MediaInput.Ping(0uL).validate())
        assertNull(MediaInput.Ping(ULong.MAX_VALUE).validate())
    }

    @Test
    fun `pong parses from what navetted sends`() {
        val decoded = mediaJson.decodeFromString(MediaServerMessage.serializer(), """{"type":"pong","nonce":7}""")
        assertEquals(MediaServerMessage.Pong(7uL), decoded)
    }

    @Test
    fun `an unknown server message is still rejected rather than guessed at`() {
        // MediaClient.onMessage relies on this failing, not throwing past its
        // runCatching -- it is what makes an old daemon's unknown reply a log
        // line instead of a crash.
        assertThrows(SerializationException::class.java) {
            mediaJson.decodeFromString(MediaServerMessage.serializer(), """{"type":"nonsense"}""")
        }
    }

    private fun writeUnsignedInt(bytes: ByteArray, offset: Int, value: Long) {
        bytes[offset] = (value ushr 24 and 0xFF).toByte()
        bytes[offset + 1] = (value ushr 16 and 0xFF).toByte()
        bytes[offset + 2] = (value ushr 8 and 0xFF).toByte()
        bytes[offset + 3] = (value and 0xFF).toByte()
    }

    @Test
    fun setClipboardEncodesToWire() {
        val encoded = Json.encodeToString(
            MediaInput.serializer(),
            MediaInput.SetClipboard("hello"),
        )
        assertEquals("""{"type":"set_clipboard","text":"hello"}""", encoded)
    }

    @Test
    fun clipboardServerMessageDecodesFromWire() {
        val decoded = Json.decodeFromString(
            MediaServerMessage.serializer(),
            """{"type":"clipboard","text":"hello"}""",
        )
        assertEquals(MediaServerMessage.Clipboard("hello"), decoded)
    }

    @Test
    fun setClipboardPassesValidation() {
        assertNull(MediaInput.SetClipboard("hello").validate())
    }

    @Test
    fun imageBlobDescriptorsMatchRustWireValidation() {
        val blob = BlobDescriptor(
            id = "0123456789abcdef0123456789abcdef",
            mime = "image/png",
            size = 42,
        )
        assertNull(blob.validate())
        assertEquals(
            """{"type":"set_clipboard_blob","blob":{"id":"0123456789abcdef0123456789abcdef","mime":"image/png","size":42}}""",
            mediaJson.encodeToString(MediaInput.serializer(), MediaInput.SetClipboardBlob(blob)),
        )
        assertEquals(
            MediaServerMessage.ClipboardBlob(blob),
            mediaJson.decodeFromString(
                MediaServerMessage.serializer(),
                """{"type":"clipboard_blob","blob":{"id":"0123456789abcdef0123456789abcdef","mime":"image/png","size":42}}""",
            ),
        )
        assertTrue(BlobDescriptor("../path", "image/png", 1).validate() != null)
        assertTrue(BlobDescriptor(blob.id, "image/svg+xml", 1).validate() != null)
        assertTrue(BlobDescriptor(blob.id, "image/jpeg", 0).validate() != null)
    }
}
