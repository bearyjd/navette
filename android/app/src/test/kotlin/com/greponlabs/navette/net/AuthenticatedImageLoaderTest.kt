package com.greponlabs.navette.net

import android.graphics.Bitmap
import java.io.ByteArrayInputStream
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.test.runTest
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import okio.Buffer
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

/**
 * The wire contract with `navetted`'s image routes, pinned against a real
 * socket like `WakeClientTest`: the daemon side is written against exactly
 * this request shape and these status meanings.
 *
 * Decoding is faked throughout -- see `TestBitmaps.kt` for why a JVM test
 * cannot decode a real one -- and the one thing asserted about the real
 * decoder is that it is the default.
 */
class AuthenticatedImageLoaderTest {
    private val token = "ABCD1234ABCD1234ABCD1234"
    private val thumbnailPath = sessionThumbnailPath("firefox-1")
    private lateinit var server: MockWebServer

    /** Records what it was asked to decode and answers with one fixed bitmap, or `null` when told to refuse. */
    private class RecordingDecoder(private val bitmap: Bitmap? = stubBitmap()) : BitmapDecoder {
        val bodies = mutableListOf<ByteArray>()

        override fun decode(bytes: ByteArray): Bitmap? {
            bodies.add(bytes)
            return bitmap
        }
    }

    @Before
    fun setUp() {
        server = MockWebServer()
        server.start()
    }

    @After
    fun tearDown() {
        runCatching { server.shutdown() }
    }

    private fun host() = Pairing(server.hostName, server.port, token)

    @Test
    fun `image url follows the WebSocket URLs' IPv6 bracketing`() {
        assertEquals("http://tower:9417/v1/apps/firefox/icon", imageUrl("tower", 9417, "/v1/apps/firefox/icon"))
        assertEquals("http://192.168.1.5:19417/v1/x", imageUrl("192.168.1.5", 19417, "/v1/x"))
        assertEquals("http://[fd7a:115c:a1e0::1]:9417/v1/x", imageUrl("fd7a:115c:a1e0::1", 9417, "/v1/x"))
    }

    @Test
    fun `routes match the daemon's, with the app id percent-encoded`() {
        assertEquals("/v1/sessions/firefox-1/thumbnail", sessionThumbnailPath("firefox-1"))
        assertEquals("/v1/apps/org.mozilla.firefox/icon", appIconPath("org.mozilla.firefox"))
        assertEquals("/v1/apps/my%20app/icon", appIconPath("my app"))
        assertEquals("/v1/apps/a%2Fb/icon", appIconPath("a/b"))
        assertEquals("/v1/apps/a%3Fb%23c/icon", appIconPath("a?b#c"))
    }

    @Test
    fun `a 200 is Loaded with the decoded body and the ETag, and the request is what the daemon expects`() =
        runTest {
            server.enqueue(MockResponse().setResponseCode(200).setHeader("ETag", "\"v1\"").setHeader("Content-Type", "image/png").setBody(Buffer().write(ONE_BY_ONE_PNG)))
            val decoder = RecordingDecoder()

            val result = HttpImageFetcher(decoder).fetch(host(), thumbnailPath, etag = null)

            val loaded = result as ImageFetch.Loaded
            assertEquals("\"v1\"", loaded.etag)
            assertEquals(1, decoder.bodies.size)
            assertArrayEquals(ONE_BY_ONE_PNG, decoder.bodies.single())

            val request = server.takeRequest(5, TimeUnit.SECONDS) ?: error("no request reached the daemon")
            assertEquals("GET", request.method)
            assertEquals("/v1/sessions/firefox-1/thumbnail", request.path)
            assertEquals("Bearer $token", request.getHeader("Authorization"))
            assertNull("no cached copy, so nothing to revalidate", request.getHeader("If-None-Match"))
            assertNull("the app never sends an Origin", request.getHeader("Origin"))
        }

