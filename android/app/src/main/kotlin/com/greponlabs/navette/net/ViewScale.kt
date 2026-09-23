package com.greponlabs.navette.net

/**
 * How much smaller than the surface the guest lays out; the decoded frame is
 * upscaled to fill. Lives in `net` because the pairing registry persists it
 * per host (`SavedPairing.viewScale`) and `net` must not import `ui.session`.
 * No display string here: the UI layer derives "1×"/"2×" from [factor].
 */
enum class ViewScale(val factor: Float) {
    X1(1f),
    X1_5(1.5f),
    X2(2f),
    X3(3f),
    ;

    companion object {
        /** `null` for a factor that is not a preset: a registry value written by a build with other presets. */
        fun fromFactor(factor: Float?): ViewScale? = entries.firstOrNull { it.factor == factor }

        /** Phones default to 2×, tablets (sw600dp and up, Android's own threshold) to 1×. Never stored. */
        fun defaultFor(smallestScreenWidthDp: Int): ViewScale = if (smallestScreenWidthDp < TABLET_MIN_SW_DP) X2 else X1
    }
}

/**
 * Android's own phone/tablet boundary: `sw600dp` is where the platform's
 * large-screen resource qualifiers begin.
 */
const val TABLET_MIN_SW_DP: Int = 600
