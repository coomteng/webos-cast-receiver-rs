//! GENA eventing: SUBSCRIBE / renew / UNSUBSCRIBE per service, and NOTIFY
//! delivery of LastChange (AVTransport / RenderingControl) and plain
//! variables (ConnectionManager). This is how the phone learns about
//! TransportState changes — in particular STOPPED after a clip ends.
//!
//! LastChange format (AVT): `<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/">
//!   <InstanceID val="0"><TransportState val="STOPPED"/>...</InstanceID></Event>`
//! embedded XML-escaped inside `<e:property><LastChange>..</LastChange></e:property>`.
//!
//! Delivery is abstracted behind [`Notifier`] so unit tests capture the
//! NOTIFYs; the real one is an HTTP client with a 5 s timeout.
use crate::device_xml::SINK_PROTOCOL_INFO;
use crate::renderer::{Event, Vars};
use crate::soap::format_time;
use crate::util::Clock;
use crate::xml::xml_escape;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

pub const DEFAULT_TIMEOUT: u64 = 300; // s
pub const MAX_TIMEOUT: u64 = 1800;
pub const MODERATION_MS: u64 = 200; // coalesce bursts of LastChange
const INITIAL_DELAY_MS: u64 = 50; // initial event must follow the SUBSCRIBE response

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Service {
    Avt,
    Rcs,
    Cms,
}

impl Service {
    pub fn parse(s: &str) -> Option<Service> {
        match s {
            "avt" => Some(Service::Avt),
            "rcs" => Some(Service::Rcs),
            "cms" => Some(Service::Cms),
            _ => None,
        }
    }
    fn last_change_ns(self) -> &'static str {
        match self {
            Service::Avt => "urn:schemas-upnp-org:metadata-1-0/AVT/",
            Service::Rcs => "urn:schemas-upnp-org:metadata-1-0/RCS/",
            Service::Cms => "",
        }
    }
}

pub fn parse_callback(header: Option<&str>) -> Vec<String> {
    let Some(h) = header else { return vec![] };
    Regex::new(r"<([^>]+)>")
        .unwrap()
        .captures_iter(h)
        .map(|c| c[1].to_string())
        .filter(|u| u.to_ascii_lowercase().starts_with("http://"))
        .collect()
}

pub fn parse_timeout(header: Option<&str>) -> u64 {
    let Some(h) = header else { return DEFAULT_TIMEOUT };
    let re = Regex::new(r"(?i)Second-(\d+|infinite)").unwrap();
    let Some(c) = re.captures(h) else { return DEFAULT_TIMEOUT };
    if c[1].eq_ignore_ascii_case("infinite") {
        return MAX_TIMEOUT;
    }
    c[1].parse::<u64>().map(|n| n.clamp(30, MAX_TIMEOUT)).unwrap_or(DEFAULT_TIMEOUT)
}

