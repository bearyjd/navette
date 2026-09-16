package com.greponlabs.navette.ui.session

import java.io.File
import kotlin.io.path.createTempDirectory
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ClipboardImageCacheTest {
    @Test
    fun retentionKeepsCurrentImageAndBoundsOlderImages() {
        val directory = createTempDirectory("navette-clipboard-cache").toFile()
        try {
            val old = File(directory, "old.png").apply { writeBytes(byteArrayOf(1, 2, 3)) }
            val current = File(directory, "current.png").apply { writeBytes(byteArrayOf(4, 5, 6)) }
            val spare = File(directory, "spare.jpg").apply { writeBytes(byteArrayOf(7, 8, 9)) }
            old.setLastModified(1)
            current.setLastModified(2)
            spare.setLastModified(3)

            retainClipboardImages(directory, current, maxFiles = 2, maxBytes = 6)

            assertTrue("the active clipboard URI must survive trimming", current.exists())
            assertTrue(spare.exists())
            assertFalse(old.exists())
        } finally {
            directory.deleteRecursively()
        }
    }
}
