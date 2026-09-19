package com.greponlabs.navette.net

import android.graphics.Bitmap

/**
 * In-memory LRU of decoded images plus a short-lived record of which keys the
 * daemon answered 404 for, so a thumbnail that does not exist yet is not
 * re-requested on every recomposition of the card that wants it.
 *
 * Keys are whatever the caller chooses; [ImageRepository] uses the image's
 * absolute URL, so entries for one host can never be served for another.
 * Thread-safe: it is touched from the composition (initial value of a card)
 * and from the repository's coroutines. Timestamps are passed in rather than
 * read from a clock here, so the TTL is testable without one.
 */
class ImageCache(private val maxEntries: Int = DEFAULT_MAX_ENTRIES) {
    data class Entry(val bitmap: Bitmap, val etag: String?, val fetchedAt: Long)

    private val lock = Any()
    private val entries = lruMap<Entry>()
    private val missingUntil = lruMap<Long>()

    fun get(key: String): Entry? = synchronized(lock) { entries[key] }

    /** A fresh image supersedes a remembered 404 for the same key. */
    fun put(key: String, entry: Entry) {
        synchronized(lock) {
            missingUntil.remove(key)
            entries[key] = entry
        }
    }

    /** A 404 supersedes whatever image was held: a thumbnail the daemon no longer has is stale, not cached. */
    fun markMissing(key: String, now: Long) {
        synchronized(lock) {
            entries.remove(key)
            missingUntil[key] = now + MISSING_TTL_MS
        }
    }

    fun isMissing(key: String, now: Long): Boolean =
        synchronized(lock) {
            val until = missingUntil[key] ?: return false
            if (now < until) {
                true
            } else {
                missingUntil.remove(key)
                false
            }
        }

    fun clear() {
        synchronized(lock) {
            entries.clear()
            missingUntil.clear()
        }
    }

    private fun <V> lruMap(): LinkedHashMap<String, V> =
        object : LinkedHashMap<String, V>(INITIAL_CAPACITY, LOAD_FACTOR, true) {
            override fun removeEldestEntry(eldest: MutableMap.MutableEntry<String, V>?): Boolean = size > maxEntries
        }

    companion object {
        const val DEFAULT_MAX_ENTRIES = 64

        /** How long a 404 is believed before the route is asked again. */
        const val MISSING_TTL_MS = 10_000L

        private const val INITIAL_CAPACITY = 16
        private const val LOAD_FACTOR = 0.75f
    }
}
