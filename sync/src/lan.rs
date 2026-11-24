//! 局域网：同一个网里的设备自己找到彼此，直接把信封送过去，不经过任何服务器。
//!
//! - **发现**：每隔几秒往 UDP [`PORT`] 广播一条带签名的小消息（设备编号 + 收信的 TCP 端口）。签名用组密钥
//!   派生的密钥，不是同一组的设备认不出来，也冒充不了。
//! - **送信**：TCP 连上对方，发一帧（4 字节长度 + 信封）就断开。信封本身加了密、验得出真假，所以连接
//!   不用再握手；谁都能连上来，但塞进来的东西拆不开就丢掉。
//! - **配对**时主持的一方另外广播「我在等配对」（[`offer`]），加入的一方听到以后连过去走 [`crate::pairing`]。
//!
//! 防火墙要放行 [`PORT`] 的 UDP 和 TCP 入站，不然别的设备送不进来（发出去不受影响）。

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use crate::crypto::{DeviceId, GroupKeys, DEVICE_ID_LEN};
use crate::pairing::Channel;

/// 发现用的 UDP 端口，也是收信的 TCP 端口（被占了才换别的，换了以后防火墙规则就对不上了）。
pub const PORT: u16 = 39217;
/// 一帧最大多少（信封里的文字最多 16 KB，留足余量）。
pub const MAX_FRAME: usize = 256 * 1024;

const BEACON_MAGIC: &[u8; 4] = b"WTSB";
const OFFER_MAGIC: &[u8; 4] = b"WTSP";
const VERSION: u8 = 1;
const BEACON_LEN: usize = 4 + 1 + DEVICE_ID_LEN + 2 + 16;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// 这台设备在局域网上的广播内容。
pub fn beacon(keys: &GroupKeys, device: &DeviceId, tcp_port: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(BEACON_LEN);
    out.extend_from_slice(BEACON_MAGIC);
    out.push(VERSION);
    out.extend_from_slice(device);
    out.extend_from_slice(&tcp_port.to_be_bytes());
    let mac = keys.beacon_mac(&out);
    out.extend_from_slice(&mac);
    out
}

/// 认一条广播：是同一组设备发的就返回（设备编号, 收信端口）。
pub fn parse_beacon(keys: &GroupKeys, data: &[u8]) -> Option<(DeviceId, u16)> {
    if data.len() != BEACON_LEN || &data[..4] != BEACON_MAGIC || data[4] != VERSION {
        return None;
    }
    let (body, mac) = data.split_at(BEACON_LEN - 16);
    if !crate::crypto::same(mac, &keys.beacon_mac(body)) {
        return None;
    }
    let mut device = [0u8; DEVICE_ID_LEN];
    device.copy_from_slice(&body[5..5 + DEVICE_ID_LEN]);
    let port = u16::from_be_bytes([body[13], body[14]]);
    (port != 0).then_some((device, port))
}

/// 「我在等配对」的广播：收配对连接的 TCP 端口和设备名。没有签名（这时候对方还没有密钥），
/// 真假由配对码把关。
pub fn offer(tcp_port: u16, name: &str) -> Vec<u8> {
    let name: String = name.chars().take(40).collect();
    let mut out = Vec::with_capacity(8 + name.len());
    out.extend_from_slice(OFFER_MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&tcp_port.to_be_bytes());
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
    out
}

pub fn parse_offer(data: &[u8]) -> Option<(u16, String)> {
    if data.len() < 8 || &data[..4] != OFFER_MAGIC || data[4] != VERSION {
        return None;
    }
    let port = u16::from_be_bytes([data[5], data[6]]);
    let name = data.get(8..8 + data[7] as usize)?;
    let name: String = String::from_utf8_lossy(name)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    (port != 0).then_some((port, name))
}

/// 绑 UDP 端口收广播。同一台机器上输入法和设置界面（配对时）会同时听 [`PORT`]，所以允许重复绑定。
pub fn bind_udp(port: u16) -> io::Result<UdpSocket> {
    let socket = bind_udp_reuse(port)?;
    socket.set_broadcast(true)?;
    Ok(socket)
}

#[cfg(unix)]
fn bind_udp_reuse(port: u16) -> io::Result<UdpSocket> {
    use std::os::unix::io::FromRawFd;
    // std 的 UdpSocket::bind 之前没法设 SO_REUSEADDR，自己建套接字
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | cloexec(), 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // 先交给 UdpSocket 管，出错时自动关掉
        let socket = UdpSocket::from_raw_fd(fd);
        set_cloexec(fd);
        let on: libc::c_int = 1;
        for option in [libc::SO_REUSEADDR, libc::SO_REUSEPORT] {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                option,
                &on as *const _ as *const libc::c_void,
                std::mem::size_of_val(&on) as libc::socklen_t,
            );
        }
        let mut addr: libc::sockaddr_in = std::mem::zeroed();
        addr.sin_family = libc::AF_INET as libc::sa_family_t;
        addr.sin_port = port.to_be();
        addr.sin_addr.s_addr = libc::INADDR_ANY;
        if libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of_val(&addr) as libc::socklen_t,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(socket)
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn cloexec() -> libc::c_int {
    libc::SOCK_CLOEXEC
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn cloexec() -> libc::c_int {
    0
}

/// 不让子进程（curl、llama-server）继承这个套接字。
#[cfg(unix)]
unsafe fn set_cloexec(fd: libc::c_int) {
    let flags = libc::fcntl(fd, libc::F_GETFD);
    if flags >= 0 {
        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }
}

#[cfg(not(unix))]
fn bind_udp_reuse(port: u16) -> io::Result<UdpSocket> {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))
}

