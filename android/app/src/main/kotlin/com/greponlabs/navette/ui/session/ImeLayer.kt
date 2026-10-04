package com.greponlabs.navette.ui.session

import android.view.KeyEvent
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.autofill.ContentDataType
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.key.KeyEventType
import androidx.compose.ui.input.key.onPreviewKeyEvent
import androidx.compose.ui.input.key.type
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.semantics.contentDataType
import androidx.compose.ui.semantics.hideFromAccessibility
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import com.greponlabs.navette.net.ViewScale

/**
 * The on-screen-keyboard path: an off-screen text field that reports what the
 * IME commits, plus the controls at the top-right (Keys, Scale, Keyboard).
 *
 * The field is sized 1.dp at zero alpha rather than truly zero-size, since a
 * zero-size composable can be skipped by the IME system on some versions.
 *
 * The field is emptied only between compositions and only after the delta for
 * the current edit has been sent: a programmatic value change does not call
 * `onValueChange`, so the reset itself generates no backspaces. It is emptied
 * when a held modifier wrapped the typed text (Gboard committed "c" for
 * Ctrl+C, and a later Backspace must not delete text the guest never
 * received) and when the buffer passes [IME_BUFFER_LIMIT]. Everything else
 * advances `typed` in lockstep with the field, so every change is diffed
 * against exactly what preceded it. A reset mid-composition would restart the
 * IME's input session and drop the composition in flight, hence the
 * `composition == null` guard; with the password type Gboard does not compose,
 * so in practice the guard is always open.
 *
 * The toggle exists because focus is the screen's scarce resource: while the
 * field holds it the soft keyboard is up and consuming keys, and while the
 * video surface holds it a hardware keyboard works. Handing focus back to
 * [surfaceFocus] on dismissal is what restores the hardware path.
 *
 * [imeRaised] and [fieldFocus] are hoisted to the caller rather than owned
 * here: a reconnect's own focus-restoring effect needs both -- whether the
 * IME is up, and the exact [FocusRequester] to point back at -- to actively
 * re-assert the field's focus after every rebuild, not merely decline to
 * steal it (see that effect's comment for why the weaker form measured wrong
 * on a real device). [keyBarShown] is hoisted the same way because the bar it
 * controls is laid out by the caller, below the stream -- and it is the
 * *effective* visibility, not a pin flag, so the button's label never claims
 * something the screen contradicts (see [KeyBarState]).
 */