    @Test
    fun `a 304 is NotModified, and the cached ETag went out as If-None-Match`() =
        runTest {
            server.enqueue(MockResponse().setResponseCode(304))
            val decoder = RecordingDecoder()

            assertEquals(ImageFetch.NotModified, HttpImageFetcher(decoder).fetch(host(), thumbnailPath, etag = "\"v1\""))

            val request = server.takeRequest(5, TimeUnit.SECONDS) ?: error("no request reached the daemon")
            assertEquals("\"v1\"", request.getHeader("If-None-Match"))
            assertTrue("nothing to decode on a 304", decoder.bodies.isEmpty())
        }

    @Test
    fun `a 404 is Missing`() =
        runTest {
            server.enqueue(MockResponse().setResponseCode(404).setBody("""{"error":"no thumbnail"}"""))
            assertEquals(ImageFetch.Missing, HttpImageFetcher(RecordingDecoder()).fetch(host(), thumbnailPath, etag = null))
        }

    @Test
    fun `any other status is Failed`() =
        runTest {
            listOf(400, 401, 403, 500, 503).forEach { code ->
                server.enqueue(MockResponse().setResponseCode(code).setBody("""{"error":"nope"}"""))
                assertEquals("HTTP $code", ImageFetch.Failed, HttpImageFetcher(RecordingDecoder()).fetch(host(), thumbnailPath, etag = null))
            }
        }

    @Test
    fun `a body the decoder rejects is Failed`() =
        runTest {
            server.enqueue(MockResponse().setResponseCode(200).setBody("not an image"))
            assertEquals(ImageFetch.Failed, HttpImageFetcher(RecordingDecoder(bitmap = null)).fetch(host(), thumbnailPath, etag = null))
        }

    @Test
    fun `a declared length over the cap is Failed without reading the body`() =
        runTest {
            // Throttled to 1 KiB/s: reading the body would take over half an
            // hour, so a prompt Failed proves the length header was enough.
            val oversized = Buffer().write(ByteArray(MAX_IMAGE_BYTES + 1))
            server.enqueue(MockResponse().setResponseCode(200).setBody(oversized).throttleBody(1024, 1, TimeUnit.SECONDS))
            val decoder = RecordingDecoder()

            val started = System.nanoTime()
            val result = HttpImageFetcher(decoder).fetch(host(), thumbnailPath, etag = null)
            val elapsedMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - started)

