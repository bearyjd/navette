package com.greponlabs.navette.net

import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import org.junit.Assert.assertTrue
import org.junit.Test

class NoDelaySocketFactoryTest {
    private val factory = NoDelaySocketFactory()

    @Test
    fun `the unconnected socket OkHttp asks for has Nagle off`() {
        factory.createSocket().use { assertTrue(it.tcpNoDelay) }
    }

    @Test
    fun `every connecting overload has Nagle off too`() {
        ServerSocket(0, 50, InetAddress.getLoopbackAddress()).use { server ->
            val loopback = InetAddress.getLoopbackAddress()
            val port = server.localPort
            val sockets: Map<String, () -> Socket> =
                mapOf(
                    "(String, Int)" to { factory.createSocket(loopback.hostAddress, port) },
                    "(String, Int, InetAddress, Int)" to { factory.createSocket(loopback.hostAddress, port, loopback, 0) },
                    "(InetAddress, Int)" to { factory.createSocket(loopback, port) },
                    "(InetAddress, Int, InetAddress, Int)" to { factory.createSocket(loopback, port, loopback, 0) },
                )
            for ((overload, open) in sockets) open().use { assertTrue(overload, it.tcpNoDelay) }
        }
    }
}
