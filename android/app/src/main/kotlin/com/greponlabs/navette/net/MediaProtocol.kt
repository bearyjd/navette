package com.greponlabs.navette.net

import java.nio.ByteBuffer
import java.nio.ByteOrder
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

/**
 * Kotlin mirror of `crates/navette-protocol/src/media.rs`. The wire shape is
 * not negotiable from this side -- the bridge defines it -- so every field
 * name, byte offset and validation bound here is a direct port, and the
 * round-trip tests in `MediaProtocolTest` reuse the fixtures that file's own
 * `#[cfg(test)]` module asserts against so a drift breaks loudly.
 *
 * Unlike the control channel, this protocol needs no hand-rolled codec: its
 * Rust enums are `#[serde(tag = "type", rename_all = "snake_case")]` with no
 * flattened fields, which is exactly what kotlinx.serialization's built-in
 * polymorphism produces with the default `classDiscriminator = "type"`.
 *
 * **Unsigned widths**: Rust's `u32` fields are carried as [Long] here and
 * masked on decode, so a value above `Int.MAX_VALUE` stays positive instead
 * of reading back as a negative Int -- which would silently pass the
 * "reject before allocating" bound below. `u64` fields (`stream_id`,
 * `sequence`, `timestamp_us`, `client_id`, `surface_id`) are carried as
 * [Long] with identical two's-complement bits: they round-trip byte-exactly
 * within this binary protocol, and a value above `Long.MAX_VALUE` reads back
 * negative but is never compared or arithmetic'd on there.
 *
 * `client_id`/`surface_id` are the exception to "only echoed back": they
 * also flow *out* through [MediaInput], which is JSON, not this binary
 * format -- and unlike a byte-for-byte binary echo, JSON has no
 * two's-complement concept. A `Long` above `Long.MAX_VALUE` serializes as a
 * negative decimal literal, which the bridge's `u64` field then rejects
 * outright (`serde` does not accept a `-` sign for an unsigned type) --
 * confirmed on a real device against a session whose `client_id` genuinely
 * exceeds `Long.MAX_VALUE`, where every pointer/keyboard event was silently
 * rejected server-side this way. [MediaInput]'s `client_id`/`surface_id`
 * fields are therefore `ULong`, not `Long` -- kotlinx.serialization encodes
 * a `ULong` as its correct unsigned decimal JSON number, which is exactly
 * what a `u64` field expects. `StreamConfig`'s own `Long` fields are
 * unchanged; only the JSON-crossing boundary in `InputMapper` converts.
 */

/** Matches `navette_protocol::media::MEDIA_WEBSOCKET_SUBPROTOCOL`. */
const val MEDIA_WEBSOCKET_SUBPROTOCOL: String = "navette.media.v1"

val MEDIA_MAGIC: ByteArray = byteArrayOf('N'.code.toByte(), 'V'.code.toByte(), 'T'.code.toByte(), 'M'.code.toByte())

const val MEDIA_VERSION: Int = 1
const val MEDIA_HEADER_LEN: Int = 44
const val MAX_MEDIA_PAYLOAD: Long = 16L * 1024 * 1024
const val STREAM_CONFIG_VERSION: Int = 1
const val STREAM_CONFIG_PREFIX_LEN: Int = 21

/**
 * Number of keyboard layouts a client may index into. Valid indices are
 * `0..<MAX_LAYOUTS`, matching `media.rs`'s `MAX_LAYOUTS`.
 */
const val MAX_LAYOUTS: Int = 16

/** Evdev `BTN_LEFT`, per `/usr/include/linux/input-event-codes.h`. */
const val BTN_LEFT: Int = 0x110

/** Evdev `BTN_RIGHT`, per `/usr/include/linux/input-event-codes.h`. */
const val BTN_RIGHT: Int = 0x111

/** Inclusive evdev button range the bridge accepts (`BTN_LEFT`..`BTN_TASK` and spare). */
const val BUTTON_MIN: Int = 0x110
const val BUTTON_MAX: Int = 0x11f

/** Highest evdev keycode the bridge accepts. */
const val MAX_KEYCODE: Int = 767

/** Viewport bounds the bridge validates against (`media.rs`'s `ViewportResize` arm). */
const val MIN_VIEWPORT_WIDTH: Int = 320
const val MAX_VIEWPORT_WIDTH: Int = 3840
const val MIN_VIEWPORT_HEIGHT: Int = 240
const val MAX_VIEWPORT_HEIGHT: Int = 2160

enum class MediaKind(val wire: Int) {
    STREAM_CONFIG(1),
    VIDEO(2),
    STREAM_END(3),
    METRICS(4),
    ;

    companion object {
        fun fromWire(value: Int): MediaKind? = entries.firstOrNull { it.wire == value }
    }
}

