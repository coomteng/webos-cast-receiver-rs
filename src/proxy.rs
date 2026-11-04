//! URL proxy: turns whatever the phone pushed (https, redirects, CDN links
//! that want a browser UA / Referer) into a plain LAN http URL the TV's
//! `<video>` can play. Streams bytes through; passes Range both ways so the
//! TV can seek. HLS playlists get their segment URLs rewritten to go through
//! the proxy too.
//!
//! `register(url) -> "/proxy/<id>"`; `handle(req, id)` serves it.
use crate::config::ProxyMode;
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode};
use futures_util::StreamExt;
use rand::Rng;
use regex::Regex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use url::Url;

const MAX_REDIRECTS: usize = 5;
const MAX_ENTRIES: usize = 2000;
/// Prefetch: the upstream request for a new clip is opened the moment the
/// phone sets the URI, and is registered as soon as upstream HEADERS arrive
/// (so the CDN handshake + TTFB are off the start-up path). Bytes flow into a
/// bounded channel; when it is full the task simply blocks, so the prefetch
/// never races more than ~PREFETCH_CHUNKS ahead of the player and never
/// competes with it for CDN bandwidth once the screen is consuming.
const PREFETCH_CHUNKS: usize = 256; // reqwest chunks are ~8–16 KB → ~2–4 MB
/// Unclaimed prefetches are dropped (upstream closed) after this long.
const PREFETCH_TTL: Duration = Duration::from_secs(20);

