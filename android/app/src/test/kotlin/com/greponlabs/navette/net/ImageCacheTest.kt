package com.greponlabs.navette.net

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class ImageCacheTest {
    private fun entry(etag: String? = null, fetchedAt: Long = 0L) = ImageCache.Entry(stubBitmap(), etag, fetchedAt)

    @Test
    fun `holds what was put, by key`() {
        val cache = ImageCache(maxEntries = 4)
        val a = entry(etag = "\"a\"")
        cache.put("a", a)

        assertSame(a, cache.get("a"))
        assertNull(cache.get("b"))
    }

    @Test
    fun `evicts the least recently used entry, where a get counts as use`() {
        val cache = ImageCache(maxEntries = 2)
        val a = entry()
        val b = entry()
        val c = entry()
        cache.put("a", a)
        cache.put("b", b)
        cache.get("a") // a is now newer than b

        cache.put("c", c)

        assertNull("b was the least recently used", cache.get("b"))
        assertSame(a, cache.get("a"))
        assertSame(c, cache.get("c"))
    }

    @Test
    fun `untouched entries are evicted in insertion order`() {
        val cache = ImageCache(maxEntries = 2)
        cache.put("a", entry())
        cache.put("b", entry())
        cache.put("c", entry())

        assertNull(cache.get("a"))
        assertTrue(cache.get("b") != null && cache.get("c") != null)
    }

    @Test
    fun `a 404 is remembered for the TTL and then forgotten`() {
        val cache = ImageCache()
        val markedAt = 1_000L
        cache.markMissing("k", now = markedAt)

        assertTrue(cache.isMissing("k", now = markedAt))
        assertTrue(cache.isMissing("k", now = markedAt + ImageCache.MISSING_TTL_MS - 1))
        assertFalse(cache.isMissing("k", now = markedAt + ImageCache.MISSING_TTL_MS))
        // Expiry is sticky: an earlier `now` cannot resurrect it.
        assertFalse(cache.isMissing("k", now = markedAt))
    }

    @Test
    fun `keys never marked are not missing`() {
        assertFalse(ImageCache().isMissing("never", now = 0L))
    }

    @Test
    fun `a fresh image supersedes a remembered 404, and a 404 drops the image`() {
        val cache = ImageCache()
        cache.markMissing("k", now = 0L)
        cache.put("k", entry())
        assertFalse(cache.isMissing("k", now = 1L))
        assertTrue(cache.get("k") != null)

        cache.markMissing("k", now = 2L)
        assertNull(cache.get("k"))
        assertTrue(cache.isMissing("k", now = 3L))
    }

    @Test
    fun `clear forgets images and 404s alike`() {
        val cache = ImageCache()
        cache.put("a", entry())
        cache.markMissing("b", now = 0L)

        cache.clear()

        assertNull(cache.get("a"))
        assertFalse(cache.isMissing("b", now = 0L))
    }

    @Test
    fun `the default capacity is 64`() {
        assertEquals(64, ImageCache.DEFAULT_MAX_ENTRIES)
        val cache = ImageCache()
        repeat(65) { cache.put("k$it", entry()) }
        assertNull("the first one in is the first one out", cache.get("k0"))
        assertTrue(cache.get("k1") != null && cache.get("k64") != null)
    }
}
