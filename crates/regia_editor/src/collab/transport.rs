// SPDX-License-Identifier: MPL-2.0
//! Bounded nonblocking datagrams; the test transport preserves the same byte boundary.

use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};

pub(super) const MAX_PACKET: usize = 60 * 1024;

pub(super) enum Transport {
    Udp(UdpSocket),
    #[cfg(test)]
    Loopback {
        address: SocketAddr,
        hub: Loopback,
    },
}

impl Transport {
    pub(super) fn bind(address: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(address)?;
        socket.set_nonblocking(true)?;
        Ok(Self::Udp(socket))
    }

    pub(super) fn address(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Udp(socket) => socket.local_addr(),
            #[cfg(test)]
            Self::Loopback { address, .. } => Ok(*address),
        }
    }

    pub(super) fn send(&self, bytes: &[u8], to: SocketAddr) -> io::Result<()> {
        if bytes.len() > MAX_PACKET {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "presence packet too large",
            ));
        }
        match self {
            Self::Udp(socket) => socket.send_to(bytes, to).map(|_| ()),
            #[cfg(test)]
            Self::Loopback { address, hub } => {
                let mut queues = hub.0.lock().map_err(|_| io::Error::other("loopback poisoned"))?;
                let queue = queues.entry(to).or_default();
                if queue.len() < 64 {
                    queue.push_back(Ok((bytes.to_vec(), *address)));
                }
                Ok(())
            }
        }
    }

    pub(super) fn receive(&self, bytes: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self {
            Self::Udp(socket) => socket.recv_from(bytes),
            #[cfg(test)]
            Self::Loopback { address, hub } => {
                let mut queues = hub.0.lock().map_err(|_| io::Error::other("loopback poisoned"))?;
                let Some(packet) = queues.entry(*address).or_default().pop_front() else {
                    return Err(io::ErrorKind::WouldBlock.into());
                };
                let (packet, from) = packet.map_err(io::Error::from)?;
                let len = packet.len().min(bytes.len());
                bytes[..len].copy_from_slice(&packet[..len]);
                Ok((len, from))
            }
        }
    }
}

pub(super) fn lan_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local(),
    }
}

#[cfg(test)]
type Packet = Result<(Vec<u8>, SocketAddr), io::ErrorKind>;

#[cfg(test)]
type Queues = std::collections::BTreeMap<SocketAddr, std::collections::VecDeque<Packet>>;

#[cfg(test)]
#[derive(Clone, Default)]
pub(super) struct Loopback(std::sync::Arc<std::sync::Mutex<Queues>>);

#[cfg(test)]
impl Loopback {
    pub(super) fn fail_next(&self, id: u16, error: io::ErrorKind) -> io::Result<()> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("loopback poisoned"))?
            .entry(self.address(id))
            .or_default()
            .push_front(Err(error));
        Ok(())
    }

    pub(super) fn address(&self, id: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 40000 + id))
    }

    pub(super) fn bind(&self, id: u16) -> Transport {
        Transport::Loopback {
            address: self.address(id),
            hub: self.clone(),
        }
    }
}