/** Packed flag bits, laid out exactly as `media.rs`'s `MediaFlags`. */
@JvmInline
value class MediaFlags(val bits: Int) {
    val keyframe: Boolean get() = bits and KEYFRAME != 0

    val discontinuity: Boolean get() = bits and DISCONTINUITY != 0

    companion object {
        const val KEYFRAME: Int = 1
        const val DISCONTINUITY: Int = 2
        const val KNOWN: Int = KEYFRAME or DISCONTINUITY

        fun of(keyframe: Boolean, discontinuity: Boolean): MediaFlags =
            MediaFlags((if (keyframe) KEYFRAME else 0) or (if (discontinuity) DISCONTINUITY else 0))
    }
}

sealed interface MediaDecodeError {
    data object TruncatedHeader : MediaDecodeError

    data object InvalidMagic : MediaDecodeError

    data class UnsupportedVersion(val version: Int) : MediaDecodeError

    data class UnknownKind(val kind: Int) : MediaDecodeError

    data class UnknownFlags(val flags: Int) : MediaDecodeError

    data class PayloadTooLarge(val payloadLen: Long) : MediaDecodeError

    data object LengthMismatch : MediaDecodeError

    data object TruncatedStreamConfig : MediaDecodeError

    data class UnsupportedStreamConfigVersion(val version: Int) : MediaDecodeError
}

class MediaDecodeException(val error: MediaDecodeError) : Exception(error.toString())

/** The 44-byte big-endian frame header, per `media.rs`'s `MediaHeader`. */
data class MediaHeader(
    val kind: MediaKind,
    val flags: MediaFlags,
    val streamId: Long,
    val sequence: Long,
    val timestampUs: Long,
    val payloadLen: Long,
    val width: Long,
    val height: Long,
) {
    fun encode(): ByteArray {
        val buffer = ByteBuffer.allocate(MEDIA_HEADER_LEN).order(ByteOrder.BIG_ENDIAN)
        buffer.put(MEDIA_MAGIC)
        buffer.putShort(MEDIA_VERSION.toShort())
        buffer.put(kind.wire.toByte())
        buffer.put(flags.bits.toByte())
        buffer.putLong(streamId)
        buffer.putLong(sequence)
        buffer.putLong(timestampUs)
        buffer.putInt(payloadLen.toInt())
        buffer.putInt(width.toInt())
        buffer.putInt(height.toInt())
        return buffer.array()
    }

    companion object {
        /**
         * Rejects in the same order `media.rs:89-117` does -- length, magic,
         * version, unknown flags, oversized payload, then unknown kind. The
         * ordering is load-bearing: the payload bound is checked before any
         * buffer that size could be allocated, which is what the Rust side's
         * `malformed_or_oversized_packets_are_rejected_before_payload_copy`
         * pins down.
         */
        fun decode(bytes: ByteArray): MediaHeader {
            if (bytes.size < MEDIA_HEADER_LEN) throw MediaDecodeException(MediaDecodeError.TruncatedHeader)
            for (index in MEDIA_MAGIC.indices) {
                if (bytes[index] != MEDIA_MAGIC[index]) {
                    throw MediaDecodeException(MediaDecodeError.InvalidMagic)
                }
            }
            val buffer = ByteBuffer.wrap(bytes, 0, MEDIA_HEADER_LEN).order(ByteOrder.BIG_ENDIAN)
            buffer.position(4)
            val version = buffer.short.toInt() and 0xFFFF
            if (version != MEDIA_VERSION) {
                throw MediaDecodeException(MediaDecodeError.UnsupportedVersion(version))
            }
            val kindByte = buffer.get().toInt() and 0xFF
            val flagBits = buffer.get().toInt() and 0xFF
            if (flagBits and MediaFlags.KNOWN.inv() != 0) {
                throw MediaDecodeException(MediaDecodeError.UnknownFlags(flagBits))
            }
            val streamId = buffer.long
            val sequence = buffer.long
            val timestampUs = buffer.long
            val payloadLen = buffer.int.toUnsignedLong()
            if (payloadLen > MAX_MEDIA_PAYLOAD) {
                throw MediaDecodeException(MediaDecodeError.PayloadTooLarge(payloadLen))
            }
            val kind = MediaKind.fromWire(kindByte) ?: throw MediaDecodeException(MediaDecodeError.UnknownKind(kindByte))
            return MediaHeader(
                kind = kind,
                flags = MediaFlags(flagBits),
                streamId = streamId,
                sequence = sequence,
                timestampUs = timestampUs,
                payloadLen = payloadLen,
                width = buffer.int.toUnsignedLong(),
                height = buffer.int.toUnsignedLong(),
            )
        }
    }
}

