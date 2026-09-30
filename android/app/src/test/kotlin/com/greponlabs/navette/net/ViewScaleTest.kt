package com.greponlabs.navette.net

import org.junit.Assert.assertEquals
import org.junit.Test

class ViewScaleTest {
    @Test
    fun `fromFactor maps each preset by its exact factor`() {
        assertEquals(ViewScale.X1, ViewScale.fromFactor(1f))
        assertEquals(ViewScale.X1_5, ViewScale.fromFactor(1.5f))
        assertEquals(ViewScale.X2, ViewScale.fromFactor(2f))
        assertEquals(ViewScale.X3, ViewScale.fromFactor(3f))
        for (scale in ViewScale.entries) assertEquals(scale, ViewScale.fromFactor(scale.factor))
    }

    @Test
    fun `fromFactor rejects anything that is not a preset`() {
        // A registry value written by a build with other presets degrades to "no preference".
        // NaN and the infinities are here rather than in PairingRegistryTest's decode
        // loop because neither is a valid JSON number: a registry can only reach them
        // through a non-number, which is Corrupt, not a degrade.
        for (bad in listOf(null, 0f, 0.5f, 1.25f, 2.5f, 4f, -2f, Float.NaN, Float.POSITIVE_INFINITY, Float.NEGATIVE_INFINITY)) {
            assertEquals("factor=$bad", null, ViewScale.fromFactor(bad))
        }
    }

    @Test
    fun `phones default to 2x and tablets to 1x, split at Android's sw600dp`() {
        assertEquals(ViewScale.X2, ViewScale.defaultFor(360))
        assertEquals(ViewScale.X2, ViewScale.defaultFor(599))
        assertEquals(ViewScale.X1, ViewScale.defaultFor(600))
        assertEquals(ViewScale.X1, ViewScale.defaultFor(840))
        assertEquals(600, TABLET_MIN_SW_DP)
    }

    @Test
    fun `the presets are exactly the four the design lists, in ascending order`() {
        assertEquals(listOf(1f, 1.5f, 2f, 3f), ViewScale.entries.map { it.factor })
    }
}
