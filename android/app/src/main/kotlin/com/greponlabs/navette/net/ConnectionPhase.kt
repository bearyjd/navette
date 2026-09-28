package com.greponlabs.navette.net

/**
 * What the UI does about a [ConnectionState], decided in exactly one place.
 *
 * Every consumer used to classify the state itself with a subject-less
 * `when { state is ... -> }` behind an `else`, which sealed-type exhaustiveness
 * never checks: adding [ConnectionState.Unauthorized] compiled cleanly at every
 * site and would have fallen silently into "connection lost". [phase] is a
 * `when (this)` with no `else`, so a new variant fails to compile here until
 * someone decides which phase it is -- and every consumer follows from that.
 */
enum class ConnectionPhase {
    /** A socket is being opened. */
    Connecting,

    /** The socket is open. */
    Live,

    /** Failed or closed; a retry may bring it back. */
    Dropped,

    /** The daemon refused the token. Terminal: only pairing again helps. */
    Rejected,
}

val ConnectionState.phase: ConnectionPhase
    get() =
        when (this) {
            ConnectionState.Connecting -> ConnectionPhase.Connecting
            ConnectionState.Connected -> ConnectionPhase.Live
            ConnectionState.Disconnected, is ConnectionState.Failed -> ConnectionPhase.Dropped
            ConnectionState.Unauthorized -> ConnectionPhase.Rejected
        }
