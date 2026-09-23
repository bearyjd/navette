package com.greponlabs.navette.ui.session

import android.view.KeyEvent
import com.greponlabs.navette.net.BTN_LEFT
import com.greponlabs.navette.net.BTN_RIGHT
import com.greponlabs.navette.net.MAX_VIEWPORT_HEIGHT
import com.greponlabs.navette.net.MAX_VIEWPORT_WIDTH
import com.greponlabs.navette.net.MIN_VIEWPORT_HEIGHT
import com.greponlabs.navette.net.MIN_VIEWPORT_WIDTH
import com.greponlabs.navette.net.InputValidationError
import com.greponlabs.navette.net.MediaInput
import com.greponlabs.navette.net.mediaJson
import com.greponlabs.navette.net.validate
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

private const val CLIENT_ID = 11L
private const val SURFACE_ID = 12L

class InputMapperTest {
    private fun press(evdevCode: Int) = InputMapper.keyboardKey(CLIENT_ID, SURFACE_ID, evdevCode, pressed = true)

    private fun release(evdevCode: Int) = InputMapper.keyboardKey(CLIENT_ID, SURFACE_ID, evdevCode, pressed = false)

    private fun delta(previous: String, current: String) =
        InputMapper.imeTextDelta(CLIENT_ID, SURFACE_ID, previous, current)

    /**
     * Expected values are written as literals transcribed from
     * `/usr/include/linux/input-event-codes.h`, not from [KeycodeMap]'s own
     * named constants -- referring to those would assert the table against
     * itself and pass however wrong it was.
     */
    @Test
    fun `letters map to their evdev QWERTY-position codes`() {
        val expected =
            mapOf(
                KeyEvent.KEYCODE_A to 30,
                KeyEvent.KEYCODE_B to 48,
                KeyEvent.KEYCODE_C to 46,
                KeyEvent.KEYCODE_D to 32,
                KeyEvent.KEYCODE_E to 18,
                KeyEvent.KEYCODE_F to 33,
                KeyEvent.KEYCODE_G to 34,
                KeyEvent.KEYCODE_H to 35,
                KeyEvent.KEYCODE_I to 23,
                KeyEvent.KEYCODE_J to 36,
                KeyEvent.KEYCODE_K to 37,
                KeyEvent.KEYCODE_L to 38,
                KeyEvent.KEYCODE_M to 50,
                KeyEvent.KEYCODE_N to 49,
                KeyEvent.KEYCODE_O to 24,
                KeyEvent.KEYCODE_P to 25,
                KeyEvent.KEYCODE_Q to 16,
                KeyEvent.KEYCODE_R to 19,
                KeyEvent.KEYCODE_S to 31,
                KeyEvent.KEYCODE_T to 20,
                KeyEvent.KEYCODE_U to 22,
                KeyEvent.KEYCODE_V to 47,
                KeyEvent.KEYCODE_W to 17,
                KeyEvent.KEYCODE_X to 45,
                KeyEvent.KEYCODE_Y to 21,
                KeyEvent.KEYCODE_Z to 44,
            )

        for ((androidCode, evdevCode) in expected) {
            assertEquals("keycode $androidCode", evdevCode, KeycodeMap.androidKeycodeToEvdev(androidCode))
        }
    }

    @Test
    fun `digits map to the evdev number row where zero comes last`() {
        val expected =
            mapOf(
                KeyEvent.KEYCODE_1 to 2,
                KeyEvent.KEYCODE_2 to 3,
                KeyEvent.KEYCODE_3 to 4,
                KeyEvent.KEYCODE_4 to 5,
                KeyEvent.KEYCODE_5 to 6,
                KeyEvent.KEYCODE_6 to 7,
                KeyEvent.KEYCODE_7 to 8,
                KeyEvent.KEYCODE_8 to 9,
                KeyEvent.KEYCODE_9 to 10,
                KeyEvent.KEYCODE_0 to 11,
            )

        for ((androidCode, evdevCode) in expected) {
            assertEquals("keycode $androidCode", evdevCode, KeycodeMap.androidKeycodeToEvdev(androidCode))
        }
    }