fn val_str(name: &str, v: &Value) -> String {
    match v {
        Value::Number(n) if name.contains("Duration") || name.contains("TimePosition") => format_time(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn last_change_xml(service: Service, vars: &Vars) -> String {
    let mut inner = String::new();
    for (k, v) in vars {
        let v = xml_escape(&val_str(k, v));
        if service == Service::Rcs && (*k == "Volume" || *k == "Mute") {
            inner.push_str(&format!("<{k} channel=\"Master\" val=\"{v}\"/>"));
        } else {
            inner.push_str(&format!("<{k} val=\"{v}\"/>"));
        }
    }
    format!("<Event xmlns=\"{}\"><InstanceID val=\"0\">{inner}</InstanceID></Event>", service.last_change_ns())
}

pub fn property_set(props: &[(&str, String)]) -> String {
    let mut body = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">");
    for (k, v) in props {
        body.push_str(&format!("<e:property><{k}>{}</{k}></e:property>", xml_escape(v)));
    }
    body + "</e:propertyset>"
}

/// One outgoing NOTIFY.
#[derive(Debug, Clone)]
pub struct Notify {
    pub url: String,
    pub sid: String,
    pub seq: u32,
    pub body: String,
}

pub type NotifyFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
pub trait Notifier: Send + Sync {
    fn notify(&self, n: Notify) -> NotifyFuture;
}

pub struct HttpNotifier(pub reqwest::Client);
impl HttpNotifier {
    /// GENA callbacks are plain `http://` URLs on the LAN. Skip the system CA
    /// store so the binary also runs in a `scratch` image without certificates.
    pub fn new() -> Self {
        crate::util::install_crypto();
        let c = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .expect("gena client");
        HttpNotifier(c)
    }
}
impl Default for HttpNotifier {
    fn default() -> Self {
        Self::new()
    }
}
impl Notifier for HttpNotifier {
    fn notify(&self, n: Notify) -> NotifyFuture {
        let client = self.0.clone();
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(b"NOTIFY").unwrap();
            let r = client
                .request(method, &n.url)
                .header("content-type", "text/xml; charset=\"utf-8\"")
                .header("NT", "upnp:event")
                .header("NTS", "upnp:propchange")
                .header("SID", &n.sid)
                .header("SEQ", n.seq.to_string())
                .header("connection", "close")
                .timeout(Duration::from_secs(5))
                .body(n.body)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let _ = r.bytes().await;
            Ok(())
        })
    }
}

struct Sub {
    service: Service,
    callbacks: Vec<String>,
    expires: f64,
    seq: u32,
    failures: u32,
}

#[derive(Default)]
struct Inner {
    subs: HashMap<String, Sub>,
    pending: HashMap<Service, Vars>,
}

/// Renderer-state snapshot for a new subscriber's initial event.
pub type InitialState = Arc<dyn Fn(Service) -> Vars + Send + Sync>;

pub struct Gena {
    now: Clock,
    verbose: bool,
    notifier: Arc<dyn Notifier>,
    initial: InitialState,
    inner: Mutex<Inner>,
}

pub struct SubResult {
    pub status: u16,
    pub sid: Option<String>,
    pub timeout: Option<u64>,
}

impl Gena {
    pub fn new(now: Clock, notifier: Arc<dyn Notifier>, initial: InitialState, verbose: bool) -> Arc<Self> {
        Arc::new(Gena { now, verbose, notifier, initial, inner: Mutex::new(Inner::default()) })
    }