fn hop_headers() -> &'static HashSet<&'static str> {
    static H: OnceLock<HashSet<&'static str>> = OnceLock::new();
    H.get_or_init(|| {
        [
            "connection", "keep-alive", "transfer-encoding", "te", "trailer", "upgrade", "proxy-authorization",
            "proxy-authenticate", "host", "content-security-policy", "set-cookie", "strict-transport-security",
        ]
        .into_iter()
        .collect()
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub url: String,
    pub referer: Option<String>,
}

pub fn is_playlist(url: &str, content_type: Option<&str>) -> bool {
    if content_type.map(|c| c.to_ascii_lowercase().contains("mpegurl")).unwrap_or(false) {
        return true;
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)\.m3u8?$").unwrap());
    Url::parse(url).map(|u| re.is_match(u.path())).unwrap_or(false)
}

pub struct Proxy {
    mode: ProxyMode,
    user_agent: String,
    verbose: bool,
    client: reqwest::Client,
    // Insertion-ordered so the oldest entry is evicted first.
    entries: Mutex<(VecDeque<String>, HashMap<String, Entry>)>,
    counter: Mutex<u64>,
    // id -> prefetched `bytes=0-` response, claimed by the first full GET.
    prefetched: Mutex<HashMap<String, Prefetched>>,
}

type Chunk = Result<bytes::Bytes, reqwest::Error>;

/// A `GET bytes=0-` answered by upstream; the body is still being pulled by
/// the prefetch task into `rx` (bounded). Whoever claims it owns the stream.
struct Prefetched {
    status: StatusCode,
    headers: HeaderMap,
    rx: tokio::sync::mpsc::Receiver<Chunk>,
    buffered: Arc<std::sync::atomic::AtomicUsize>,
    started: std::time::Instant,
}

/// mpsc receiver as a Stream (avoids pulling in tokio-stream).
struct RecvStream(tokio::sync::mpsc::Receiver<Chunk>);
impl futures_util::Stream for RecvStream {
    type Item = Chunk;
    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Chunk>> {
        self.0.poll_recv(cx)
    }
}

impl Proxy {
    pub fn new(mode: ProxyMode, user_agent: &str, verbose: bool) -> Arc<Self> {
        crate::util::install_crypto();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .danger_accept_invalid_certs(true)
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(20))
            .pool_max_idle_per_host(16)
            .no_proxy()
            .build()
            .expect("reqwest client");
        Arc::new(Proxy { mode, user_agent: user_agent.to_string(), verbose, client, entries: Mutex::new(Default::default()), counter: Mutex::new(0), prefetched: Mutex::new(HashMap::new()) })
    }

    pub fn register(&self, url: &str) -> String {
        self.register_with(url, None)
    }

    pub fn register_with(&self, url: &str, referer: Option<&str>) -> String {
        let lower = url.to_ascii_lowercase();
        let is_http = lower.starts_with("http://");
        let is_https = lower.starts_with("https://");
        if url.is_empty() || self.mode == ProxyMode::Off || (!is_http && !is_https) || (self.mode == ProxyMode::HttpsOnly && is_http) {
            return url.to_string();
        }
        let n = {
            let mut c = self.counter.lock().unwrap();
            *c += 1;
            *c
        };
        let rnd: u32 = rand::rng().random();
        let id = format!("{}-{:06x}", to_base36(n), rnd & 0xff_ffff);
        let mut e = self.entries.lock().unwrap();
        e.1.insert(id.clone(), Entry { url: url.to_string(), referer: referer.map(str::to_string) });
        e.0.push_back(id.clone());
        while e.0.len() > MAX_ENTRIES {
            if let Some(old) = e.0.pop_front() {
                e.1.remove(&old);
            }
        }
        format!("/proxy/{id}")
    }

    pub fn lookup(&self, id: &str) -> Option<Entry> {
        self.entries.lock().unwrap().1.get(id).cloned()
    }

    /// Resolve redirects manually so we can keep headers and see the final URL.
    async fn fetch_upstream(&self, url: &str, method: Method, fwd: &HeaderMap, referer: Option<&str>) -> Result<(reqwest::Response, String), String> {
        let mut url = url.to_string();
        for _hop in 0..=MAX_REDIRECTS {
            let u = Url::parse(&url).map_err(|_| "bad url".to_string())?;
            let mut req = self
                .client
                .request(method.clone(), u.clone())
                .header(header::USER_AGENT, &self.user_agent)
                .header(header::ACCEPT, "*/*")
                .header(header::ACCEPT_ENCODING, "identity");
            for (k, v) in fwd {
                req = req.header(k, v);
            }
            if let Some(r) = referer {
                req = req.header(header::REFERER, r);
            }
            let res = req.send().await.map_err(|e| e.to_string())?;
            let st = res.status().as_u16();
            if matches!(st, 301 | 302 | 303 | 307 | 308) {
                if let Some(loc) = res.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()) {
                    let next = u.join(loc).map_err(|_| "bad redirect".to_string())?.to_string();
                    if self.verbose {
                        crate::logln!("[proxy] redirect {} -> {}", st, next.chars().take(120).collect::<String>());
                    }
                    url = next;
                    continue;
                }
            }
            return Ok((res, url));
        }
        Err("too many redirects".into())
    }

    /// Start fetching `bytes=0-` for a registered id right now, so the
    /// screen's first request is served from memory. Call right after
    /// `register` for the clip that is about to be loaded. No-op for
    /// non-proxied URLs, HLS playlists are not prefetched (they are rewritten
    /// per request).
    pub fn prefetch(self: &Arc<Self>, playable: &str) {
        let Some(id) = playable.rsplit_once("/proxy/").map(|(_, id)| id.to_string()) else { return };
        let Some(entry) = self.lookup(&id) else { return };
        if is_playlist(&entry.url, None) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let t0 = std::time::Instant::now();
            let mut fwd = HeaderMap::new();
            fwd.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));
            let Ok((up, final_url)) = me.fetch_upstream(&entry.url, Method::GET, &fwd, entry.referer.as_deref()).await else {
                if me.verbose {
                    crate::logln!("[proxy] prefetch {} failed", id);
                }
                return;
            };
            let status = up.status();
            if status.as_u16() >= 400 || is_playlist(&final_url, up.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok())) {
                return; // let the real request see the error / playlist path
            }
            if me.verbose {
                crate::logln!("[proxy] prefetch {} headers -> {} ttfb={}ms", id, status.as_u16(), t0.elapsed().as_millis());
            }
            let (tx, rx) = tokio::sync::mpsc::channel::<Chunk>(PREFETCH_CHUNKS);
            let buffered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            me.prefetched.lock().unwrap().insert(id.clone(), Prefetched { status, headers: up.headers().clone(), rx, buffered: buffered.clone(), started: t0 });
            // Expire if nobody claims it (screen never asked / asked with another Range).
            let me2 = me.clone();
            let id2 = id.clone();
            tokio::spawn(async move {
                tokio::time::sleep(PREFETCH_TTL).await;
                if me2.prefetched.lock().unwrap().remove(&id2).is_some() && me2.verbose {
                    crate::logln!("[proxy] prefetch {} expired unclaimed", id2);
                }
            });
            // Pump: blocks when the channel is full; ends when upstream ends or
            // the receiver is gone (expired, cleared, or client disconnected).
            let mut body = up.bytes_stream();
            while let Some(chunk) = body.next().await {
                let n = chunk.as_ref().map(|b| b.len()).unwrap_or(0);
                if tx.send(chunk).await.is_err() {
                    break;
                }
                buffered.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }

    /// Drop every pending prefetch (new clip supersedes the old one).
    pub fn clear_prefetch(&self) {
        self.prefetched.lock().unwrap().clear();
    }

    fn take_prefetched(&self, id: &str) -> Option<Prefetched> {
        self.prefetched.lock().unwrap().remove(id)
    }

    fn rewrite_playlist(&self, text: &str, base: &str, referer: Option<&str>) -> String {
        static URI: OnceLock<Regex> = OnceLock::new();
        let uri_re = URI.get_or_init(|| Regex::new(r#"URI="([^"]+)""#).unwrap());
        let base_url = Url::parse(base).ok();
        let abs = |s: &str| base_url.as_ref().and_then(|b| b.join(s).ok()).map(|u| u.to_string());
        text.split('\n')
            .map(|line| {
                let t = line.trim();
                if t.is_empty() {
                    return line.to_string();
                }
                if t.starts_with('#') {
                    // #EXT-X-KEY / #EXT-X-MAP carry URI="..."
                    return uri_re
                        .replace_all(line, |c: &regex::Captures| match abs(&c[1]) {
                            Some(u) => format!("URI=\"{}\"", self.register_with(&u, referer)),
                            None => c[0].to_string(),
                        })
                        .into_owned();
                }
                match abs(t) {
                    Some(u) => self.register_with(&u, referer),
                    None => line.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub async fn handle(&self, method: &Method, headers: &HeaderMap, id: &str) -> Response<Body> {
        let Some(entry) = self.lookup(id) else { return text(StatusCode::NOT_FOUND, "unknown proxy id") };
        let method = if *method == Method::HEAD { Method::HEAD } else { Method::GET };
        let mut fwd = HeaderMap::new();
        for k in [header::RANGE, header::IF_RANGE] {
            if let Some(v) = headers.get(&k) {
                fwd.insert(k, v.clone());
            }
        }
        let t0 = std::time::Instant::now();
        let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).unwrap_or("-").to_string();
        let from_start = range == "-" || range == "bytes=0-";
        if method == Method::GET && from_start {
            if let Some(pf) = self.take_prefetched(id) {
                return self.serve_prefetched(id, pf, range == "-");
            }
        }
        let (up, final_url) = match self.fetch_upstream(&entry.url, method.clone(), &fwd, entry.referer.as_deref()).await {
            Ok(x) => x,
            Err(e) => {
                if self.verbose {
                    crate::logln!("[proxy] upstream error {} {}", entry.url.chars().take(100).collect::<String>(), e);
                }
                return text(StatusCode::BAD_GATEWAY, &format!("upstream: {e}"));
            }
        };
        let status = up.status();
        let ttfb = t0.elapsed();
        if self.verbose {
            crate::logln!(
                "[proxy] {} {} range={} -> {} ttfb={}ms len={} cr={}",
                method, id, range, status.as_u16(), ttfb.as_millis(),
                up.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).unwrap_or("-"),
                up.headers().get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()).unwrap_or("-")
            );
        }
        let mut out = shape_headers(up.headers(), status);
        let ct = up.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
        if status.as_u16() < 400 && method == Method::GET && is_playlist(&final_url, ct.as_deref()) {
            return match up.bytes().await {
                Ok(b) => {
                    let body = self.rewrite_playlist(&String::from_utf8_lossy(&b), &final_url, entry.referer.as_deref());
                    out.remove(header::CONTENT_LENGTH);
                    out.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/vnd.apple.mpegurl"));
                    out.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
                    respond(StatusCode::OK, out, Body::from(body))
                }
                Err(_) => text(StatusCode::BAD_GATEWAY, "upstream playlist read failed"),
            };
        }
        if !out.contains_key(header::CONTENT_TYPE) {
            out.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
        }
        if method == Method::HEAD {
            return respond(status, out, Body::empty());
        }
        // Stream through. An upstream error mid-body just ends the stream
        // (the client sees a truncated body / reset), it never touches the
        // already-sent headers and never takes the process down.
        let verbose = self.verbose;
        let id_owned = id.to_string();
        let stream = up.bytes_stream().map(move |r| {
            r.map_err(|e| {
                if verbose {
                    crate::logln!("[proxy] upstream stream error {}", e);
                }
                std::io::Error::other(e)
            })
        });
        let stream = StreamAccount { inner: stream, bytes: 0, start: t0, id: id_owned, verbose };
        respond(status, out, Body::from_stream(stream))
    }
}

/// Counts streamed bytes and logs one line when the body ends (upstream EOF,
/// upstream error, or the client going away and dropping the body).
struct StreamAccount<S> {
    inner: S,
    bytes: u64,
    start: std::time::Instant,
    id: String,
    verbose: bool,
}

impl<S: futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin> futures_util::Stream for StreamAccount<S> {
    type Item = Result<bytes::Bytes, std::io::Error>;
    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        let r = std::pin::Pin::new(&mut self.inner).poll_next(cx);
        if let std::task::Poll::Ready(Some(Ok(b))) = &r {
            self.bytes += b.len() as u64;
        }
        r
    }
}

impl<S> Drop for StreamAccount<S> {
    fn drop(&mut self) {
        if self.verbose {
            let ms = self.start.elapsed().as_millis().max(1) as u64;
            crate::logln!("[proxy] end {} bytes={} dur={}ms rate={}KB/s", self.id, self.bytes, ms, self.bytes * 1000 / ms / 1024);
        }
    }
}

/// Upstream headers -> what we send the screen.
fn shape_headers(up: &HeaderMap, status: StatusCode) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in up {
        if hop_headers().contains(k.as_str()) {
            continue;
        }
        out.append(k.clone(), v.clone());
    }
    out.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // LG/webOS players seek by Range; advertise it when upstream didn't.
    if !out.contains_key(header::ACCEPT_RANGES) && status == StatusCode::OK {
        out.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    }
    out
}

