//! SSDP advertiser for a MediaRenderer. Joins 239.255.255.250:1900, answers
//! M-SEARCH for our targets, sends NOTIFY ssdp:alive periodically and a
//! byebye burst on shutdown. Packet building / matching is pure so tests
//! cover it without a socket.
use rand::Rng;
use regex::Regex;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::net::UdpSocket;

pub const SSDP_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
pub const SSDP_PORT: u16 = 1900;
const MAX_AGE: u64 = 1800;
pub const SERVER_HDR: &str = "Linux/4.4 UPnP/1.0 webos-cast-receiver/1";

pub fn targets_of(uuid: &str) -> Vec<String> {
    vec![
        "upnp:rootdevice".into(),
        format!("uuid:{uuid}"),
        "urn:schemas-upnp-org:device:MediaRenderer:1".into(),
        "urn:schemas-upnp-org:service:AVTransport:1".into(),
        "urn:schemas-upnp-org:service:RenderingControl:1".into(),
        "urn:schemas-upnp-org:service:ConnectionManager:1".into(),
    ]
}

fn usn_for(uuid: &str, st: &str) -> String {
    if st == format!("uuid:{uuid}") { st.to_string() } else { format!("uuid:{uuid}::{st}") }
}

pub struct Ssdp {
    pub uuid: String,
    pub location: String,
    pub bind_addr: Option<Ipv4Addr>,
}

/// Parsed M-SEARCH: which targets to answer and the MX (capped at 3).
pub struct Search {
    pub targets: Vec<String>,
    pub mx: u64,
}