    /// Consume renderer events: moderate LastChange and deliver NOTIFYs.
    /// Runs until the renderer is dropped.
    pub async fn run(self: Arc<Self>, mut events: mpsc::UnboundedReceiver<Event>) {
        loop {
            let Some(ev) = events.recv().await else { return };
            self.queue(ev);
            // Coalesce: everything arriving in the next MODERATION_MS joins this batch.
            let deadline = tokio::time::sleep(Duration::from_millis(MODERATION_MS));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    more = events.recv() => match more {
                        Some(ev) => self.queue(ev),
                        None => { self.flush_all().await; return }
                    },
                }
            }
            self.flush_all().await;
        }
    }

    fn queue(&self, ev: Event) {
        let (service, vars) = match ev {
            Event::Avt(v) => (Service::Avt, v),
            Event::Rcs(v) => (Service::Rcs, v),
            Event::Ended => return,
        };
        let mut inner = self.inner.lock().unwrap();
        let p = inner.pending.entry(service).or_default();
        for (k, v) in vars {
            if let Some(slot) = p.iter_mut().find(|(n, _)| *n == k) {
                slot.1 = v;
            } else {
                p.push((k, v));
            }
        }
    }

    /// Deliver pending LastChange for both services (public for tests).
    pub async fn flush_all(&self) {
        let batches: Vec<Notify> = {
            let mut inner = self.inner.lock().unwrap();
            let pending = std::mem::take(&mut inner.pending);
            let mut out = vec![];
            for (service, vars) in pending {
                if vars.is_empty() {
                    continue;
                }
                let props = vec![("LastChange", last_change_xml(service, &vars))];
                let sids: Vec<String> = inner.subs.iter().filter(|(_, s)| s.service == service).map(|(k, _)| k.clone()).collect();
                for sid in sids {
                    if let Some(n) = Self::prepare(&mut inner, &sid, &props) {
                        out.push(n);
                    }
                }
            }
            out
        };
        for n in batches {
            self.deliver(n).await;
        }
    }

    fn prepare(inner: &mut Inner, sid: &str, props: &[(&str, String)]) -> Option<Notify> {
        let sub = inner.subs.get_mut(sid)?;
        // spec: deliver to the first callback; keep it simple
        let url = sub.callbacks.first()?.clone();
        let n = Notify { url, sid: sid.to_string(), seq: sub.seq, body: property_set(props) };
        sub.seq = sub.seq.wrapping_add(1);
        Some(n)
    }

    async fn deliver(&self, n: Notify) {
        let sid = n.sid.clone();
        let url = n.url.clone();
        match self.notifier.notify(n).await {
            Ok(()) => {
                if let Some(s) = self.inner.lock().unwrap().subs.get_mut(&sid) {
                    s.failures = 0;
                }
            }
            Err(e) => {
                if self.verbose {
                    crate::logln!("[gena] notify failed {} {} {}", sid, url, e);
                }
                let mut inner = self.inner.lock().unwrap();
                let drop = inner.subs.get_mut(&sid).map(|s| {
                    s.failures += 1;
                    s.failures >= 3
                });
                if drop == Some(true) {
                    inner.subs.remove(&sid);
                    if self.verbose {
                        crate::logln!("[gena] dropped dead subscriber {}", sid);
                    }
                }
            }
        }
    }

    fn initial_props(&self, service: Service) -> Vec<(&'static str, String)> {
        if service == Service::Cms {
            return vec![
                ("SourceProtocolInfo", String::new()),
                ("SinkProtocolInfo", SINK_PROTOCOL_INFO.to_string()),
                ("CurrentConnectionIDs", "0".to_string()),
            ];
        }
        vec![("LastChange", last_change_xml(service, &(self.initial)(service)))]
    }

    fn purge(&self, inner: &mut Inner) {
        let t = (self.now)();
        inner.subs.retain(|_, s| s.expires > t);
    }

    pub fn subscribe(self: &Arc<Self>, service: Service, sid: Option<&str>, nt: Option<&str>, callback: Option<&str>, timeout: Option<&str>) -> SubResult {
        let mut inner = self.inner.lock().unwrap();
        self.purge(&mut inner);
        let secs = parse_timeout(timeout);
        if let Some(sid) = sid {
            let Some(sub) = inner.subs.get_mut(sid) else { return SubResult { status: 412, sid: None, timeout: None } };
            if sub.service != service {
                return SubResult { status: 412, sid: None, timeout: None };
            }
            if callback.is_some() || nt.is_some() {
                return SubResult { status: 400, sid: None, timeout: None };
            }
            sub.expires = (self.now)() + secs as f64 * 1000.0;
            return SubResult { status: 200, sid: Some(sid.to_string()), timeout: Some(secs) };
        }
        if nt.map(str::trim) != Some("upnp:event") {
            return SubResult { status: 412, sid: None, timeout: None };
        }
        let callbacks = parse_callback(callback);
        if callbacks.is_empty() {
            return SubResult { status: 412, sid: None, timeout: None };
        }
        let sid = format!("uuid:{}", uuid::Uuid::new_v4());
        if self.verbose {
            crate::logln!("[gena] subscribe {:?} {} {}", service, sid, callbacks[0]);
        }
        inner.subs.insert(sid.clone(), Sub { service, callbacks, expires: (self.now)() + secs as f64 * 1000.0, seq: 0, failures: 0 });
        drop(inner);
        let me = self.clone();
        let sid2 = sid.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(INITIAL_DELAY_MS)).await;
            me.send_initial(service, &sid2).await;
        });
        SubResult { status: 200, sid: Some(sid), timeout: Some(secs) }
    }

    pub async fn send_initial(&self, service: Service, sid: &str) {
        let props = self.initial_props(service);
        let n = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.subs.contains_key(sid) {
                return;
            }
            Self::prepare(&mut inner, sid, &props)
        };
        if let Some(n) = n {
            self.deliver(n).await;
        }
    }

    pub fn unsubscribe(&self, service: Service, sid: Option<&str>) -> u16 {
        let Some(sid) = sid else { return 412 };
        let mut inner = self.inner.lock().unwrap();
        match inner.subs.get(sid) {
            Some(s) if s.service == service => {
                inner.subs.remove(sid);
                200
            }
            _ => 412,
        }
    }

    pub fn sub_count(&self) -> usize {
        self.inner.lock().unwrap().subs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::tests::make;
    use crate::renderer::TvReport;
    use crate::xml::xml_unescape;
    use serde_json::json;

    struct Capture(Mutex<Vec<Notify>>);
    impl Notifier for Capture {
        fn notify(&self, n: Notify) -> NotifyFuture {
            self.0.lock().unwrap().push(n);
            Box::pin(async { Ok(()) })
        }
    }

    fn inner_lastchange(body: &str) -> String {
        let re = Regex::new(r"(?s)<LastChange>(.*?)</LastChange>").unwrap();
        xml_unescape(&re.captures(body).unwrap()[1])
    }

    #[test]
    fn parse_helpers() {
        assert_eq!(parse_callback(Some("<http://192.168.0.5:49152/cb><http://x/y>")), ["http://192.168.0.5:49152/cb", "http://x/y"]);
        assert_eq!(parse_timeout(Some("Second-1800")), 1800);
        assert_eq!(parse_timeout(Some("Second-5")), 30);
        assert_eq!(parse_timeout(None), 300);
        assert_eq!(parse_timeout(Some("Second-infinite")), 1800);
    }

    #[test]
    fn last_change_shapes() {
        let a = last_change_xml(Service::Avt, &vec![("TransportState", json!("PLAYING")), ("CurrentTrackDuration", json!(83))]);
        assert_eq!(a, "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\"><InstanceID val=\"0\"><TransportState val=\"PLAYING\"/><CurrentTrackDuration val=\"0:01:23\"/></InstanceID></Event>");
        let r = last_change_xml(Service::Rcs, &vec![("Volume", json!(10)), ("Mute", json!(0))]);
        assert!(r.contains("<Volume channel=\"Master\" val=\"10\"/><Mute channel=\"Master\" val=\"0\"/>"));
    }

    #[tokio::test]
    async fn subscribe_renew_unsubscribe() {
        let cap = Arc::new(Capture(Mutex::new(vec![])));
        let g = Gena::new(Arc::new(|| 0.0), cap.clone(), Arc::new(|_| vec![("TransportState", json!("NO_MEDIA_PRESENT"))]), false);
        let r = g.subscribe(Service::Avt, None, Some("upnp:event"), Some("<http://192.168.0.5:4000/cb>"), Some("Second-600"));
        assert_eq!(r.status, 200);
        let sid = r.sid.clone().unwrap();
        assert!(sid.starts_with("uuid:"));
        assert_eq!(r.timeout, Some(600));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let sent = cap.0.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].url, "http://192.168.0.5:4000/cb");
        assert_eq!(sent[0].seq, 0);
        assert!(sent[0].body.contains("<e:propertyset"));
        assert!(inner_lastchange(&sent[0].body).contains("<TransportState val=\"NO_MEDIA_PRESENT\"/>"));
        assert_eq!(g.subscribe(Service::Avt, Some(&sid), None, None, Some("Second-300")).status, 200);
        assert_eq!(g.subscribe(Service::Avt, Some("uuid:nope"), None, None, None).status, 412);
        assert_eq!(g.subscribe(Service::Avt, None, Some("wrong"), Some("<http://a/b>"), None).status, 412);
        assert_eq!(g.unsubscribe(Service::Avt, Some(&sid)), 200);
        assert_eq!(g.unsubscribe(Service::Avt, Some(&sid)), 412);
    }

    #[tokio::test]
    async fn moderated_notify_with_seq() {
        let cap = Arc::new(Capture(Mutex::new(vec![])));
        let mut h = make(true);
        let g = Gena::new(Arc::new(|| 0.0), cap.clone(), Arc::new(|_| vec![]), false);
        let r = g.subscribe(Service::Avt, None, Some("upnp:event"), Some("<http://h:1/cb>"), None);
        let sid = r.sid.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        cap.0.lock().unwrap().clear();
        h.r.set_uri("http://x/a.mp4", "").unwrap();
        h.r.on_tv_report(&TvReport { load_id: Some(1), state: Some("playing".into()), position: Some(0.0), duration: Some(12.0), ..Default::default() });
        for e in h.drain() {
            g.queue(e);
        }
        g.flush_all().await;
        let sent = cap.0.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "burst coalesced into one NOTIFY");
        assert_eq!(sent[0].seq, 1);
        let inner = inner_lastchange(&sent[0].body);
        assert!(inner.contains("<TransportState val=\"PLAYING\"/>"));
        assert!(inner.contains("<AVTransportURI val=\"http://x/a.mp4\"/>"));
        assert!(inner.contains("<CurrentTrackDuration val=\"0:00:12\"/>"));
        cap.0.lock().unwrap().clear();
        h.r.on_tv_report(&TvReport { load_id: Some(1), state: Some("ended".into()), position: Some(12.0), ..Default::default() });
        for e in h.drain() {
            g.queue(e);
        }
        g.flush_all().await;
        let sent = cap.0.lock().unwrap().clone();
        assert!(inner_lastchange(&sent[0].body).contains("<TransportState val=\"STOPPED\"/>"));
        assert_eq!(sent[0].seq, 2);
        assert_eq!(sent[0].sid, sid);
    }

    #[tokio::test]
    async fn rcs_subscribers_only_get_volume() {
        let cap = Arc::new(Capture(Mutex::new(vec![])));
        let mut h = make(true);
        let g = Gena::new(Arc::new(|| 0.0), cap.clone(), Arc::new(|_| vec![]), false);
        g.subscribe(Service::Rcs, None, Some("upnp:event"), Some("<http://h:1/cb>"), None);
        tokio::time::sleep(Duration::from_millis(100)).await;
        cap.0.lock().unwrap().clear();
        h.r.set_uri("http://x/a.mp4", "").unwrap();
        h.r.set_volume("70").unwrap();
        for e in h.drain() {
            g.queue(e);
        }
        g.flush_all().await;
        let sent = cap.0.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        let inner = inner_lastchange(&sent[0].body);
        assert!(inner.contains("RCS/"));
        assert!(inner.contains("<Volume channel=\"Master\" val=\"70\"/>"));
        assert!(!inner.contains("TransportState"));
    }

    #[tokio::test]
    async fn three_failures_drop_subscriber() {
        struct Fail;
        impl Notifier for Fail {
            fn notify(&self, _: Notify) -> NotifyFuture {
                Box::pin(async { Err("ECONNREFUSED".into()) })
            }
        }
        let g = Gena::new(Arc::new(|| 0.0), Arc::new(Fail), Arc::new(|_| vec![]), false);
        g.subscribe(Service::Avt, None, Some("upnp:event"), Some("<http://h:1/cb>"), None);
        tokio::time::sleep(Duration::from_millis(100)).await; // initial = failure 1
        assert_eq!(g.sub_count(), 1);
        for _ in 0..2 {
            g.queue(Event::Avt(vec![("TransportState", json!("PLAYING"))]));
            g.flush_all().await;
        }
        assert_eq!(g.sub_count(), 0);
    }
}
