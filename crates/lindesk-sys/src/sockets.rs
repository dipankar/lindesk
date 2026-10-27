//! Who is talking to whom over the loopback interface.
//!
//! The kernel can't lend a client's priority to the server it is waiting on: nothing ties a
//! request on a socket to the thread that will answer it. What can be seen from outside is
//! the connection itself. `/proc/net/tcp` and `/proc/net/tcp6` list every TCP socket with
//! its two endpoints and its inode, and a connection between two local processes appears
//! twice, once from each end. A process's descriptors (`/proc/<pid>/fd`) name the inodes of
//! its sockets. Together: given one process, the sockets at the other end of its local
//! connections, and from those the processes serving it.
//!
//! Only TCP over loopback, and only within this network namespace: a dev server, a local
//! database, a language server on a port. Unix sockets have no such table (their peers
//! need `sock_diag`), and a container's network is its own namespace.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

use crate::util::read_file;

/// `st` of an established connection in `/proc/net/tcp`.
const ESTABLISHED: &str = "01";

/// One end of an established TCP connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpSocket {
    pub local: (IpAddr, u16),
    pub remote: (IpAddr, u16),
    pub inode: u64,
}

/// The kernel prints each 32-bit word of an address as a native integer, so the bytes in
/// memory (which are in network order) come back with `to_ne_bytes`.
fn word(hex: &str) -> Option<[u8; 4]> {
    u32::from_str_radix(hex, 16).ok().map(u32::to_ne_bytes)
}

fn endpoint(s: &str) -> Option<(IpAddr, u16)> {
    let (addr, port) = s.rsplit_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let ip = match addr.len() {
        8 => IpAddr::V4(Ipv4Addr::from(word(addr)?)),
        32 => {
            let mut bytes = [0u8; 16];
            for (i, chunk) in bytes.chunks_mut(4).enumerate() {
                chunk.copy_from_slice(&word(addr.get(i * 8..i * 8 + 8)?)?);
            }
            IpAddr::V6(Ipv6Addr::from(bytes))
        }
        _ => return None,
    };
    Some((ip, port))
}

/// The established connections in the text of `/proc/net/tcp` or `/proc/net/tcp6`.
pub fn parse_tcp(table: &str) -> Vec<TcpSocket> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 || f[3] != ESTABLISHED {
                return None;
            }
            Some(TcpSocket { local: endpoint(f[1])?, remote: endpoint(f[2])?, inode: f[9].parse().ok()? })
        })
        .filter(|s| s.inode != 0)
        .collect()
}

