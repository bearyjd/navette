package com.greponlabs.navette.ui.session

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * The performance HUD, drawn over the video.
 *
 * Renders a computed [HudSample] and calculates nothing -- every figure comes
 * from [SessionHud], which is where they can be tested. Monospaced, because a
 * proportional font makes a number that changes every second jitter sideways
 * and become much harder to read at a glance.
 *
 * Top-aligned: the bottom of this screen is where the IME and the gesture
 * surface live, and a HUD there would sit under the on-screen keyboard.
 */
@Composable
internal fun SessionHudOverlay(sample: HudSample?, visible: Boolean) {
    if (!visible || sample == null) return
    Box(modifier = Modifier.fillMaxWidth(), contentAlignment = Alignment.TopCenter) {
        Text(
            text = sample.format(),
            color = Color.White,
            fontFamily = FontFamily.Monospace,
            fontSize = 11.sp,
            modifier =
                Modifier
                    // Not transparent: this sits over live video, and white on
                    // a bright guest window is unreadable without a ground.
                    .background(Color.Black.copy(alpha = 0.6f))
                    .padding(horizontal = 8.dp, vertical = 4.dp),
        )
    }
}
