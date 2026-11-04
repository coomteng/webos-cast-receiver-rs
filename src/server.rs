//! HTTP face. Routes (identical to the Node version):
//!   /dlna/device.xml, /dlna/{avt,rcs,cms}.xml        descriptions
//!   POST /dlna/{avt,rcs,cms}/control                 SOAP
//!   SUBSCRIBE|UNSUBSCRIBE /dlna/{avt,rcs,cms}/event  GENA
//!   /proxy/<id>                                      media relay
//!   /tv/poll /tv/state /tv/info                      TV app channel
//!   /tv/ /sim /                                      static (TV app, simulator)
//!   /status /healthz                                 ops
use crate::actions;
use crate::config::Config;
use crate::device_xml::{avt_scpd, cms_scpd, device_description, rcs_scpd, DeviceIdentity, AVT, CMS, RCS};
use crate::gena::{Gena, HttpNotifier, Service};
use crate::proxy::Proxy;
use crate::renderer::{Command, Renderer, Screen, TvReport};
use crate::soap::{action_from_soapaction, soap_fault, soap_response, UpnpError};
use crate::ssdp::SERVER_HDR;
use crate::tvchannel::{ClientInfo, Notice, PollResponse, TvChannel};
use crate::util::system_clock;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use axum::routing::{any, get};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Component, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const POLL_MS: u64 = 25_000;
const BODY_LIMIT: usize = 512 * 1024;

pub struct App {
    pub cfg: Config,
    pub renderer: Arc<Mutex<Renderer>>,
    pub tv: Arc<TvChannel>,
    pub gena: Arc<Gena>,
    pub proxy: Arc<Proxy>,
}

pub type Shared = Arc<App>;

/// Renderer <-> TV channel glue: the renderer only sees the Screen trait.
struct TvScreen(Arc<TvChannel>);
impl Screen for TvScreen {
    fn is_online(&self) -> bool {
        self.0.is_online()
    }
    fn send(&self, cmd: Command) {
        self.0.send(cmd);
    }
}

/// Build everything and spawn the background tasks (GENA delivery, screen sweep).
pub fn build(cfg: Config) -> (Shared, Router) {
    crate::util::install_crypto();
    let now = system_clock();
    let verbose = cfg.verbose;
    let proxy = Proxy::new(cfg.proxy_mode, &cfg.proxy_user_agent, verbose);
    let tv = TvChannel::new(cfg.tv_timeout_ms, now.clone(), verbose);
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let base = cfg.base.clone();
    let p2 = proxy.clone();
    let renderer = Arc::new(Mutex::new(Renderer::new(
        Arc::new(TvScreen(tv.clone())),
        Box::new(move |uri| {
            let p = p2.register(uri);
            if p.starts_with('/') { format!("{base}{p}") } else { p }
        }),
        now.clone(),
        ev_tx,
        verbose,
    )));
    let r2 = renderer.clone();
    let gena = Gena::new(
        now,
        Arc::new(HttpNotifier::new()),
        Arc::new(move |svc| initial_state(&r2.lock().unwrap(), svc)),
        verbose,
    );
    let app = Arc::new(App { cfg, renderer, tv, gena, proxy });
    tokio::spawn(app.gena.clone().run(ev_rx));
    // Sweep: detect screens that stopped polling/reporting.
    let a2 = app.clone();
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_millis(a2.cfg.tv_timeout_ms / 2 + 50));
        loop {
            iv.tick().await;
            let notices = a2.tv.sweep();
            apply_notices(&a2, notices);
        }
    });
    let router = Router::new()
        .route("/dlna/device.xml", get(device_xml))
        .route("/dlna/avt.xml", get(|| async { xml_res(StatusCode::OK, avt_scpd()) }))
        .route("/dlna/rcs.xml", get(|| async { xml_res(StatusCode::OK, rcs_scpd()) }))
        .route("/dlna/cms.xml", get(|| async { xml_res(StatusCode::OK, cms_scpd()) }))
        .route("/dlna/{svc}/control", any(control))
        .route("/dlna/{svc}/event", any(event))
        .route("/proxy/{id}", any(proxy_route))
        .route("/tv/poll", any(tv_poll))
        .route("/tv/state", any(tv_state))
        .route("/tv/info", any(tv_info))
        .route("/healthz", get(|| async { json_res(StatusCode::OK, json!({"ok": true})) }))
        .route("/status", get(status))
        .fallback(static_files)
        .with_state(app.clone());
    (app, router)
}

