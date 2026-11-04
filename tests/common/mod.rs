//! Shared helpers for the HTTP-level tests: a tiny raw HTTP/1.1 client (so we
//! can send SUBSCRIBE/NOTIFY verbs and see every header untouched) and a
//! fake CDN / callback server built on hyper-free std TCP + tokio.
#![allow(dead_code)]
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct Resp {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers.get(&k.to_ascii_lowercase()).map(String::as_str)
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("bad json {e}: {}", self.text()))
    }
}

/// `base` like `http://127.0.0.1:1234`. Reads until the server closes or
/// content-length is satisfied (we always send `Connection: close`).
pub async fn req(base: &str, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> Resp {
    let hostport = base.trim_start_matches("http://");
    let mut s = TcpStream::connect(hostport).await.expect("connect");
    let mut out = format!("{method} {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        out.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    out.push_str("\r\n");
    s.write_all(out.as_bytes()).await.unwrap();
    if let Some(b) = body {
        s.write_all(b).await.unwrap();
    }
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf).await;
    parse_response(&buf)
}

pub fn parse_response(buf: &[u8]) -> Resp {
    let sep = buf.windows(4).position(|w| w == b"\r\n\r\n").expect("no header end");
    let head = String::from_utf8_lossy(&buf[..sep]).into_owned();
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut headers = HashMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut body = buf[sep + 4..].to_vec();
    if headers.get("transfer-encoding").map(|v| v.contains("chunked")).unwrap_or(false) {
        body = dechunk(&body);
    }
    Resp { status, headers, body }
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    while let Some(i) = b.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(String::from_utf8_lossy(&b[..i]).trim().split(';').next().unwrap(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        b = &b[i + 2..];
        out.extend_from_slice(&b[..size.min(b.len())]);
        b = &b[(size + 2).min(b.len())..];
    }
    out
}

pub async fn get(base: &str, path: &str) -> Resp {
    req(base, "GET", path, &[], None).await
}

pub async fn soap(base: &str, svc: &str, action: &str, args: &str) -> Resp {
    let kind = match svc { "avt" => "AVTransport", "rcs" => "RenderingControl", _ => "ConnectionManager" };
    let body = format!("<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:{action} xmlns:u=\"urn:schemas-upnp-org:service:{kind}:1\"><InstanceID>0</InstanceID>{args}</u:{action}></s:Body></s:Envelope>");
    let sa = format!("\"urn:schemas-upnp-org:service:{kind}:1#{action}\"");
    req(base, "POST", &format!("/dlna/{svc}/control"), &[("Content-Type", "text/xml; charset=\"utf-8\""), ("SOAPAction", &sa)], Some(body.as_bytes())).await
}

pub async fn tv_post(base: &str, v: serde_json::Value) -> Resp {
    req(base, "POST", "/tv/state", &[("Content-Type", "application/json")], Some(v.to_string().as_bytes())).await
}

/// Minimal HTTP/1.1 server for tests: `handler(method, path, headers, body) -> (status, headers, body)`.
pub type Handler = std::sync::Arc<dyn Fn(String, String, HashMap<String, String>, Vec<u8>) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync>;

pub async fn serve(handler: Handler) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { break };
            let h = handler.clone();
            tokio::spawn(async move {
                let mut buf = vec![];
                let mut tmp = [0u8; 4096];
                let (method, path, headers, body) = loop {
                    let n = match s.read(&mut tmp).await { Ok(0) | Err(_) => return, Ok(n) => n };
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(sep) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..sep]).into_owned();
                        let mut lines = head.split("\r\n");
                        let mut first = lines.next().unwrap().split_whitespace();
                        let method = first.next().unwrap().to_string();
                        let path = first.next().unwrap().to_string();
                        let mut headers = HashMap::new();
                        for l in lines {
                            if let Some((k, v)) = l.split_once(':') {
                                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                            }
                        }
                        let cl: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                        while buf.len() < sep + 4 + cl {
                            let n = match s.read(&mut tmp).await { Ok(0) | Err(_) => return, Ok(n) => n };
                            buf.extend_from_slice(&tmp[..n]);
                        }
                        break (method, path, headers, buf[sep + 4..sep + 4 + cl].to_vec());
                    }
                };
                let (status, hdrs, body) = h(method, path, headers, body);
                let mut out = format!("HTTP/1.1 {status} X\r\nConnection: close\r\n");
                let has_len = hdrs.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
                for (k, v) in &hdrs {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                if !has_len {
                    out.push_str(&format!("Content-Length: {}\r\n", body.len()));
                }
                out.push_str("\r\n");
                let _ = s.write_all(out.as_bytes()).await;
                let _ = s.write_all(&body).await;
                let _ = s.shutdown().await;
            });
        }
    });
    base
}

pub fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}
