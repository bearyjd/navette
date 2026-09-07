package com.greponlabs.navette.ui.session

import android.view.KeyEvent

/**
 * Android keycodes and printable ASCII, translated to raw Linux evdev codes.
 *
 * `navette-bridge` forwards `MediaInput.KeyboardKey.keycode` to wprs as-is
 * with no translation of its own (`crates/navette-bridge/src/input.rs:147-199`
 * hands `raw_code` straight through), so what this table produces *is* what
 * the guest receives. The protocol's only bound is `keycode <= 767`
 * (`crates/navette-protocol/src/media.rs:301-303`), which every plausible
 * transcription error also satisfies -- a wrong entry here reaches the guest
 * as the wrong character with no error anywhere in the pipeline.
 *
 * Every value below was read from `/usr/include/linux/input-event-codes.h`,
 * not recalled. Note that evdev orders the letters by QWERTY physical
 * position (`KEY_A=30`, `KEY_B=48`, `KEY_C=46`), so there is no arithmetic
 * offset from Android's alphabetical `KEYCODE_A=29` -- the table is written
 * out in full for that reason.
 */
object KeycodeMap {
    const val KEY_ESC = 1
    const val KEY_1 = 2
    const val KEY_2 = 3
    const val KEY_3 = 4
    const val KEY_4 = 5
    const val KEY_5 = 6
    const val KEY_6 = 7
    const val KEY_7 = 8
    const val KEY_8 = 9
    const val KEY_9 = 10
    const val KEY_0 = 11
    const val KEY_MINUS = 12
    const val KEY_EQUAL = 13
    const val KEY_BACKSPACE = 14
    const val KEY_TAB = 15
    const val KEY_Q = 16
    const val KEY_W = 17
    const val KEY_E = 18
    const val KEY_R = 19
    const val KEY_T = 20
    const val KEY_Y = 21
    const val KEY_U = 22
    const val KEY_I = 23
    const val KEY_O = 24
    const val KEY_P = 25
    const val KEY_LEFTBRACE = 26
    const val KEY_RIGHTBRACE = 27
    const val KEY_ENTER = 28
    const val KEY_LEFTCTRL = 29
    const val KEY_A = 30
    const val KEY_S = 31
    const val KEY_D = 32
    const val KEY_F = 33
    const val KEY_G = 34
    const val KEY_H = 35
    const val KEY_J = 36
    const val KEY_K = 37
    const val KEY_L = 38
    const val KEY_SEMICOLON = 39
    const val KEY_APOSTROPHE = 40
    const val KEY_GRAVE = 41
    const val KEY_LEFTSHIFT = 42
    const val KEY_BACKSLASH = 43
    const val KEY_Z = 44
    const val KEY_X = 45
    const val KEY_C = 46
    const val KEY_V = 47
    const val KEY_B = 48
    const val KEY_N = 49
    const val KEY_M = 50
    const val KEY_COMMA = 51
    const val KEY_DOT = 52
    const val KEY_SLASH = 53
    const val KEY_RIGHTSHIFT = 54
    const val KEY_LEFTALT = 56
    const val KEY_SPACE = 57
    const val KEY_CAPSLOCK = 58
    const val KEY_F1 = 59
    const val KEY_F2 = 60
    const val KEY_F3 = 61
    const val KEY_F4 = 62
    const val KEY_F5 = 63
    const val KEY_F6 = 64
    const val KEY_F7 = 65
    const val KEY_F8 = 66
    const val KEY_F9 = 67
    const val KEY_F10 = 68
    const val KEY_NUMLOCK = 69
    const val KEY_SCROLLLOCK = 70
    const val KEY_F11 = 87
    const val KEY_F12 = 88
    const val KEY_RIGHTCTRL = 97
    const val KEY_RIGHTALT = 100
    const val KEY_HOME = 102
    const val KEY_UP = 103
    const val KEY_PAGEUP = 104
    const val KEY_LEFT = 105
    const val KEY_RIGHT = 106
    const val KEY_END = 107
    const val KEY_DOWN = 108
    const val KEY_PAGEDOWN = 109
    const val KEY_INSERT = 110
    const val KEY_DELETE = 111
    const val KEY_LEFTMETA = 125
    const val KEY_RIGHTMETA = 126