    @Test
    fun `editing, modifier, navigation and function keys map to their evdev codes`() {
        val expected =
            mapOf(
                KeyEvent.KEYCODE_ESCAPE to 1,
                // Android's DEL is Backspace, not Delete -- getting these two
                // the wrong way round deletes in the wrong direction.
                KeyEvent.KEYCODE_DEL to 14,
                KeyEvent.KEYCODE_FORWARD_DEL to 111,
                KeyEvent.KEYCODE_TAB to 15,
                KeyEvent.KEYCODE_ENTER to 28,
                KeyEvent.KEYCODE_SPACE to 57,
                KeyEvent.KEYCODE_INSERT to 110,
                KeyEvent.KEYCODE_CAPS_LOCK to 58,
                KeyEvent.KEYCODE_NUM_LOCK to 69,
                KeyEvent.KEYCODE_SCROLL_LOCK to 70,
                KeyEvent.KEYCODE_SHIFT_LEFT to 42,
                KeyEvent.KEYCODE_SHIFT_RIGHT to 54,
                KeyEvent.KEYCODE_CTRL_LEFT to 29,
                KeyEvent.KEYCODE_CTRL_RIGHT to 97,
                KeyEvent.KEYCODE_ALT_LEFT to 56,
                KeyEvent.KEYCODE_ALT_RIGHT to 100,
                KeyEvent.KEYCODE_META_LEFT to 125,
                KeyEvent.KEYCODE_META_RIGHT to 126,
                KeyEvent.KEYCODE_DPAD_UP to 103,
                KeyEvent.KEYCODE_DPAD_DOWN to 108,
                KeyEvent.KEYCODE_DPAD_LEFT to 105,
                KeyEvent.KEYCODE_DPAD_RIGHT to 106,
                KeyEvent.KEYCODE_MOVE_HOME to 102,
                KeyEvent.KEYCODE_MOVE_END to 107,
                KeyEvent.KEYCODE_PAGE_UP to 104,
                KeyEvent.KEYCODE_PAGE_DOWN to 109,
                KeyEvent.KEYCODE_F1 to 59,
                KeyEvent.KEYCODE_F2 to 60,
                KeyEvent.KEYCODE_F3 to 61,
                KeyEvent.KEYCODE_F4 to 62,
                KeyEvent.KEYCODE_F5 to 63,
                KeyEvent.KEYCODE_F6 to 64,
                KeyEvent.KEYCODE_F7 to 65,
                KeyEvent.KEYCODE_F8 to 66,
                KeyEvent.KEYCODE_F9 to 67,
                KeyEvent.KEYCODE_F10 to 68,
                KeyEvent.KEYCODE_F11 to 87,
                KeyEvent.KEYCODE_F12 to 88,
            )

        for ((androidCode, evdevCode) in expected) {
            assertEquals("keycode $androidCode", evdevCode, KeycodeMap.androidKeycodeToEvdev(androidCode))
        }
    }

    @Test
    fun `punctuation maps to its evdev codes`() {
        val expected =
            mapOf(
                KeyEvent.KEYCODE_MINUS to 12,
                KeyEvent.KEYCODE_EQUALS to 13,
                KeyEvent.KEYCODE_LEFT_BRACKET to 26,
                KeyEvent.KEYCODE_RIGHT_BRACKET to 27,
                KeyEvent.KEYCODE_SEMICOLON to 39,
                KeyEvent.KEYCODE_APOSTROPHE to 40,
                KeyEvent.KEYCODE_GRAVE to 41,
                KeyEvent.KEYCODE_BACKSLASH to 43,
                KeyEvent.KEYCODE_COMMA to 51,
                KeyEvent.KEYCODE_PERIOD to 52,
                KeyEvent.KEYCODE_SLASH to 53,
            )

        for ((androidCode, evdevCode) in expected) {
            assertEquals("keycode $androidCode", evdevCode, KeycodeMap.androidKeycodeToEvdev(androidCode))
        }
    }

    @Test
    fun `an unmapped keycode returns null so the caller can drop it`() {
        assertNull(KeycodeMap.androidKeycodeToEvdev(KeyEvent.KEYCODE_VOLUME_UP))
        assertNull(KeycodeMap.androidKeycodeToEvdev(KeyEvent.KEYCODE_CAMERA))
        assertNull(KeycodeMap.androidKeycodeToEvdev(KeyEvent.KEYCODE_UNKNOWN))
    }

