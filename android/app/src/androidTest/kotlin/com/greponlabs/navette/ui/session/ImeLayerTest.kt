package com.greponlabs.navette.ui.session

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.assertTextEquals
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
import com.greponlabs.navette.media.PrimaryStream
import com.greponlabs.navette.net.MediaInput
import com.greponlabs.navette.net.StreamConfig
import com.greponlabs.navette.net.ViewScale
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test

/** Exercises the real editable and its state lifetime, rather than calling the mapper directly. */
class ImeLayerTest {
    @get:Rule
    val compose = createComposeRule()

    @Test
    fun reconnectResetsEditableAndRestoresFocusWithoutSendingOldTextOrEnter() {
        val oldEvents = mutableListOf<MediaInput>()
        val newEvents = mutableListOf<MediaInput>()
        val oldKeyboard = keyboard(oldEvents)
        val currentKeyboard = mutableStateOf(oldKeyboard)
        showLayer(currentKeyboard::value)

        compose.onNodeWithText("Keyboard").performClick()
        val field = compose.onNode(hasSetTextAction(), useUnmergedTree = true)
        field.assertIsFocused().performTextInput("abc")
        field.assertTextEquals("abc")
        compose.runOnIdle { assertEquals(plainKeys(30, 48, 46), oldEvents) }

        compose.runOnIdle { currentKeyboard.value = keyboard(newEvents) }

        // Check focus before performTextInput: the action itself requests focus
        // and would otherwise conceal a failed reconnect focus restoration.
        field.assertIsFocused().assertTextEquals("")
        compose.onNodeWithText("Hide keyboard").assertExists()
        compose.runOnIdle {
            assertEquals(plainKeys(30, 48, 46), oldEvents)
            assertEquals(emptyList<MediaInput>(), newEvents)
        }

        field.performTextInput("d")
        field.assertTextEquals("d")
        compose.runOnIdle {
            assertEquals(plainKeys(30, 48, 46), oldEvents)
            assertEquals(plainKeys(32), newEvents)
        }

        // Resetting the old editable must not disable an intentional IME Enter.
        field.performTextInput("\n")
        compose.runOnIdle { assertEquals(plainKeys(32, KeycodeMap.KEY_ENTER), newEvents) }
    }

    @Test
    fun ordinaryRecompositionKeepsBufferAndContinuesDiffAgainstPreviousText() {
        val events = mutableListOf<MediaInput>()
        val keyboard = keyboard(events)
        val scale = mutableStateOf(ViewScale.X1)
        showLayer({ keyboard }, scale::value)
        compose.onNodeWithText("Keyboard").performClick()
        val field = compose.onNode(hasSetTextAction(), useUnmergedTree = true)
        field.performTextInput("a")

        compose.runOnIdle { scale.value = ViewScale.X2 }
        field.assertIsFocused().assertTextEquals("a")
        field.performTextInput("b")
        field.assertTextEquals("ab")
        compose.runOnIdle { assertEquals(plainKeys(30, 48), events) }
    }

    private fun showLayer(
        currentKeyboard: () -> SessionKeyboard,
        currentScale: () -> ViewScale = { ViewScale.X1 },
    ) {
        compose.setContent {
            val keyboard = currentKeyboard()
            val fieldFocus = remember { FocusRequester() }
            val surfaceFocus = remember { FocusRequester() }
            val imeRaised = remember { mutableStateOf(false) }
            MaterialTheme {
                Box(Modifier.fillMaxSize()) {
                    ImeLayer(
                        keyboard = keyboard,
                        surfaceFocus = surfaceFocus,
                        fieldFocus = fieldFocus,
                        imeRaised = imeRaised.value,
                        onImeRaisedChange = { imeRaised.value = it },
                        keyBarShown = false,
                        onToggleKeyBar = {},
                        viewScale = currentScale(),
                        onViewScaleChange = {},
                    )
                }
            }
            // SessionScreen's controller-keyed effect reasserts field focus
            // on reconnect when the hoisted keyboard visibility stays raised.
            LaunchedEffect(keyboard) {
                if (imeRaised.value) fieldFocus.requestFocus()
            }
        }
    }

    private fun keyboard(events: MutableList<MediaInput>): SessionKeyboard {
        val stream = PrimaryStream(1L, StreamConfig(21L, 22L, byteArrayOf()), 1280, 720)
        return SessionKeyboard(primary = { stream }, send = { events.add(it) })
    }

    private fun plainKeys(vararg codes: Int): List<MediaInput> =
        codes.flatMap { code ->
            listOf(
                InputMapper.keyboardKey(21L, 22L, code, pressed = true),
                InputMapper.keyboardKey(21L, 22L, code, pressed = false),
            )
        }
}