/**
 * Header plus payload. Not a `data class`: [payload] is a [ByteArray], whose
 * generated `equals` would be reference identity and would make every
 * round-trip assertion in the tests pass or fail for the wrong reason.
 */
class MediaPacket(val header: MediaHeader, val payload: ByteArray) {
    fun encode(): ByteArray {
        if (payload.size.toLong() > MAX_MEDIA_PAYLOAD) {
            throw MediaDecodeException(MediaDecodeError.PayloadTooLarge(payload.size.toLong()))
        }
        if (payload.size.toLong() != header.payloadLen) {
            throw MediaDecodeException(MediaDecodeError.LengthMismatch)
        }
        return header.encode() + payload
    }

    override fun equals(other: Any?): Boolean =
        this === other ||
            (other is MediaPacket && header == other.header && payload.contentEquals(other.payload))

    override fun hashCode(): Int = 31 * header.hashCode() + payload.contentHashCode()

    override fun toString(): String = "MediaPacket(header=$header, payload=${payload.size} bytes)"

    companion object {
        /** Mirrors `MediaPacket::new`: the header's `payload_len` is derived, never trusted from the caller. */
        fun of(header: MediaHeader, payload: ByteArray): MediaPacket {
            if (payload.size.toLong() > MAX_MEDIA_PAYLOAD) {
                throw MediaDecodeException(MediaDecodeError.PayloadTooLarge(payload.size.toLong()))
            }
            return MediaPacket(header.copy(payloadLen = payload.size.toLong()), payload)
        }

        fun decode(bytes: ByteArray): MediaPacket {
            val header = MediaHeader.decode(bytes)
            if (bytes.size.toLong() != MEDIA_HEADER_LEN + header.payloadLen) {
                throw MediaDecodeException(MediaDecodeError.LengthMismatch)
            }
            return MediaPacket(header, bytes.copyOfRange(MEDIA_HEADER_LEN, bytes.size))
        }
    }
}

/**
 * Codec bootstrap and scene identity, per `media.rs`'s `StreamConfig`. Not a
 * `data class`, for the same [ByteArray] reason as [MediaPacket] -- and here
 * it matters twice over: `StreamGate` compares two of these to tell a real
 * reconfigure from the hub's replay of an identical configuration, and
 * reference equality would report "changed" every time.
 */
class StreamConfig(val clientId: Long, val surfaceId: Long, val codecConfig: ByteArray) {
    fun encode(): ByteArray {
        if (codecConfig.size.toLong() > MAX_MEDIA_PAYLOAD - STREAM_CONFIG_PREFIX_LEN) {
            throw MediaDecodeException(MediaDecodeError.PayloadTooLarge(codecConfig.size.toLong()))
        }
        val buffer =
            ByteBuffer.allocate(STREAM_CONFIG_PREFIX_LEN + codecConfig.size).order(ByteOrder.BIG_ENDIAN)
        buffer.put(STREAM_CONFIG_VERSION.toByte())
        buffer.putLong(clientId)
        buffer.putLong(surfaceId)
        buffer.putInt(codecConfig.size)
        buffer.put(codecConfig)
        return buffer.array()
    }

    override fun equals(other: Any?): Boolean =
        this === other ||
            (
                other is StreamConfig &&
                    clientId == other.clientId &&
                    surfaceId == other.surfaceId &&
                    codecConfig.contentEquals(other.codecConfig)
            )

    override fun hashCode(): Int {
        var result = clientId.hashCode()
        result = 31 * result + surfaceId.hashCode()
        result = 31 * result + codecConfig.contentHashCode()
        return result
    }

    override fun toString(): String =
        "StreamConfig(clientId=$clientId, surfaceId=$surfaceId, codecConfig=${codecConfig.size} bytes)"

    companion object {
        fun decode(bytes: ByteArray): StreamConfig {
            if (bytes.size < STREAM_CONFIG_PREFIX_LEN) {
                throw MediaDecodeException(MediaDecodeError.TruncatedStreamConfig)
            }
            val buffer = ByteBuffer.wrap(bytes).order(ByteOrder.BIG_ENDIAN)
            val version = buffer.get().toInt() and 0xFF
            if (version != STREAM_CONFIG_VERSION) {
                throw MediaDecodeException(MediaDecodeError.UnsupportedStreamConfigVersion(version))
            }
            val clientId = buffer.long
            val surfaceId = buffer.long
            val codecLen = buffer.int.toUnsignedLong()
            // Exact, not at-least: `media.rs:162` rejects trailing bytes too.
            if (bytes.size.toLong() != STREAM_CONFIG_PREFIX_LEN + codecLen || bytes.size.toLong() > MAX_MEDIA_PAYLOAD) {
                throw MediaDecodeException(MediaDecodeError.LengthMismatch)
            }
            return StreamConfig(clientId, surfaceId, bytes.copyOfRange(STREAM_CONFIG_PREFIX_LEN, bytes.size))
        }
    }
}

