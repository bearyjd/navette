package com.greponlabs.navette.net

import android.graphics.Bitmap
import android.util.Log
import java.util.concurrent.ConcurrentHashMap
import kotlin.coroutines.cancellation.CancellationException
import kotlin.coroutines.coroutineContext
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Deferred
import kotlinx.coroutines.async
import kotlinx.coroutines.ensureActive

/**
 * The drawer's one source of thumbnails and icons: a cache in front of an
 * [ImageFetcher], with ETag revalidation and request coalescing.
 *
 * Cache keys are the image's absolute URL ([imageUrl]), so a fetch that was in
 * flight when the user switched computers lands under the old host's key and
 * can never be shown for the new one. [clear] is still called on every pairing
 * change so the old host's bitmaps do not sit in memory until the LRU gets to
 * them.
 *
 * Fetches run in [scope] -- the ViewModel's -- rather than the caller's, so
 * twenty cards asking for the same icon share one request, and the first card
 * to scroll out of view cancels only its own wait, not the fetch the other
 * nineteen are sharing.
 */
class ImageRepository(
    private val fetcher: ImageFetcher,
    private val cache: ImageCache,
    private val scope: CoroutineScope,
    private val clock: () -> Long = System::currentTimeMillis,
) {
    private val inFlight = ConcurrentHashMap<String, Deferred<Bitmap?>>()

    /** What the cache holds right now, for a card's first frame; never fetches. */
    fun cached(pairing: Pairing, path: String): Bitmap? = cache.get(keyFor(pairing, path))?.bitmap

    /**
     * The image to show for [path] on [pairing]'s host, or `null` for "show the
     * placeholder". With [revalidate] false a cached image is returned as is;
     * with it true the daemon is asked with the cached ETag, and a 304 or a
     * failure keeps what was cached while a 404 forgets it. A remembered 404
     * inside [ImageCache.MISSING_TTL_MS] is answered without a request either way.
     */
    suspend fun load(pairing: Pairing, path: String, revalidate: Boolean): Bitmap? {
        val key = keyFor(pairing, path)
        val cached = cache.get(key)
        if (cached != null && !revalidate) return cached.bitmap
        if (cached == null && cache.isMissing(key, clock())) return null
        return fetchCoalesced(key, pairing, path, cached)
    }

    /**
     * Forgets everything, including fetches still in flight: one that completed
     * after this would otherwise repopulate the cache for a host the user has
     * left. Their waiters, cards of the old drawer, are being torn down anyway.
     */
    fun clear() {
        inFlight.values.forEach { it.cancel() }
        inFlight.clear()
        cache.clear()
    }

    private suspend fun fetchCoalesced(key: String, pairing: Pairing, path: String, cached: ImageCache.Entry?): Bitmap? {
        val fetch =
            inFlight.computeIfAbsent(key) {
                // Lazy, so the completion handler is registered before the fetch
                // can possibly finish and try to remove itself.
                scope.async(start = CoroutineStart.LAZY) { fetchAndStore(key, pairing, path, cached) }
                    .also { deferred -> deferred.invokeOnCompletion { inFlight.remove(key, deferred) } }
            }
        return fetch.await()
    }

    private suspend fun fetchAndStore(key: String, pairing: Pairing, path: String, cached: ImageCache.Entry?): Bitmap? {
        val result = fetchSafely(pairing, path, cached?.etag)
        // A fetcher that returned normally after clear() cancelled this job (a
        // blocking socket read that finished anyway) must not write to a cache
        // the user has left behind: cancellation is only guaranteed to be
        // observed at a suspension point, and this is the last one.
        coroutineContext.ensureActive()
        return when (result) {
            is ImageFetch.Loaded -> {
                cache.put(key, ImageCache.Entry(result.bitmap, result.etag, clock()))
                result.bitmap
            }
            ImageFetch.NotModified -> cached?.bitmap
            ImageFetch.Missing -> {
                cache.markMissing(key, clock())
                null
            }
            ImageFetch.Failed -> cached?.bitmap
        }
    }

    /**
     * [HttpImageFetcher] already maps IOException; this is the net for anything
     * else, so a broken image can never take down the drawer that wanted it.
     */
    private suspend fun fetchSafely(pairing: Pairing, path: String, etag: String?): ImageFetch =
        try {
            fetcher.fetch(pairing, path, etag)
        } catch (error: CancellationException) {
            throw error
        } catch (error: Exception) {
            Log.w(TAG, "image $path failed: ${error.message}")
            ImageFetch.Failed
        }

    private fun keyFor(pairing: Pairing, path: String): String = imageUrl(pairing.host, pairing.port, path)

    private companion object {
        const val TAG = "ImageRepository"
    }
}