/// 广播往哪发：受限广播地址，加上每块网卡自己的广播地址（多块网卡、开着 VPN 时受限广播只走默认那块）。
pub fn broadcast_targets(port: u16) -> Vec<SocketAddr> {
    let mut targets = vec![SocketAddr::from((Ipv4Addr::BROADCAST, port))];
    for ip in interface_broadcasts() {
        let addr = SocketAddr::from((ip, port));
        if !targets.contains(&addr) {
            targets.push(addr);
        }
    }
    targets
}

#[cfg(unix)]
fn interface_broadcasts() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cursor = list;
        while !cursor.is_null() {
            let entry = &*cursor;
            cursor = entry.ifa_next;
            let flags = entry.ifa_flags as libc::c_int;
            if flags & libc::IFF_UP == 0
                || flags & libc::IFF_BROADCAST == 0
                || flags & libc::IFF_LOOPBACK != 0
                || entry.ifa_addr.is_null()
                || entry.ifa_netmask.is_null()
                || (*entry.ifa_addr).sa_family as libc::c_int != libc::AF_INET
            {
                continue;
            }
            let ip = (*(entry.ifa_addr as *const libc::sockaddr_in)).sin_addr.s_addr;
            let mask = (*(entry.ifa_netmask as *const libc::sockaddr_in))
                .sin_addr
                .s_addr;
            // 都是网络字节序，按位算完再转
            out.push(Ipv4Addr::from(u32::from_be(ip | !mask)));
        }
        libc::freeifaddrs(list);
    }
    out
}

#[cfg(not(unix))]
fn interface_broadcasts() -> Vec<Ipv4Addr> {
    Vec::new()
}

/// 绑收信的 TCP 端口：先试 `preferred`，被占了让系统挑一个。
pub fn bind_tcp(preferred: u16) -> io::Result<TcpListener> {
    TcpListener::bind((Ipv4Addr::UNSPECIFIED, preferred))
        .or_else(|_| TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)))
}

pub fn write_frame(stream: &mut impl Write, data: &[u8]) -> io::Result<()> {
    if data.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "消息太大"));
    }
    stream.write_all(&(data.len() as u32).to_be_bytes())?;
    stream.write_all(data)?;
    stream.flush()
}

pub fn read_frame(stream: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "消息太大"));
    }
    let mut data = vec![0u8; len];
    stream.read_exact(&mut data)?;
    Ok(data)
}

/// 连上对方、送一帧。
pub fn send_frame(addr: SocketAddr, data: &[u8]) -> io::Result<()> {
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.set_nodelay(true).ok();
    write_frame(&mut stream, data)
}

/// 收一条刚连上来的连接里的那一帧。
pub fn receive_frame(mut stream: TcpStream) -> io::Result<Vec<u8>> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    read_frame(&mut stream)
}

/// 配对时用的线路：一条 TCP 连接，一帧一条消息。
pub struct TcpChannel(pub TcpStream);

impl TcpChannel {
    pub fn connect(addr: SocketAddr) -> io::Result<Self> {
        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        stream.set_nodelay(true).ok();
        Ok(Self(stream))
    }
}

impl Channel for TcpChannel {
    fn send(&mut self, message: &[u8]) -> io::Result<()> {
        write_frame(&mut self.0, message)
    }
    fn recv(&mut self, timeout: Duration) -> io::Result<Vec<u8>> {
        self.0
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        read_frame(&mut self.0)
    }
}

/// 最近听到过的同组设备。
#[derive(Default)]
pub struct Peers {
    seen: HashMap<DeviceId, (SocketAddr, Instant)>,
}

impl Peers {
    /// 多久没听到广播就当它不在了。
    pub const FRESH: Duration = Duration::from_secs(20);

    pub fn saw(&mut self, device: DeviceId, addr: SocketAddr) {
        self.seen.insert(device, (addr, Instant::now()));
    }

    /// 现在还在的设备的收信地址。
    pub fn alive(&mut self) -> Vec<SocketAddr> {
        self.seen
            .retain(|_, (_, at)| at.elapsed() < Self::FRESH);
        let mut addrs: Vec<SocketAddr> = self.seen.values().map(|(addr, _)| *addr).collect();
        addrs.sort();
        addrs.dedup();
        addrs
    }

    pub fn count(&mut self) -> usize {
        self.alive().len()
    }

    pub fn clear(&mut self) {
        self.seen.clear();
    }
}

