package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.MediaKind
import com.greponlabs.navette.net.MediaPacket
import java.util.Locale

/** Rolling window every rate in a [HudSample] is measured over. */
const val HUD_WINDOW_MS: Long = 1000L

/**
 * How long a round-trip reading stays on screen before it is treated as
 * history. Pings go out once a second, so anything older than this means
 * several went unanswered and the number no longer describes the link.
 */
const val RTT_STALE_MS: Long = 5000L

/**
 * How many of a stream's first packets only establish the sequence baseline
 * instead of being audited for gaps.
 *
 * A direct port of `hud.rs`'s `BASELINE_PACKETS`, for the same reason:
 * attaching replays up to two packets per stream -- the stream's latest
 * configuration and its latest keyframe -- at the sequence numbers they were
 * originally published with, and live traffic then resumes from wherever the
 * stream has actually got to. That jump is history this client was never
 * sent, not a drop. Three covers the replay plus the first live packet.
 */
private const val BASELINE_PACKETS: Long = 3

/**
 * How many fed access units are remembered while waiting to be presented.
 *
 * Bounded because a decoder that stops presenting must not grow this without
 * limit. Small: the interval being measured is a handful of milliseconds, so
 * anything still unmatched after this many newer units is not going to be.
 */
private const val PENDING_FEEDS = 16

/**
 * The HUD's figures at a point in time. Nulls mean "no reading", never zero.
 *
 * Two scopes in one line, which is deliberate. [droppedPackets],
 * [discontinuities] and [bitrateBps] describe the one stream being rendered,
 * because sequence numbering and payload are per-stream. [fps], [decodeMs],
 * [ageMs] and [rttMs] are socket-wide: the first three are only ever fed for
 * the adopted stream anyway, and the round trip is a property of the media
 * socket, which carries no per-stream ping.
 */
data class HudSample(
    val fps: Double,
    val bitrateBps: Double,
    val decodeMs: Double?,
    val ageMs: Long?,
    val rttMs: Long?,
    val droppedPackets: Long,
    val discontinuities: Long,
)

/**
 * A compact single line, short enough to sit over a phone-sized picture.
 *
 * `--` rather than `0` wherever there is no reading: a zero round trip and an
 * unanswered ping are very different things, and the whole point of this
 * overlay is telling them apart.
 */
fun HudSample.format(): String {
    // Locale.ROOT throughout: the default locale renders a comma decimal
    // separator across much of the world, which would put "29,9" on the
    // overlay and make this function's output depend on the phone's region.
    val dec = decodeMs?.let { String.format(Locale.ROOT, "%.1fMS", it) } ?: "--"
    val age = ageMs?.let { "${it}MS" } ?: "--"
    val rtt = rttMs?.let { "${it}MS" } ?: "--"
    val rate = String.format(Locale.ROOT, "%.1f", fps)
    val kbps = String.format(Locale.ROOT, "%.0f", bitrateBps / 1000.0)
    return "FPS $rate  KBPS $kbps  DEC $dec  AGE $age  RTT $rtt  " +
        "DROP $droppedPackets  DISC $discontinuities"
}

/**
 * Accumulates one session's HUD inputs.
 *
 * Split by scope, because the media socket carries every one of the guest's
 * streams and only one of them is on the surface: the sequence, drop,
 * discontinuity and byte counters are kept per stream and read back by
 * [sample]'s `streamId`, while frames, decode time, frame age and the round
 * trip stay socket-wide. See [HudSample] for why each field falls where it
 * does.
 *
 * Every figure is reconstructed from what the client already receives --
 * packet headers, decoder callbacks, and the pong answering a ping this class
 * was told about. Nothing here reads a clock: time arrives as a parameter, the
 * same discipline `crates/navette-viewer/src/hud.rs` keeps, so the rolling
 * windows are asserted against a scripted clock in a plain JVM test.
 *
 * **Not thread-safe.** `SessionController` reaches this from four threads --
 * the packet loop (`Dispatchers.Default`), the `MediaCodec` callback thread,
 * `hudJob` (`Main.immediate`), and OkHttp's reader thread via `onPong` -- and
 * guards every access with its own lock. Unlike the controller's gesture
 * state, which really is confined to one thread, this class depends entirely
 * on that external synchronization for safety.
 */
class SessionHud {
    /**
     * The counters that only mean anything within one stream.
     *
     * Kept per stream for the same reason `hud.rs` keeps a `StreamHud` per
     * stream (`session.rs:47`): sequence numbering is per-stream
     * (`bridge.rs:1177-1181`) and the hub replays every stream to a new
     * attachment (`media.rs:150-153`), so one shared sequence cursor reads
     * each hop between two streams as a gap of thousands of packets.
     */
    private class StreamCounters {
        val videoBytes = ArrayDeque<Pair<Long, Int>>()
        var lastSequence: Long? = null
        var packets: Long = 0
        var droppedPackets: Long = 0
        var discontinuities: Long = 0
    }

    private val streams = HashMap<Long, StreamCounters>()

    private val frames = ArrayDeque<Long>()
    private val pendingFeeds = ArrayDeque<Pair<Long, Long>>()

    private var decodeMs: Double? = null
    private var lastFrameAtMs: Long? = null

    private var pendingPing: Pair<ULong, Long>? = null
    private var rtt: Pair<Long, Long>? = null