@Composable
internal fun BoxScope.ImeLayer(
    keyboard: SessionKeyboard,
    surfaceFocus: FocusRequester,
    fieldFocus: FocusRequester,
    imeRaised: Boolean,
    onImeRaisedChange: (Boolean) -> Unit,
    keyBarShown: Boolean,
    onToggleKeyBar: () -> Unit,
    viewScale: ViewScale,
    onViewScaleChange: (ViewScale) -> Unit,
) {
    val softKeyboard = LocalSoftwareKeyboardController.current

    // A reconnect replaces SessionKeyboard. Key the actual editable to that
    // instance, rather than merely updating its callback, so Compose disposes
    // the old editable and creates a fresh, empty field. The prior buffer and
    // composition must not carry over to the new controller. imeRaised is
    // hoisted, so the focus-restoring effect requests focus and shows the
    // keyboard again.
    key(keyboard) {
        var typed by remember { mutableStateOf(TextFieldValue()) }

        BasicTextField(
            value = typed,
            onValueChange = { next ->
                val chorded = keyboard.onImeText(previous = typed.text, current = next.text)
                val reset =
                    InputMapper.shouldResetImeBuffer(
                        text = next.text,
                        hasComposition = next.composition != null,
                        chorded = chorded,
                    )
                typed = if (reset) TextFieldValue() else next
            },
            keyboardOptions =
                KeyboardOptions(
                    // Password, not Text + no-suggestions: Compose 1.10.6 never
                    // sets TYPE_TEXT_FLAG_NO_SUGGESTIONS, and Gboard turns off
                    // suggestions, autocorrect and glide typing only for the
                    // password variation. Each of those otherwise arrives as a
                    // composition update the diff turns into a backspace-and-
                    // retype burst on the guest.
                    keyboardType = KeyboardType.Password,
                    autoCorrectEnabled = false,
                    // Default, not None or Done: with singleLine = false this is
                    // what adds IME_FLAG_NO_ENTER_ACTION, so Enter inserts "\n"
                    // and the diff maps it to KEY_ENTER (KeycodeMap.asciiCharToEvdev).
                    imeAction = ImeAction.Default,
                ),
            // The field is invisible anyway; None keeps the password type from
            // substituting bullets into `typed.text`, which is the diff base.
            visualTransformation = VisualTransformation.None,
            modifier =
                Modifier
                    .size(1.dp)
                    .alpha(0f)
                    // Belt and braces beside the view-level switch above: no
                    // autofill data type, and out of the accessibility tree that
                    // autofill walks. The field is invisible and 1.dp, so nothing
                    // is lost by hiding it from either.
                    .semantics {
                        contentDataType = ContentDataType.None
                        hideFromAccessibility()
                    }
                    .focusRequester(fieldFocus)
                    // Gboard deletes with deleteSurroundingText while there is
                    // text; on an empty field it sends KEYCODE_DEL as a key event
                    // instead, which the diff cannot see. Preview, not onKeyEvent:
                    // the field's own handler would consume DEL before it bubbled.
                    // The raw text is checked, not sendableText: an untypable
                    // leftover (a curly quote) still gives Gboard something to
                    // deleteSurroundingText, which produces neither a diff nor a
                    // key event -- the pre-existing "deleting only the unmapped
                    // character sends nothing" case. Repeats are skipped as the
                    // hardware path does; Gboard's own repeat is fresh down/up pairs.
                    .onPreviewKeyEvent { event ->
                        if (event.nativeKeyEvent.keyCode != KeyEvent.KEYCODE_DEL || typed.text.isNotEmpty()) {
                            return@onPreviewKeyEvent false
                        }
                        if (event.type == KeyEventType.KeyDown && event.nativeKeyEvent.repeatCount == 0) {
                            keyboard.onKeyBarKey(KeycodeMap.KEY_BACKSPACE)
                        }
                        true
                    },
        )
    }

    Row(modifier = Modifier.align(Alignment.TopEnd).padding(8.dp)) {
        TextButton(onClick = onToggleKeyBar) {
            Text(text = if (keyBarShown) "Hide keys" else "Keys", color = Color.White)
        }
        ScaleMenuButton(current = viewScale, onSelect = onViewScaleChange)
        TextButton(
            onClick = {
                val raised = !imeRaised
                onImeRaisedChange(raised)
                if (raised) {
                    fieldFocus.requestFocus()
                    softKeyboard?.show()
                } else {
                    softKeyboard?.hide()
                    surfaceFocus.requestFocus()
                }
            },
        ) {
            Text(text = if (imeRaised) "Hide keyboard" else "Keyboard", color = Color.White)
        }
    }
}

/**
 * The Scale menu's text for a preset. A UI string, so it lives here rather
 * than on `net.ViewScale`, which the pairing registry persists.
 */
private val ViewScale.displayLabel: String
    get() =
        when (this) {
            ViewScale.X1 -> "1\u00d7"
            ViewScale.X1_5 -> "1.5\u00d7"
            ViewScale.X2 -> "2\u00d7"
            ViewScale.X3 -> "3\u00d7"
        }

/**
 * A menu, not a cycle button: each step is an encoder restart on the daemon
 * (~0.8 s end to end), so a jump straight to 3x must be one tap, not two.
 * Selecting the current preset closes the menu and changes nothing.
 */
@Composable
private fun ScaleMenuButton(current: ViewScale, onSelect: (ViewScale) -> Unit) {
    var open by remember { mutableStateOf(false) }
    Box {
        TextButton(onClick = { open = true }) {
            Text(text = "Scale ${current.displayLabel}", color = Color.White)
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            for (scale in ViewScale.entries) {
                DropdownMenuItem(
                    text = { Text(if (scale == current) "${scale.displayLabel}  \u2713" else scale.displayLabel) },
                    onClick = {
                        open = false
                        onSelect(scale)
                    },
                )
            }
        }
    }
}