    @Test
    fun `every mapped keycode stays inside the protocol's keycode bound`() {
        // A fixed sweep rather than KeyEvent.getMaxKeyCode(): that is a
        // method, not a compile-time constant, so under the unit-test
        // android.jar stubs it returns 0 and would make this assert nothing.
        // 512 is comfortably past the highest KEYCODE_* the platform defines.
        var mapped = 0
        for (androidCode in 0..512) {
            val evdevCode = KeycodeMap.androidKeycodeToEvdev(androidCode) ?: continue
            mapped++
            // media.rs bounds keycode at 767; a table entry above it would be
            // refused client-side and never reach the guest.
            assertNull(
                "keycode $androidCode maps to an out-of-range evdev code $evdevCode",
                InputMapper.keyboardKey(CLIENT_ID, SURFACE_ID, evdevCode, true).validate(),
            )
        }
        // 26 letters + 10 digits + 26 editing/modifier/navigation + 12 function
        // keys + 11 punctuation. Pinned so a key silently dropped from the
        // table fails here rather than only on a device.
        assertEquals("the sweep must actually reach the table", 85, mapped)
    }

    @Test
    fun `no two android keycodes map to the same evdev code`() {
        // A copy-paste slip in a table this size shows up as a duplicate long
        // before anyone notices the wrong character on a device.
        val seen = mutableMapOf<Int, Int>()
        for (androidCode in 0..512) {
            val evdevCode = KeycodeMap.androidKeycodeToEvdev(androidCode) ?: continue
            val previous = seen.put(evdevCode, androidCode)
            assertNull("keycodes $previous and $androidCode both map to evdev $evdevCode", previous)
        }
    }

    @Test
    fun `a tap maps to a left button press and release`() {
        val pressed = InputMapper.pointerButton(CLIENT_ID, SURFACE_ID, BTN_LEFT, pressed = true)

        assertEquals(BTN_LEFT, pressed.button)
        assertTrue(pressed.pressed)
        assertNull(pressed.validate())
        assertTrue(!InputMapper.pointerButton(CLIENT_ID, SURFACE_ID, BTN_LEFT, pressed = false).pressed)
    }

    /**
     * Literals transcribed from `/usr/include/linux/input-event-codes.h`,
     * for the same reason the keycode tests use them.
     */
    @Test
    fun `left and right buttons are the evdev BTN codes and pass the bridge's bound`() {
        assertEquals(0x110, InputMapper.pointerButton(CLIENT_ID, SURFACE_ID, BTN_LEFT, pressed = true).button)
        assertEquals(0x111, InputMapper.pointerButton(CLIENT_ID, SURFACE_ID, BTN_RIGHT, pressed = true).button)
        for (button in listOf(BTN_LEFT, BTN_RIGHT)) {
            for (pressed in listOf(true, false)) {
                assertNull(InputMapper.pointerButton(CLIENT_ID, SURFACE_ID, button, pressed).validate())
            }
        }
    }

    @Test
    fun `pointer axis carries the surface identity and passes validation`() {
        val axis = InputMapper.pointerAxis(CLIENT_ID, SURFACE_ID, 1.5, -2.5)

        assertEquals(MediaInput.PointerAxis(CLIENT_ID.toULong(), SURFACE_ID.toULong(), 1.5, -2.5), axis)
        assertNull(axis.validate())
    }

    /**
     * The 8cc011b regression, pinned for the new sender too: a `client_id`
     * above `Long.MAX_VALUE` must serialise as a positive decimal or the
     * bridge's `u64` field rejects it and the scroll silently does nothing.
     */
    @Test
    fun `pointer axis serialises a client id above Long MAX_VALUE as an unsigned decimal`() {
        // The value from the real session that surfaced the original bug.
        val huge = java.lang.Long.parseUnsignedLong("15272202610726850855")
        val axis = InputMapper.pointerAxis(huge, SURFACE_ID, 0.0, 1.0)

        val json = mediaJson.encodeToString(MediaInput.serializer(), axis)
        assertTrue(json, json.contains("\"client_id\":15272202610726850855"))
        assertTrue(json, !json.contains("-"))
    }

