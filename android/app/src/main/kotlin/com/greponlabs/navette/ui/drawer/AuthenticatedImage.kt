package com.greponlabs.navette.ui.drawer

import androidx.compose.foundation.Image
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import com.greponlabs.navette.net.ImageRepository
import com.greponlabs.navette.net.Pairing

/**
 * An image from `navetted`'s bearer-authenticated HTTP API, or [placeholder]
 * until one is available. No image-loading library: the daemon's routes want
 * a token header that none of them speak natively, and two routes with
 * ETags do not justify a dependency.
 *
 * The bitmap state is keyed on [path] alone, so a card whose path changes
 * starts from that path's cache entry (or nothing) rather than briefly showing
 * the previous path's image; a change of [refreshKey] only re-runs the load,
 * keeping the current bitmap on screen while the daemon answers 304 or 200.
 * A cached image is revalidated only when there is a [refreshKey] to
 * revalidate on: icons pass `null` and are served from the cache for the
 * whole session, thumbnails pass the ViewModel's refresh tick.
 */
@Composable
fun AuthenticatedImage(
    repository: ImageRepository,
    pairing: Pairing,
    path: String,
    refreshKey: Any?,
    contentDescription: String?,
    modifier: Modifier = Modifier,
    contentScale: ContentScale = ContentScale.Crop,
    placeholder: @Composable () -> Unit,
) {
    var bitmap by remember(repository, pairing, path) { mutableStateOf(repository.cached(pairing, path)) }
    LaunchedEffect(repository, pairing, path, refreshKey) {
        val revalidate = bitmap != null && refreshKey != null
        bitmap = repository.load(pairing, path, revalidate)
    }

    val current = bitmap
    if (current == null) {
        placeholder()
    } else {
        val image = remember(current) { current.asImageBitmap() }
        Image(bitmap = image, contentDescription = contentDescription, modifier = modifier, contentScale = contentScale)
    }
}
