package com.greponlabs.navette.ui.session

import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
import android.content.pm.ActivityInfo
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.ui.platform.LocalContext

/**
 * Sets landscape on entry and unlocks orientation on exit.
 *
 * Restores `UNSPECIFIED` rather than whatever `requestedOrientation` held on
 * entry: nothing else in this app sets it, and reading it back to restore it
 * is precisely what breaks if the Activity is ever recreated mid-lock -- the
 * captured "previous" would be the lock this effect just applied, leaving the
 * whole app landscape-locked for the rest of the process. `MainActivity`
 * declares `configChanges` so that recreation does not happen; this makes the
 * restore correct even if it did.
 *
 * The Activity is found by walking the `ContextWrapper` chain rather than
 * casting `LocalContext.current`: today's host is `MainActivity` calling
 * `setContent` directly, but a hard cast that starts crashing if that ever
 * changes is the worst failure mode for a screen with no automated coverage.
 */
@Composable
internal fun LockLandscapeWhileAttached() {
    val context = LocalContext.current
    DisposableEffect(context) {
        val activity = context.findActivity()
        activity?.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_LANDSCAPE
        onDispose { activity?.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED }
    }
}

private tailrec fun Context.findActivity(): Activity? =
    when (this) {
        is Activity -> this
        is ContextWrapper -> baseContext.findActivity()
        else -> null
    }
