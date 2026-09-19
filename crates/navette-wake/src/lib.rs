//! Wake-on-LAN magic packets.
//!
//! A magic packet is six `0xFF` bytes followed by the target's MAC address
//! repeated sixteen times, sent as a UDP datagram to a broadcast address. The
//! NIC of a sleeping host matches on the payload alone, so the destination
//! port is a convention (9, "discard") rather than a requirement.
//!
//! This crate is `std`-only and synchronous: the packet is 102 bytes and the
//! send is one syscall, so callers on an async runtime should wrap
//! [`send_magic_packet`] in `spawn_blocking` rather than have this crate pick
//! a runtime for them.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, UdpSocket};
use std::str::FromStr;

/// The limited-broadcast address.
///
/// Not "every interface": from an unbound socket Linux routes
/// `255.255.255.255` out whichever single interface the routing table picks
/// for it, normally the default route's. On a multi-homed host, or one whose
/// default route is a VPN or Tailscale exit node, that is the wrong interface,
/// and a subnet-directed address such as `192.168.1.255` is what actually
/// reaches the LAN.
pub const DEFAULT_BROADCAST: Ipv4Addr = Ipv4Addr::BROADCAST;

/// UDP "discard": the port wake-on-LAN packets are conventionally sent to.
pub const DEFAULT_PORT: u16 = 9;

/// Six synchronisation bytes plus sixteen copies of a six-byte MAC.
pub const MAGIC_PACKET_LEN: usize = 6 + 16 * 6;

const MAC_OCTETS: usize = 6;

/// A 48-bit IEEE 802 MAC address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MacAddress([u8; MAC_OCTETS]);

/// The input did not follow one of the accepted MAC address grammars.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("expected a MAC address as aa:bb:cc:dd:ee:ff, aa-bb-cc-dd-ee-ff or aabbccddeeff")]
pub struct ParseMacError;

impl MacAddress {
    pub const fn new(octets: [u8; MAC_OCTETS]) -> Self {
        Self(octets)
    }

    pub const fn octets(self) -> [u8; MAC_OCTETS] {
        self.0
    }
}

impl FromStr for MacAddress {
    type Err = ParseMacError;

    /// Accepts colon-separated, hyphen-separated, or bare hex, case-insensitive.
    /// Only the grammar is validated: any six octets are a MAC address here,
    /// including all-zero and multicast ones.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        // Byte-wise throughout: groups sit at fixed offsets, and slicing a
        // `&str` there would panic inside a multi-byte character. The length
        // gate comes first and nothing allocates, so an oversized input (the
        // daemon accepts bodies up to axum's limit) costs one comparison.
        let bytes = input.as_bytes();
        let stride = match bytes.len() {
            len if len == 2 * MAC_OCTETS => 2,
            len if len == 3 * MAC_OCTETS - 1 => 3,
            _ => return Err(ParseMacError),
        };
        let separator = match stride {
            3 => match bytes[2] {
                separator @ (b':' | b'-') => Some(separator),
                _ => return Err(ParseMacError),
            },
            _ => None,
        };
        let mut octets = [0; MAC_OCTETS];
        for (index, octet) in octets.iter_mut().enumerate() {
            let start = index * stride;
            if let Some(separator) = separator
                && index > 0
                && bytes[start - 1] != separator
            {
                return Err(ParseMacError);
            }
            *octet = (hex_value(bytes[start])? << 4) | hex_value(bytes[start + 1])?;
        }
        Ok(Self(octets))
    }
}

/// Whether `address` is somewhere a wake-on-LAN relay may legitimately send.
///
/// The daemon relays magic packets for authenticated clients; without this it
/// would also be a UDP reflector for any address they name. Accepted: the
/// limited broadcast, RFC 1918 private ranges (which cover subnet-directed
/// broadcasts such as `192.168.1.255`), link-local, loopback (used by the
/// tests and by a local receiver), and the CGNAT range `100.64.0.0/10` that
/// tailnets use.
pub fn is_lan_broadcast_target(address: Ipv4Addr) -> bool {
    let [first, second, ..] = address.octets();
    let is_cgnat = first == 100 && (64..128).contains(&second);
    address.is_broadcast()
        || address.is_private()
        || address.is_link_local()
        || address.is_loopback()
        || is_cgnat
}

/// The rule [`is_lan_broadcast_target`] applies, phrased for an error message.
pub const LAN_BROADCAST_RULE: &str = "broadcast must be 255.255.255.255, a private (RFC 1918), link-local or loopback address, or in 100.64.0.0/10";

/// Decodes one hex digit. Hand-rolled rather than `u8::from_str_radix`, which
/// would accept a leading `+` as part of a two-character group.
fn hex_value(byte: u8) -> Result<u8, ParseMacError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(ParseMacError),
    }
}

impl fmt::Display for MacAddress {
    /// The canonical lowercase colon-separated form.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, f] = self.0;
        write!(formatter, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}")
    }
}

/// Builds the 102-byte magic packet for `mac`.
pub fn magic_packet(mac: MacAddress) -> [u8; MAGIC_PACKET_LEN] {
    let mut packet = [0xFF; MAGIC_PACKET_LEN];
    for copy in packet[MAC_OCTETS..].as_chunks_mut::<MAC_OCTETS>().0 {
        *copy = mac.0;
    }
    packet
}

