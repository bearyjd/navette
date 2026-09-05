package com.greponlabs.navette.media

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.HandlerThread
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer

/** What the decoder tells the screen about its own lifecycle. */
sealed interface DecoderEvent {
    /**
     * A frame has just been rendered at this size. Reported on presentation
     * rather than on format change, so the screen's touch-coordinate mapping
     * tracks what is actually on the surface -- the same rule
     * `crates/navette-viewer/src/native.rs` follows for `content_size`.
     */
    data class Configured(val width: Int, val height: Int) : DecoderEvent

    /**
     * An access unit was dropped, so the frames that referenced it can no
     * longer decode. The caller must ask the bridge for a keyframe; nothing
     * else re-primes the picture before the encoder's own GOP cadence.
     */
    data object NeedsKeyframe : DecoderEvent

    data class Failed(val reason: String) : DecoderEvent
}

/**
 * Decodes one H.264 stream onto a [Surface] with `MediaCodec` in asynchronous
 * mode.
 *
 * **Lifecycle is caller-driven and strict.** [start] must not be called
 * before `SurfaceHolder.Callback.surfaceCreated`, and [stop] must complete
 * synchronously inside `surfaceDestroyed`: a `Surface` released out from
 * under a running codec throws.
 *
 * **Full teardown on reconfigure**, no adaptive playback. That matches the
 * Rust `StreamRouter` this otherwise mirrors, which also rebuilds rather than
 * resizing in place, and avoids committing to a max-size hint upfront for a
 * feature whose device support would need a fallback path anyway.
 *
 * **Two threads by construction**, unlike the other classes in this slice:
 * `MediaCodec`'s callbacks arrive on its own [HandlerThread], while [feed],
 * [start] and [stop] are called by the owner from elsewhere -- [feed] from
 * the background coroutine draining the media socket, [stop] from the main
 * thread inside `surfaceDestroyed`. Everything mutable below is guarded by
 * [lock] for that reason, and [onEvent] is always raised outside it.
 */
