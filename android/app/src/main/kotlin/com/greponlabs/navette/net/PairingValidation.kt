package com.greponlabs.navette.net

import java.net.Inet6Address
import java.net.InetAddress
import java.util.Locale

/**
 * The trust boundary for every endpoint that can become a WebSocket authority.
 * Pairing codes, manual entry and persisted data all take this path.  Keep it
 * deliberately stricter than a URL parser: a pairing contains a host, never a
 * URL, userinfo, a path, or a second embedded port.
 */
internal fun canonicalHost(raw: String): String? {
    if (raw.isEmpty() || raw != raw.trim() || raw.length > 253) return null
    if (raw.any { it.code <= 0x20 || it.code >= 0x7f || it == '%' || it == '/' || it == '?' || it == '#' || it == '@' }) return null

    val bracketed = raw.startsWith("[") || raw.endsWith("]")
    val candidate =
        when {
            raw.startsWith("[") && raw.endsWith("]") -> raw.substring(1, raw.length - 1)
            bracketed -> return null
            else -> raw
        }
    if (candidate.isEmpty()) return null

    if (candidate.contains(':')) {
        // The character pre-check means this never asks DNS to resolve a name.
        if (!candidate.all { it in "0123456789abcdefABCDEF:" }) return null
        val address = runCatching { InetAddress.getByName(candidate) }.getOrNull() as? Inet6Address ?: return null
        if (address.isLoopbackAddress || address.isAnyLocalAddress) return null
        return address.hostAddress?.lowercase(Locale.ROOT)
    }

    if (candidate.all { it.isDigit() || it == '.' }) {
        val octets = candidate.split('.')
        if (octets.size != 4 || octets.any { it.isEmpty() || (it.length > 1 && it.startsWith('0')) || it.toIntOrNull() !in 0..255 }) return null
        if (octets[0] == "127" || octets.all { it == "0" }) return null
        return octets.joinToString(".")
    }

    if (candidate.equals("localhost", ignoreCase = true)) return null
    val labels = candidate.split('.')
    if (labels.any { label ->
            label.isEmpty() || label.length > 63 || !label.first().isLetterOrDigit() ||
                !label.last().isLetterOrDigit() || label.any { !it.isLetterOrDigit() && it != '-' }
        }
    ) return null
    return candidate.lowercase(Locale.ROOT)
}

internal fun canonicalPort(raw: String): Int? = raw.toIntOrNull()?.takeIf { it in 1..65535 }

/**
 * Mirrors the MAC grammar `navetted`'s `/v1/wake` accepts: six hex octets as
 * `aa:bb:cc:dd:ee:ff`, `aa-bb-cc-dd-ee-ff` or `aabbccddeeff`, any case, and
 * nothing else -- no mixed separators, no surrounding whitespace (the caller
 * trims, as [canonicalHost]'s callers do). Grammar only: broadcast and
 * all-zero addresses pass, because refusing what the daemon would send is a
 * second definition of "valid" that can drift from the first.
 */
internal fun canonicalMac(raw: String): String? {
    if (raw.isEmpty() || raw != raw.trim()) return null
    val octets =
        when (raw.length) {
            MAC_BARE_LENGTH -> raw.chunked(2)
            MAC_SEPARATED_LENGTH -> {
                val separator = raw[2]
                if (separator != ':' && separator != '-') return null
                raw.split(separator)
            }
            else -> return null
        }
    if (octets.size != MAC_OCTETS || octets.any { octet -> octet.length != 2 || !octet.all { it in HEX_DIGITS } }) return null
    return octets.joinToString(":") { it.lowercase(Locale.ROOT) }
}

private const val MAC_OCTETS = 6
private const val MAC_BARE_LENGTH = MAC_OCTETS * 2
private const val MAC_SEPARATED_LENGTH = MAC_BARE_LENGTH + MAC_OCTETS - 1
private const val HEX_DIGITS = "0123456789abcdefABCDEF"

/** Mirrors navette-auth's parser: formatting separators are accepted, not aliases. */
internal fun normalizePairingToken(raw: String): String? {
    // Rust validates bytes with to_ascii_uppercase; Unicode case folding (for
    // example long-s becoming S) must never turn a non-ASCII paste into a
    // credential the daemon would interpret differently.
    if (raw.any { it.code > 0x7f }) return null
    val normalized = raw.filterNot { it == '-' || it == ' ' }.uppercase(Locale.ROOT)
    return normalized.takeIf { it.length == 24 && it.all { char -> char in "0123456789ABCDEFGHJKMNPQRSTVWXYZ" } }
}

internal fun validatedPairing(host: String, port: Int, token: String): Pairing? {
    val canonicalHost = canonicalHost(host) ?: return null
    val canonicalToken = normalizePairingToken(token) ?: return null
    if (port !in 1..65535) return null
    return Pairing(canonicalHost, port, canonicalToken)
}
