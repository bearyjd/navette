package com.greponlabs.navette.net

import android.graphics.Bitmap
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.async
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.withContext
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

private val pairing = Pairing("tower", 9417, "ABCD1234ABCD1234ABCD1234")
private val path = appIconPath("firefox")

@OptIn(ExperimentalCoroutinesApi::class)
class ImageRepositoryTest {
    /** Hand-written fake, per this project's testing convention -- no mocking framework. */
    private class FakeImageFetcher : ImageFetcher {
        data class Call(val path: String, val etag: String?)

        val calls = mutableListOf<Call>()
        var respond: suspend (Call) -> ImageFetch = { ImageFetch.Failed }

        override suspend fun fetch(pairing: Pairing, path: String, etag: String?): ImageFetch {
            val call = Call(path, etag)
            calls.add(call)
            return respond(call)
        }
    }

    private class Harness(scope: TestScope) {
        var now = 0L
        val fetcher = FakeImageFetcher()
        val repository = ImageRepository(fetcher, ImageCache(), scope, clock = { now })

        /** Puts one image in the cache the way the daemon would: a 200 with an ETag. */
        suspend fun prime(etag: String = "\"v1\""): Bitmap {
            val bitmap = stubBitmap()
            fetcher.respond = { ImageFetch.Loaded(bitmap, etag) }
            return checkNotNull(repository.load(pairing, path, revalidate = false))
        }
    }

    @Test
    fun `a miss fetches without an ETag and caches the result`() =
        runTest {
            val h = Harness(this)
            val bitmap = stubBitmap()
            h.fetcher.respond = { ImageFetch.Loaded(bitmap, "\"v1\"") }

            assertSame(bitmap, h.repository.load(pairing, path, revalidate = false))

            assertEquals(listOf(FakeImageFetcher.Call(path, etag = null)), h.fetcher.calls)
            assertSame(bitmap, h.repository.cached(pairing, path))
        }

    @Test
    fun `a cache hit without revalidate makes no request`() =
        runTest {
            val h = Harness(this)
            val bitmap = h.prime()
            h.fetcher.respond = { error("must not be asked") }

            assertSame(bitmap, h.repository.load(pairing, path, revalidate = false))
            assertEquals(1, h.fetcher.calls.size)
        }

    @Test
    fun `cached never fetches`() =
        runTest {
            val h = Harness(this)
            assertNull(h.repository.cached(pairing, path))
            assertTrue(h.fetcher.calls.isEmpty())
        }

    @Test
    fun `revalidate sends the cached ETag, and a 304 keeps the image`() =
        runTest {
            val h = Harness(this)
            val bitmap = h.prime(etag = "\"v1\"")
            h.fetcher.respond = { ImageFetch.NotModified }

            assertSame(bitmap, h.repository.load(pairing, path, revalidate = true))

            assertEquals(FakeImageFetcher.Call(path, etag = "\"v1\""), h.fetcher.calls.last())
            assertSame(bitmap, h.repository.cached(pairing, path))
        }

    @Test
    fun `revalidate with a 200 replaces the image and the ETag sent next time`() =
        runTest {
            val h = Harness(this)
            h.prime(etag = "\"v1\"")
            val newer = stubBitmap()
            h.fetcher.respond = { ImageFetch.Loaded(newer, "\"v2\"") }

            assertSame(newer, h.repository.load(pairing, path, revalidate = true))
            assertSame(newer, h.repository.cached(pairing, path))

            h.fetcher.respond = { ImageFetch.NotModified }
            h.repository.load(pairing, path, revalidate = true)
            assertEquals("\"v2\"", h.fetcher.calls.last().etag)
        }

    @Test
    fun `a 404 is remembered for the TTL, then asked again`() =
        runTest {
            val h = Harness(this)
            h.fetcher.respond = { ImageFetch.Missing }

            assertNull(h.repository.load(pairing, path, revalidate = false))
            assertNull(h.repository.load(pairing, path, revalidate = false))
            assertNull("a refresh inside the TTL is answered from memory too", h.repository.load(pairing, path, revalidate = true))
            assertEquals(1, h.fetcher.calls.size)

            h.now += ImageCache.MISSING_TTL_MS
            assertNull(h.repository.load(pairing, path, revalidate = false))
            assertEquals(2, h.fetcher.calls.size)
        }

    @Test
    fun `a 404 on revalidation drops the cached image`() =
        runTest {
            val h = Harness(this)
            h.prime()
            h.fetcher.respond = { ImageFetch.Missing }

            assertNull(h.repository.load(pairing, path, revalidate = true))
            assertNull(h.repository.cached(pairing, path))
        }

