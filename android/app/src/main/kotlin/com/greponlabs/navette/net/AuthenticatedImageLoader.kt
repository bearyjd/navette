package com.greponlabs.navette.net

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.io.InputStream
import java.net.HttpURLConnection
import java.net.URL
import kotlin.coroutines.cancellation.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import okhttp3.HttpUrl

/** One answer from an image route: `/v1/sessions/{name}/thumbnail` or `/v1/apps/{id}/icon`. */
sealed interface ImageFetch {
    /** A 200 whose body decoded; [etag] is what to send back as `If-None-Match` next time. */
    data class Loaded(val bitmap: Bitmap, val etag: String?) : ImageFetch

    /** A 304: the copy the caller already holds is still current. */
    data object NotModified : ImageFetch

    /** A 404: no thumbnail yet (never attached, or the daemon restarted), no icon, or the session is not live. */
    data object Missing : ImageFetch

    /** Anything else -- another status, a body over [MAX_IMAGE_BYTES], bytes that would not decode, or no HTTP answer at all. */
    data object Failed : ImageFetch
}

/**
 * What [ImageRepository] needs to fetch one image. Exists so tests can inject a
 * fake instead of standing up real networking -- see `ImageRepositoryTest`.
 */
interface ImageFetcher {
    suspend fun fetch(pairing: Pairing, path: String, etag: String?): ImageFetch
}

/**
 * Turns a response body into a [Bitmap], or `null` when the bytes are not an
 * image the platform can decode. Injectable because [BitmapFactory] is an
 * `android.jar` stub in JVM unit tests: with `isReturnDefaultValues` it
 * returns `null` for every input, so the HTTP contract in
 * `AuthenticatedImageLoaderTest` is pinned with a fake decoder instead.
 */
fun interface BitmapDecoder {
    fun decode(bytes: ByteArray): Bitmap?
}

/**
 * The platform decoder [HttpImageFetcher] uses by default. Reads the image's
 * bounds first: a 400 KB PNG can declare 20000x20000 pixels and decode to over
 * a gigabyte, so anything over [MAX_DECODED_PIXELS] is subsampled by
 * [sampleSizeFor], or refused when subsampling cannot bring it under.
 */
object AndroidBitmapDecoder : BitmapDecoder {
    override fun decode(bytes: ByteArray): Bitmap? {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeByteArray(bytes, 0, bytes.size, bounds)
        val sampleSize = sampleSizeFor(bounds.outWidth, bounds.outHeight, MAX_DECODED_PIXELS) ?: return null
        val options = BitmapFactory.Options().apply { inSampleSize = sampleSize }
        return BitmapFactory.decodeByteArray(bytes, 0, bytes.size, options)
    }
}

/**
 * Fetches an image from `navetted`'s HTTP API with the pairing's bearer token.
 * One request, no retry: the drawer asks again on its next refresh, and a
 * placeholder is the right thing to show meanwhile.
 */
class HttpImageFetcher(internal val decoder: BitmapDecoder = AndroidBitmapDecoder) : ImageFetcher {
    override suspend fun fetch(pairing: Pairing, path: String, etag: String?): ImageFetch =
        withContext(Dispatchers.IO) {
            try {
                val connection = URL(imageUrl(pairing.host, pairing.port, path)).openConnection() as HttpURLConnection
                try {
                    connection.requestMethod = "GET"
                    // As navette-cli's http_client: a redirect must not turn an
                    // authenticated request into one to another authority, and
                    // the API never needs redirects.
                    connection.instanceFollowRedirects = false
                    // The token never reaches a log: nothing below prints the connection or its headers.
                    connection.setRequestProperty("Authorization", "Bearer ${pairing.token}")
                    if (etag != null) connection.setRequestProperty("If-None-Match", etag)
                    connection.connectTimeout = CONNECT_TIMEOUT_MS
                    connection.readTimeout = READ_TIMEOUT_MS
                    when (connection.responseCode) {
                        HttpURLConnection.HTTP_OK -> decode(connection)
                        HttpURLConnection.HTTP_NOT_MODIFIED -> ImageFetch.NotModified
                        HttpURLConnection.HTTP_NOT_FOUND -> ImageFetch.Missing
                        else -> ImageFetch.Failed
                    }
                } finally {
                    connection.disconnect()
                }
            } catch (error: IOException) {
                ImageFetch.Failed
            }
        }