/// 127.0.0.0/8, ::1, and the IPv4 loopback addresses as IPv6 sockets see them.
pub fn is_loopback(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

/// An address in one form, so an IPv4 client matches an IPv6 socket that accepted it.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// The sockets at the other end of the loopback connections that `mine` (socket inodes) are
/// one end of.
pub fn loopback_peers(sockets: &[TcpSocket], mine: &BTreeSet<u64>) -> BTreeSet<u64> {
    type Key = ((IpAddr, u16), (IpAddr, u16));
    let key = |a: (IpAddr, u16), b: (IpAddr, u16)| -> Key { ((canonical(a.0), a.1), (canonical(b.0), b.1)) };
    let local: Vec<&TcpSocket> =
        sockets.iter().filter(|s| is_loopback(&s.local.0) && is_loopback(&s.remote.0)).collect();
    let by_ends: BTreeMap<Key, u64> = local.iter().map(|s| (key(s.local, s.remote), s.inode)).collect();
    local
        .iter()
        .filter(|s| mine.contains(&s.inode))
        .filter_map(|s| by_ends.get(&key(s.remote, s.local)).copied())
        .filter(|peer| !mine.contains(peer))
        .collect()
}

/// The inodes of the sockets a process holds (`/proc/<pid>/fd/* -> socket:[N]`). Empty when
/// the process is gone or its descriptors can't be read.
pub fn socket_inodes(proc_root: &Path, pid: i32) -> BTreeSet<u64> {
    let Ok(dir) = std::fs::read_dir(proc_root.join(pid.to_string()).join("fd")) else { return BTreeSet::new() };
    dir.filter_map(|e| {
        let link = std::fs::read_link(e.ok()?.path()).ok()?;
        link.to_str()?.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
    })
    .collect()
}

/// Every established TCP connection in this network namespace, IPv4 and IPv6.
pub fn tcp_sockets(proc_root: &Path) -> Vec<TcpSocket> {
    ["net/tcp", "net/tcp6"]
        .iter()
        .filter_map(|f| read_file(&proc_root.join(f)).ok())
        .flat_map(|t| parse_tcp(&t))
        .collect()
}

/// Read the application's network namespaces, not the broker's private namespace.
/// No setns or network access is needed. Unreadable/exited processes are skipped.
pub fn tcp_sockets_for_pids(proc_root: &Path, pids: &[i32]) -> Vec<TcpSocket> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for pid in pids {
        let root = proc_root.join(pid.to_string());
        let namespace = std::fs::read_link(root.join("ns/net")).unwrap_or_else(|_| root.clone());
        if seen.contains(&namespace) {
            continue;
        }
        let tables: Vec<_> = ["net/tcp", "net/tcp6"].iter().filter_map(|p| read_file(&root.join(p)).ok()).collect();
        if tables.is_empty() {
            continue;
        }
        seen.insert(namespace);
        out.extend(tables.iter().flat_map(|t| parse_tcp(t)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 500 1 0000000000000000 100 0 0 10 0
   1: 0100007F:D431 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 601 1 0000000000000000 20 4 30 10 -1
   2: 0100007F:1F90 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 602 1 0000000000000000 20 4 30 10 -1
   3: 0A00000A:C001 22D8B85D:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 700 1 0000000000000000 20 4 30 10 -1
   4: 0100007F:D432 0100007F:0CEA 01 00000000:00000000 00:00000000 00000000  1000        0 603 1 0000000000000000 20 4 30 10 -1
";
    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0000000000000000FFFF00000100007F:0CEA 0000000000000000FFFF00000100007F:D432 01 00000000:00000000 00:00000000 00000000  1000        0 604 1 0000000000000000 20 4 30 10 -1
   1: 00000000000000000000000001000000:8AE0 00000000000000000000000001000000:1F91 01 00000000:00000000 00:00000000 00000000  1000        0 605 1 0000000000000000 20 4 30 10 -1
   2: 00000000000000000000000001000000:1F91 00000000000000000000000001000000:8AE0 01 00000000:00000000 00:00000000 00000000  1000        0 606 1 0000000000000000 20 4 30 10 -1
";

    #[test]
    fn reads_application_table_when_broker_namespace_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("42/net")).unwrap();
        std::fs::create_dir_all(dir.path().join("net")).unwrap();
        std::fs::write(dir.path().join("net/tcp"), "").unwrap();
        std::fs::write(dir.path().join("42/net/tcp"), TCP).unwrap();
        assert!(tcp_sockets(dir.path()).is_empty());
        assert_eq!(tcp_sockets_for_pids(dir.path(), &[999, 42]).len(), parse_tcp(TCP).len());
    }

    #[test]
    fn tables_are_read_as_the_kernel_prints_them() {
        if cfg!(target_endian = "big") {
            return; // the samples are a little-endian machine's
        }
        let v4 = parse_tcp(TCP);
        assert_eq!(v4.len(), 4, "listening sockets are not connections");
        assert_eq!(v4[0].local, ("127.0.0.1".parse().unwrap(), 0xD431));
        assert_eq!(v4[0].remote, ("127.0.0.1".parse().unwrap(), 8080));
        assert_eq!(v4[2].remote.0, "93.184.216.34".parse::<IpAddr>().unwrap());
        let v6 = parse_tcp(TCP6);
        assert_eq!(v6[0].local, ("::ffff:127.0.0.1".parse().unwrap(), 3306));
        assert_eq!(v6[1].local, ("::1".parse().unwrap(), 0x8AE0));
        assert!(is_loopback(&v6[0].local.0) && is_loopback(&v6[1].local.0) && !is_loopback(&v4[2].remote.0));
        assert!(parse_tcp("garbage\nmore garbage\n").is_empty());
    }

    #[test]
    fn the_other_end_of_local_connections() {
        if cfg!(target_endian = "big") {
            return;
        }
        let mut all = parse_tcp(TCP);
        all.extend(parse_tcp(TCP6));
        // A browser holding the client ends: a dev server on 8080, a database that
        // accepted the IPv4 client on an IPv6 socket, a server on ::1. Its connection to
        // the internet has no local peer.
        let mine = BTreeSet::from([601, 603, 605, 700]);
        assert_eq!(loopback_peers(&all, &mine), BTreeSet::from([602, 604, 606]));
        // Seen from the server: its peer is the browser's socket.
        assert_eq!(loopback_peers(&all, &BTreeSet::from([602])), BTreeSet::from([601]));
        // A process talking to itself has no peer to protect.
        assert!(loopback_peers(&all, &BTreeSet::from([601, 602])).is_empty());
        assert!(loopback_peers(&all, &BTreeSet::new()).is_empty());
    }

    /// On the real kernel: connect to ourselves and find both ends.
    #[test]
    fn a_real_loopback_connection() {
        let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
            eprintln!("skipped: cannot listen on loopback here");
            return;
        };
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut served, _) = listener.accept().unwrap();
        client.write_all(b"x").unwrap();
        let mut b = [0u8; 1];
        served.read_exact(&mut b).unwrap();

        let proc = Path::new("/proc");
        let mine = socket_inodes(proc, std::process::id() as i32);
        assert!(mine.len() >= 3, "listener, client, served: {mine:?}");
        let all = tcp_sockets(proc);
        let port = listener.local_addr().unwrap().port();
        let ends: Vec<&TcpSocket> = all.iter().filter(|s| s.local.1 == port || s.remote.1 == port).collect();
        assert_eq!(ends.len(), 2, "both ends of the connection are listed");
        assert!(ends.iter().all(|s| mine.contains(&s.inode)));
        // Pretend only the client end is ours: the peer is the served end.
        let client_inode = ends.iter().find(|s| s.remote.1 == port).unwrap().inode;
        let served_inode = ends.iter().find(|s| s.local.1 == port).unwrap().inode;
        assert_eq!(loopback_peers(&all, &BTreeSet::from([client_inode])), BTreeSet::from([served_inode]));
    }
}