impl Ssdp {
    pub fn m_search_response(&self, st: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age={MAX_AGE}\r\nDATE: {}\r\nEXT:\r\nLOCATION: {}\r\nSERVER: {SERVER_HDR}\r\nST: {st}\r\nUSN: {}\r\n\r\n",
            httpdate::fmt_http_date(std::time::SystemTime::now()),
            self.location,
            usn_for(&self.uuid, st)
        )
    }

    pub fn notify_packet(&self, nt: &str, nts: &str) -> String {
        format!(
            "NOTIFY * HTTP/1.1\r\nHOST: {SSDP_ADDR}:{SSDP_PORT}\r\nCACHE-CONTROL: max-age={MAX_AGE}\r\nLOCATION: {}\r\nNT: {nt}\r\nNTS: {nts}\r\nSERVER: {SERVER_HDR}\r\nUSN: {}\r\n\r\n",
            self.location,
            usn_for(&self.uuid, nt)
        )
    }

    /// `None` when the datagram is not an M-SEARCH we should answer.
    pub fn parse_search(&self, text: &str) -> Option<Search> {
        static HEAD: OnceLock<Regex> = OnceLock::new();
        static MAN: OnceLock<Regex> = OnceLock::new();
        static ST: OnceLock<Regex> = OnceLock::new();
        static MX: OnceLock<Regex> = OnceLock::new();
        if !HEAD.get_or_init(|| Regex::new(r"(?i)^M-SEARCH \* HTTP/1\.1").unwrap()).is_match(text) {
            return None;
        }
        if !MAN.get_or_init(|| Regex::new(r#"(?i)MAN:\s*"ssdp:discover""#).unwrap()).is_match(text) {
            return None;
        }
        let st = ST.get_or_init(|| Regex::new(r"(?im)^ST:\s*(.+?)\s*$").unwrap()).captures(text)?[1].to_string();
        let all = targets_of(&self.uuid);
        let targets: Vec<String> = if st == "ssdp:all" { all } else { all.into_iter().filter(|t| *t == st).collect() };
        if targets.is_empty() {
            return None;
        }
        let mx = MX
            .get_or_init(|| Regex::new(r"(?im)^MX:\s*(\d+)").unwrap())
            .captures(text)
            .and_then(|c| c[1].parse::<u64>().ok())
            .unwrap_or(1)
            .min(3);
        Some(Search { targets, mx })
    }

    fn open_socket(&self) -> std::io::Result<UdpSocket> {
        use socket2::{Domain, Protocol, Socket, Type};
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        s.set_reuse_address(true)?;
        #[cfg(unix)]
        let _ = s.set_reuse_port(true);
        s.set_nonblocking(true)?;
        s.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSDP_PORT)).into())?;
        let iface = self.bind_addr.unwrap_or(Ipv4Addr::UNSPECIFIED);
        s.join_multicast_v4(&SSDP_ADDR, &iface)?;
        if let Some(b) = self.bind_addr {
            s.set_multicast_if_v4(&b)?;
        }
        s.set_multicast_ttl_v4(4)?;
        UdpSocket::from_std(s.into())
    }

    async fn notify_all(&self, sock: &UdpSocket, nts: &str) {
        let dst = SocketAddrV4::new(SSDP_ADDR, SSDP_PORT);
        for nt in targets_of(&self.uuid) {
            let _ = sock.send_to(self.notify_packet(&nt, nts).as_bytes(), dst).await;
        }
    }

    /// Runs until `shutdown` resolves, then sends byebye.
    pub async fn run(self: Arc<Self>, shutdown: impl std::future::Future<Output = ()>) {
        let sock = match self.open_socket() {
            Ok(s) => Arc::new(s),
            Err(e) => {
                crate::logln!("[ssdp] cannot open UDP {}: {} (no discovery!)", SSDP_PORT, e);
                shutdown.await;
                return;
            }
        };
        // Spec: send alive twice at start (UDP may lose the first).
        self.notify_all(&sock, "ssdp:alive").await;
        crate::logln!("[ssdp] advertising {} (uuid:{})", self.location, self.uuid);
        let me = self.clone();
        let s2 = sock.clone();
        let alive = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            me.notify_all(&s2, "ssdp:alive").await;
            let mut iv = tokio::time::interval(Duration::from_secs(MAX_AGE / 2));
            iv.tick().await;
            loop {
                iv.tick().await;
                me.notify_all(&s2, "ssdp:alive").await;
            }
        });
        let me = self.clone();
        let s3 = sock.clone();
        let reader = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = s3.recv_from(&mut buf).await else { continue };
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                let Some(search) = me.parse_search(&text) else { continue };
                for st in search.targets {
                    // Spread over [0, min(MX,3)) s but keep the first answer fast:
                    // phone UIs often list devices as soon as one response lands.
                    let delay = rand::rng().random_range(0..(search.mx * 300).max(100));
                    let pkt = me.m_search_response(&st);
                    let s4 = s3.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                        let _ = s4.send_to(pkt.as_bytes(), from).await;
                    });
                }
            }
        });
        shutdown.await;
        alive.abort();
        reader.abort();
        self.notify_all(&sock, "ssdp:byebye").await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn s() -> Ssdp {
        Ssdp { uuid: "abc-123".into(), location: "http://192.168.0.9:8790/dlna/device.xml".into(), bind_addr: None }
    }

    #[test]
    fn targets_cover_renderer_and_services() {
        let t = targets_of("x");
        assert!(t.contains(&"urn:schemas-upnp-org:device:MediaRenderer:1".to_string()));
        assert!(t.contains(&"urn:schemas-upnp-org:service:AVTransport:1".to_string()));
        assert_eq!(t.len(), 6);
    }

    #[test]
    fn packets() {
        let s = s();
        let r = s.m_search_response("urn:schemas-upnp-org:device:MediaRenderer:1");
        assert!(r.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(r.contains("LOCATION: http://192.168.0.9:8790/dlna/device.xml\r\n"));
        assert!(r.contains("USN: uuid:abc-123::urn:schemas-upnp-org:device:MediaRenderer:1\r\n"));
        let n = s.notify_packet("uuid:abc-123", "ssdp:alive");
        assert!(n.starts_with("NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n"));
        assert!(n.contains("NTS: ssdp:alive\r\n"));
        assert!(n.contains("USN: uuid:abc-123\r\n"));
    }

    #[test]
    fn search_matching() {
        let s = s();
        let m = s.parse_search("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n").unwrap();
        assert_eq!(m.targets, ["urn:schemas-upnp-org:device:MediaRenderer:1"]);
        assert_eq!(m.mx, 1);
        let all = s.parse_search("M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nMX: 7\r\nST: ssdp:all\r\n\r\n").unwrap();
        assert_eq!(all.targets.len(), 6);
        assert_eq!(all.mx, 3);
        assert!(s.parse_search("M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: urn:schemas-upnp-org:device:MediaServer:1\r\n\r\n").is_none());
        assert!(s.parse_search("NOTIFY * HTTP/1.1\r\nST: ssdp:all\r\n").is_none());
        assert!(s.parse_search("M-SEARCH * HTTP/1.1\r\nST: ssdp:all\r\n").is_none(), "missing MAN");
    }
}