fn initial_state(r: &Renderer, svc: Service) -> crate::renderer::Vars {
    match svc {
        Service::Avt => vec![
            ("TransportState", json!(r.state.as_str())),
            ("TransportStatus", json!(r.status)),
            ("CurrentTransportActions", json!(r.current_actions())),
            ("AVTransportURI", json!(r.uri)),
            ("AVTransportURIMetaData", json!(r.metadata)),
            ("CurrentTrackURI", json!(r.uri)),
            ("CurrentTrackMetaData", json!(r.metadata)),
            ("CurrentTrackDuration", json!(r.duration)),
            ("CurrentMediaDuration", json!(r.duration)),
            ("NumberOfTracks", json!(if r.uri.is_empty() { 0 } else { 1 })),
            ("CurrentTrack", json!(if r.uri.is_empty() { 0 } else { 1 })),
            ("NextAVTransportURI", json!(r.next_uri)),
            ("NextAVTransportURIMetaData", json!(r.next_metadata)),
            ("PlaybackStorageMedium", json!(if r.uri.is_empty() { "NONE" } else { "NETWORK" })),
            ("CurrentPlayMode", json!("NORMAL")),
            ("TransportPlaySpeed", json!("1")),
        ],
        Service::Rcs => vec![("Volume", json!(r.volume)), ("Mute", json!(if r.mute { 1 } else { 0 }))],
        Service::Cms => vec![],
    }
}

/// Screen-channel side effects that must reach the renderer (outside the
/// channel lock).
fn apply_notices(app: &App, notices: Vec<Notice>) {
    for n in notices {
        match n {
            Notice::ActiveChanged(Some(_)) => app.renderer.lock().unwrap().on_screen_changed(),
            Notice::ActiveChanged(None) => {}
            Notice::Offline => app.renderer.lock().unwrap().on_tv_offline(),
        }
    }
}

// ---- response helpers --------------------------------------------------------

fn xml_res(status: StatusCode, body: String) -> Response<Body> {
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = status;
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/xml; charset=\"utf-8\""));
    h.insert(header::SERVER, HeaderValue::from_static(SERVER_HDR));
    h.insert("ext", HeaderValue::from_static(""));
    r
}