@Serializable
sealed interface MediaInput {
    @Serializable
    @SerialName("pointer_motion")
    data class PointerMotion(
        @SerialName("client_id") val clientId: ULong,
        @SerialName("surface_id") val surfaceId: ULong,
        val x: Double,
        val y: Double,
    ) : MediaInput

    @Serializable
    @SerialName("pointer_button")
    data class PointerButton(
        @SerialName("client_id") val clientId: ULong,
        @SerialName("surface_id") val surfaceId: ULong,
        val button: Int,
        val pressed: Boolean,
    ) : MediaInput

    @Serializable
    @SerialName("pointer_axis")
    data class PointerAxis(
        @SerialName("client_id") val clientId: ULong,
        @SerialName("surface_id") val surfaceId: ULong,
        val horizontal: Double,
        val vertical: Double,
    ) : MediaInput

    @Serializable
    @SerialName("keyboard_key")
    data class KeyboardKey(
        @SerialName("client_id") val clientId: ULong,
        @SerialName("surface_id") val surfaceId: ULong,
        val keycode: Int,
        val pressed: Boolean,
    ) : MediaInput

    @Serializable
    @SerialName("keyboard_modifiers")
    data class KeyboardModifiers(
        @SerialName("client_id") val clientId: ULong,
        @SerialName("surface_id") val surfaceId: ULong,
        val ctrl: Boolean,
        val alt: Boolean,
        val shift: Boolean,
        @SerialName("caps_lock") val capsLock: Boolean,
        val logo: Boolean,
        @SerialName("num_lock") val numLock: Boolean,
        @SerialName("layout_index") val layoutIndex: Int,
    ) : MediaInput

    @Serializable
    @SerialName("viewport_resize")
    data class ViewportResize(val width: Int, val height: Int) : MediaInput

    @Serializable
    @SerialName("request_keyframe")
    data object RequestKeyframe : MediaInput

    @Serializable
    @SerialName("ping")
    data class Ping(val nonce: ULong) : MediaInput
}

sealed interface InputValidationError {
    data object NonFiniteCoordinate : InputValidationError

    data class ButtonOutOfRange(val button: Int) : InputValidationError

    data class KeyOutOfRange(val keycode: Int) : InputValidationError

    data class LayoutOutOfRange(val layoutIndex: Int) : InputValidationError

    data object ViewportOutOfRange : InputValidationError
}

/**
 * Every bound `media.rs:286-314` enforces, checked client-side so a local bug
 * never reaches the wire looking like a protocol violation. `null` means the
 * input is in range.
 */
fun MediaInput.validate(): InputValidationError? =
    when (this) {
        is MediaInput.PointerMotion ->
            if (!x.isFinite() || !y.isFinite()) InputValidationError.NonFiniteCoordinate else null
        is MediaInput.PointerAxis ->
            if (!horizontal.isFinite() || !vertical.isFinite()) InputValidationError.NonFiniteCoordinate else null
        is MediaInput.PointerButton ->
            if (button !in BUTTON_MIN..BUTTON_MAX) InputValidationError.ButtonOutOfRange(button) else null
        is MediaInput.KeyboardKey ->
            if (keycode > MAX_KEYCODE || keycode < 0) InputValidationError.KeyOutOfRange(keycode) else null
        is MediaInput.KeyboardModifiers ->
            if (layoutIndex >= MAX_LAYOUTS || layoutIndex < 0) {
                InputValidationError.LayoutOutOfRange(layoutIndex)
            } else {
                null
            }
        is MediaInput.ViewportResize ->
            if (width !in MIN_VIEWPORT_WIDTH..MAX_VIEWPORT_WIDTH ||
                height !in MIN_VIEWPORT_HEIGHT..MAX_VIEWPORT_HEIGHT
            ) {
                InputValidationError.ViewportOutOfRange
            } else {
                null
            }
        MediaInput.RequestKeyframe -> null
        is MediaInput.Ping -> null
    }

/** The bridge reports protocol problems as JSON text frames; they are informational. */
@Serializable
sealed interface MediaServerMessage {
    @Serializable
    @SerialName("error")
    data class Error(val code: String, val message: String) : MediaServerMessage

    @Serializable
    @SerialName("pong")
    data class Pong(val nonce: ULong) : MediaServerMessage
}

/**
 * `classDiscriminator` stays at the library default `"type"`, which is
 * precisely what serde's `#[serde(tag = "type")]` produces on the Rust side.
 */
val mediaJson: Json =
    Json {
        ignoreUnknownKeys = true
    }

private fun Int.toUnsignedLong(): Long = this.toLong() and 0xFFFFFFFFL
