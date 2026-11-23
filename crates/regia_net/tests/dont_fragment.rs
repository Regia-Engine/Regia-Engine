// SPDX-License-Identifier: MPL-2.0
//! Don't-fragment on real UDP sockets (ADR-8893, KI-NT-028, spec 120 § 4.3).
//!
//! Linux only, without root: the test enters a new user and network namespace (`unshare(2)`
//! needs a single-threaded process, so this target has no libtest harness), brings its loopback
//! up with a 1,400-byte MTU and then checks, on real sockets:
//!
//! 1. the control: a plain socket's 1,400-byte datagram arrives (the kernel fragments it);
//! 2. `UdpTransport` refuses the same datagram (`EMSGSIZE`), lowers its `max_datagram_size`
//!    below it, and still carries a datagram that fits;
//! 3. two probing endpoints over `UdpTransport` settle on a 1,280-byte path MTU instead of the
//!    1,452 bytes fragmentation used to confirm, and the session keeps working.
//!
//! Where unprivileged namespaces are off, the test says so and passes without checking.

#[cfg(target_os = "linux")]
mod linux {
    #![allow(unsafe_code)]

    use std::io;
    use std::net::{SocketAddr, UdpSocket};
    use std::time::Duration;

    use regia_net::connection::LinkConfig;
    use regia_net::pmtud::BASE_PLPMTU;
    use regia_net::prelude::*;
    use regia_net::socket::{DatagramSocket, MAX_DATAGRAM_BYTES, UdpTransport};

    const LOOPBACK_MTU: i32 = 1400;
    /// IPv4 (20) + UDP (8) headers.
    const HEADERS: usize = 28;

    #[derive(Debug)]
    struct Game;
    impl NetGame for Game {
        type Message = u32;
        type Input = u32;
        type JoinData = ();
        const GAME_ID: &'static str = "regia.test.dont_fragment";
        const PROTOCOL_VERSION: u32 = 1;
    }

    /// `struct ifreq` (`netdevice(7)`): a 16-byte name, then a 24-byte union.
    fn ifreq(name: &str) -> [u8; 40] {
        let mut req = [0_u8; 40];
        req[..name.len()].copy_from_slice(name.as_bytes());
        req
    }

