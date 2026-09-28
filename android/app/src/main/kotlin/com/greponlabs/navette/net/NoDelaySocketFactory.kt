package com.greponlabs.navette.net

import java.net.InetAddress
import java.net.Socket
import javax.net.SocketFactory

/**
 * A [SocketFactory] whose sockets have Nagle's algorithm off (`TCP_NODELAY`).
 *
 * The media socket carries input as small frames written back to back: a key
 * is a press and a release microseconds apart. With Nagle on, the release is
 * held until the press is ACKed -- captured on the tailnet (2026-09-27) as a
 * 123-byte segment and a 124-byte segment a steady ~32 ms apart on a ~15 ms
 * RTT, i.e. the server's delayed ACK. wprsd starts auto-repeating a key held
 * 200 ms, so whenever that ACK is late (a busy host, Wi-Fi power save) one
 * tap arrives in the guest as a burst of the same character. Nothing here is
 * worth batching: every frame is either latency-bound input or a ping.
 *
 * OkHttp asks for an unconnected socket and connects it itself; the other
 * overloads are covered so the guarantee does not depend on which one it uses.
 * Not covered: a SOCKS proxy, where OkHttp builds `Socket(proxy)` itself and
 * never calls the factory. `MediaClientTest` pins the direct path.
 */
internal class NoDelaySocketFactory(
    private val delegate: SocketFactory = getDefault(),
) : SocketFactory() {
    override fun createSocket(): Socket = delegate.createSocket().noDelay()

    override fun createSocket(host: String?, port: Int): Socket = delegate.createSocket(host, port).noDelay()

    override fun createSocket(host: String?, port: Int, localHost: InetAddress?, localPort: Int): Socket =
        delegate.createSocket(host, port, localHost, localPort).noDelay()

    override fun createSocket(host: InetAddress?, port: Int): Socket = delegate.createSocket(host, port).noDelay()

    override fun createSocket(address: InetAddress?, port: Int, localAddress: InetAddress?, localPort: Int): Socket =
        delegate.createSocket(address, port, localAddress, localPort).noDelay()

    private fun Socket.noDelay(): Socket = apply { tcpNoDelay = true }
}
