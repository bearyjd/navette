package com.greponlabs.navette.ui.session

import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Test

private const val WIDTH = 1000
private const val HEIGHT = 500
private const val EPSILON = 1e-9

class ViewTransformTest {
    @Test
    fun `at rest the video fits the screen with no offset`() {
        val transform = ViewTransform()

        assertEquals(1.0, transform.zoom, EPSILON)
        assertEquals(0.0, transform.offsetX, EPSILON)
        assertEquals(0.0, transform.offsetY, EPSILON)
        assertEquals(false, transform.isZoomed)
    }

    @Test
    fun `zoom is clamped into its range`() {
        val zoomedIn = ViewTransform().zoomedAbout(10.0, 0.0, 0.0, WIDTH, HEIGHT)
        assertEquals(MAX_ZOOM, zoomedIn.zoom, EPSILON)

        val zoomedOut = zoomedIn.zoomedAbout(0.01, 0.0, 0.0, WIDTH, HEIGHT)
        assertEquals(MIN_ZOOM, zoomedOut.zoom, EPSILON)
    }

    /**
     * The content point under the focal point before the zoom must be the
     * content point under it afterwards -- what makes a pinch feel anchored
     * to the fingers.
     */
    @Test
    fun `zooming about a focal point keeps the content under it in place`() {
        val focalX = 100.0
        val focalY = 200.0
        val before = ViewTransform()
        val (localX, localY) = checkNotNull(before.screenToLocal(focalX.toFloat(), focalY.toFloat()))

        val after = before.zoomedAbout(2.0, focalX, focalY, WIDTH, HEIGHT)

        assertEquals(2.0, after.zoom, EPSILON)
        val (screenX, screenY) = after.localToScreen(localX.toFloat(), localY.toFloat())
        assertEquals(focalX, screenX, EPSILON)
        assertEquals(focalY, screenY, EPSILON)
    }

    @Test
    fun `panning cannot pull the content's top-left edge inside the screen`() {
        val zoomed = ViewTransform(zoom = 2.0)

        val panned = zoomed.pannedBy(10_000.0, 10_000.0, WIDTH, HEIGHT)

        assertEquals(0.0, panned.offsetX, EPSILON)
        assertEquals(0.0, panned.offsetY, EPSILON)
    }

    @Test
    fun `panning cannot pull the content's bottom-right edge inside the screen`() {
        val zoomed = ViewTransform(zoom = 2.0)

        val panned = zoomed.pannedBy(-10_000.0, -10_000.0, WIDTH, HEIGHT)

        // At 2x the content is 2000x1000 on a 1000x500 screen, so the far
        // edge is reached when the offset is minus one screen.
        assertEquals(-1000.0, panned.offsetX, EPSILON)
        assertEquals(-500.0, panned.offsetY, EPSILON)
    }

    @Test
    fun `an in-range pan is applied exactly`() {
        val zoomed = ViewTransform(zoom = 2.0)

        val panned = zoomed.pannedBy(-300.0, -120.0, WIDTH, HEIGHT)

        assertEquals(-300.0, panned.offsetX, EPSILON)
        assertEquals(-120.0, panned.offsetY, EPSILON)
    }

    @Test
    fun `panning at each content edge returns the rejected same-sign residual`() {
        val leftTop = ViewTransform(zoom = 2.0)
        val atRightBottom = ViewTransform(zoom = 2.0, offsetX = -1000.0, offsetY = -500.0)

        val left = leftTop.panned(40.0, 0.0, WIDTH, HEIGHT)
        val top = leftTop.panned(0.0, 30.0, WIDTH, HEIGHT)
        val right = atRightBottom.panned(-40.0, 0.0, WIDTH, HEIGHT)
        val bottom = atRightBottom.panned(0.0, -30.0, WIDTH, HEIGHT)

        assertEquals(40.0, left.residualX, EPSILON)
        assertEquals(30.0, top.residualY, EPSILON)
        assertEquals(-40.0, right.residualX, EPSILON)
        assertEquals(-30.0, bottom.residualY, EPSILON)
    }

    @Test
    fun `a partial mixed-axis pan keeps local movement and returns only edge overflow`() {
        val result = ViewTransform(zoom = 2.0, offsetX = -980.0, offsetY = -100.0)
            .panned(-50.0, 40.0, WIDTH, HEIGHT)

        assertEquals(-1000.0, result.transform.offsetX, EPSILON)
        assertEquals(-60.0, result.transform.offsetY, EPSILON)
        assertEquals(-30.0, result.residualX, EPSILON)
        assertEquals(0.0, result.residualY, EPSILON)
    }