impl Proxy {
    /// Answer a `GET bytes=0-` from the prefetched head, then continue on the
    /// same upstream stream. A request without any Range header gets a plain
    /// 200 with the full length (the prefetch asked for `bytes=0-`, which a
    /// 206 `Content-Range: bytes 0-N/N+1` describes exactly).
    fn serve_prefetched(&self, id: &str, pf: Prefetched, no_range: bool) -> Response<Body> {
        if self.verbose {
            let pumped = pf.buffered.load(std::sync::atomic::Ordering::Relaxed);
            crate::logln!("[proxy] GET {} range=bytes=0- -> {} from prefetch ({}KB already pulled, {} chunks waiting, age {}ms)", id, pf.status.as_u16(), pumped / 1024, pf.rx.len(), pf.started.elapsed().as_millis());
        }
        let mut status = pf.status;
        let mut out = shape_headers(&pf.headers, status);
        if no_range && status == StatusCode::PARTIAL_CONTENT {
            out.remove(header::CONTENT_RANGE);
            status = StatusCode::OK;
            if !out.contains_key(header::ACCEPT_RANGES) {
                out.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            }
        }
        if !out.contains_key(header::CONTENT_TYPE) {
            out.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
        }
        let verbose = self.verbose;
        let stream = RecvStream(pf.rx).map(move |r| {
            r.map_err(|e| {
                if verbose {
                    crate::logln!("[proxy] upstream stream error {}", e);
                }
                std::io::Error::other(e)
            })
        });
        let stream = StreamAccount { inner: stream, bytes: 0, start: std::time::Instant::now(), id: id.to_string(), verbose };
        respond(status, out, Body::from_stream(stream))
    }
}

