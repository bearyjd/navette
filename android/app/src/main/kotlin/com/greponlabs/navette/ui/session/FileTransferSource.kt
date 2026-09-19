package com.greponlabs.navette.ui.session

import android.content.ContentResolver
import android.database.Cursor
import android.net.Uri
import android.provider.OpenableColumns
import com.greponlabs.navette.net.MAX_BLOB_BYTES
import java.io.InputStream
import java.nio.charset.StandardCharsets

/** The largest payload accepted by the daemon's file-transfer endpoint. */
internal const val MAX_FILE_TRANSFER_BYTES: Long = MAX_BLOB_BYTES

/**
 * Re-openable file data. Retrying opens a new provider stream; bytes are never
 * cached in memory or on an app-owned temporary file.
 */
internal interface FileTransferSource {
    val name: String
    val mime: String
    val size: Long

    fun open(): InputStream?
}

/** Android DocumentsProvider-backed [FileTransferSource]. */
internal class ContentResolverFileTransferSource private constructor(
    private val resolver: ContentResolver,
    private val uri: Uri,
    override val name: String,
    override val mime: String,
    override val size: Long,
) : FileTransferSource {
    override fun open(): InputStream? = resolver.openInputStream(uri)

    companion object {
        /**
         * Refuse unknown sizes rather than streaming an unbounded provider.
         * This mirrors the daemon validation before it reserves session quota.
         */
        fun from(resolver: ContentResolver, uri: Uri): ContentResolverFileTransferSource? {
            val row =
                runCatching {
                    resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)
                        ?.use(::readDocumentMetadata)
                }.getOrNull() ?: return null
            val name = row.first ?: return null
            val size = row.second ?: return null
            val mime = runCatching { resolver.getType(uri) }.getOrNull() ?: "application/octet-stream"
            if (!isSafeFileTransferMetadata(name, mime, size)) return null
            return ContentResolverFileTransferSource(resolver, uri, name, mime, size)
        }

        private fun readDocumentMetadata(cursor: Cursor): Pair<String?, Long?> {
            if (!cursor.moveToFirst()) return null to null
            val name = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                .takeIf { it >= 0 && !cursor.isNull(it) }
                ?.let(cursor::getString)
            val size = cursor.getColumnIndex(OpenableColumns.SIZE)
                .takeIf { it >= 0 && !cursor.isNull(it) }
                ?.let(cursor::getLong)
            return name to size
        }
    }
}

internal fun isSafeFileTransferMetadata(name: String, mime: String, size: Long): Boolean =
    name.isNotEmpty() &&
        name.toByteArray(StandardCharsets.UTF_8).size <= 255 &&
        name != "." &&
        name != ".." &&
        '/' !in name &&
        '\\' !in name &&
        name.none(Char::isISOControl) &&
        size in 1..MAX_FILE_TRANSFER_BYTES &&
        isSafeMime(mime)

/** Kotlin mirror of the daemon's conservative RFC 9110 media-type validation. */
internal fun isSafeMime(mime: String): Boolean {
    if (mime.isEmpty() || mime.toByteArray(StandardCharsets.UTF_8).size > 255) return false
    val slash = mime.indexOf('/')
    if (slash <= 0 || slash != mime.lastIndexOf('/') || slash == mime.lastIndex) return false
    return mime.all {
        it == '/' ||
            it in 'a'..'z' ||
            it in 'A'..'Z' ||
            it in '0'..'9' ||
            it in "!#$%&'*+-.^_`|~"
    }
}