    fn ioctl(fd: i32, request: libc::Ioctl, req: &mut [u8; 40]) -> io::Result<()> {
        // SAFETY: `fd` is an open socket; `req` is a live 40-byte `ifreq` the call reads and
        // writes within its size.
        let rc = unsafe { libc::ioctl(fd, request, req.as_mut_ptr()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Enters a new user + network namespace and brings `lo` up at `mtu`. `Ok(false)` when the
    /// host does not allow unprivileged namespaces.
    pub fn enter_small_mtu_namespace(mtu: i32) -> io::Result<bool> {
        // SAFETY: `unshare` takes flags only; the process is single-threaded here (no harness).
        if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) } != 0 {
            let error = io::Error::last_os_error();
            eprintln!("dont_fragment: SKIPPED, no unprivileged user+net namespace here: {error}");
            return Ok(false);
        }
        // Any AF_INET datagram socket carries the interface ioctls (`lo` is still down).
        let probe = UdpSocket::bind("0.0.0.0:0")?;
        let fd = std::os::fd::AsRawFd::as_raw_fd(&probe);
        let mut req = ifreq("lo");
        ioctl(fd, libc::SIOCGIFFLAGS, &mut req)?;
        let flags = i16::from_ne_bytes([req[16], req[17]]) | libc::IFF_UP as i16;
        req[16..18].copy_from_slice(&flags.to_ne_bytes());
        ioctl(fd, libc::SIOCSIFFLAGS, &mut req)?;
        let mut req = ifreq("lo");
        req[16..20].copy_from_slice(&mtu.to_ne_bytes());
        ioctl(fd, libc::SIOCSIFMTU, &mut req)?;
        Ok(true)
    }

    fn recv_blocking(socket: &UdpSocket, buf: &mut [u8]) -> Option<usize> {
        socket.set_read_timeout(Some(Duration::from_millis(500))).ok()?;
        socket.recv_from(buf).ok().map(|(n, _)| n)
    }

    /// 1 and 2: a plain socket's oversize datagram is fragmented and arrives; the transport's is
    /// refused, and its budget toward that destination drops below the refused size.
    pub fn oversize_datagram_is_refused_not_fragmented() {
        let rx = UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        let dest = rx.local_addr().expect("receiver addr");
        let oversize = vec![0x5a_u8; LOOPBACK_MTU as usize];
        let mut buf = vec![0_u8; 4096];

        let plain = UdpSocket::bind("127.0.0.1:0").expect("bind plain");
        plain
            .send_to(&oversize, dest)
            .expect("the kernel fragments a plain socket's datagram");
        assert_eq!(
            recv_blocking(&rx, &mut buf),
            Some(oversize.len()),
            "control: without don't-fragment the oversize datagram arrives (fragmented)"
        );

        let mut transport = UdpTransport::bind("127.0.0.1:0".parse().expect("addr")).expect("bind transport");
        assert!(
            transport.buffer_report().dont_fragment,
            "don't-fragment reads back on"
        );
        assert_eq!(transport.max_datagram_size(dest), MAX_DATAGRAM_BYTES);
        let refused = transport
            .send_to(&oversize, dest)
            .expect_err("a don't-fragment datagram over the path MTU is refused");
        assert_eq!(refused.raw_os_error(), Some(libc::EMSGSIZE), "{refused}");
        assert!(
            transport.max_datagram_size(dest) < oversize.len(),
            "the refusal lowers the budget toward {dest}: {}",
            transport.max_datagram_size(dest)
        );
        let other: SocketAddr = "127.0.0.2:9".parse().expect("addr");
        assert_eq!(
            transport.max_datagram_size(other),
            MAX_DATAGRAM_BYTES,
            "per destination"
        );
        assert_eq!(recv_blocking(&rx, &mut buf), None, "nothing arrived");

        let fits = vec![0xa5_u8; LOOPBACK_MTU as usize - HEADERS];
        transport
            .send_to(&fits, dest)
            .expect("a datagram that fits leaves");
        assert_eq!(recv_blocking(&rx, &mut buf), Some(fits.len()));
        println!(
            "dont_fragment: control arrived fragmented; transport refused {} bytes (EMSGSIZE), budget now {}",
            oversize.len(),
            transport.max_datagram_size(dest)
        );
    }

    /// 3: probing endpoints on real sockets find 1,280 on the 1,400-byte path, not 1,452.
    pub fn probing_endpoints_find_the_real_path_mtu() {
        let link = LinkConfig {
            mtu_probing: true,
            ..LinkConfig::default()
        };
        let server_socket = UdpTransport::bind("127.0.0.1:0".parse().expect("addr")).expect("bind server");
        let addr = server_socket.local_addr().expect("server addr");
        let mut server: NetServer<Game, _> = NetServer::with_secret(
            server_socket,
            ServerConfig {
                link: link.clone(),
                ..ServerConfig::default()
            },
            [7; 32],
        );
        let mut client: NetClient<Game, _> = NetClient::connect(
            UdpTransport::bind("127.0.0.1:0".parse().expect("addr")).expect("bind client"),
            addr,
            ClientConfig {
                link,
                ..ClientConfig::default()
            },
            &(),
        )
        .expect("connect");
        let mut delivered = 0_u32;
        // Virtual milliseconds; loopback delivery is synchronous, so no wall clock is involved.
        for now in 0..4_000_u64 {
            client.update(now);
            server.update(now);
            if now.is_multiple_of(16) && client.is_connected() {
                let _ = client.send(Channel::GAME_RPC, &(now as u32));
            }
            while let Some(event) = server.poll_event() {
                if matches!(event, ServerEvent::Message { .. }) {
                    delivered += 1;
                }
            }
        }
        assert!(client.is_connected(), "joined");
        let stats = client.status().link.expect("client link");
        let row = server.peer_rows().into_iter().next().expect("one peer");
        println!(
            "dont_fragment: client path_mtu {} {:?} probes {}/{} lost; server peer path_mtu {} {:?}; {delivered} messages",
            stats.path_mtu,
            stats.mtu_search,
            stats.mtu_probes_sent,
            stats.mtu_probes_lost,
            row.link.path_mtu,
            row.link.mtu_search
        );
        assert!(delivered > 100, "the session carried its messages: {delivered}");
        for (who, mtu) in [("client", stats.path_mtu), ("server", row.link.path_mtu)] {
            let mtu = mtu as usize;
            assert!(
                mtu >= BASE_PLPMTU && mtu + HEADERS <= LOOPBACK_MTU as usize,
                "{who} path MTU {mtu} fits the {LOOPBACK_MTU}-byte path"
            );
        }
        assert_eq!(stats.path_mtu, 1280, "the largest probe size that fits");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    const NAME: &str = "dont_fragment_refuses_oversize_on_a_small_mtu_path";
    if args.iter().any(|a| a == "--list") {
        println!("{NAME}: test");
        return;
    }
    if let Some(filter) = args.iter().find(|a| !a.starts_with('-'))
        && !NAME.contains(filter.as_str())
    {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        match linux::enter_small_mtu_namespace(1400) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => panic!("namespace set-up failed after unshare: {error}"),
        }
        linux::oversize_datagram_is_refused_not_fragmented();
        linux::probing_endpoints_find_the_real_path_mtu();
        println!("test {NAME} ... ok");
    }
    #[cfg(not(target_os = "linux"))]
    println!("{NAME}: Linux only (the per-OS read-back tests are in socket.rs)");
}