fn to_base36(mut n: u64) -> String {
    const D: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut s = vec![];
    while n > 0 {
        s.push(D[(n % 36) as usize]);
        n /= 36;
    }
    s.reverse();
    String::from_utf8(s).unwrap()
}

fn respond(status: StatusCode, headers: HeaderMap, body: Body) -> Response<Body> {
    let mut r = Response::new(body);
    *r.status_mut() = status;
    *r.headers_mut() = headers;
    r
}

fn text(status: StatusCode, s: &str) -> Response<Body> {
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    respond(status, h, Body::from(s.to_string()))
}

#[allow(dead_code)]
pub fn header_name(s: &str) -> Option<HeaderName> {
    HeaderName::from_bytes(s.as_bytes()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_respects_mode() {
        let p = Proxy::new(ProxyMode::Always, "UA", false);
        assert!(p.register("https://a/b.mp4").starts_with("/proxy/"));
        assert_eq!(Proxy::new(ProxyMode::Off, "UA", false).register("https://a/b"), "https://a/b");
        let ho = Proxy::new(ProxyMode::HttpsOnly, "UA", false);
        assert_eq!(ho.register("http://a/b"), "http://a/b");
        assert!(ho.register("https://a/b").starts_with("/proxy/"));
        assert_eq!(p.register("rtsp://x"), "rtsp://x");
        let id = p.register("https://a/b.mp4");
        assert_eq!(p.lookup(&id["/proxy/".len()..]).unwrap().url, "https://a/b.mp4");
    }

    #[test]
    fn playlist_detection_and_rewrite() {
        assert!(is_playlist("http://x/a.m3u8?t=1", None));
        assert!(is_playlist("http://x/a", Some("application/vnd.apple.mpegurl")));
        assert!(!is_playlist("http://x/a.mp4", Some("video/mp4")));
        let p = Proxy::new(ProxyMode::Always, "UA", false);
        let out = p.rewrite_playlist("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4,\nseg0.ts\n#EXTINF:4,\nhttp://other/seg1.ts\n#EXT-X-ENDLIST\n", "http://cdn/p/pl.m3u8", Some("http://ref"));
        let lines: Vec<&str> = out.split('\n').collect();
        assert!(lines[1].contains("URI=\"/proxy/"));
        assert!(lines[3].starts_with("/proxy/"));
        assert_eq!(p.lookup(&lines[3]["/proxy/".len()..]).unwrap(), Entry { url: "http://cdn/p/seg0.ts".into(), referer: Some("http://ref".into()) });
        assert_eq!(p.lookup(&lines[5]["/proxy/".len()..]).unwrap().url, "http://other/seg1.ts");
        assert_eq!(lines[6], "#EXT-X-ENDLIST");
    }

    #[test]
    fn eviction() {
        let p = Proxy::new(ProxyMode::Always, "UA", false);
        let first = p.register("http://a/0");
        for i in 1..=MAX_ENTRIES {
            p.register(&format!("http://a/{i}"));
        }
        assert!(p.lookup(&first["/proxy/".len()..]).is_none());
    }
}