    private fun decode(connection: HttpURLConnection): ImageFetch {
        val body = readBodyCapped(connection) ?: return ImageFetch.Failed
        val bitmap = decodeSafely(body) ?: return ImageFetch.Failed
        return ImageFetch.Loaded(bitmap, connection.getHeaderField("ETag"))
    }

    /**
     * The one place a `Throwable` is caught: `OutOfMemoryError` is an `Error`,
     * so the IOException and Exception nets both let it through, and a decode
     * that ran out of memory is a Failed image, not a dead app. Undecodable
     * bytes and refused bounds are `null` and land in the same place.
     */
    private fun decodeSafely(body: ByteArray): Bitmap? =
        try {
            decoder.decode(body)
        } catch (error: CancellationException) {
            throw error
        } catch (error: Throwable) {
            null
        }

    /**
     * A declared length over the cap is refused before a byte of body is read;
     * an undeclared (chunked) one is refused the moment it crosses the cap, so
     * a hostile or broken daemon cannot make the phone buffer an unbounded
     * response.
     */
    private fun readBodyCapped(connection: HttpURLConnection): ByteArray? {
        if (connection.contentLengthLong > MAX_IMAGE_BYTES) return null
        return connection.inputStream.use { input -> readCapped(input, MAX_IMAGE_BYTES) }
    }

    private companion object {
        const val CONNECT_TIMEOUT_MS = 10_000
        const val READ_TIMEOUT_MS = 15_000
    }
}

/** The most an image route may send: a 320-px-wide JPEG thumbnail or a PNG icon is a small fraction of this. */
const val MAX_IMAGE_BYTES = 2 * 1024 * 1024

/**
 * The most pixels a decoded image may have: a 320-px-wide thumbnail is
 * ~60 k, a 512x512 icon 262 k, so 4 MP (16 MB of ARGB) is generous without
 * letting a hostile daemon allocate the phone's whole heap.
 */
const val MAX_DECODED_PIXELS = 4_000_000

/**
 * The power-of-two [BitmapFactory.Options.inSampleSize] that brings a
 * [width] x [height] image within [maxPixels], or `null` when even
 * [MAX_SAMPLE_SIZE] cannot, or when the bounds are not positive -- which is
 * what `inJustDecodeBounds` leaves behind for bytes that are not an image.
 */
fun sampleSizeFor(width: Int, height: Int, maxPixels: Int): Int? {
    if (width <= 0 || height <= 0) return null
    val pixels = width.toLong() * height.toLong()
    var sampleSize = 1
    while (pixels / (sampleSize.toLong() * sampleSize) > maxPixels) {
        if (sampleSize >= MAX_SAMPLE_SIZE) return null
        sampleSize *= 2
    }
    return sampleSize
}

/** Past 1/8 on each side the image is a 64th of what the daemon sent; refuse rather than show that. */
private const val MAX_SAMPLE_SIZE = 8

/** Reads [input] to its end, or returns `null` as soon as more than [cap] bytes have arrived. */
internal fun readCapped(input: InputStream, cap: Int): ByteArray? {
    val out = ByteArrayOutputStream()
    val buffer = ByteArray(READ_CHUNK_BYTES)
    while (true) {
        val read = input.read(buffer)
        if (read < 0) return out.toByteArray()
        if (out.size() + read > cap) return null
        out.write(buffer, 0, read)
    }
}

private const val READ_CHUNK_BYTES = 8 * 1024

/** An image route's absolute URL, with the same IPv6 bracketing as the WebSocket URLs. */
internal fun imageUrl(host: String, port: Int, path: String): String = "http://${formatAuthorityHost(host)}:$port$path"

/**
 * `GET /v1/sessions/{name}/thumbnail`. [session] is deliberately not
 * percent-encoded, for the reason [mediaWebSocketUrl] gives: `navetted` only
 * lets `[a-z0-9_-]` names exist.
 */
fun sessionThumbnailPath(session: String): String = "/v1/sessions/$session/thumbnail"

/**
 * `GET /v1/apps/{id}/icon`. App ids are XDG desktop-file ids and, unlike
 * session names, are not validated to a charset anywhere, so the segment is
 * percent-encoded -- via OkHttp, which is already on the classpath, rather
 * than a hand-rolled encoder. Ids made of `[A-Za-z0-9._-]`, which is all of
 * them in practice, come out unchanged.
 */
fun appIconPath(appId: String): String = "/v1/apps/${encodePathSegment(appId)}/icon"

private fun encodePathSegment(segment: String): String =
    HttpUrl.Builder().scheme("http").host("navette.invalid").addPathSegment(segment).build().encodedPathSegments.single()