    @Test
    fun `non-finite scroll is refused before the wire`() {
        assertEquals(
            InputValidationError.NonFiniteCoordinate,
            InputMapper.pointerAxis(CLIENT_ID, SURFACE_ID, Double.NaN, 0.0).validate(),
        )
        assertEquals(
            InputValidationError.NonFiniteCoordinate,
            InputMapper.pointerAxis(CLIENT_ID, SURFACE_ID, 0.0, Double.POSITIVE_INFINITY).validate(),
        )
    }

    /**
     * One pixel of finger travel is one pixel of guest scroll, negated: a
     * finger moving up (negative delta) asks for content further down, which
     * is Wayland's positive axis direction.
     */
    @Test
    fun `scroll units follow the finger one to one and invert the sign`() {
        assertEquals(-60.0, InputMapper.scrollUnits(60f), 1e-9)
        assertEquals(60.0, InputMapper.scrollUnits(-60f), 1e-9)
        assertEquals(0.0, InputMapper.scrollUnits(0f), 1e-9)
    }

    @Test
    fun `pointer motion carries the surface identity it is addressed to`() {
        val motion = InputMapper.pointerMotion(CLIENT_ID, SURFACE_ID, 12.5, 34.75)

        assertEquals(MediaInput.PointerMotion(CLIENT_ID.toULong(), SURFACE_ID.toULong(), 12.5, 34.75), motion)
        assertNull(motion.validate())
    }

    @Test
    fun `modifiers are derived from the meta-state bits`() {
        val metaState = KeyEvent.META_CTRL_ON or KeyEvent.META_SHIFT_ON or KeyEvent.META_CAPS_LOCK_ON
        val modifiers = InputMapper.keyboardModifiers(CLIENT_ID, SURFACE_ID, metaState)

        assertTrue(modifiers.ctrl)
        assertTrue(modifiers.shift)
        assertTrue(modifiers.capsLock)
        assertTrue(!modifiers.alt)
        assertTrue(!modifiers.logo)
        assertTrue(!modifiers.numLock)
        assertEquals(0, modifiers.layoutIndex)
        assertNull(modifiers.validate())
    }

    @Test
    fun `no meta-state bits means no modifiers held`() {
        val modifiers = InputMapper.keyboardModifiers(CLIENT_ID, SURFACE_ID, 0)

        assertEquals(
            MediaInput.KeyboardModifiers(
                CLIENT_ID.toULong(),
                SURFACE_ID.toULong(),
                false,
                false,
                false,
                false,
                false,
                false,
                0,
            ),
            modifiers,
        )
    }

    @Test
    fun `typing an unshifted character is one press and one release`() {
        assertEquals(listOf(press(30), release(30)), delta("", "a"))
    }

    @Test
    fun `typing a shifted character wraps the key in a shift press and release`() {
        assertEquals(
            listOf(press(42), press(30), release(30), release(42)),
            delta("", "A"),
        )
        // '!' is Shift plus the '1' key, not a key of its own.
        assertEquals(
            listOf(press(42), press(2), release(2), release(42)),
            delta("", "!"),
        )
    }

    @Test
    fun `appending several characters types them in order`() {
        assertEquals(
            listOf(press(35), release(35), press(23), release(23)),
            delta("", "hi"),
        )
        // Only the appended suffix is typed; the shared prefix is untouched.
        assertEquals(listOf(press(23), release(23)), delta("h", "hi"))
    }

    @Test
    fun `a shrink becomes one backspace pair per deleted character`() {
        assertEquals(
            listOf(press(14), release(14)),
            delta("abc", "ab"),
        )
        assertEquals(
            listOf(press(14), release(14), press(14), release(14)),
            delta("abc", "a"),
        )
    }

    /**
     * The autocomplete case the plan calls out: one `onValueChange` replacing
     * "teh" with "the". The shared prefix is "t", so two characters are
     * backspaced and "he" retyped.
     */
    @Test
    fun `an autocomplete-style replacement backspaces to the shared prefix and retypes`() {
        assertEquals(
            listOf(
                press(14), release(14),
                press(14), release(14),
                press(35), release(35),
                press(18), release(18),
            ),
            delta("teh", "the"),
        )
    }

