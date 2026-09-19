package com.greponlabs.navette.net

import android.graphics.Bitmap

/*
 * There is no Robolectric here, and no mocking framework by convention, so a
 * real Bitmap cannot be decoded in a JVM unit test: android.jar's
 * BitmapFactory is a stub that, with isReturnDefaultValues, answers null.
 * The mockable jar does, however, turn Bitmap's package-private no-arg
 * constructor into a no-op, so an instance can exist for identity-only
 * assertions -- which is all the cache and repository tests need.
 */

/** A distinct [Bitmap] instance with no pixels behind it; compare it by reference only. */
internal fun stubBitmap(): Bitmap = Bitmap::class.java.getDeclaredConstructor().apply { isAccessible = true }.newInstance()

/** A valid 1x1 opaque-black PNG, so the fetcher's tests send a real image body over the wire. */
internal val ONE_BY_ONE_PNG: ByteArray =
    byteArrayOf(
        0x89.toByte(), 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
        0x08, 0x02, 0x00, 0x00, 0x00, 0x90.toByte(), 0x77, 0x53,
        0xDE.toByte(), 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41,
        0x54, 0x78, 0x9C.toByte(), 0x63, 0x60, 0x60, 0x60, 0x00,
        0x00, 0x00, 0x04, 0x00, 0x01, 0xF6.toByte(), 0x17, 0x38,
        0x55, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
        0x44, 0xAE.toByte(), 0x42, 0x60, 0x82.toByte(),
    )
