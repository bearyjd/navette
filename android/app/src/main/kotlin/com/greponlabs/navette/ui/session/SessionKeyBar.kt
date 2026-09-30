package com.greponlabs.navette.ui.session

import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.unit.dp

/**
 * One key on the bar: a named evdev key, a character typed through
 * [KeycodeMap.asciiCharToEvdev], or a sticky modifier.
 */
private sealed interface BarKey {
    val label: String

    data class Named(override val label: String, val evdevCode: Int) : BarKey

    data class Char(override val label: String, val char: kotlin.Char) : BarKey

    data class Sticky(override val label: String, val key: StickyKey) : BarKey
}

/**
 * What a shell session needs and Gboard lacks, in the order a hand reaches
 * for them. The characters at the end are the ones Gboard hides behind its
 * symbols page; every one is in [KeycodeMap.asciiCharToEvdev]'s table.
 */
private val BAR_KEYS: List<BarKey> =
    listOf(
        BarKey.Named("Esc", KeycodeMap.KEY_ESC),
        BarKey.Named("Tab", KeycodeMap.KEY_TAB),
        BarKey.Sticky("Ctrl", StickyKey.Ctrl),
        BarKey.Sticky("Alt", StickyKey.Alt),
        BarKey.Named("↑", KeycodeMap.KEY_UP),
        BarKey.Named("↓", KeycodeMap.KEY_DOWN),
        BarKey.Named("←", KeycodeMap.KEY_LEFT),
        BarKey.Named("→", KeycodeMap.KEY_RIGHT),
        BarKey.Named("Home", KeycodeMap.KEY_HOME),
        BarKey.Named("End", KeycodeMap.KEY_END),
        BarKey.Named("PgUp", KeycodeMap.KEY_PAGEUP),
        BarKey.Named("PgDn", KeycodeMap.KEY_PAGEDOWN),
        BarKey.Named("Del", KeycodeMap.KEY_DELETE),
        BarKey.Char("~", '~'),
        BarKey.Char("|", '|'),
        BarKey.Char("-", '-'),
        BarKey.Char("/", '/'),
        BarKey.Char(":", ':'),
    )

/**
 * The tint for an armed or locked modifier. A literal rather than
 * `MaterialTheme.colorScheme.primary`: this bar sits on the session's black
 * video ground in both themes, and the light theme's primary is NavetteInk
 * (`ui/theme/Theme.kt`), which would vanish against it. The value is
 * NavetteTeal from the same file.
 */
private val ARMED_TINT = Color(0xFF7CE0C6)

private val LOCKED_GROUND = Color.White.copy(alpha = 0.2f)

/**
 * The row of keys between the stream and the IME.
 *
 * Every key goes out through [onKey] as a press/release pair -- the same
 * shape [InputMapper.imeTextDelta] uses to type a character -- wrapped in
 * whatever [modifiers] are held. The bar scrolls rather than wraps so it
 * costs one row of the stream's height, not two. No haptics, matching the
 * rest of `ui/session`. The chips do not take focus on tap, so the hidden
 * IME field keeps it and Gboard stays up (verified on the device checklist).
 */
@Composable
internal fun SessionKeyBar(
    modifiers: StickyModifiers,
    onKey: (evdevCode: Int, needsShift: Boolean) -> Unit,
    onModifierTapped: (StickyKey) -> Unit,
    onModifierLocked: (StickyKey) -> Unit,
    modifier: Modifier = Modifier,
) {
    Row(
        modifier
            .fillMaxWidth()
            .background(Color.Black)
            .horizontalScroll(rememberScrollState())
            .padding(horizontal = 4.dp),
    ) {
        for (key in BAR_KEYS) {
            when (key) {
                is BarKey.Named -> BarButton(key.label) { onKey(key.evdevCode, false) }
                is BarKey.Char ->
                    // Non-null for every listed character; `let` keeps the
                    // table the single source of truth for the code and shift.
                    KeycodeMap.asciiCharToEvdev(key.char)?.let { (code, shift) ->
                        BarButton(key.label) { onKey(code, shift) }
                    }
                is BarKey.Sticky ->
                    StickyKeyChip(
                        label = key.label,
                        state =
                            when (key.key) {
                                StickyKey.Ctrl -> modifiers.ctrl
                                StickyKey.Alt -> modifiers.alt
                            },
                        onClick = { onModifierTapped(key.key) },
                        onLongClick = { onModifierLocked(key.key) },
                    )
            }
        }
    }
}

/** The session screen's button style: white text on the video ground. */
@Composable
private fun BarButton(label: String, onClick: () -> Unit) {
    TextButton(onClick = onClick) { Text(label, color = Color.White) }
}

/** Tap = armed (tinted), long-press = locked (tinted on a filled chip), tap again = off. */
@Composable
private fun StickyKeyChip(
    label: String,
    state: StickyState,
    onClick: () -> Unit,
    onLongClick: () -> Unit,
) {
    val ground = if (state == StickyState.Locked) Modifier.background(LOCKED_GROUND, RoundedCornerShape(6.dp)) else Modifier
    Box(
        modifier =
            Modifier
                .padding(horizontal = 4.dp, vertical = 6.dp)
                .then(ground)
                .semantics {
                    stateDescription = state.spoken
                    selected = state.engaged
                }.combinedClickable(
                    onClickLabel = if (state == StickyState.Off) "arm for the next key" else "release",
                    onClick = onClick,
                    onLongClickLabel = "lock",
                    onLongClick = onLongClick,
                )
                .padding(horizontal = 12.dp, vertical = 8.dp),
    ) {
        Text(label, color = if (state == StickyState.Off) Color.White else ARMED_TINT)
    }
}

/**
 * What TalkBack reads after a chip's label. The tint and the filled ground are
 * the only other signal, so without this an armed or locked Ctrl sounds the
 * same as an idle one.
 */
internal val StickyState.spoken: String
    get() =
        when (this) {
            StickyState.Off -> "Off"
            StickyState.Armed -> "Armed for the next key"
            StickyState.Locked -> "Locked"
        }

/** Whether the chip reads as selected: armed and locked both hold the modifier. */
internal val StickyState.engaged: Boolean get() = this != StickyState.Off