    @Test
    fun `an unmapped character is dropped rather than throwing`() {
        // Neither the accented letter nor the emoji has an evdev code here;
        // the mapped characters around them still get typed.
        assertEquals(listOf(press(30), release(30)), delta("", "é😀a"))
        assertEquals(emptyList<MediaInput>(), delta("", "é"))
    }

    /**
     * The regression this guards: the backspace count must reflect what the
     * guest actually received, not what the text field holds. A curly
     * apostrophe (what an IME substitutes for `'` on its own) is untypable
     * here, so `"it’s"` reaches the guest as three characters. Counting
     * backspaces against the field's four would delete a character the user
     * never typed -- and nothing re-syncs the two afterwards.
     */
    @Test
    fun `backspaces count only characters the guest actually received`() {
        // Typing "it’s" sends i, t, s -- the curly apostrophe is dropped.
        assertEquals(
            listOf(press(23), release(23), press(20), release(20), press(31), release(31)),
            delta("", "it’s"),
        )

        // Clearing the field must undo exactly those three, not four.
        assertEquals(3, delta("it’s", "").count { it == press(14) })
        assertEquals(6, delta("it’s", "").size)
    }

    @Test
    fun `deleting only the unmapped character sends nothing`() {
        // The guest never saw the apostrophe, so removing it is a no-op there.
        assertEquals(emptyList<MediaInput>(), delta("it’s", "its"))
        assertEquals(emptyList<MediaInput>(), delta("a😀", "a"))
    }

    @Test
    fun `typing continues correctly after an unmapped character`() {
        // Guest holds "it"; the field gains a mapped char after the unmapped
        // one, so only that character is typed -- no spurious correction.
        assertEquals(listOf(press(31), release(31)), delta("it’", "it’s"))
    }

    @Test
    fun `sendableText keeps only what the keycode table can type`() {
        assertEquals("its", InputMapper.sendableText("it’s"))
        assertEquals("", InputMapper.sendableText("é😀"))
        assertEquals("Hello, world!", InputMapper.sendableText("Hello, world!"))
    }

    @Test
    fun `an unchanged value is a no-op`() {
        assertEquals(emptyList<MediaInput>(), delta("", ""))
        assertEquals(emptyList<MediaInput>(), delta("hello", "hello"))
    }

    @Test
    fun `every event an IME delta produces is in range`() {
        val events = delta("teh", "The Quick brown_fox! 42%")

        assertTrue(events.isNotEmpty())
        for (event in events) {
            assertNull("$event should be valid", event.validate())
        }
    }

    @Test
    fun `touch coordinates are rescaled into the presented frame's space`() {
        // Surface is twice the frame's width, so an x halfway across the
        // surface is halfway across the frame.
        assertEquals(
            320.0 to 240.0,
            InputMapper.rescaleToContent(640f, 480f, 1280, 960, 640, 480),
        )
        // The steady state: sizes agree, so nothing moves.
        assertEquals(
            100.0 to 50.0,
            InputMapper.rescaleToContent(100f, 50f, 1280, 720, 1280, 720),
        )
    }

    @Test
    fun `a degenerate surface size rescales to nothing rather than dividing by zero`() {
        assertNull(InputMapper.rescaleToContent(10f, 10f, 0, 720, 1280, 720))
        assertNull(InputMapper.rescaleToContent(10f, 10f, 1280, 0, 1280, 720))
    }

    @Test
    fun `viewport sizes are clamped into the range the bridge accepts`() {
        assertEquals(MIN_VIEWPORT_WIDTH to MIN_VIEWPORT_HEIGHT, InputMapper.clampViewport(100, 100))
        assertEquals(MAX_VIEWPORT_WIDTH to MAX_VIEWPORT_HEIGHT, InputMapper.clampViewport(9999, 9999))
        assertEquals(1280 to 720, InputMapper.clampViewport(1280, 720))
    }

    @Test
    fun `viewport sizes are rounded down to even`() {
        assertEquals(1280 to 720, InputMapper.clampViewport(1281, 721))
        assertEquals(MIN_VIEWPORT_WIDTH to MIN_VIEWPORT_HEIGHT, InputMapper.clampViewport(321, 241))
    }