/// 听一会儿（最多 `wait`，找到一台以后再听 0.4 秒就回），找出局域网里正在等配对的设备：（配对地址, 设备名）。
pub fn discover_offers(socket: &UdpSocket, wait: Duration) -> Vec<(SocketAddr, String)> {
    let mut deadline = Instant::now() + wait;
    let mut found: Vec<(SocketAddr, String)> = Vec::new();
    let mut buffer = [0u8; 512];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if socket
            .set_read_timeout(Some(left.max(Duration::from_millis(1))))
            .is_err()
        {
            break;
        }
        let Ok((len, from)) = socket.recv_from(&mut buffer) else {
            continue;
        };
        if let Some((port, name)) = parse_offer(&buffer[..len]) {
            let addr = SocketAddr::new(from.ip(), port);
            if !found.iter().any(|(a, _)| *a == addr) {
                if found.is_empty() {
                    // 找到一台了：再听一小会儿（同一个网里还有别人在配对的话也收进来），不用等满
                    deadline = deadline.min(Instant::now() + Duration::from_millis(400));
                }
                found.push((addr, name));
            }
        }
    }
    found
}

/// 本机地址里对方连得上的那个（显示给用户、或者手动填地址时用）。
pub fn local_ip() -> Option<IpAddr> {
    // 不真的发包：连一个外网地址，看系统挑哪块网卡
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacons_are_signed() {
        let keys = GroupKeys::derive(&[1u8; 32]);
        let data = beacon(&keys, &[7u8; 8], 4321);
        assert_eq!(parse_beacon(&keys, &data), Some(([7u8; 8], 4321)));
        // 别的组认不出来
        assert_eq!(parse_beacon(&GroupKeys::derive(&[2u8; 32]), &data), None);
        // 改了端口（想把别人的信引到自己这里）签名就不对了
        let mut forged = data.clone();
        forged[14] ^= 1;
        assert_eq!(parse_beacon(&keys, &forged), None);
        assert_eq!(parse_beacon(&keys, &data[..10]), None);
        assert_eq!(parse_beacon(&keys, &beacon(&keys, &[7u8; 8], 0)), None);
    }

    #[test]
    fn offers() {
        let data = offer(5000, "客厅的电脑");
        assert_eq!(parse_offer(&data), Some((5000, "客厅的电脑".to_string())));
        assert_eq!(parse_offer(b"WTSP"), None);
        assert_eq!(parse_offer(&offer(0, "x")), None);
        let mut cut = data.clone();
        cut.truncate(data.len() - 1);
        assert_eq!(parse_offer(&cut), None, "名字不完整");
        let long = offer(1, &"长".repeat(100));
        assert_eq!(parse_offer(&long).unwrap().1.chars().count(), 40);
    }

    #[test]
    fn frames_over_tcp() {
        let listener = bind_tcp(0).unwrap();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, listener.local_addr().unwrap().port()));
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            receive_frame(stream).unwrap()
        });
        send_frame(addr, "一帧".as_bytes()).unwrap();
        assert_eq!(server.join().unwrap(), "一帧".as_bytes());
        assert!(write_frame(&mut Vec::new(), &vec![0u8; MAX_FRAME + 1]).is_err());
        // 声称的长度太大：不照着分配内存
        let mut huge = &[0xffu8, 0xff, 0xff, 0xff, 1][..];
        assert!(read_frame(&mut huge).is_err());
    }

    #[test]
    fn udp_port_can_be_shared() {
        let a = bind_udp(0).unwrap();
        let port = a.local_addr().unwrap().port();
        // 输入法和设置界面同时听同一个端口
        let b = bind_udp(port);
        assert!(b.is_ok(), "{:?}", b.err());
    }

    #[test]
    fn discovers_offers() {
        let listener = bind_udp(0).unwrap();
        let port = listener.local_addr().unwrap().port();
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        sender.send_to(b"junk", (Ipv4Addr::LOCALHOST, port)).unwrap();
        sender
            .send_to(&offer(7000, "书房"), (Ipv4Addr::LOCALHOST, port))
            .unwrap();
        sender
            .send_to(&offer(7000, "书房"), (Ipv4Addr::LOCALHOST, port))
            .unwrap();
        let found = discover_offers(&listener, Duration::from_millis(300));
        assert_eq!(
            found,
            vec![(
                SocketAddr::from((Ipv4Addr::LOCALHOST, 7000)),
                "书房".to_string()
            )]
        );
    }

    #[test]
    fn peers_expire() {
        let mut peers = Peers::default();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 1));
        peers.saw([1u8; 8], addr);
        peers.saw([1u8; 8], addr);
        assert_eq!(peers.alive(), vec![addr]);
        peers.seen.get_mut(&[1u8; 8]).unwrap().1 = Instant::now() - Peers::FRESH;
        assert!(peers.alive().is_empty());
    }

    #[test]
    fn broadcast_targets_include_limited_broadcast() {
        let targets = broadcast_targets(PORT);
        assert_eq!(targets[0], SocketAddr::from((Ipv4Addr::BROADCAST, PORT)));
        assert!(targets.iter().all(|t| t.port() == PORT));
    }
}