    @Test
    fun `a failure keeps the cached image`() =
        runTest {
            val h = Harness(this)
            val bitmap = h.prime()
            h.fetcher.respond = { ImageFetch.Failed }

            assertSame(bitmap, h.repository.load(pairing, path, revalidate = true))
            assertSame(bitmap, h.repository.cached(pairing, path))
        }

    @Test
    fun `a failure with nothing cached is null and is not remembered like a 404`() =
        runTest {
            val h = Harness(this)
            h.fetcher.respond = { ImageFetch.Failed }

            assertNull(h.repository.load(pairing, path, revalidate = false))
            assertNull(h.repository.load(pairing, path, revalidate = false))
            assertEquals("a transient failure may be retried on the next load", 2, h.fetcher.calls.size)
        }

    @Test
    fun `a fetcher that throws is a failure, not a crash`() =
        runTest {
            val h = Harness(this)
            val bitmap = h.prime()
            h.fetcher.respond = { throw IllegalStateException("boom") }

            assertSame(bitmap, h.repository.load(pairing, path, revalidate = true))
            assertSame(bitmap, h.repository.cached(pairing, path))
        }

    @Test
    fun `concurrent loads for one path share one request`() =
        runTest {
            val h = Harness(this)
            val gate = CompletableDeferred<ImageFetch>()
            h.fetcher.respond = { gate.await() }

            val first = async { h.repository.load(pairing, path, revalidate = false) }
            val second = async { h.repository.load(pairing, path, revalidate = false) }
            runCurrent()
            assertEquals("both waited on one fetch", 1, h.fetcher.calls.size)

            val bitmap = stubBitmap()
            gate.complete(ImageFetch.Loaded(bitmap, "\"v1\""))
            assertSame(bitmap, first.await())
            assertSame(bitmap, second.await())
            assertEquals(1, h.fetcher.calls.size)
        }

    @Test
    fun `loads for different paths do not share a request`() =
        runTest {
            val h = Harness(this)
            val gate = CompletableDeferred<ImageFetch>()
            h.fetcher.respond = { gate.await() }

            val icon = async { h.repository.load(pairing, appIconPath("firefox"), revalidate = false) }
            val thumbnail = async { h.repository.load(pairing, sessionThumbnailPath("firefox-1"), revalidate = false) }
            runCurrent()
            assertEquals(2, h.fetcher.calls.size)

            gate.complete(ImageFetch.Missing)
            assertNull(icon.await())
            assertNull(thumbnail.await())
        }

    @Test
    fun `the same path on another host is another image`() =
        runTest {
            val h = Harness(this)
            h.prime()
            val other = Pairing("nas", 9417, "ZZZZ1234ZZZZ1234ZZZZ1234")

            assertNull(h.repository.cached(other, path))
            h.fetcher.respond = { ImageFetch.Missing }
            assertNull(h.repository.load(other, path, revalidate = false))
            assertEquals(2, h.fetcher.calls.size)
        }

    @Test
    fun `clear forgets cached images and cancels fetches in flight along with their waiters`() =
        runTest {
            val h = Harness(this)
            h.prime()
            val gate = CompletableDeferred<ImageFetch>()
            h.fetcher.respond = { gate.await() }
            val inFlight = async { h.repository.load(pairing, sessionThumbnailPath("firefox-1"), revalidate = false) }
            runCurrent()

            h.repository.clear()

            assertNull(h.repository.cached(pairing, path))
            gate.complete(ImageFetch.Loaded(stubBitmap(), "\"late\""))
            runCurrent()
            assertTrue("the waiter was cancelled along with the fetch", inFlight.isCancelled)
            assertNull(h.repository.cached(pairing, sessionThumbnailPath("firefox-1")))
        }

    @Test
    fun `an answer that lands after clear does not repopulate the cache`() =
        runTest {
            // HttpImageFetcher blocks in a socket read that cancellation cannot
            // interrupt, so its answer can arrive after clear() has run. The
            // fake reproduces that by finishing under NonCancellable.
            val h = Harness(this)
            val gate = CompletableDeferred<ImageFetch>()
            h.fetcher.respond = { withContext(NonCancellable) { gate.await() } }
            val thumbnail = sessionThumbnailPath("firefox-1")
            val inFlight = async { h.repository.load(pairing, thumbnail, revalidate = false) }
            runCurrent()
            assertEquals(1, h.fetcher.calls.size)

            h.repository.clear()
            gate.complete(ImageFetch.Loaded(stubBitmap(), "\"late\""))
            runCurrent()

            assertTrue(inFlight.isCancelled)
            assertNull("host A's late thumbnail must not outlive the switch to host B", h.repository.cached(pairing, thumbnail))
        }
}