    @Test
    fun `a clamped viewport is never one the bridge would reject`() {
        for (width in listOf(0, 1, 319, 320, 321, 1080, 3839, 3840, 3841, 100_000)) {
            for (height in listOf(0, 1, 239, 240, 241, 1080, 2159, 2160, 2161, 100_000)) {
                val clamped = InputMapper.clampViewport(width, height) ?: continue
                assertNull(
                    "${width}x$height clamped to $clamped, which the bridge rejects",
                    MediaInput.ViewportResize(clamped.first, clamped.second).validate(),
                )
            }
        }
    }

    @Test
    fun `a degenerate surface reports no viewport at all`() {
        assertNull(InputMapper.clampViewport(0, 720))
        assertNull(InputMapper.clampViewport(1280, -1))
    }

    @Test
    fun `a scaled viewport divides the surface and rounds down to even`() {
        assertEquals(1200 to 540, InputMapper.scaledViewport(2400, 1080, 2f))
        assertEquals(1600 to 720, InputMapper.scaledViewport(2400, 1080, 1.5f))
        // 2402 / 2 = 1201, which is odd; H.264 wants even, so 1200.
        assertEquals(1200 to 540, InputMapper.scaledViewport(2402, 1080, 2f))
    }

    /**
     * The aspect rule: a landscape phone with the IME up has ~450 px of
     * surface height, and 450 / 2 = 225 < 240. Clamping the height alone
     * would leave the width at 1200 and MediaCodec's scale-to-fit would
     * stretch the picture, so the divisor drops to what the height allows.
     */
    @Test
    fun `a scaled viewport keeps the aspect ratio when the minimum height is reached`() {
        // fit = min(2, 2400/320 = 7.5, 450/240 = 1.875) = 1.875
        assertEquals(1280 to 240, InputMapper.scaledViewport(2400, 450, 2f))
        // Landing exactly on the floor: fit = min(3, 4, 3) = 3; 1280/3 = 426.67 -> 426.
        assertEquals(426 to 240, InputMapper.scaledViewport(1280, 720, 3f))
    }

    @Test
    fun `a scaled viewport at 1x is exactly the clamped viewport`() {
        for (width in listOf(1, 319, 320, 321, 1080, 2400, 3840, 3841, 100_000)) {
            for (height in listOf(1, 239, 240, 241, 450, 1080, 2160, 2161, 100_000)) {
                assertEquals(
                    "${width}x$height at 1x",
                    InputMapper.clampViewport(width, height),
                    InputMapper.scaledViewport(width, height, 1f),
                )
            }
        }
    }

    @Test
    fun `a scaled viewport is never one the bridge would reject`() {
        for (width in listOf(320, 321, 1080, 2400, 3840)) {
            for (height in listOf(240, 241, 450, 1080, 2160)) {
                for (factor in listOf(1f, 1.5f, 2f, 3f)) {
                    val scaled = InputMapper.scaledViewport(width, height, factor)
                    assertTrue("${width}x$height at $factor produced nothing", scaled != null)
                    scaled ?: continue
                    assertNull(
                        "${width}x$height at $factor scaled to $scaled, which the bridge rejects",
                        MediaInput.ViewportResize(scaled.first, scaled.second).validate(),
                    )
                }
            }
        }
    }

    @Test
    fun `a degenerate surface or factor scales to no viewport at all`() {
        assertNull(InputMapper.scaledViewport(0, 720, 2f))
        assertNull(InputMapper.scaledViewport(1280, 0, 2f))
        assertNull(InputMapper.scaledViewport(1280, 720, 0.5f))
        assertNull(InputMapper.scaledViewport(1280, 720, Float.NaN))
        assertNull(InputMapper.scaledViewport(1280, 720, Float.POSITIVE_INFINITY))
    }

    private fun held(ctrl: Boolean = false, alt: Boolean = false) = HeldModifiers(ctrl = ctrl, alt = alt)

    private fun modifiers(ctrl: Boolean = false, alt: Boolean = false) =
        InputMapper.heldModifiers(CLIENT_ID, SURFACE_ID, held(ctrl, alt))

