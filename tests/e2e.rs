//! End-to-end over real HTTP: a fake phone (SOAP + GENA callback server) and
//! fake screens (poll/state) against the full router, no SSDP.
//! Port of the Node version's test/server.test.js.
mod common;
use common::*;
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use webos_cast_receiver::config::Config;
use webos_cast_receiver::server::build;

async fn start(tv_timeout_ms: &str) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let app_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../webos-cast-receiver/app");
    let env: HashMap<String, String> = [
        ("DATA_DIR", tmp.path().to_str().unwrap()),
        ("PORT", "0"),
        ("ADVERTISE_HOST", "127.0.0.1"),
        ("PROXY_MODE", "off"),
        ("TV_TIMEOUT_MS", tv_timeout_ms),
        ("APP_DIR", app_dir),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let cfg = Config::from_map(env);
    let (_app, router) = build(cfg);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(l, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
    });
    std::mem::forget(tmp);
    base
}

fn cmds(v: &serde_json::Value) -> Vec<String> {
    v["commands"].as_array().unwrap().iter().map(|c| c["cmd"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn descriptions_and_basics() {
    let base = start("2000").await;
    let d = get(&base, "/dlna/device.xml").await;
    assert_eq!(d.status, 200);
    assert!(d.header("content-type").unwrap().contains("text/xml"));
    assert!(d.text().contains("MediaRenderer:1"));
    for s in ["avt", "rcs", "cms"] {
        assert_eq!(get(&base, &format!("/dlna/{s}.xml")).await.status, 200);
    }
    let pi = soap(&base, "cms", "GetProtocolInfo", "").await;
    assert_eq!(pi.status, 200);
    assert!(pi.text().contains("video/mp4"));
    let v = soap(&base, "rcs", "GetVolume", "<Channel>Master</Channel>").await;
    assert!(v.text().contains("<CurrentVolume>50</CurrentVolume>"));
    soap(&base, "rcs", "SetVolume", "<Channel>Master</Channel><DesiredVolume>20</DesiredVolume>").await;
    assert!(soap(&base, "rcs", "GetVolume", "<Channel>Master</Channel>").await.text().contains("<CurrentVolume>20</CurrentVolume>"));

    // unknown action -> 500 + UPnP 401; Play with no TV -> 701
    let r = soap(&base, "avt", "Bogus", "").await;
    assert_eq!(r.status, 500);
    assert!(r.text().contains("<errorCode>401</errorCode>"));
    soap(&base, "avt", "SetAVTransportURI", "<CurrentURI>http://cdn/a.mp4</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>").await;
    let p = soap(&base, "avt", "Play", "<Speed>1</Speed>").await;
    assert_eq!(p.status, 500);
    assert!(p.text().contains("<errorCode>701</errorCode>"));

    // static + ops
    assert_eq!(get(&base, "/healthz").await.status, 200);
    assert_eq!(get(&base, "/tv/info").await.status, 200);
    let r = get(&base, "/").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.text().contains("<video"));
    assert_eq!(get(&base, "/js/app.js").await.status, 200);
    assert_eq!(get(&base, "/tv/../Cargo.toml").await.status, 404);
    assert_eq!(get(&base, "/tv/%2e%2e/Cargo.toml").await.status, 404);
}

#[tokio::test]
async fn full_cast_flow_with_gena_and_offline() {
    let base = start("2000").await;
    // GENA subscriber (fake phone callback server)
    let notifies: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(vec![]));
    let n2 = notifies.clone();
    let cb = serve(Arc::new(move |_m, _p, h, b| {
        n2.lock().unwrap().push((h.get("seq").cloned().unwrap_or_default(), String::from_utf8_lossy(&b).into_owned()));
        (200, vec![], vec![])
    }))
    .await;
    let sub = req(&base, "SUBSCRIBE", "/dlna/avt/event", &[("NT", "upnp:event"), ("CALLBACK", &format!("<{cb}/cb>")), ("TIMEOUT", "Second-300")], None).await;
    assert_eq!(sub.status, 200);
    assert!(sub.header("sid").unwrap().starts_with("uuid:"));

    // TV comes online: a poll parks
    let b2 = base.clone();
    let poll_p = tokio::spawn(async move { get(&b2, "/tv/poll?client=tv1&role=tv&after=0").await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    tv_post(&base, json!({"client": "tv1", "role": "tv", "name": "LG", "state": "idle"})).await;
    let st = get(&base, "/status").await.json();
    assert_eq!(st["tv"]["online"], true);

    // Phone pushes a URI -> TV's parked poll returns the load command
    let set = soap(&base, "avt", "SetAVTransportURI", "<CurrentURI>http://cdn/a.mp4</CurrentURI><CurrentURIMetaData>&lt;DIDL-Lite&gt;&lt;item&gt;&lt;dc:title&gt;hi&lt;/dc:title&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;</CurrentURIMetaData>").await;
    assert_eq!(set.status, 200);
    let poll = poll_p.await.unwrap().json();
    // first poll answers immediately with 'active'; the load lands in the next one
    assert_eq!(cmds(&poll), ["active"]);
    let poll_l = get(&base, &format!("/tv/poll?client=tv1&role=tv&after={}", poll["commands"][0]["seq"])).await.json();
    assert_eq!(cmds(&poll_l), ["load"]);
    let load = &poll_l["commands"][0];
    assert_eq!(load["url"], "http://cdn/a.mp4", "proxy off -> raw url");
    assert_eq!(load["title"], "hi");
    let load_id = load["loadId"].as_u64().unwrap();
    let load_seq = load["seq"].as_u64().unwrap();
    assert!(soap(&base, "avt", "GetTransportInfo", "").await.text().contains("TRANSITIONING"));

    // Play while TRANSITIONING is a no-op success (Douyin sends Play right after SetURI)
    assert_eq!(soap(&base, "avt", "Play", "<Speed>1</Speed>").await.status, 200);

    // TV reports playing
    tv_post(&base, json!({"client": "tv1", "role": "tv", "loadId": load_id, "state": "playing", "position": 2, "duration": 15})).await;
    let pi = soap(&base, "avt", "GetPositionInfo", "").await.text();
    assert!(pi.contains("<TrackDuration>0:00:15</TrackDuration>"), "{pi}");
    assert!(pi.contains("<RelTime>0:00:02</RelTime>") || pi.contains("<RelTime>0:00:03</RelTime>"), "{pi}");

    // Phone pauses -> TV's next poll gets it
    let b2 = base.clone();
    let p2 = tokio::spawn(async move { get(&b2, &format!("/tv/poll?client=tv1&role=tv&after={load_seq}")).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(soap(&base, "avt", "Pause", "").await.status, 200);
    let poll2 = p2.await.unwrap().json();
    assert_eq!(cmds(&poll2), ["pause"]);
    let seq2 = poll2["commands"][0]["seq"].as_u64().unwrap();

    // Seek
    let b2 = base.clone();
    let p3 = tokio::spawn(async move { get(&b2, &format!("/tv/poll?client=tv1&role=tv&after={seq2}")).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    soap(&base, "avt", "Seek", "<Unit>REL_TIME</Unit><Target>0:00:10</Target>").await;
    let poll3 = p3.await.unwrap().json();
    assert_eq!(poll3["commands"][0]["cmd"], "seek");
    assert_eq!(poll3["commands"][0]["position"], 10.0);

    // Ended -> STOPPED and a NOTIFY carrying it
    tv_post(&base, json!({"client": "tv1", "role": "tv", "loadId": load_id, "state": "ended", "position": 15, "duration": 15})).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(soap(&base, "avt", "GetTransportInfo", "").await.text().contains("<CurrentTransportState>STOPPED</CurrentTransportState>"));
    let n = notifies.lock().unwrap().clone();
    assert!(n.len() >= 2, "initial + change notifies, got {}", n.len());
    assert_eq!(n[0].0, "0");
    assert!(n.last().unwrap().1.contains("TransportState val=&quot;STOPPED&quot;"), "{}", n.last().unwrap().1);

    // TV offline after timeout -> 701 again and STOPPED
    tokio::time::sleep(Duration::from_millis(2300)).await;
    let st = get(&base, "/status").await.json();
    assert_eq!(st["tv"]["online"], false);
    let p = soap(&base, "avt", "Play", "<Speed>1</Speed>").await;
    assert!(p.text().contains("<errorCode>701</errorCode>"));
}

#[tokio::test]
async fn browser_screen_then_tv_takes_over_with_start_at() {
    let base = start("2000").await;
    let w1 = get(&base, "/tv/poll?client=web1&role=web&after=0").await.json();
    assert_eq!(cmds(&w1), ["active"]);
    tv_post(&base, json!({"client": "web1", "role": "web", "state": "idle"})).await;
    let b2 = base.clone();
    let s1 = w1["commands"][0]["seq"].as_u64().unwrap();
    let wp = tokio::spawn(async move { get(&b2, &format!("/tv/poll?client=web1&role=web&after={s1}")).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    soap(&base, "avt", "SetAVTransportURI", "<CurrentURI>http://cdn/b.mp4</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>").await;
    let w2 = wp.await.unwrap().json();
    assert_eq!(w2["commands"][0]["cmd"], "load");
    let load_id = w2["commands"][0]["loadId"].as_u64().unwrap();
    let s2 = w2["commands"][0]["seq"].as_u64().unwrap();
    tv_post(&base, json!({"client": "web1", "role": "web", "loadId": load_id, "state": "playing", "position": 7, "duration": 60})).await;
    assert!(soap(&base, "avt", "GetTransportInfo", "").await.text().contains("PLAYING"));
    // TV appears: gets active + load with startAt≈7; web gets standby
    let b2 = base.clone();
    let wp2 = tokio::spawn(async move { get(&b2, &format!("/tv/poll?client=web1&role=web&after={s2}")).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let t1 = get(&base, "/tv/poll?client=tv9&role=tv&after=0").await.json();
    assert_eq!(cmds(&t1), ["active", "load"]);
    let start_at = t1["commands"][1]["startAt"].as_f64().unwrap();
    assert!((7.0..9.0).contains(&start_at), "resume position carried over: {start_at}");
    assert_eq!(t1["active"], true);
    let w3 = wp2.await.unwrap().json();
    assert_eq!(cmds(&w3), ["standby"]);
    // web's late report is ignored, TV's is trusted
    tv_post(&base, json!({"client": "web1", "role": "web", "loadId": load_id, "state": "paused", "position": 8})).await;
    assert!(soap(&base, "avt", "GetTransportInfo", "").await.text().contains("TRANSITIONING"));
    tv_post(&base, json!({"client": "tv9", "role": "tv", "loadId": load_id, "state": "playing", "position": 7.5, "duration": 60})).await;
    assert!(soap(&base, "avt", "GetTransportInfo", "").await.text().contains("PLAYING"));
    let st = get(&base, "/status").await.json();
    assert_eq!(st["tv"]["active"]["role"], "tv");
    soap(&base, "avt", "Stop", "").await;
}

#[tokio::test]
async fn gena_renew_unsubscribe_and_cors() {
    let base = start("2000").await;
    let sub = req(&base, "SUBSCRIBE", "/dlna/rcs/event", &[("NT", "upnp:event"), ("CALLBACK", "<http://127.0.0.1:9/cb>")], None).await;
    assert_eq!(sub.status, 200);
    let sid = sub.header("sid").unwrap().to_string();
    assert_eq!(sub.header("timeout"), Some("Second-300"));
    let renew = req(&base, "SUBSCRIBE", "/dlna/rcs/event", &[("SID", &sid), ("TIMEOUT", "Second-60")], None).await;
    assert_eq!(renew.status, 200);
    assert_eq!(renew.header("timeout"), Some("Second-60"));
    assert_eq!(req(&base, "SUBSCRIBE", "/dlna/avt/event", &[("SID", &sid)], None).await.status, 412, "wrong service");
    assert_eq!(req(&base, "UNSUBSCRIBE", "/dlna/rcs/event", &[("SID", &sid)], None).await.status, 200);
    assert_eq!(req(&base, "UNSUBSCRIBE", "/dlna/rcs/event", &[("SID", &sid)], None).await.status, 412);
    assert_eq!(req(&base, "GET", "/dlna/rcs/event", &[], None).await.status, 405);
    let o = req(&base, "OPTIONS", "/tv/state", &[], None).await;
    assert_eq!(o.status, 204);
    assert_eq!(o.header("access-control-allow-origin"), Some("*"));
    assert_eq!(req(&base, "GET", "/dlna/avt/control", &[], None).await.status, 405);
}