    @Test
    fun `a pan with room to move has no residual`() {
        val result = ViewTransform(zoom = 2.0, offsetX = -300.0, offsetY = -120.0)
            .panned(-50.0, 40.0, WIDTH, HEIGHT)

        assertEquals(0.0, result.residualX, EPSILON)
        assertEquals(0.0, result.residualY, EPSILON)
    }

    @Test
    fun `zooming all the way out resets the pan`() {
        val pannedWhileZoomed =
            ViewTransform(zoom = 2.0).pannedBy(-400.0, -200.0, WIDTH, HEIGHT)

        val fit = pannedWhileZoomed.zoomedAbout(0.5, 900.0, 400.0, WIDTH, HEIGHT)

        assertEquals(MIN_ZOOM, fit.zoom, EPSILON)
        assertEquals(0.0, fit.offsetX, EPSILON)
        assertEquals(0.0, fit.offsetY, EPSILON)
    }

    @Test
    fun `a degenerate surface size leaves the transform untouched`() {
        val transform = ViewTransform(zoom = 2.0, offsetX = -100.0, offsetY = -50.0)

        assertSame(transform, transform.clampedTo(0, HEIGHT))
        assertSame(transform, transform.clampedTo(WIDTH, 0))
        assertSame(transform, transform.clampedTo(-1, -1))
    }

    @Test
    fun `a non-finite or non-positive scale factor is ignored`() {
        val transform = ViewTransform(zoom = 2.0)

        assertSame(transform, transform.zoomedAbout(Double.NaN, 0.0, 0.0, WIDTH, HEIGHT))
        assertSame(transform, transform.zoomedAbout(Double.POSITIVE_INFINITY, 0.0, 0.0, WIDTH, HEIGHT))
        assertSame(transform, transform.zoomedAbout(0.0, 0.0, 0.0, WIDTH, HEIGHT))
        assertSame(transform, transform.zoomedAbout(-1.0, 0.0, 0.0, WIDTH, HEIGHT))
        assertSame(transform, transform.zoomedAbout(2.0, Double.NaN, 0.0, WIDTH, HEIGHT))
    }

    @Test
    fun `a non-finite pan is ignored`() {
        val transform = ViewTransform(zoom = 2.0)

        assertSame(transform, transform.pannedBy(Double.NaN, 0.0, WIDTH, HEIGHT))
        assertSame(transform, transform.pannedBy(0.0, Double.NEGATIVE_INFINITY, WIDTH, HEIGHT))
        assertEquals(PanResult(transform, 0.0, 0.0), transform.panned(Double.NaN, 0.0, WIDTH, HEIGHT))
    }

    @Test
    fun `a pan on a degenerate surface is ignored without a residual`() {
        val transform = ViewTransform(zoom = 2.0, offsetX = -100.0, offsetY = -50.0)

        assertEquals(PanResult(transform, 0.0, 0.0), transform.panned(10.0, 20.0, 0, HEIGHT))
    }

    @Test
    fun `pannedBy retains its legacy translated result for a degenerate surface`() {
        val transform = ViewTransform(zoom = 2.0, offsetX = -100.0, offsetY = -50.0)

        val zeroWidth = transform.pannedBy(10.0, 20.0, 0, HEIGHT)
        val negativeHeight = transform.pannedBy(-10.0, 20.0, WIDTH, -1)

        assertEquals(-90.0, zeroWidth.offsetX, EPSILON)
        assertEquals(-30.0, zeroWidth.offsetY, EPSILON)
        assertEquals(-110.0, negativeHeight.offsetX, EPSILON)
        assertEquals(-30.0, negativeHeight.offsetY, EPSILON)
    }

    @Test
    fun `screen and local coordinates round-trip`() {
        val transform = ViewTransform(zoom = 2.5, offsetX = -30.0, offsetY = -40.0)

        val (screenX, screenY) = transform.localToScreen(123.5f, 67.25f)
        val (localX, localY) = checkNotNull(transform.screenToLocal(screenX.toFloat(), screenY.toFloat()))

        assertEquals(123.5, localX, 1e-4)
        assertEquals(67.25, localY, 1e-4)
    }

    @Test
    fun `local to screen applies zoom then offset`() {
        val transform = ViewTransform(zoom = 2.0, offsetX = -100.0, offsetY = -50.0)

        val (screenX, screenY) = transform.localToScreen(300f, 100f)

        assertEquals(500.0, screenX, EPSILON)
        assertEquals(150.0, screenY, EPSILON)
    }

    @Test
    fun `screen to local is null for a non-positive zoom`() {
        assertEquals(null, ViewTransform(zoom = 0.0).screenToLocal(1f, 1f))
    }
}