    @Test
    fun `a plain chord is one press and one release`() {
        assertEquals(
            listOf(press(1), release(1)),
            InputMapper.keyChord(CLIENT_ID, SURFACE_ID, 1, needsShift = false, held = HeldModifiers.NONE),
        )
    }

    /**
     * The raw KEY_LEFTCTRL press is the load-bearing part: wprsd derives
     * ctrl/alt from raw key presses through xkb and ignores those flags in
     * `KeyboardEvent::Modifiers`. The Modifiers pair is sent as well so the
     * wire matches what a hardware Ctrl produces.
     */
    @Test
    fun `a ctrl chord wraps the key in a raw ctrl press and the modifiers pair`() {
        assertEquals(
            listOf(press(29), modifiers(ctrl = true), press(46), release(46), modifiers(), release(29)),
            InputMapper.keyChord(CLIENT_ID, SURFACE_ID, 46, needsShift = false, held = held(ctrl = true)),
        )
    }

    @Test
    fun `ctrl is outermost, alt inside it, shift innermost, and releases mirror the presses`() {
        assertEquals(
            listOf(
                press(29),
                press(56),
                modifiers(ctrl = true, alt = true),
                press(42),
                press(2),
                release(2),
                release(42),
                modifiers(),
                release(56),
                release(29),
            ),
            InputMapper.keyChord(CLIENT_ID, SURFACE_ID, 2, needsShift = true, held = held(ctrl = true, alt = true)),
        )
    }

    @Test
    fun `held modifiers carry only ctrl and alt`() {
        val ctrlAlt = modifiers(ctrl = true, alt = true)
        assertTrue(ctrlAlt.ctrl)
        assertTrue(ctrlAlt.alt)
        assertTrue(!ctrlAlt.shift)
        assertTrue(!ctrlAlt.capsLock)
        assertTrue(!ctrlAlt.logo)
        assertTrue(!ctrlAlt.numLock)
        assertEquals(0, ctrlAlt.layoutIndex)
        assertNull(ctrlAlt.validate())
        assertEquals(InputMapper.keyboardModifiers(CLIENT_ID, SURFACE_ID, 0), modifiers())
    }

    @Test
    fun `every event a chord produces is in range`() {
        val barKeys = listOf(1, 15, 103, 108, 105, 106, 102, 107, 104, 109, 111, 14, 41, 43)
        val combos = listOf(held(), held(ctrl = true), held(alt = true), held(ctrl = true, alt = true))
        for (code in barKeys) {
            for (combo in combos) {
                for (shift in listOf(false, true)) {
                    for (event in InputMapper.keyChord(CLIENT_ID, SURFACE_ID, code, shift, combo)) {
                        assertNull("$event should be valid", event.validate())
                    }
                }
            }
        }
    }

    @Test
    fun `a chorded IME character is the chord for that key`() {
        assertEquals(
            listOf(press(29), modifiers(ctrl = true), press(46), release(46), modifiers(), release(29)),
            InputMapper.imeTextDelta(CLIENT_ID, SURFACE_ID, "", "c", held(ctrl = true)),
        )
        // A shifted character keeps its shift wrap inside the chord.
        assertEquals(
            listOf(press(56), modifiers(alt = true), press(42), press(30), release(30), release(42), modifiers(), release(56)),
            InputMapper.imeTextDelta(CLIENT_ID, SURFACE_ID, "", "A", held(alt = true)),
        )
    }

    /**
     * "Armed for the next key" means one key. A single edit can carry several
     * characters -- a paste, or a keyboard that commits a whole word -- and
     * chording each of them turned a paste of "abc" under armed Ctrl into
     * Ctrl+A, Ctrl+B, Ctrl+C, where Ctrl+A alone is select-all in most guests.
     */
    @Test
    fun `only the first character of a multi-character commit is chorded`() {
        assertEquals(
            listOf(press(29), modifiers(ctrl = true), press(30), release(30), modifiers(), release(29)) +
                listOf(press(48), release(48)) +
                listOf(press(46), release(46)),
            InputMapper.imeTextDelta(CLIENT_ID, SURFACE_ID, "", "abc", held(ctrl = true)),
        )
    }

