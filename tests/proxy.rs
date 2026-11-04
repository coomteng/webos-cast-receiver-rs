//! Proxy over real HTTP against a fake CDN. Port of test/proxy.test.js.
mod common;
use common::*;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use webos_cast_receiver::config::Config;
use webos_cast_receiver::server::build;

type Seen = Arc<Mutex<Vec<(String, HashMap<String, String>)>>>;

/// Fake CDN: /redir -> 302 -> /video ; /video serves 1000 bytes with Range;
/// /pl.m3u8 is an HLS playlist with relative segments; /fail is 403.
async fn cdn(seen: Seen) -> String {
    let data = vec![7u8; 1000];
    serve(Arc::new(move |_m, path, headers, _b| {
        seen.lock().unwrap().push((path.clone(), headers.clone()));
        match path.as_str() {
            "/redir" => (302, h(&[("Location", "/video")]), vec![]),
            "/video" => {
                if let Some(r) = headers.get("range").and_then(|r| r.strip_prefix("bytes=")) {
                    let (s, e) = r.split_once('-').unwrap();
                    let s: usize = s.parse().unwrap();
                    let e: usize = if e.is_empty() { data.len() - 1 } else { e.parse().unwrap() };
                    let cr = format!("bytes {s}-{e}/{}", data.len());
                    return (206, h(&[("Content-Type", "video/mp4"), ("Content-Range", &cr), ("Accept-Ranges", "bytes")]), data[s..=e].to_vec());
                }
                (200, h(&[("Content-Type", "video/mp4")]), data.clone())
            }
            "/pl.m3u8" => (200, h(&[("Content-Type", "application/vnd.apple.mpegurl")]), b"#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4,\nseg0.ts\n#EXTINF:4,\nhttp://other/seg1.ts\n#EXT-X-ENDLIST\n".to_vec()),
            "/fail" => (403, vec![], b"nope".to_vec()),
            _ => (404, vec![], vec![]),
        }
    }))
    .await
}

async fn front(mode: &str) -> (String, Arc<webos_cast_receiver::server::App>) {
    let env: HashMap<String, String> = [("PORT", "0"), ("ADVERTISE_HOST", "127.0.0.1"), ("PROXY_MODE", mode), ("PROXY_UA", "TestUA/1"), ("UUID", "00000000-0000-0000-0000-000000000000")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let (app, router) = build(Config::from_map(env));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap() });
    (base, app)
}

#[tokio::test]
async fn redirect_ua_range_head_errors_hls() {
    let seen: Seen = Arc::new(Mutex::new(vec![]));
    let cdn = cdn(seen.clone()).await;
    let (fb, app) = front("always").await;
    let p = &app.proxy;

    // follows redirect, sets UA, streams full body
    let id = p.register(&format!("{cdn}/redir"));
    let r = get(&fb, &id).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body.len(), 1000);
    assert_eq!(r.header("content-type"), Some("video/mp4"));
    assert_eq!(r.header("accept-ranges"), Some("bytes"));
    {
        let s = seen.lock().unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].1.get("user-agent").map(String::as_str), Some("TestUA/1"));
    }

    // Range passes through both ways
    let id = p.register(&format!("{cdn}/video"));
    let r = req(&fb, "GET", &id, &[("Range", "bytes=100-199")], None).await;
    assert_eq!(r.status, 206);
    assert_eq!(r.header("content-range"), Some("bytes 100-199/1000"));
    assert_eq!(r.body.len(), 100);

    // HEAD
    let r = req(&fb, "HEAD", &id, &[], None).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-length"), Some("1000"));
    assert_eq!(r.body.len(), 0);

    // upstream 403 passed through; unknown id 404; dead upstream 502
    assert_eq!(get(&fb, &p.register(&format!("{cdn}/fail"))).await.status, 403);
    assert_eq!(get(&fb, "/proxy/nope").await.status, 404);
    assert_eq!(get(&fb, &p.register("http://127.0.0.1:9/x")).await.status, 502);

    // HLS rewrite
    let r = get(&fb, &p.register(&format!("{cdn}/pl.m3u8"))).await;
    assert_eq!(r.status, 200);
    let text = r.text();
    let lines: Vec<&str> = text.split('\n').collect();
    assert!(lines[1].contains("URI=\"/proxy/"), "{text}");
    assert!(lines[3].starts_with("/proxy/"));
    assert!(lines[5].starts_with("/proxy/"));
    assert_eq!(p.lookup(&lines[3]["/proxy/".len()..]).unwrap().url, format!("{cdn}/seg0.ts"));
    assert_eq!(p.lookup(&lines[5]["/proxy/".len()..]).unwrap().url, "http://other/seg1.ts");
    assert_eq!(r.header("content-type"), Some("application/vnd.apple.mpegurl"));
}

#[tokio::test]
async fn set_uri_registers_through_proxy_and_mode_off_passes_raw() {
    let (fb, app) = front("always").await;
    soap(&fb, "avt", "SetAVTransportURI", "<CurrentURI>https://cdn.example/a.mp4</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>").await;
    let st = get(&fb, "/status").await.json();
    assert_eq!(st["renderer"]["uri"], "https://cdn.example/a.mp4");
    let playable = app.renderer.lock().unwrap().playable_uri.clone();
    assert!(playable.starts_with("http://127.0.0.1:"), "{playable}");
    assert!(playable.contains("/proxy/"));
    let (fb2, app2) = front("off").await;
    soap(&fb2, "avt", "SetAVTransportURI", "<CurrentURI>https://cdn.example/a.mp4</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>").await;
    assert_eq!(app2.renderer.lock().unwrap().playable_uri, "https://cdn.example/a.mp4");
}

