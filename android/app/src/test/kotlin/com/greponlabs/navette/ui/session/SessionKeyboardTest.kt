package com.greponlabs.navette.ui.session

import com.greponlabs.navette.media.PrimaryStream
import com.greponlabs.navette.net.MediaInput
import com.greponlabs.navette.net.StreamConfig
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

private const val CLIENT_ID = 21L
private const val SURFACE_ID = 22L

/**
 * The stream and the send are lambdas, so every path that used to sit
 * behind the controller's packet loop -- which runs on Dispatchers.Default
 * and never delivers a `StreamConfig` on the JVM -- is driven directly here.
 */
class SessionKeyboardTest {
    private val stream = PrimaryStream(1L, StreamConfig(CLIENT_ID, SURFACE_ID, byteArrayOf()), 1280, 720)
    private var primary: PrimaryStream? = stream

    /** Every input the keyboard tried to send, delivered or not. */
    private val attempted = mutableListOf<MediaInput>()

    /** The ones the socket accepted -- what the guest actually saw. */
    private val sent = mutableListOf<MediaInput>()

    /** The attempt at which the socket starts refusing, as MediaClient does with no socket. */
    private var failFromAttempt = Int.MAX_VALUE
    private val keyboard =
        SessionKeyboard(
            primary = { primary },
            send = { input ->
                attempted += input
                val accepted = attempted.size < failFromAttempt
                if (accepted) sent += input
                accepted
            },
        )

    private fun press(evdevCode: Int) = InputMapper.keyboardKey(CLIENT_ID, SURFACE_ID, evdevCode, pressed = true)

    private fun release(evdevCode: Int) = InputMapper.keyboardKey(CLIENT_ID, SURFACE_ID, evdevCode, pressed = false)

    private fun modifiers(ctrl: Boolean = false, alt: Boolean = false) =
        InputMapper.heldModifiers(CLIENT_ID, SURFACE_ID, HeldModifiers(ctrl = ctrl, alt = alt))

    @Test
    fun `a bar key with nothing armed is a plain press and release`() {
        keyboard.onKeyBarKey(KeycodeMap.KEY_ESC)

        assertEquals(listOf(press(1), release(1)), sent)
        assertEquals(StickyModifiers(), keyboard.modifiers.value)
    }

    @Test
    fun `a shifted bar key is wrapped in shift`() {
        keyboard.onKeyBarKey(KeycodeMap.KEY_GRAVE, needsShift = true)

        assertEquals(listOf(press(42), press(41), release(41), release(42)), sent)
    }

    @Test
    fun `an armed modifier wraps the next bar key and is spent`() {
        keyboard.onModifierTapped(StickyKey.Ctrl)
        keyboard.onKeyBarKey(KeycodeMap.KEY_C)

        assertEquals(listOf(press(29), modifiers(ctrl = true), press(46), release(46), modifiers(), release(29)), sent)
        assertEquals(StickyModifiers(), keyboard.modifiers.value)

        // The next key is plain again.
        sent.clear()
        keyboard.onKeyBarKey(KeycodeMap.KEY_TAB)
        assertEquals(listOf(press(15), release(15)), sent)
    }

    @Test
    fun `a locked modifier wraps every key and stays locked`() {
        keyboard.onModifierLocked(StickyKey.Ctrl)
        repeat(5) { keyboard.onKeyBarKey(KeycodeMap.KEY_A) }

        assertEquals(5, sent.count { it == press(29) })
        assertEquals(5, sent.count { it == release(29) })
        assertEquals(StickyModifiers(ctrl = StickyState.Locked), keyboard.modifiers.value)
    }

    @Test
    fun `a chorded IME character is the chord, is reported, and spends the modifier`() {
        keyboard.onModifierTapped(StickyKey.Ctrl)

        assertTrue(keyboard.onImeText(previous = "", current = "c"))

        assertEquals(listOf(press(29), modifiers(ctrl = true), press(46), release(46), modifiers(), release(29)), sent)
        assertEquals(StickyModifiers(), keyboard.modifiers.value)
    }

    @Test
    fun `an IME edit with nothing held is not chorded`() {
        assertFalse(keyboard.onImeText(previous = "", current = "hi"))
        assertEquals(listOf(press(35), release(35), press(23), release(23)), sent)
    }

    /**
     * A deletion is Gboard reconciling its own buffer, not a chord: the
     * backspace goes out plain and the armed modifier waits for a real key.
     */
    @Test
    fun `a deletion under an armed modifier is unchorded and leaves it armed`() {
        keyboard.onModifierTapped(StickyKey.Alt)

        assertFalse(keyboard.onImeText(previous = "ab", current = "a"))

        assertEquals(listOf(press(14), release(14)), sent)
        assertEquals(StickyModifiers(alt = StickyState.Armed), keyboard.modifiers.value)
    }

    @Test
    fun `an edit that types nothing the guest can receive is not chorded`() {
        keyboard.onModifierTapped(StickyKey.Ctrl)

        assertFalse(keyboard.onImeText(previous = "", current = "é"))

        assertEquals(emptyList<MediaInput>(), sent)
        assertEquals(StickyModifiers(ctrl = StickyState.Armed), keyboard.modifiers.value)
    }