/// Sends the magic packet for `mac` as one UDP datagram to `broadcast:port`.
///
/// Binds an ephemeral IPv4 socket with `SO_BROADCAST` set, so `broadcast` may
/// be the limited broadcast address, a subnet-directed one, or a unicast
/// address (useful for testing against a local receiver). Port 0 is refused
/// up front rather than left to the kernel, which reports it as a bare
/// `EINVAL`.
pub fn send_magic_packet(mac: MacAddress, broadcast: Ipv4Addr, port: u16) -> io::Result<()> {
    if port == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "wake-on-LAN port must be 1-65535",
        ));
    }
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    let packet = magic_packet(mac);
    let sent = socket.send_to(&packet, (broadcast, port))?;
    if sent != packet.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!("sent {sent} of {} magic packet bytes", packet.len()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const SAMPLE: MacAddress = MacAddress::new([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);

    #[test]
    fn parses_colon_hyphen_and_bare_forms_case_insensitively() {
        for input in [
            "aa:bb:cc:dd:ee:ff",
            "AA:BB:CC:DD:EE:FF",
            "Aa:bB:cC:Dd:eE:Ff",
            "aa-bb-cc-dd-ee-ff",
            "AA-BB-CC-DD-EE-FF",
            "aabbccddeeff",
            "AABBCCDDEEFF",
        ] {
            assert_eq!(input.parse::<MacAddress>(), Ok(SAMPLE), "{input:?}");
        }
    }

    #[test]
    fn rejects_everything_outside_the_three_grammars() {
        for input in [
            "",
            "aa:bb:cc:dd:ee",
            "aa:bb:cc:dd:ee:ff:00",
            "aa:bb-cc:dd:ee:ff",
            "gg:bb:cc:dd:ee:ff",
            "aa:bb:cc:dd:ee:ff:",
            "aa:bb:cc:dd:ee:ffx",
            "aa:bb:cc:dd:ee:ff ",
            " aa:bb:cc:dd:ee:ff",
            "+a:bb:cc:dd:ee:ff",
            "a:bb:cc:dd:ee:ff",
            "aaa:bb:cc:dd:ee:f",
            "aabbccddeef",
            "aabbccddeeff0",
            "aabbccddeefg",
            "aa.bb.cc.dd.ee.ff",
            "aabb.ccdd.eeff",
            // Twelve bytes but not twelve ASCII characters: a `&str` slice at
            // offset 10 would land inside the `é` and panic.
            "aabbccddeéa",
        ] {
            assert_eq!(input.parse::<MacAddress>(), Err(ParseMacError), "{input:?}");
        }
    }

    #[test]
    fn oversized_input_is_rejected_before_any_allocation() {
        // Item 2 of the review: the old parser collected every separator
        // group into a Vec before counting them, so a body-limit-sized run
        // of colons cost tens of MiB per request. The length gate now runs
        // first; this pins the outcome and, with the Vec gone, the cost.
        let colons = ":".repeat(4096);
        assert_eq!(colons.parse::<MacAddress>(), Err(ParseMacError));
        assert_eq!(":".repeat(17).parse::<MacAddress>(), Err(ParseMacError));
        assert_eq!("-".repeat(12).parse::<MacAddress>(), Err(ParseMacError));
    }

    #[test]
    fn lan_broadcast_targets_are_the_reserved_and_broadcast_ranges_only() {
        for accepted in [
            "255.255.255.255",
            "192.168.1.255",
            "192.168.0.1",
            "10.0.0.255",
            "10.255.255.255",
            "172.16.0.255",
            "172.31.255.255",
            "169.254.1.255",
            "127.0.0.1",
            "100.64.0.0",
            "100.100.100.100",
            "100.127.255.255",
        ] {
            let address: Ipv4Addr = accepted.parse().unwrap();
            assert!(
                is_lan_broadcast_target(address),
                "{accepted} must be accepted"
            );
        }
        for rejected in [
            "8.8.8.8",
            "1.1.1.1",
            "0.0.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "224.0.0.1",
            "203.0.113.255",
        ] {
            let address: Ipv4Addr = rejected.parse().unwrap();
            assert!(
                !is_lan_broadcast_target(address),
                "{rejected} must be rejected"
            );
        }
    }

    #[test]
    fn display_is_canonical_and_round_trips() {
        assert_eq!(SAMPLE.to_string(), "aa:bb:cc:dd:ee:ff");
        assert_eq!(
            "AA-BB-CC-DD-EE-FF"
                .parse::<MacAddress>()
                .unwrap()
                .to_string(),
            "aa:bb:cc:dd:ee:ff"
        );
        assert_eq!(
            MacAddress::new([0, 1, 2, 3, 4, 5]).to_string(),
            "00:01:02:03:04:05"
        );
        assert_eq!(SAMPLE.to_string().parse::<MacAddress>(), Ok(SAMPLE));
    }

    #[test]
    fn magic_packet_is_six_ff_then_the_mac_sixteen_times() {
        let packet = magic_packet(SAMPLE);
        assert_eq!(packet.len(), 102);
        assert_eq!(&packet[..6], &[0xFF; 6]);
        for (index, copy) in packet[6..].chunks(6).enumerate() {
            assert_eq!(copy, SAMPLE.octets(), "copy {index}");
        }
        assert_eq!(packet[6..].chunks(6).count(), 16);
    }

    #[test]
    fn sends_the_packet_to_a_loopback_receiver() {
        let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let port = receiver.local_addr().unwrap().port();

        send_magic_packet(SAMPLE, Ipv4Addr::LOCALHOST, port).unwrap();

        let mut buffer = [0; 256];
        let (received, _) = receiver.recv_from(&mut buffer).unwrap();
        assert_eq!(received, 102);
        assert_eq!(&buffer[..received], &magic_packet(SAMPLE));
    }

    #[test]
    fn refuses_port_zero_without_touching_the_network() {
        let error = send_magic_packet(SAMPLE, Ipv4Addr::LOCALHOST, 0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