fn json_res(status: StatusCode, v: Value) -> Response<Body> {
    let mut r = Response::new(Body::from(v.to_string()));
    *r.status_mut() = status;
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn plain(status: StatusCode, s: &str) -> Response<Body> {
    let mut r = Response::new(Body::from(s.to_string()));
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    r
}

fn empty(status: StatusCode) -> Response<Body> {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = status;
    r
}

async fn read_body(body: Body) -> Result<String, Box<Response<Body>>> {
    match axum::body::to_bytes(body, BODY_LIMIT).await {
        Ok(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
        Err(_) => Err(Box::new(plain(StatusCode::PAYLOAD_TOO_LARGE, "body too large"))),
    }
}

// ---- descriptions --------------------------------------------------------------

async fn device_xml(State(app): State<Shared>) -> Response<Body> {
    let c = &app.cfg;
    xml_res(StatusCode::OK, device_description(&DeviceIdentity { uuid: &c.uuid, friendly_name: &c.friendly_name, manufacturer: &c.manufacturer, model_name: &c.model_name }))
}

// ---- SOAP control ----------------------------------------------------------------

async fn control(State(app): State<Shared>, Path(svc): Path<String>, method: Method, headers: HeaderMap, body: Body) -> Response<Body> {
    let Some(service) = Service::parse(&svc) else { return plain(StatusCode::NOT_FOUND, "not found") };
    if method != Method::POST {
        return empty(StatusCode::METHOD_NOT_ALLOWED);
    }
    let action = action_from_soapaction(headers.get("soapaction").and_then(|v| v.to_str().ok()));
    let body = match read_body(body).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let (kind, result) = {
        let mut r = app.renderer.lock().unwrap();
        match (service, action.as_deref()) {
            (Service::Avt, Some(a)) => (AVT, actions::avt(&mut r, a, &body)),
            (Service::Rcs, Some(a)) => (RCS, actions::rcs(&mut r, a, &body)),
            (Service::Cms, Some(a)) => (CMS, actions::cms(a)),
            _ => (AVT, None),
        }
    };
    let action = action.unwrap_or_else(|| "(none)".into());
    match result {
        None => {
            crate::logln!("[soap] {} {} -> 401 invalid action", svc, action);
            xml_res(StatusCode::INTERNAL_SERVER_ERROR, soap_fault(401, "Invalid Action"))
        }
        Some(Ok(out)) => {
            if !action.starts_with("Get") {
                let extra = if action == "SetAVTransportURI" { app.renderer.lock().unwrap().uri.chars().take(100).collect::<String>() } else { String::new() };
                crate::logln!("[soap] {} {}", action, extra);
            }
            xml_res(StatusCode::OK, soap_response(kind, &action, &out))
        }
        Some(Err(UpnpError { code, message })) => {
            crate::logln!("[soap] {} -> {} {}", action, code, message);
            xml_res(StatusCode::INTERNAL_SERVER_ERROR, soap_fault(code, &message))
        }
    }
}

// ---- GENA ------------------------------------------------------------------------

async fn event(State(app): State<Shared>, Path(svc): Path<String>, method: Method, headers: HeaderMap) -> Response<Body> {
    let Some(service) = Service::parse(&svc) else { return plain(StatusCode::NOT_FOUND, "not found") };
    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    let mut r = Response::new(Body::empty());
    r.headers_mut().insert(header::SERVER, HeaderValue::from_static(SERVER_HDR));
    r.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    match method.as_str() {
        "SUBSCRIBE" => {
            let res = app.gena.subscribe(service, h("sid"), h("nt"), h("callback"), h("timeout"));
            *r.status_mut() = StatusCode::from_u16(res.status).unwrap();
            if let (Some(sid), Some(t)) = (res.sid, res.timeout) {
                r.headers_mut().insert("sid", HeaderValue::from_str(&sid).unwrap());
                r.headers_mut().insert("timeout", HeaderValue::from_str(&format!("Second-{t}")).unwrap());
            }
        }
        "UNSUBSCRIBE" => *r.status_mut() = StatusCode::from_u16(app.gena.unsubscribe(service, h("sid"))).unwrap(),
        _ => *r.status_mut() = StatusCode::METHOD_NOT_ALLOWED,
    }
    r
}

// ---- media proxy -----------------------------------------------------------------

async fn proxy_route(State(app): State<Shared>, Path(id): Path<String>, method: Method, headers: HeaderMap) -> Response<Body> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return plain(StatusCode::NOT_FOUND, "not found");
    }
    app.proxy.handle(&method, &headers, &id).await
}

// ---- TV channel ------------------------------------------------------------------

fn cors_preflight() -> Response<Body> {
    let mut r = empty(StatusCode::NO_CONTENT);
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET,POST,OPTIONS"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("content-type"));
    h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("86400"));
    r
}

#[derive(Deserialize)]
struct PollQuery {
    client: Option<String>,
    role: Option<String>,
    after: Option<String>,
}

/// Guard that frees the parked waiter if the client goes away before we answer.
struct PollGuard {
    app: Shared,
    client: Option<String>,
    ip: String,
    done: bool,
}
impl Drop for PollGuard {
    fn drop(&mut self) {
        if !self.done {
            self.app.tv.poll_done(self.client.as_deref(), Some(&self.ip));
        }
    }
}