    /**
     * The evdev code for [keyCode], or `null` when this client has no mapping
     * for it. Callers must drop a `null` rather than substituting anything --
     * there is no safe default, since every value in range is a real key
     * somewhere.
     */
    fun androidKeycodeToEvdev(keyCode: Int): Int? =
        when (keyCode) {
            KeyEvent.KEYCODE_A -> KEY_A
            KeyEvent.KEYCODE_B -> KEY_B
            KeyEvent.KEYCODE_C -> KEY_C
            KeyEvent.KEYCODE_D -> KEY_D
            KeyEvent.KEYCODE_E -> KEY_E
            KeyEvent.KEYCODE_F -> KEY_F
            KeyEvent.KEYCODE_G -> KEY_G
            KeyEvent.KEYCODE_H -> KEY_H
            KeyEvent.KEYCODE_I -> KEY_I
            KeyEvent.KEYCODE_J -> KEY_J
            KeyEvent.KEYCODE_K -> KEY_K
            KeyEvent.KEYCODE_L -> KEY_L
            KeyEvent.KEYCODE_M -> KEY_M
            KeyEvent.KEYCODE_N -> KEY_N
            KeyEvent.KEYCODE_O -> KEY_O
            KeyEvent.KEYCODE_P -> KEY_P
            KeyEvent.KEYCODE_Q -> KEY_Q
            KeyEvent.KEYCODE_R -> KEY_R
            KeyEvent.KEYCODE_S -> KEY_S
            KeyEvent.KEYCODE_T -> KEY_T
            KeyEvent.KEYCODE_U -> KEY_U
            KeyEvent.KEYCODE_V -> KEY_V
            KeyEvent.KEYCODE_W -> KEY_W
            KeyEvent.KEYCODE_X -> KEY_X
            KeyEvent.KEYCODE_Y -> KEY_Y
            KeyEvent.KEYCODE_Z -> KEY_Z

            KeyEvent.KEYCODE_0 -> KEY_0
            KeyEvent.KEYCODE_1 -> KEY_1
            KeyEvent.KEYCODE_2 -> KEY_2
            KeyEvent.KEYCODE_3 -> KEY_3
            KeyEvent.KEYCODE_4 -> KEY_4
            KeyEvent.KEYCODE_5 -> KEY_5
            KeyEvent.KEYCODE_6 -> KEY_6
            KeyEvent.KEYCODE_7 -> KEY_7
            KeyEvent.KEYCODE_8 -> KEY_8
            KeyEvent.KEYCODE_9 -> KEY_9

            KeyEvent.KEYCODE_ENTER -> KEY_ENTER
            // Android's DEL is Backspace; FORWARD_DEL is the Delete key.
            KeyEvent.KEYCODE_DEL -> KEY_BACKSPACE
            KeyEvent.KEYCODE_FORWARD_DEL -> KEY_DELETE
            KeyEvent.KEYCODE_TAB -> KEY_TAB
            KeyEvent.KEYCODE_SPACE -> KEY_SPACE
            KeyEvent.KEYCODE_ESCAPE -> KEY_ESC
            KeyEvent.KEYCODE_INSERT -> KEY_INSERT

            KeyEvent.KEYCODE_SHIFT_LEFT -> KEY_LEFTSHIFT
            KeyEvent.KEYCODE_SHIFT_RIGHT -> KEY_RIGHTSHIFT
            KeyEvent.KEYCODE_CTRL_LEFT -> KEY_LEFTCTRL
            KeyEvent.KEYCODE_CTRL_RIGHT -> KEY_RIGHTCTRL
            KeyEvent.KEYCODE_ALT_LEFT -> KEY_LEFTALT
            KeyEvent.KEYCODE_ALT_RIGHT -> KEY_RIGHTALT
            KeyEvent.KEYCODE_META_LEFT -> KEY_LEFTMETA
            KeyEvent.KEYCODE_META_RIGHT -> KEY_RIGHTMETA
            KeyEvent.KEYCODE_CAPS_LOCK -> KEY_CAPSLOCK
            KeyEvent.KEYCODE_NUM_LOCK -> KEY_NUMLOCK
            KeyEvent.KEYCODE_SCROLL_LOCK -> KEY_SCROLLLOCK

            KeyEvent.KEYCODE_DPAD_UP -> KEY_UP
            KeyEvent.KEYCODE_DPAD_DOWN -> KEY_DOWN
            KeyEvent.KEYCODE_DPAD_LEFT -> KEY_LEFT
            KeyEvent.KEYCODE_DPAD_RIGHT -> KEY_RIGHT
            KeyEvent.KEYCODE_MOVE_HOME -> KEY_HOME
            KeyEvent.KEYCODE_MOVE_END -> KEY_END
            KeyEvent.KEYCODE_PAGE_UP -> KEY_PAGEUP
            KeyEvent.KEYCODE_PAGE_DOWN -> KEY_PAGEDOWN

            KeyEvent.KEYCODE_F1 -> KEY_F1
            KeyEvent.KEYCODE_F2 -> KEY_F2
            KeyEvent.KEYCODE_F3 -> KEY_F3
            KeyEvent.KEYCODE_F4 -> KEY_F4
            KeyEvent.KEYCODE_F5 -> KEY_F5
            KeyEvent.KEYCODE_F6 -> KEY_F6
            KeyEvent.KEYCODE_F7 -> KEY_F7
            KeyEvent.KEYCODE_F8 -> KEY_F8
            KeyEvent.KEYCODE_F9 -> KEY_F9
            KeyEvent.KEYCODE_F10 -> KEY_F10
            KeyEvent.KEYCODE_F11 -> KEY_F11
            KeyEvent.KEYCODE_F12 -> KEY_F12

            KeyEvent.KEYCODE_MINUS -> KEY_MINUS
            KeyEvent.KEYCODE_EQUALS -> KEY_EQUAL
            KeyEvent.KEYCODE_COMMA -> KEY_COMMA
            KeyEvent.KEYCODE_PERIOD -> KEY_DOT
            KeyEvent.KEYCODE_SLASH -> KEY_SLASH
            KeyEvent.KEYCODE_SEMICOLON -> KEY_SEMICOLON
            KeyEvent.KEYCODE_APOSTROPHE -> KEY_APOSTROPHE
            KeyEvent.KEYCODE_LEFT_BRACKET -> KEY_LEFTBRACE
            KeyEvent.KEYCODE_RIGHT_BRACKET -> KEY_RIGHTBRACE
            KeyEvent.KEYCODE_BACKSLASH -> KEY_BACKSLASH
            KeyEvent.KEYCODE_GRAVE -> KEY_GRAVE

            else -> null
        }