    @Test
    fun `nothing is sent and nothing is spent before the first stream config`() {
        primary = null
        keyboard.onModifierTapped(StickyKey.Ctrl)

        keyboard.onKeyBarKey(KeycodeMap.KEY_ESC)
        assertFalse(keyboard.onImeText(previous = "", current = "c"))

        assertEquals(emptyList<MediaInput>(), sent)
        assertEquals(StickyModifiers(ctrl = StickyState.Armed), keyboard.modifiers.value)
    }

    @Test
    fun `tapping and locking publish the sticky state the bar renders`() {
        keyboard.onModifierTapped(StickyKey.Ctrl)
        assertEquals(StickyState.Armed, keyboard.modifiers.value.ctrl)

        keyboard.onModifierLocked(StickyKey.Alt)
        assertEquals(StickyModifiers(ctrl = StickyState.Armed, alt = StickyState.Locked), keyboard.modifiers.value)

        keyboard.onModifierTapped(StickyKey.Ctrl)
        keyboard.onModifierTapped(StickyKey.Alt)
        assertEquals(StickyModifiers(), keyboard.modifiers.value)
    }

    /**
     * The defect: the armed modifier used to be spent unconditionally, so a
     * chord that never reached the socket (no socket yet, or a full queue)
     * left the user's armed Ctrl consumed and the next key unchorded -- the
     * same reasoning the `primary == null` early return already applied.
     */
    @Test
    fun `an armed modifier is not spent by a chord that never reached the socket`() {
        failFromAttempt = 1
        keyboard.onModifierTapped(StickyKey.Ctrl)

        keyboard.onKeyBarKey(KeycodeMap.KEY_C)

        assertEquals(emptyList<MediaInput>(), sent)
        assertEquals(StickyModifiers(ctrl = StickyState.Armed), keyboard.modifiers.value)
    }

    /**
     * A chord cut off after its Ctrl press would leave the guest holding Ctrl
     * for every later key. The remaining events are abandoned (the socket has
     * just refused one) and a release is attempted for what was pressed.
     */
    @Test
    fun `a chord cut off mid-flight releases the modifier it already pressed`() {
        keyboard.onModifierTapped(StickyKey.Ctrl)
        // press LEFTCTRL and the Modifiers message land; the key press does not.
        failFromAttempt = 3

        keyboard.onKeyBarKey(KeycodeMap.KEY_C)

        assertEquals("the chord stops at the refused send", listOf(press(29), modifiers(ctrl = true), press(46)), attempted.dropLast(1))
        assertEquals("and unwinds what it pressed", release(29), attempted.last())
        assertEquals(StickyModifiers(), keyboard.modifiers.value)
    }

    @Test
    fun `arming a modifier presses nothing on its own`() {
        keyboard.onModifierTapped(StickyKey.Ctrl)

        assertEquals(emptyList<MediaInput>(), sent)
    }

    /**
     * Also the manual recovery for a Ctrl the guest still believes is down
     * because an earlier chord's release was dropped downstream: tapping the
     * chip off always sends the release.
     */
    @Test
    fun `disarming a modifier releases it in the guest`() {
        keyboard.onModifierTapped(StickyKey.Ctrl)
        keyboard.onModifierTapped(StickyKey.Ctrl)

        assertEquals(listOf(release(29)), sent)
        assertEquals(StickyModifiers(), keyboard.modifiers.value)
    }

    @Test
    fun `unlocking a locked modifier releases it too`() {
        keyboard.onModifierLocked(StickyKey.Alt)
        keyboard.onModifierTapped(StickyKey.Alt)

        assertEquals(listOf(release(56)), sent)
    }

    @Test
    fun `locking an armed modifier releases nothing`() {
        keyboard.onModifierTapped(StickyKey.Ctrl)
        keyboard.onModifierLocked(StickyKey.Ctrl)

        assertEquals(emptyList<MediaInput>(), sent)
        assertEquals(StickyModifiers(ctrl = StickyState.Locked), keyboard.modifiers.value)
    }

    @Test
    fun `a locked modifier surviving a key is not released by that key`() {
        keyboard.onModifierLocked(StickyKey.Ctrl)

        keyboard.onKeyBarKey(KeycodeMap.KEY_A)

        assertEquals("only the chord's own release", 1, sent.count { it == release(29) })
        assertEquals(StickyModifiers(ctrl = StickyState.Locked), keyboard.modifiers.value)
    }

    /**
     * Leaving the session must not strand a modifier. Unconditional and
     * idempotent: the bridge forwards a release for a keycode it never saw
     * pressed (`crates/navette-bridge/src/input.rs:173-198` collapses only a
     * redundant *press*), and the server's own healing shares the bounded
     * input queue that may have dropped the release in the first place.
     */
    @Test
    fun `releasing held modifiers sends every modifier release once`() {
        keyboard.onModifierLocked(StickyKey.Ctrl)

        keyboard.releaseHeldModifiers()

        assertEquals(listOf(release(29), release(56), release(42)), sent)
        assertEquals("the state goes with them", StickyModifiers(), keyboard.modifiers.value)
    }

    @Test
    fun `releasing held modifiers before the first stream config sends nothing`() {
        primary = null

        keyboard.releaseHeldModifiers()

        assertEquals(emptyList<MediaInput>(), sent)
    }
}
