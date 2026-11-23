// SPDX-License-Identifier: MPL-2.0
//! The address block one subscriber controls (ADR-3283): one IPv4 address, or one IPv6 /64.
//!
//! An IPv6 subscriber is normally given a whole /64 or more (RFC 6177 § 2; the low 64 bits are
//! the interface identifier, RFC 4291 § 2.5.1), so it can rotate source addresses inside it at no
//! cost. Every per-address rule of `regia_net` (the per-address slot cap, IP bans, the pre-session
//! token buckets, the handshake table, the relay's per-address lobby cap) therefore counts an IPv6
//! source by its /64, through this one helper. An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) is
//! its IPv4 address, so a dual-stack socket cannot give one host two spellings.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};

/// Bits of an IPv6 address that name its block (the rest is the interface identifier).
pub const IPV6_BLOCK_PREFIX: u8 = 64;

/// The block an address belongs to: its IPv4 address, or its IPv6 /64 network address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AddrBlock(IpAddr);

impl AddrBlock {
    /// The block of `ip`. IPv4-mapped IPv6 addresses are taken as IPv4.
    pub fn of(ip: IpAddr) -> Self {
        match ip.to_canonical() {
            IpAddr::V4(v4) => Self(IpAddr::V4(v4)),
            IpAddr::V6(v6) => {
                let host_bits = 128 - u32::from(IPV6_BLOCK_PREFIX);
                let network = (u128::from(v6) >> host_bits) << host_bits;
                Self(IpAddr::V6(Ipv6Addr::from(network)))
            }
        }
    }

    /// The block's network address (the IPv4 address itself, or the /64 with a zero host part).
    pub fn network(self) -> IpAddr {
        self.0
    }

    /// True when `ip` lies in this block.
    pub fn contains(self, ip: IpAddr) -> bool {
        Self::of(ip) == self
    }

    /// Sixteen bytes that identify the block (IPv4 as its mapped form), for keyed hashing.
    pub fn key_bytes(self) -> [u8; 16] {
        match self.0 {
            IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
            IpAddr::V6(v6) => v6.octets(),
        }
    }
}

impl fmt::Display for AddrBlock {
    /// `192.0.2.9` for IPv4, `2001:db8:1:2::/64` for IPv6.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            IpAddr::V4(v4) => write!(f, "{v4}"),
            IpAddr::V6(v6) => write!(f, "{v6}/{IPV6_BLOCK_PREFIX}"),
        }
    }
}

/// True when `a` and `b` are one IPv4 address or lie in one IPv6 /64.
pub fn same_block(a: IpAddr, b: IpAddr) -> bool {
    AddrBlock::of(a) == AddrBlock::of(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("ip")
    }

    #[test]
    fn blocks_are_one_v4_address_or_one_v6_64() {
        assert!(same_block(ip("2001:db8:1:2::1"), ip("2001:db8:1:2:ffff::9")));
        assert!(!same_block(ip("2001:db8:1:2::1"), ip("2001:db8:1:3::1")));
        assert!(same_block(ip("::ffff:10.0.0.1"), ip("10.0.0.1")));
        assert!(!same_block(ip("10.0.0.1"), ip("10.0.0.2")));
        assert!(AddrBlock::of(ip("2001:db8::")).contains(ip("2001:db8::ffff:ffff:ffff:ffff")));
        assert!(!AddrBlock::of(ip("::1")).contains(ip("::2:0:0:0:1")));
    }

    #[test]
    fn key_bytes_keep_v4_and_its_mapped_form_together() {
        assert_eq!(
            AddrBlock::of(ip("10.0.0.1")).key_bytes(),
            AddrBlock::of(ip("::ffff:10.0.0.1")).key_bytes()
        );
        assert_ne!(
            AddrBlock::of(ip("2001:db8::1")).key_bytes(),
            AddrBlock::of(ip("2001:db8:0:1::1")).key_bytes()
        );
    }
}
