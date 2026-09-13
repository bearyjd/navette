package com.greponlabs.navette.net

import java.io.File
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The pairing store is EncryptedSharedPreferences. A backup carries its
 * ciphertext but cannot carry its Android Keystore master key -- keystore keys
 * are non-exportable by design -- so a restored or transferred file is
 * undecryptable forever, and every `load()`/`save()` throws from then on
 * (`by lazy` does not memoize a thrown initializer). The round-1 guards stop
 * that crashing, which turns it into "pairing never works on this device
 * again" with no in-app way out.
 *
 * Android resource XML cannot reference [EncryptedPairingStore.PREFS_NAME], so
 * the filename really does live in three files. These tests are the link: a
 * rename that silently unprotected the store would otherwise surface only as a
 * bug report about switching phones.
 */
class PairingBackupRulesTest {
    private fun resource(name: String): String {
        // Gradle runs unit tests with the module directory as the working dir.
        val file = File("src/main/res/xml/$name")
        assertTrue("expected $name at ${file.absolutePath}", file.exists())
        return file.readText()
    }

    private val excluded = "${EncryptedPairingStore.PREFS_NAME}.xml"

    @Test
    fun `legacy backup rules exclude the pairing store`() {
        // android:fullBackupContent, the API 23-30 path. minSdk is 26, so this
        // is live on real devices.
        val rules = resource("backup_rules.xml")
        assertTrue(
            "backup_rules.xml must exclude $excluded, got:\n$rules",
            rules.contains(excluded) && rules.contains("domain=\"sharedpref\""),
        )
    }

    @Test
    fun `api 31 rules exclude the pairing store from both backup and transfer`() {
        // android:dataExtractionRules, the API 31+ path. targetSdk is 36.
        // device-transfer matters as much as cloud-backup: a phone-to-phone
        // transfer moves the file without the keystore just as a restore does,
        // so excluding only the cloud path leaves the commonest upgrade route
        // broken.
        val rules = resource("data_extraction_rules.xml")
        val cloud = rules.substringAfter("<cloud-backup>").substringBefore("</cloud-backup>")
        val transfer = rules.substringAfter("<device-transfer>").substringBefore("</device-transfer>")
        assertTrue("cloud-backup must exclude $excluded, got:\n$cloud", cloud.contains(excluded))
        assertTrue("device-transfer must exclude $excluded, got:\n$transfer", transfer.contains(excluded))
    }

    @Test
    fun `the manifest wires up both rule files`() {
        // A rule file nothing references protects nothing, and only one of the
        // two attributes being set leaves the other API path live.
        val manifest = File("src/main/AndroidManifest.xml").readText()
        assertTrue(manifest.contains("android:fullBackupContent=\"@xml/backup_rules\""))
        assertTrue(manifest.contains("android:dataExtractionRules=\"@xml/data_extraction_rules\""))
    }
}