class H264Decoder(
    private val surface: Surface,
    private val onEvent: (DecoderEvent) -> Unit,
) {
    private class AccessUnit(val bytes: ByteArray, val timestampUs: Long)

    private val lock = Any()

    private var codec: MediaCodec? = null
    private var handlerThread: HandlerThread? = null

    /**
     * Terminal once [stop] has run: this instance is dead and [start] must
     * never build a codec again.
     *
     * That is what lets the owner call [start] outside its own lock. If the
     * `Surface` is torn down in the window between this decoder being
     * published and started, [stop] runs first and [start] then becomes a
     * no-op instead of configuring a codec onto a released `Surface`. The
     * owner always constructs a fresh instance per stream, so there is no
     * restart case to support.
     */
    private var released = false

    /** Input buffers the codec has offered that had no access unit waiting. */
    private val freeInputBuffers = ArrayDeque<Int>()

    /** Access units that arrived with no input buffer free. */
    private val pendingUnits = ArrayDeque<AccessUnit>()

    /**
     * Bytes held in [pendingUnits]. Bounding that queue by object count alone
     * is not a memory bound -- the protocol permits a 16 MiB access unit -- so
     * the byte total is tracked alongside the count.
     */
    private var pendingBytes = 0L

    /** Set by `onOutputFormatChanged`, reported only once a frame is actually rendered. */
    private var sizeAwaitingPresentation: Pair<Int, Int>? = null

    /** Builds the codec for [stream] and starts it. Safe to call only when stopped. */
    fun start(stream: PrimaryStream) {
        // The failure reason is carried out of the lock and reported after it
        // is released: onEvent reaches the caller, and calling out under a
        // lock this class's own callbacks also take invites a lock-order
        // problem for no benefit.
        val failure =
            synchronized(lock) {
                if (released) {
                    // stop() won the race: the Surface this was built for is
                    // already gone, so there is nothing to start onto.
                    Log.d(TAG, "ignoring start() on a decoder that was already released")
                    return
                }
                if (codec != null) return
                val thread = HandlerThread("navette-decode").apply { start() }
                handlerThread = thread

                val created =
                    try {
                        MediaCodec.createDecoderByType(MIME_TYPE)
                    } catch (error: Exception) {
                        thread.quitSafely()
                        handlerThread = null
                        return@synchronized "no H.264 decoder available: ${error.message}"
                    }

                try {
                    // The callback must be installed before configure(), per
                    // MediaCodec's asynchronous-mode contract.
                    created.setCallback(callback, android.os.Handler(thread.looper))
                    created.configure(formatFor(stream), surface, null, 0)
                    created.start()
                } catch (error: Exception) {
                    runCatching { created.release() }
                    thread.quitSafely()
                    handlerThread = null
                    return@synchronized "failed to start the H.264 decoder: ${error.message}"
                }
                codec = created
                Log.d(TAG, "decoder started at ${stream.width}x${stream.height}")
                null
            }
        if (failure != null) fail(failure)
    }

    /**
     * Queues one access unit. Dropped if the decoder is not running, if the
     * unit is implausibly large, or if the backlog is full -- every drop
     * raises [DecoderEvent.NeedsKeyframe], since the frames after a missing
     * access unit cannot decode without one.
     */
    fun feed(accessUnit: ByteArray, timestampUs: Long) {
        var dropped = false
        synchronized(lock) {
            val running = codec ?: return
            if (accessUnit.size > MAX_ACCESS_UNIT_BYTES) {
                // The protocol permits up to 16 MiB; no real access unit from
                // this bridge is anywhere near this. Rejecting here rather
                // than in submit() is what covers the pending path too, and is
                // what keeps pendingBytes bounded.
                Log.w(TAG, "access unit of ${accessUnit.size} bytes is implausible; dropping it")
                dropped = true
            } else {
                val index = freeInputBuffers.removeFirstOrNull()
                if (index == null) {
                    val full =
                        pendingUnits.size >= MAX_PENDING_ACCESS_UNITS ||
                            pendingBytes + accessUnit.size > MAX_PENDING_BYTES
                    if (full) {
                        // The NEWEST is dropped, not the oldest. Units already
                        // queued may be decode references for this one, so
                        // discarding them corrupts what is still in flight,
                        // where discarding the newest only costs the frames
                        // from here to the keyframe just asked for.
                        Log.w(TAG, "decoder input backlog full; dropping the incoming access unit")
                        dropped = true
                    } else {
                        pendingUnits.addLast(AccessUnit(accessUnit, timestampUs))
                        pendingBytes += accessUnit.size
                    }
                } else {
                    dropped = !submit(running, index, accessUnit, timestampUs)
                }
            }
        }
        // Raised outside the lock: the caller sends this on to the socket, and
        // holding the decoder lock across that is needless coupling.
        if (dropped) onEvent(DecoderEvent.NeedsKeyframe)
    }

    /**
     * Tears the codec down and marks this instance dead. Idempotent, and safe
     * to call from `surfaceDestroyed` -- including before [start] has run, in
     * which case [start] will find [released] and do nothing.
     */
    fun stop() {
        val (stopping, thread) =
            synchronized(lock) {
                released = true
                val stopping = codec
                val thread = handlerThread
                codec = null
                handlerThread = null
                freeInputBuffers.clear()
                pendingUnits.clear()
                pendingBytes = 0
                sizeAwaitingPresentation = null
                stopping to thread
            }
        if (stopping != null) {
            runCatching { stopping.stop() }
                .onFailure { Log.w(TAG, "decoder stop failed: ${it.message}") }
            runCatching { stopping.release() }
                .onFailure { Log.w(TAG, "decoder release failed: ${it.message}") }
        }
        thread?.quitSafely()
    }

    private fun formatFor(stream: PrimaryStream): MediaFormat {
        val format = MediaFormat.createVideoFormat(MIME_TYPE, stream.width, stream.height)
        // Setting csd-0/csd-1 explicitly rather than relying on the headers
        // repeated in-band on keyframes: in-band works on most decoders but
        // not all, and setting them here is idempotent when both are present.
        val (sps, pps) = AnnexB.splitSpsPps(stream.config.codecConfig)
        if (sps != null) format.setByteBuffer("csd-0", ByteBuffer.wrap(sps))
        if (pps != null) format.setByteBuffer("csd-1", ByteBuffer.wrap(pps))
        if (sps == null || pps == null) {
            Log.w(TAG, "stream config carried no ${if (sps == null) "SPS" else "PPS"}; relying on in-band headers")
        }
        return format
    }

    /**
     * Hands one access unit to the codec. Caller must hold [lock].
     *
     * Returns `false` when the unit was dropped, so the caller can raise
     * [DecoderEvent.NeedsKeyframe] *after* releasing the lock -- every drop
     * needs that, and calling out under the lock is what this class otherwise
     * avoids everywhere.
     */
    private fun submit(codec: MediaCodec, index: Int, accessUnit: ByteArray, timestampUs: Long): Boolean {
        try {
            val buffer = codec.getInputBuffer(index)
            if (buffer == null) {
                // The index is handed back rather than abandoned: leaking it
                // costs a buffer slot permanently, and if it really is stale
                // the next submit on it throws below and is caught.
                Log.w(TAG, "input buffer $index vanished; dropping an access unit")
                freeInputBuffers.addLast(index)
                return false
            }
            buffer.clear()
            if (buffer.remaining() < accessUnit.size) {
                // [feed] already rejected implausible sizes; this catches the
                // case only reachable here -- a device whose input buffers are
                // smaller than the unit at hand. Copying past the buffer throws
                // BufferOverflowException, which is not an IllegalStateException
                // and would escape onto whichever thread called this. Hand the
                // index back so the slot is not lost along with the unit.
                Log.w(TAG, "access unit of ${accessUnit.size} exceeds the input buffer; dropping it")
                freeInputBuffers.addLast(index)
                return false
            }
            buffer.put(accessUnit)
            codec.queueInputBuffer(index, 0, accessUnit.size, timestampUs, 0)
            return true
        } catch (error: IllegalStateException) {
            // The codec was torn down between the offer and this submission.
            // The index is deliberately not handed back -- stop() has already
            // cleared the pool, and this decoder is finished either way.
            Log.w(TAG, "dropping an access unit for a codec that is no longer running")
            return false
        }
    }

    private fun fail(reason: String) {
        Log.e(TAG, reason)
        onEvent(DecoderEvent.Failed(reason))
    }

    private val callback =
        object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {
                val dropped =
                    synchronized(lock) {
                        if (this@H264Decoder.codec !== codec) return
                        val unit = pendingUnits.removeFirstOrNull()
                        if (unit == null) {
                            freeInputBuffers.addLast(index)
                            return
                        }
                        pendingBytes -= unit.bytes.size
                        !submit(codec, index, unit.bytes, unit.timestampUs)
                    }
                // Same rule as feed(): every drop needs a keyframe, and the
                // event is raised only after the lock is released.
                if (dropped) onEvent(DecoderEvent.NeedsKeyframe)
            }

            override fun onOutputBufferAvailable(
                codec: MediaCodec,
                index: Int,
                info: MediaCodec.BufferInfo,
            ) {
                // Surface output needs no pixel handling: render=true hands the
                // frame straight to the compositor.
                //
                // The identity check and the render happen under the lock
                // together, which orders this against stop(): stop() must take
                // the lock to clear the field, so either it has not run yet and
                // this render completes before release(), or it has and the
                // identity check sends this callback away untouched. Rendering
                // into a released codec is not reliably just an exception.
                val size =
                    synchronized(lock) {
                        if (this@H264Decoder.codec !== codec) return
                        try {
                            codec.releaseOutputBuffer(index, true)
                        } catch (error: IllegalStateException) {
                            Log.w(TAG, "output buffer $index could not be rendered: ${error.message}")
                            return
                        }
                        sizeAwaitingPresentation.also { sizeAwaitingPresentation = null }
                    }
                if (size != null) onEvent(DecoderEvent.Configured(size.first, size.second))
            }

            override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {
                val size = displaySize(format)
                Log.d(TAG, "decoder output format is now ${size.first}x${size.second}")
                synchronized(lock) { sizeAwaitingPresentation = size }
            }

            override fun onError(codec: MediaCodec, error: MediaCodec.CodecException) {
                fail("H.264 decode failed: ${error.diagnosticInfo}")
            }
        }

    private companion object {
        const val TAG = "H264Decoder"
        const val MIME_TYPE = "video/avc"

        /**
         * Access units allowed to wait for an input buffer. The bridge runs at
         * tens of packets a second, so this is roughly a second of headroom --
         * enough to ride out a hiccup, short enough that a backlog this long
         * means the decoder is not keeping up rather than merely busy.
         */
        const val MAX_PENDING_ACCESS_UNITS = 32

        /**
         * Bytes allowed to sit in the pending queue. Live-session frames run a
         * median of 1.6 KiB and a maximum of 18 KiB, so 4 MiB is far more
         * headroom than the 32-unit count bound ever needs -- it exists to cap
         * what a misbehaving server can make this client hold, not to shape
         * ordinary traffic.
         */
        const val MAX_PENDING_BYTES = 4L * 1024 * 1024

        /**
         * The largest access unit worth trying to decode. The protocol permits
         * 16 MiB and a codec input buffer is a few hundred KB, so anything
         * approaching this is malformed rather than merely large. Checked in
         * [feed] so it covers the pending path as well; [submit] still checks
         * the real buffer, which is the only place the true size is known.
         */
        const val MAX_ACCESS_UNIT_BYTES = 4 * 1024 * 1024

        /**
         * The visible size, which is the crop rectangle when the decoder
         * reports one -- coded dimensions are padded up to macroblock
         * multiples, so using them directly would skew touch mapping on any
         * stream whose height is not a multiple of 16.
         */
        fun displaySize(format: MediaFormat): Pair<Int, Int> {
            val width = format.getInteger(MediaFormat.KEY_WIDTH)
            val height = format.getInteger(MediaFormat.KEY_HEIGHT)
            if (!format.containsKey("crop-right") || !format.containsKey("crop-bottom")) {
                return width to height
            }
            val left = if (format.containsKey("crop-left")) format.getInteger("crop-left") else 0
            val top = if (format.containsKey("crop-top")) format.getInteger("crop-top") else 0
            val cropped =
                (format.getInteger("crop-right") - left + 1) to (format.getInteger("crop-bottom") - top + 1)
            return if (cropped.first > 0 && cropped.second > 0) cropped else width to height
        }
    }
}