    /** Records one packet's wire cost, sequence position and discontinuity flag. */
    fun recordPacket(nowMs: Long, packet: MediaPacket) {
        val streamId = packet.header.streamId
        // Mirrors the eviction at `session.rs:258`. Before getOrPut, so an
        // ended stream is not immediately given fresh counters to leak.
        if (packet.header.kind == MediaKind.STREAM_END) {
            streams.remove(streamId)
            return
        }
        val counters = streams.getOrPut(streamId) { StreamCounters() }
        noteSequence(counters, packet.header.sequence)
        if (packet.header.flags.discontinuity) counters.discontinuities = counters.discontinuities.saturatingInc()
        if (packet.header.kind == MediaKind.VIDEO) {
            counters.videoBytes.addLast(nowMs to packet.payload.size)
            trimBytes(counters, nowMs)
        }
    }

    /** Records that an access unit went into the decoder. */
    fun recordFed(timestampUs: Long, nowMs: Long) {
        pendingFeeds.addLast(timestampUs to nowMs)
        while (pendingFeeds.size > PENDING_FEEDS) pendingFeeds.removeFirst()
    }

    /** Records that a decoded picture reached the surface. */
    fun recordPresented(timestampUs: Long, nowMs: Long) {
        frames.addLast(nowMs)
        trimFrames(nowMs)
        lastFrameAtMs = nowMs
        val fed = pendingFeeds.firstOrNull { it.first == timestampUs } ?: return
        pendingFeeds.remove(fed)
        decodeMs = (nowMs - fed.second).toDouble()
    }

    /** Records that a ping went out. Any earlier unanswered ping is abandoned. */
    fun recordPing(nonce: ULong, nowMs: Long) {
        pendingPing = nonce to nowMs
    }

    /** Times a pong against the ping it answers, ignoring any it does not. */
    fun recordPong(nonce: ULong, nowMs: Long) {
        val (pending, sentAt) = pendingPing ?: return
        if (pending != nonce) return
        pendingPing = null
        rtt = (nowMs - sentAt) to nowMs
    }

    /**
     * Computes the current figures, discarding whatever has aged out.
     *
     * [streamId] is the stream actually on the surface -- `gate.primary`.
     * `null`, or a stream with no counters, reports blanks for the per-stream
     * half of [HudSample] rather than another stream's numbers; the
     * socket-wide half is reported regardless.
     */
    fun sample(nowMs: Long, streamId: Long?): HudSample {
        trimFrames(nowMs)
        val counters = streamId?.let { streams[it] }
        counters?.let { trimBytes(it, nowMs) }
        val videoBytes = counters?.videoBytes
        val bits = videoBytes?.sumOf { it.second.toDouble() * 8.0 } ?: 0.0
        val liveRtt = rtt?.takeIf { nowMs - it.second <= RTT_STALE_MS }?.first
        return HudSample(
            fps = rate(frames.size.toDouble(), frames.firstOrNull(), nowMs),
            bitrateBps = rate(bits, videoBytes?.firstOrNull()?.first, nowMs),
            decodeMs = decodeMs,
            ageMs = lastFrameAtMs?.let { nowMs - it },
            rttMs = liveRtt,
            droppedPackets = counters?.droppedPackets ?: 0L,
            discontinuities = counters?.discontinuities ?: 0L,
        )
    }

    /**
     * Counts packets the server never delivered.
     *
     * A sequence that does not advance is the hub replaying an older packet:
     * neither a drop nor a reason to rewind the baseline.
     */
    private fun noteSequence(counters: StreamCounters, sequence: Long) {
        val observed = counters.packets
        counters.packets = counters.packets.saturatingInc()
        val last = counters.lastSequence
        if (last == null) {
            counters.lastSequence = sequence
            return
        }
        if (sequence <= last) return
        if (observed >= BASELINE_PACKETS) {
            counters.droppedPackets = counters.droppedPackets.saturatingAdd(sequence - last - 1)
        }
        counters.lastSequence = sequence
    }

    private fun trimFrames(nowMs: Long) {
        while (frames.isNotEmpty() && nowMs - frames.first() > HUD_WINDOW_MS) frames.removeFirst()
    }

    private fun trimBytes(counters: StreamCounters, nowMs: Long) {
        val bytes = counters.videoBytes
        while (bytes.isNotEmpty() && nowMs - bytes.first().first > HUD_WINDOW_MS) bytes.removeFirst()
    }

    /**
     * A rate over the window actually observed, not the nominal one: a stream
     * half a second old must not report half its true rate.
     *
     * Mirrors `hud.rs`'s `rate()` exactly: a zero-or-negative span reports
     * `0.0` rather than dividing by a floored elapsed time. A real clock hits
     * a zero-length span constantly -- a frame presented and sampled inside
     * the same millisecond is routine, not exceptional -- so flooring the
     * divisor to 1ms would flash a wildly inflated reading (e.g. "FPS
     * 1000.0") on the overlay, and would diverge from the desktop client's
     * FPS/KBPS, which the protocol treats as directly comparable.
     */
    private fun rate(total: Double, oldestMs: Long?, nowMs: Long): Double {
        if (oldestMs == null || total == 0.0) return 0.0
        val elapsed = nowMs - oldestMs
        if (elapsed <= 0L) return 0.0
        return total * 1000.0 / elapsed.toDouble()
    }
}

/**
 * `hud.rs` counts with `saturating_add`; Kotlin has no equivalent, and a
 * 64-bit counter that did overflow would wrap negative and read as a HUD
 * going backwards. Clamping to the previous value pins it instead. Correct
 * only for a non-negative [delta], which is all these counters ever take.
 */
private fun Long.saturatingAdd(delta: Long): Long = (this + delta).coerceAtLeast(this)

private fun Long.saturatingInc(): Long = saturatingAdd(1L)
