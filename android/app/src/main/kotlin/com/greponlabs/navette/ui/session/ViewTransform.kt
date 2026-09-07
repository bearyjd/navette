package com.greponlabs.navette.ui.session

/** Fit-to-screen: the video's own pixel size fills the surface. */
const val MIN_ZOOM: Double = 1.0

/**
 * The most this screen magnifies. Past 1:1 the frames are upscaled, so this
 * is a legibility ceiling, not a quality one.
 */
const val MAX_ZOOM: Double = 4.0

/**
 * How the video is placed on the surface: `screen = local * zoom + offset`,
 * with the zoom pivoted at the top-left corner.
 *
 * Purely client-side. The `SurfaceView` is scaled and translated by exactly
 * these values; its layout size, the `ViewportResize` it reports, and the
 * guest window itself never change. That is what keeps zooming free of any
 * server round-trip, and it is also why zooming past 1:1 is soft: the
 * decoder keeps producing the same-sized frames and the GPU upscales them.
 *
 * A value: every operation returns a new instance and none mutates.
 */
data class ViewTransform(
    val zoom: Double = MIN_ZOOM,
    val offsetX: Double = 0.0,
    val offsetY: Double = 0.0,
) {
    val isZoomed: Boolean get() = zoom > MIN_ZOOM

    /**
     * Clamps [zoom] into `MIN_ZOOM..MAX_ZOOM` and the offsets so the content's
     * edges can never be pulled inside the screen. At [MIN_ZOOM] both offset
     * ranges collapse to `0.0`, which is what makes zooming all the way out
     * reset the pan for free. Unchanged for a degenerate surface size, which
     * a view can transiently report mid-layout.
     */
    fun clampedTo(surfaceWidth: Int, surfaceHeight: Int): ViewTransform {
        if (surfaceWidth <= 0 || surfaceHeight <= 0) return this
        val clampedZoom = zoom.coerceIn(MIN_ZOOM, MAX_ZOOM)
        // Zooming in moves content up and left, so an offset is never
        // positive: the range runs from the far edge (negative) up to zero.
        return ViewTransform(
            zoom = clampedZoom,
            offsetX = offsetX.coerceIn(surfaceWidth * (1.0 - clampedZoom), 0.0),
            offsetY = offsetY.coerceIn(surfaceHeight * (1.0 - clampedZoom), 0.0),
        )
    }

    /**
     * Scales by [scaleFactor] so the content under the screen point
     * ([focalX], [focalY]) stays under it -- what makes a pinch feel anchored
     * to the fingers rather than to a corner. The target zoom is clamped
     * *before* the offsets are derived from it, so hitting either limit
     * leaves the focal point where it was rather than sliding the view.
     * Unchanged for a non-finite or non-positive factor or focal point.
     */
    fun zoomedAbout(
        scaleFactor: Double,
        focalX: Double,
        focalY: Double,
        surfaceWidth: Int,
        surfaceHeight: Int,
    ): ViewTransform {
        if (!scaleFactor.isFinite() || scaleFactor <= 0.0 || zoom <= 0.0) return this
        if (!focalX.isFinite() || !focalY.isFinite()) return this
        val target = (zoom * scaleFactor).coerceIn(MIN_ZOOM, MAX_ZOOM)
        val ratio = target / zoom
        return copy(
            zoom = target,
            offsetX = focalX - (focalX - offsetX) * ratio,
            offsetY = focalY - (focalY - offsetY) * ratio,
        ).clampedTo(surfaceWidth, surfaceHeight)
    }

    /** Translates by a screen-space delta, then clamps. Unchanged for a non-finite delta. */
    fun pannedBy(dx: Double, dy: Double, surfaceWidth: Int, surfaceHeight: Int): ViewTransform {
        if (!dx.isFinite() || !dy.isFinite()) return this
        return copy(offsetX = offsetX + dx, offsetY = offsetY + dy).clampedTo(surfaceWidth, surfaceHeight)
    }

    /**
     * Lifts a point from the view's own (untransformed) coordinate space into
     * screen space. Android hands a transformed view its touch coordinates
     * already inverse-mapped, so gesture maths done in local space would see
     * the fingers move whenever the zoom changed under them; this puts them
     * back into the space the fingers physically occupy.
     */
    fun localToScreen(x: Float, y: Float): Pair<Double, Double> = (x * zoom + offsetX) to (y * zoom + offsetY)

    /** The inverse of [localToScreen]; `null` when [zoom] is not positive. */
    fun screenToLocal(x: Float, y: Float): Pair<Double, Double>? {
        if (zoom <= 0.0) return null
        return ((x - offsetX) / zoom) to ((y - offsetY) / zoom)
    }
}

/**
 * The current [ViewTransform], owned by the session screen and lent to each
 * controller it builds, so zoom and pan survive a reconnect's rebuild.
 *
 * A plain holder rather than Compose snapshot state on purpose: it is written
 * on every touch event of a pinch, and nothing in composition needs to react
 * to it -- the controller applies it to the view directly. Main-thread only,
 * like everything else on the touch path.
 */
class ViewTransformHolder {
    var value: ViewTransform = ViewTransform()
}
