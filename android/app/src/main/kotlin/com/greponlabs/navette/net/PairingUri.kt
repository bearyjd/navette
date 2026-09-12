package com.greponlabs.navette.net

import java.net.URI

data class Pairing(val host: String, val port: Int, val token: String)

/**
 * Parses the `navette://pair?host=&port=&token=` URI the daemon renders as a QR
 * code (`pairing_uri` in crates/navette-cli/src/main.rs). Returns null for
 * anything else: the scanner reads whatever QR code is in front of it, so this
 * is a validation boundary, not a convenience.
 *
 * Uses `rawAuthority`/`rawQuery`, not the decoded `authority`/`query`. The
 * producing side deliberately does not percent-encode `host` -- it validates
 * the host to a safe character set instead, so that neither side needs a
 * shared decoding convention. `URI.getQuery()` would percent-decode the whole
 * query string regardless, which is a silent behavior mismatch with the
 * producer even though no legal `host` value can contain a `%`. Reading the
 * raw forms keeps this parser a mirror of the producer's contract rather than
 * a JVM-URI-specific one.
 */
fun parsePairingUri(raw: String): Pairing? {
    val uri = runCatching { URI(raw) }.getOrNull() ?: return null
    if (uri.scheme != "navette" || uri.rawAuthority != "pair") return null
    val fields = (uri.rawQuery ?: return null)
        .split('&')
        .mapNotNull { part ->
            val index = part.indexOf('=').takeIf { it > 0 } ?: return@mapNotNull null
            part.substring(0, index) to part.substring(index + 1)
        }
        .toMap()
    val host = fields["host"]?.takeIf { it.isNotBlank() } ?: return null
    val port = fields["port"]?.toIntOrNull()?.takeIf { it in 1..65535 } ?: return null
    val token = fields["token"]?.takeIf { it.isNotBlank() } ?: return null
    return Pairing(host, port, token)
}
