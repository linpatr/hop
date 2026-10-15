//! LAN discovery: hosts broadcast a small beacon every second; launchers listen for them.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};

use crate::proto::{PROTOCOL_VERSION, control_port, discovery_port};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Beacon {
    pub hop: u32,
    pub id: String,
    pub name: String,
    pub os: String,
    pub port: u16,
}

#[derive(Clone, Debug)]
pub struct Peer {
    pub beacon: Beacon,
    pub ip: IpAddr,
    pub last_seen: Instant,
}

impl Peer {
    pub fn addr(&self) -> String {
        format!("{}:{}", self.ip, self.beacon.port)
    }
}

fn broadcast_targets() -> Vec<SocketAddr> {
    let mut v = vec![SocketAddr::from((Ipv4Addr::BROADCAST, discovery_port()))];
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for i in ifs {
            if let if_addrs::IfAddr::V4(a) = i.addr
                && !a.ip.is_loopback()
                && let Some(b) = a.broadcast
            {
                let t = SocketAddr::from((b, discovery_port()));
                if !v.contains(&t) {
                    v.push(t);
                }
            }
        }
    }
    v
}

/// Local IPv4 addresses (for display).
pub fn local_ips() -> Vec<Ipv4Addr> {
    let mut v = Vec::new();
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for i in ifs {
            if let if_addrs::IfAddr::V4(a) = i.addr
                && !a.ip.is_loopback()
                && !a.ip.is_link_local()
            {
                v.push(a.ip);
            }
        }
    }
    v
}

pub fn start_beacon(id: String, name: String, stop: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("beacon".into())
        .spawn(move || {
            let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return };
            let _ = sock.set_broadcast(true);
            let msg = serde_json::to_vec(&Beacon {
                hop: PROTOCOL_VERSION,
                id,
                name,
                os: crate::proto::os_name().into(),
                port: control_port(),
            })
            .unwrap();
            let mut targets = broadcast_targets();
            let mut n = 0u32;
            while !stop.load(Ordering::Relaxed) {
                if n % 30 == 0 {
                    targets = broadcast_targets();
                }
                for t in &targets {
                    let _ = sock.send_to(&msg, t);
                }
                n += 1;
                for _ in 0..10 {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        })
        .unwrap();
}

#[derive(Clone, Default)]
pub struct Browser {
    pub peers: Arc<Mutex<HashMap<String, Peer>>>,
}

impl Browser {
    pub fn start(own_id: String) -> Browser {
        let b = Browser::default();
        let peers = b.peers.clone();
        std::thread::Builder::new()
            .name("discovery".into())
            .spawn(move || {
                let sock = match bind_shared(discovery_port()) {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("discovery listener failed: {e}");
                        return;
                    }
                };
                let mut buf = [0u8; 2048];
                loop {
                    let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
                    let Ok(beacon) = serde_json::from_slice::<Beacon>(&buf[..n]) else { continue };
                    if beacon.id == own_id || beacon.hop != PROTOCOL_VERSION {
                        continue;
                    }
                    peers.lock().insert(beacon.id.clone(), Peer { beacon, ip: from.ip(), last_seen: Instant::now() });
                }
            })
            .unwrap();
        b
    }

    /// Peers seen in the last few seconds, sorted by name.
    pub fn list(&self) -> Vec<Peer> {
        let mut v: Vec<Peer> = self
            .peers
            .lock()
            .values()
            .filter(|p| p.last_seen.elapsed() < Duration::from_secs(5))
            .cloned()
            .collect();
        v.sort_by(|a, b| a.beacon.name.cmp(&b.beacon.name));
        v
    }
}

fn bind_shared(port: u16) -> std::io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    s.set_broadcast(true)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    Ok(s.into())
}