    @Test
    fun `the first character of a multi-character commit keeps its own shift`() {
        // "Ab" under armed Alt: Alt wraps the shifted A, and b goes out plain.
        assertEquals(
            listOf(press(56), modifiers(alt = true), press(42), press(30), release(30), release(42), modifiers(), release(56)) +
                listOf(press(48), release(48)),
            InputMapper.imeTextDelta(CLIENT_ID, SURFACE_ID, "", "Ab", held(alt = true)),
        )
    }

    /**
     * The characters after a replacement are a continuation of the same edit,
     * not new keys: only the first one the guest receives is chorded.
     */
    @Test
    fun `a replacement under an armed modifier chords only its first retyped character`() {
        val events = InputMapper.imeTextDelta(CLIENT_ID, SURFACE_ID, "teh", "the", held(ctrl = true))

        assertEquals("one chord for the whole edit", 1, events.count { it == press(29) })
        assertEquals(1, events.count { it == release(29) })
        // The backspaces still lead, unchorded.
        assertEquals(listOf(press(14), release(14), press(14), release(14)), events.take(4))
        assertEquals("the chord opens on the first retyped character", press(29), events[4])
    }

    /** A deletion is the IME reconciling its own buffer, not a user chord. */
    @Test
    fun `a chorded delta still deletes unchorded`() {
        assertEquals(
            listOf(press(14), release(14)),
            InputMapper.imeTextDelta(CLIENT_ID, SURFACE_ID, "ab", "a", held(ctrl = true)),
        )
    }

    @Test
    fun `the default held argument produces the unchorded delta`() {
        assertEquals(delta("teh", "the"), InputMapper.imeTextDelta(CLIENT_ID, SURFACE_ID, "teh", "the", HeldModifiers.NONE))
    }

    @Test
    fun `the hidden field is emptied once its buffer passes the limit`() {
        val atLimit = "a".repeat(IME_BUFFER_LIMIT)

        assertFalse(InputMapper.shouldResetImeBuffer(atLimit, hasComposition = false, chorded = false))
        assertTrue(InputMapper.shouldResetImeBuffer(atLimit + "a", hasComposition = false, chorded = false))
        assertFalse(InputMapper.shouldResetImeBuffer("echo test", hasComposition = false, chorded = false))
        assertFalse(InputMapper.shouldResetImeBuffer("", hasComposition = false, chorded = false))
    }

    /**
     * Only what the guest received counts toward the limit: a field full of
     * untypable characters has sent nothing, so emptying it would diff to
     * nothing anyway -- and the reset restarts the IME's input session.
     */
    @Test
    fun `characters the guest never received do not count toward the limit`() {
        assertFalse(
            InputMapper.shouldResetImeBuffer("é".repeat(IME_BUFFER_LIMIT * 2), hasComposition = false, chorded = false),
        )
    }

    /**
     * A reset mid-composition restarts the IME's input session and drops the
     * composition in flight, so the buffer is allowed to run past the limit
     * until the composition ends.
     */
    @Test
    fun `a pending composition defers every reset`() {
        val overLimit = "a".repeat(IME_BUFFER_LIMIT + 1)

        assertFalse(InputMapper.shouldResetImeBuffer(overLimit, hasComposition = true, chorded = false))
        assertFalse(InputMapper.shouldResetImeBuffer("c", hasComposition = true, chorded = true))
    }

    /**
     * Gboard committed "c" for Ctrl+C; the guest got a chord, not text, so a
     * later Backspace must not be diffed against a character it never
     * received as text.
     */
    @Test
    fun `a chorded character empties the field whatever its length`() {
        assertTrue(InputMapper.shouldResetImeBuffer("c", hasComposition = false, chorded = true))
        assertTrue(InputMapper.shouldResetImeBuffer("", hasComposition = false, chorded = true))
    }

    @Test
    fun `typedChars counts what the delta types, not what it deletes`() {
        assertEquals(1, InputMapper.typedChars("", "c"))
        assertEquals(2, InputMapper.typedChars("teh", "the"))
        assertEquals(0, InputMapper.typedChars("ab", "a"))
        assertEquals(0, InputMapper.typedChars("", ""))
        // An untypable character is not typed, so it is not counted.
        assertEquals(0, InputMapper.typedChars("", "é"))
        assertEquals(1, InputMapper.typedChars("it’", "it’s"))
    }
}