            assertEquals(ImageFetch.Failed, result)
            assertTrue("refused in ${elapsedMs}ms; reading past the cap would have taken far longer", elapsedMs < 10_000)
            assertTrue(decoder.bodies.isEmpty())
        }

    @Test
    fun `a chunked body that crosses the cap is Failed before it is decoded`() =
        runTest {
            val oversized = Buffer().write(ByteArray(MAX_IMAGE_BYTES + 1))
            server.enqueue(MockResponse().setResponseCode(200).setChunkedBody(oversized, 64 * 1024))
            val decoder = RecordingDecoder()

            assertEquals(ImageFetch.Failed, HttpImageFetcher(decoder).fetch(host(), thumbnailPath, etag = null))
            assertTrue(decoder.bodies.isEmpty())
        }

    @Test
    fun `readCapped keeps a body exactly at the cap and refuses one byte more`() {
        val atCap = ByteArray(1000) { it.toByte() }
        assertArrayEquals(atCap, readCapped(ByteArrayInputStream(atCap), cap = 1000))
        assertNull(readCapped(ByteArrayInputStream(ByteArray(1001)), cap = 1000))
        assertArrayEquals(ByteArray(0), readCapped(ByteArrayInputStream(ByteArray(0)), cap = 1000))
    }

    @Test
    fun `a redirect is not followed, so the bearer never reaches another authority`() =
        runTest {
            server.enqueue(MockResponse().setResponseCode(302).setHeader("Location", server.url("/elsewhere").toString()))
            server.enqueue(MockResponse().setResponseCode(200).setBody(Buffer().write(ONE_BY_ONE_PNG))) // what a follower would land on

            assertEquals(ImageFetch.Failed, HttpImageFetcher(RecordingDecoder()).fetch(host(), thumbnailPath, etag = null))
            assertEquals("exactly one request may reach the daemon", 1, server.requestCount)
        }

    @Test
    fun `a daemon that does not answer is Failed`() =
        runTest {
            val pairing = host()
            server.shutdown()

            assertEquals(ImageFetch.Failed, HttpImageFetcher(RecordingDecoder()).fetch(pairing, thumbnailPath, etag = null))
        }

    @Test
    fun `no outcome carries the token`() =
        runTest {
            // ImageFetch values end up in cache entries and, on failure, in
            // logs of them; the token must ride only in the request header.
            server.enqueue(MockResponse().setResponseCode(200).setHeader("ETag", "\"v1\"").setBody(Buffer().write(ONE_BY_ONE_PNG)))
            server.enqueue(MockResponse().setResponseCode(401))
            val fetcher = HttpImageFetcher(RecordingDecoder())

            val loaded = fetcher.fetch(host(), thumbnailPath, etag = null)
            val rejected = fetcher.fetch(host(), thumbnailPath, etag = null)

            assertTrue(loaded is ImageFetch.Loaded)
            assertEquals(ImageFetch.Failed, rejected)
            assertFalse(loaded.toString().contains(token))
            assertFalse(rejected.toString().contains(token))
        }

    @Test
    fun `a decoder that runs out of memory is Failed, not a crash`() =
        runTest {
            // OutOfMemoryError is an Error: the IOException and Exception nets
            // both miss it, and a 400 KB PNG of 20000x20000 pixels is enough.
            server.enqueue(MockResponse().setResponseCode(200).setBody(Buffer().write(ONE_BY_ONE_PNG)))
            val exploding = BitmapDecoder { throw OutOfMemoryError("Failed to allocate a 1600000016 byte allocation") }

            assertEquals(ImageFetch.Failed, HttpImageFetcher(exploding).fetch(host(), thumbnailPath, etag = null))
        }

    @Test
    fun `sampleSizeFor leaves an image within the cap at full size`() {
        assertEquals(1, sampleSizeFor(1, 1, MAX_DECODED_PIXELS))
        assertEquals(1, sampleSizeFor(320, 180, MAX_DECODED_PIXELS))
        assertEquals("exactly at the cap is within it", 1, sampleSizeFor(2000, 2000, MAX_DECODED_PIXELS))
    }

    @Test
    fun `sampleSizeFor halves by powers of two until the image is within the cap`() {
        assertEquals(2, sampleSizeFor(4000, 4000, MAX_DECODED_PIXELS))
        assertEquals(2, sampleSizeFor(8000, 2000, MAX_DECODED_PIXELS))
        assertEquals(4, sampleSizeFor(8000, 8000, MAX_DECODED_PIXELS))
        assertEquals(8, sampleSizeFor(16000, 16000, MAX_DECODED_PIXELS))
    }

    @Test
    fun `sampleSizeFor rejects what even a factor of 8 cannot bring within the cap`() {
        // 20000 x 20000 / 64 is still 6.25 MP.
        assertNull(sampleSizeFor(20000, 20000, MAX_DECODED_PIXELS))
        assertNull("no int overflow on absurd bounds", sampleSizeFor(Int.MAX_VALUE, Int.MAX_VALUE, MAX_DECODED_PIXELS))
    }

    @Test
    fun `sampleSizeFor rejects bounds BitmapFactory could not read`() {
        // inJustDecodeBounds leaves outWidth/outHeight at -1 (or 0) for bytes that are not an image.
        assertNull(sampleSizeFor(0, 0, MAX_DECODED_PIXELS))
        assertNull(sampleSizeFor(-1, -1, MAX_DECODED_PIXELS))
        assertNull(sampleSizeFor(10, 0, MAX_DECODED_PIXELS))
    }

    @Test
    fun `the default decoder is the platform's`() {
        assertSame(AndroidBitmapDecoder, HttpImageFetcher().decoder)
    }
}