async fn tv_poll(State(app): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, Query(q): Query<PollQuery>, method: Method, headers: HeaderMap) -> Response<Body> {
    if method == Method::OPTIONS {
        return cors_preflight();
    }
    let ip = addr.ip().to_string();
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(str::to_string);
    let after = q.after.as_deref().and_then(|a| a.parse::<u64>().ok());
    let (id, notices) = app.tv.touch_poll(q.client.as_deref(), q.role.as_deref(), ClientInfo { ip: Some(ip.clone()), ua, name: None });
    apply_notices(&app, notices);
    let res = app.tv.take_or_park(&id, after);
    let resp: PollResponse = match res {
        Ok(r) => r,
        Err(rx) => {
            let mut guard = PollGuard { app: app.clone(), client: q.client.clone(), ip: ip.clone(), done: false };
            let out = match tokio::time::timeout(Duration::from_millis(POLL_MS), rx).await {
                Ok(Ok(r)) => r,
                _ => {
                    let active = app.tv.poll_done(q.client.as_deref(), Some(&ip));
                    PollResponse { commands: vec![], active, replaced: None }
                }
            };
            guard.done = true;
            out
        }
    };
    json_res(StatusCode::OK, serde_json::to_value(resp).unwrap())
}

async fn tv_state(State(app): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, method: Method, body: Body) -> Response<Body> {
    if method == Method::OPTIONS {
        return cors_preflight();
    }
    if method != Method::POST {
        return plain(StatusCode::NOT_FOUND, "not found");
    }
    let body = match read_body(body).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let report: TvReport = match serde_json::from_str(if body.is_empty() { "{}" } else { &body }) {
        Ok(r) => r,
        Err(_) => return json_res(StatusCode::BAD_REQUEST, json!({"error": "bad json"})),
    };
    let info = ClientInfo { ip: Some(addr.ip().to_string()), ua: None, name: report.name.clone() };
    let (active, notices) = app.tv.report(report.client.as_deref(), report.role.as_deref(), info);
    apply_notices(&app, notices);
    if active {
        app.renderer.lock().unwrap().on_tv_report(&report);
    }
    json_res(StatusCode::OK, json!({"ok": true, "active": active, "seq": app.tv.seq()}))
}

async fn tv_info(State(app): State<Shared>, method: Method) -> Response<Body> {
    if method == Method::OPTIONS {
        return cors_preflight();
    }
    let c = &app.cfg;
    json_res(StatusCode::OK, json!({"name": c.friendly_name, "host": c.advertise_host, "port": c.port, "uuid": c.uuid, "version": VERSION}))
}

// ---- ops -------------------------------------------------------------------------

async fn status(State(app): State<Shared>) -> Response<Body> {
    let renderer = app.renderer.lock().unwrap().snapshot();
    json_res(StatusCode::OK, json!({"renderer": renderer, "tv": app.tv.status(), "name": app.cfg.friendly_name, "base": app.cfg.base}))
}

// ---- static ----------------------------------------------------------------------

fn mime_of(p: &std::path::Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript",
        "css" => "text/css",
        "png" => "image/png",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

async fn send_file(root: &std::path::Path, rel: &str) -> Response<Body> {
    // Reject any traversal component before touching the filesystem.
    let rel_path = PathBuf::from(rel.trim_start_matches('/'));
    if rel_path.components().any(|c| !matches!(c, Component::Normal(_))) {
        return plain(StatusCode::NOT_FOUND, "not found");
    }
    let file = root.join(&rel_path);
    match tokio::fs::read(&file).await {
        Ok(data) => {
            let mut r = Response::new(Body::from(data));
            let h = r.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime_of(&file)));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
            r
        }
        Err(_) => plain(StatusCode::NOT_FOUND, "not found"),
    }
}

async fn static_files(State(app): State<Shared>, req: Request<Body>) -> Response<Body> {
    let p = req.uri().path().to_string();
    let app_dir = &app.cfg.app_dir;
    let sim_dir = &app.cfg.sim_dir;
    // Root = the player page: any browser can act as the screen when the TV is off.
    if p == "/" || p == "/tv" || p == "/tv/" {
        return send_file(app_dir, "index.html").await;
    }
    if p == "/sim" || p == "/sim/" {
        return send_file(sim_dir, "index.html").await;
    }
    if let Some(rest) = p.strip_prefix("/sim/") {
        return send_file(sim_dir, rest).await;
    }
    if let Some(rest) = p.strip_prefix("/tv/") {
        return send_file(app_dir, rest).await;
    }
    if p.starts_with("/css/") || p.starts_with("/js/") || p == "/icon.png" || p == "/largeIcon.png" {
        return send_file(app_dir, &p[1..]).await;
    }
    plain(StatusCode::NOT_FOUND, "not found")
}