    /**
     * The `(evdev code, needs Shift)` pair that types [char], or `null` for a
     * character outside the mapped range.
     *
     * **US layout only.** The guest applies its own keymap to the raw code
     * this produces, so a guest on a non-US layout receives the character at
     * that physical position rather than [char]. Non-Latin input and
     * alternative layouts are out of scope for this slice by design, and the
     * hardware-keyboard path does not go through here at all -- Android
     * reports already-layout-translated keycodes for those.
     */
    fun asciiCharToEvdev(char: Char): Pair<Int, Boolean>? =
        // The arithmetic below is on ANDROID keycodes, which are alphabetical
        // and contiguous (KEYCODE_A=29..KEYCODE_Z=54, KEYCODE_0=7..KEYCODE_9=16)
        // and fixed forever by binary compatibility. The evdev value still
        // comes from the explicit table above, never from an offset -- evdev's
        // own ordering is by QWERTY position and has no usable stride.
        when (char) {
            in 'a'..'z' -> androidKeycodeToEvdev(KeyEvent.KEYCODE_A + (char - 'a'))?.let { it to false }
            in 'A'..'Z' -> androidKeycodeToEvdev(KeyEvent.KEYCODE_A + (char - 'A'))?.let { it to true }
            in '0'..'9' -> androidKeycodeToEvdev(KeyEvent.KEYCODE_0 + (char - '0'))?.let { it to false }

            ' ' -> KEY_SPACE to false
            '\n' -> KEY_ENTER to false
            '\t' -> KEY_TAB to false

            '-' -> KEY_MINUS to false
            '_' -> KEY_MINUS to true
            '=' -> KEY_EQUAL to false
            '+' -> KEY_EQUAL to true
            '[' -> KEY_LEFTBRACE to false
            '{' -> KEY_LEFTBRACE to true
            ']' -> KEY_RIGHTBRACE to false
            '}' -> KEY_RIGHTBRACE to true
            ';' -> KEY_SEMICOLON to false
            ':' -> KEY_SEMICOLON to true
            '\'' -> KEY_APOSTROPHE to false
            '"' -> KEY_APOSTROPHE to true
            '`' -> KEY_GRAVE to false
            '~' -> KEY_GRAVE to true
            '\\' -> KEY_BACKSLASH to false
            '|' -> KEY_BACKSLASH to true
            ',' -> KEY_COMMA to false
            '<' -> KEY_COMMA to true
            '.' -> KEY_DOT to false
            '>' -> KEY_DOT to true
            '/' -> KEY_SLASH to false
            '?' -> KEY_SLASH to true

            // The shifted number row, in US-layout order.
            ')' -> KEY_0 to true
            '!' -> KEY_1 to true
            '@' -> KEY_2 to true
            '#' -> KEY_3 to true
            '$' -> KEY_4 to true
            '%' -> KEY_5 to true
            '^' -> KEY_6 to true
            '&' -> KEY_7 to true
            '*' -> KEY_8 to true
            '(' -> KEY_9 to true

            else -> null
        }
}