#[tokio::test]
async fn upstream_reset_mid_body_does_not_kill_server() {
    use tokio::io::AsyncWriteExt;
    // A server that sends headers + 64 KiB then resets the connection.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rst = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { break };
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
                let _ = s.write_all(b"HTTP/1.1 206 Partial\r\nContent-Type: video/mp4\r\nContent-Range: bytes 0-9999999/10000000\r\nContent-Length: 10000000\r\n\r\n").await;
                let _ = s.write_all(&[0u8; 65536]).await;
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                // SO_LINGER=0 turns the close into an RST.
                let std_s = s.into_std().unwrap();
                let sock = socket2::Socket::from(std_s);
                let _ = sock.set_linger(Some(std::time::Duration::ZERO));
                drop(sock);
            });
        }
    });
    let seen: Seen = Arc::new(Mutex::new(vec![]));
    let cdn = cdn(seen).await;
    let (fb, app) = front("always").await;
    let id = app.proxy.register(&format!("{rst}/v"));
    let r = req(&fb, "GET", &id, &[("Range", "bytes=0-")], None).await;
    assert_eq!(r.status, 206);
    assert!(r.body.len() < 10_000_000);
    // proxy still alive
    let ok = req(&fb, "GET", &app.proxy.register(&format!("{cdn}/video")), &[("Range", "bytes=0-9")], None).await;
    assert_eq!(ok.status, 206);
    assert_eq!(ok.body.len(), 10);
}

#[tokio::test]
async fn prefetch_serves_first_get_from_one_upstream_request() {
    use std::time::Duration;
    let seen: Seen = Arc::new(Mutex::new(vec![]));
    let cdn = cdn(seen.clone()).await;
    let (fb, app) = front("always").await;
    let p = app.proxy.clone();

    // register + prefetch (what set_uri does), give the task time to land
    let id = p.register(&format!("{cdn}/redir"));
    p.prefetch(&format!("{fb}{id}"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(seen.lock().unwrap().len(), 2, "prefetch followed the redirect: /redir + /video");

    // the screen's first request: served from the prefetch, no new upstream hit
    let r = req(&fb, "GET", &id, &[("Range", "bytes=0-")], None).await;
    assert_eq!(r.status, 206);
    assert_eq!(r.header("content-range"), Some("bytes 0-999/1000"));
    assert_eq!(r.body.len(), 1000);
    assert!(r.body.iter().all(|b| *b == 7));
    assert_eq!(seen.lock().unwrap().len(), 2, "no extra upstream request");

    // second identical request: prefetch consumed, goes upstream again
    let r = req(&fb, "GET", &id, &[("Range", "bytes=0-")], None).await;
    assert_eq!(r.status, 206);
    assert_eq!(seen.lock().unwrap().len(), 4, "redirect + video again");

    // a prefetched clip asked without Range -> plain 200, full body
    let id2 = p.register(&format!("{cdn}/video"));
    p.prefetch(&format!("{fb}{id2}"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r = get(&fb, &id2).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-range"), None);
    assert_eq!(r.header("accept-ranges"), Some("bytes"));
    assert_eq!(r.body.len(), 1000);

    // a mid-file Range bypasses the prefetch (and leaves it for a later bytes=0-)
    let id3 = p.register(&format!("{cdn}/video"));
    p.prefetch(&format!("{fb}{id3}"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let n = seen.lock().unwrap().len();
    let r = req(&fb, "GET", &id3, &[("Range", "bytes=500-599")], None).await;
    assert_eq!(r.status, 206);
    assert_eq!(r.body.len(), 100);
    assert_eq!(seen.lock().unwrap().len(), n + 1);
    let r = req(&fb, "GET", &id3, &[("Range", "bytes=0-")], None).await;
    assert_eq!(r.body.len(), 1000);
    assert_eq!(seen.lock().unwrap().len(), n + 1, "bytes=0- still came from the prefetch");

    // errors are not prefetched: the real request sees the 403
    let id4 = p.register(&format!("{cdn}/fail"));
    p.prefetch(&format!("{fb}{id4}"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(get(&fb, &id4).await.status, 403);

    // clear_prefetch drops pending ones (new clip supersedes)
    let id5 = p.register(&format!("{cdn}/video"));
    p.prefetch(&format!("{fb}{id5}"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    p.clear_prefetch();
    let n = seen.lock().unwrap().len();
    assert_eq!(req(&fb, "GET", &id5, &[("Range", "bytes=0-")], None).await.body.len(), 1000);
    assert_eq!(seen.lock().unwrap().len(), n + 1, "went upstream after clear");
}

#[tokio::test]
async fn set_uri_triggers_prefetch() {
    use std::time::Duration;
    let seen: Seen = Arc::new(Mutex::new(vec![]));
    let cdn = cdn(seen.clone()).await;
    let (fb, _app) = front("always").await;
    soap(&fb, "avt", "SetAVTransportURI", &format!("<CurrentURI>{cdn}/video</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let s = seen.lock().unwrap();
    assert_eq!(s.len(), 1, "upstream contacted before any screen asked");
    assert_eq!(s[0].1.get("range").map(String::as_str), Some("bytes=0-"));
}
