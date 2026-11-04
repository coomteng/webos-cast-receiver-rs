//! Command channel to the screens. A screen is any client running the player
//! page: the LG TV app (role `tv`) or a browser that opened `/` (role `web`).
//! Several screens may be connected, but exactly one is ACTIVE and actually
//! plays: a live TV always wins; otherwise the most recently seen web screen.
//! The others idle in standby and are told so in their poll responses. When
//! the active screen changes, the old one gets a `standby` command (stops
//! playback) and the new one gets `active`.
//!
//! Each screen long-polls `/tv/poll?client=<id>&role=tv|web&after=<seq>` and
//! POSTs `/tv/state` (which doubles as its heartbeat). Only the active
//! screen's transport reports are trusted by the renderer.
//!
//! All state lives behind one `Mutex`; waiters are `oneshot` senders so the
//! HTTP handler can simply `await` them (the 25 s timeout is applied there).
use crate::renderer::Command;
use crate::util::Clock;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Serialize)]
pub struct SeqCommand {
    pub seq: u64,
    #[serde(flatten)]
    pub cmd: Command,
}

#[derive(Debug, Clone, Serialize)]
pub struct PollResponse {
    pub commands: Vec<SeqCommand>,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaced: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct ClientInfo {
    pub ip: Option<String>,
    pub ua: Option<String>,
    pub name: Option<String>,
}

struct Client {
    id: String,
    role: &'static str,
    last_seen: f64,
    queue: Vec<SeqCommand>,
    waiter: Option<oneshot::Sender<PollResponse>>,
    info: ClientInfo,
    /// Cached "am I the active screen" so flush() can answer without the map.
    info_active: bool,
}

/// Fired outside the lock after a state change.
#[derive(Debug, PartialEq, Eq)]
pub enum Notice {
    /// Active screen changed to `Some(id)` (or none).
    ActiveChanged(Option<String>),
    /// There was an active screen and now there is none.
    Offline,
}

struct Inner {
    seq: u64,
    clients: HashMap<String, Client>,
    active_id: Option<String>,
}

pub struct TvChannel {
    timeout_ms: f64,
    now: Clock,
    verbose: bool,
    inner: Mutex<Inner>,
}

fn role_of(role: Option<&str>) -> &'static str {
    if role == Some("tv") { "tv" } else { "web" }
}

impl TvChannel {
    pub fn new(timeout_ms: u64, now: Clock, verbose: bool) -> Arc<Self> {
        Arc::new(TvChannel {
            timeout_ms: timeout_ms as f64,
            now,
            verbose,
            inner: Mutex::new(Inner { seq: 0, clients: HashMap::new(), active_id: None }),
        })
    }

    fn alive(&self, c: &Client) -> bool {
        (self.now)() - c.last_seen < self.timeout_ms
    }

    fn pick_active(&self, inner: &Inner) -> Option<String> {
        // TV beats web; within a role, keep the current active one if still
        // alive (no flapping), else the most recently seen.
        let rank = |c: &Client| {
            (if c.role == "tv" { 2.0 } else { 1.0 }) * 1e15
                + if Some(&c.id) == inner.active_id.as_ref() { 1e14 } else { 0.0 }
                + c.last_seen
        };
        inner
            .clients
            .values()
            .filter(|c| self.alive(c))
            .max_by(|a, b| rank(a).partial_cmp(&rank(b)).unwrap())
            .map(|c| c.id.clone())
    }

    fn enqueue(inner: &mut Inner, id: &str, cmd: Command) -> Option<SeqCommand> {
        inner.seq += 1;
        let seq = inner.seq;
        let c = inner.clients.get_mut(id)?;
        if cmd.cmd == "load" {
            c.queue.retain(|x| matches!(x.cmd.cmd.as_str(), "volume" | "mute" | "active" | "standby"));
        }
        let msg = SeqCommand { seq, cmd };
        c.queue.push(msg.clone());
        if c.queue.len() > 50 {
            let drop = c.queue.len() - 50;
            c.queue.drain(..drop);
        }
        Self::flush(c);
        Some(msg)
    }

    fn flush(c: &mut Client) {
        if c.waiter.is_none() || c.queue.is_empty() {
            return;
        }
        let w = c.waiter.take().unwrap();
        let batch = std::mem::take(&mut c.queue);
        let active = c.info_active;
        let _ = w.send(PollResponse { commands: batch, active, replaced: None });
    }

    fn reconcile(&self, inner: &mut Inner, notices: &mut Vec<Notice>) {
        let prev = inner.active_id.clone();
        let next = self.pick_active(inner);
        if prev == next {
            return;
        }
        inner.active_id = next.clone();
        for c in inner.clients.values_mut() {
            c.info_active = Some(&c.id) == next.as_ref();
        }
        if let Some(p) = &prev {
            if inner.clients.contains_key(p) {
                Self::enqueue(inner, p, Command::new("standby"));
            }
        }
        if let Some(n) = &next {
            Self::enqueue(inner, n, Command::new("active"));
        }
        if self.verbose {
            crate::logln!("[tv] active screen: {:?} -> {:?}", prev, next);
        }
        notices.push(Notice::ActiveChanged(next.clone()));
        if prev.is_some() && next.is_none() {
            notices.push(Notice::Offline);
        }
    }

    fn touch(&self, inner: &mut Inner, id: &str, role: Option<&str>, info: Option<ClientInfo>, notices: &mut Vec<Notice>) {
        let stale = inner.clients.get(id).map(|c| !self.alive(c)).unwrap_or(false);
        if stale {
            // a returning id starts clean (no stale standby/active in its queue)
            inner.clients.remove(id);
        }
        let now = (self.now)();
        let c = inner.clients.entry(id.to_string()).or_insert_with(|| {
            if self.verbose {
                crate::logln!("[tv] screen joined {} {}", id, role_of(role));
            }
            Client { id: id.to_string(), role: role_of(role), last_seen: 0.0, queue: vec![], waiter: None, info: ClientInfo::default(), info_active: false }
        });
        if matches!(role, Some("tv") | Some("web")) {
            c.role = role_of(role);
        }
        c.last_seen = now;
        if let Some(i) = info {
            if i.ip.is_some() { c.info.ip = i.ip; }
            if i.ua.is_some() { c.info.ua = i.ua; }
            if i.name.is_some() { c.info.name = i.name; }
        }
        self.reconcile(inner, notices);
    }

    /// Drop screens unseen for 10× the timeout and recompute the active one.
    /// Call periodically (and after the timeout elapses) from a tokio task.
    pub fn sweep(&self) -> Vec<Notice> {
        let mut inner = self.inner.lock().unwrap();
        let now = (self.now)();
        let t = self.timeout_ms;
        inner.clients.retain(|_, c| !(now - c.last_seen >= t && c.waiter.is_none() && now - c.last_seen > t * 10.0));
        let mut n = vec![];
        self.reconcile(&mut inner, &mut n);
        n
    }

    // ---- renderer-facing ---------------------------------------------------
    pub fn is_online(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.active_id.as_ref().and_then(|id| inner.clients.get(id)).map(|c| self.alive(c)).unwrap_or(false)
    }

    pub fn is_active(&self, id: &str) -> bool {
        self.inner.lock().unwrap().active_id.as_deref() == Some(id)
    }

    pub fn active_id(&self) -> Option<String> {
        self.inner.lock().unwrap().active_id.clone()
    }

    pub fn send(&self, cmd: Command) -> Option<SeqCommand> {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.active_id.clone()?;
        let msg = Self::enqueue(&mut inner, &id, cmd)?;
        if self.verbose {
            crate::logln!("[tv] screen {} <- {} {}", id, msg.cmd.cmd, msg.seq);
        }
        Some(msg)
    }

    // ---- HTTP-facing -------------------------------------------------------
    /// Phase 1 of a poll: register/refresh the screen. Apply the returned
    /// notices (renderer side effects such as the handover `load`) BEFORE
    /// calling [`take_or_park`], so they land in this very response.
    pub fn touch_poll(&self, client: Option<&str>, role: Option<&str>, info: ClientInfo) -> (String, Vec<Notice>) {
        let id = client.map(str::to_string).or_else(|| info.ip.clone()).unwrap_or_else(|| "anon".into());
        let mut notices = vec![];
        let mut inner = self.inner.lock().unwrap();
        self.touch(&mut inner, &id, role, Some(info), &mut notices);
        (id, notices)
    }

    /// Phase 2: either an immediate response or a receiver to await (the
    /// caller applies the 25 s timeout and answers `{commands:[],active}`).
    pub fn take_or_park(&self, id: &str, after: Option<u64>) -> Result<PollResponse, oneshot::Receiver<PollResponse>> {
        let mut inner = self.inner.lock().unwrap();
        let active = inner.active_id.as_deref() == Some(id);
        let Some(c) = inner.clients.get_mut(id) else {
            return Ok(PollResponse { commands: vec![], active, replaced: None });
        };
        if let Some(n) = after {
            c.queue.retain(|x| x.seq > n);
        }
        if let Some(old) = c.waiter.take() {
            let _ = old.send(PollResponse { commands: vec![], active, replaced: Some(true) });
        }
        if !c.queue.is_empty() {
            let batch = std::mem::take(&mut c.queue);
            return Ok(PollResponse { commands: batch, active, replaced: None });
        }
        let (tx, rx) = oneshot::channel();
        c.waiter = Some(tx);
        Err(rx)
    }

    /// Convenience for tests / single-threaded callers: both phases, notices returned.
    pub fn poll(&self, client: Option<&str>, role: Option<&str>, after: Option<u64>, info: ClientInfo) -> (Result<PollResponse, oneshot::Receiver<PollResponse>>, Vec<Notice>) {
        let (id, notices) = self.touch_poll(client, role, info);
        (self.take_or_park(&id, after), notices)
    }

    /// Timed-out or disconnected poll: forget the waiter if it is still ours.
    pub fn poll_done(&self, client: Option<&str>, ip: Option<&str>) -> bool {
        let id = client.or(ip).unwrap_or("anon");
        let mut inner = self.inner.lock().unwrap();
        let active = inner.active_id.as_deref() == Some(id);
        if let Some(c) = inner.clients.get_mut(id) {
            if let Some(w) = c.waiter.take() {
                if w.is_closed() {
                    return active;
                }
                let _ = w.send(PollResponse { commands: vec![], active, replaced: None });
            }
        }
        active
    }

    /// Heartbeat/report from a screen. Returns true when it is the active one.
    pub fn report(&self, client: Option<&str>, role: Option<&str>, info: ClientInfo) -> (bool, Vec<Notice>) {
        let id = client.map(str::to_string).or_else(|| info.ip.clone()).unwrap_or_else(|| "anon".into());
        let mut notices = vec![];
        let mut inner = self.inner.lock().unwrap();
        self.touch(&mut inner, &id, role, Some(info), &mut notices);
        (inner.active_id.as_deref() == Some(id.as_str()), notices)
    }

    pub fn status(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        let a = inner.active_id.as_ref().and_then(|id| inner.clients.get(id));
        let online = a.map(|c| self.alive(c)).unwrap_or(false);
        json!({
            "online": online,
            "active": a.map(|c| json!({ "id": c.id, "role": c.role, "ip": c.info.ip, "name": c.info.name, "ua": c.info.ua })),
            "screens": inner.clients.values().filter(|c| self.alive(c)).map(|c| json!({ "id": c.id, "role": c.role, "ip": c.info.ip, "active": Some(&c.id) == inner.active_id.as_ref() })).collect::<Vec<_>>(),
            "seq": inner.seq,
        })
    }

    pub fn seq(&self) -> u64 {
        self.inner.lock().unwrap().seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    struct H {
        ch: Arc<TvChannel>,
        t: Arc<StdMutex<f64>>,
    }
    impl H {
        fn tick(&self, ms: f64) {
            *self.t.lock().unwrap() += ms;
        }
        /// Immediate poll result, or the parked receiver.
        fn poll(&self, client: &str, role: &str) -> (Result<PollResponse, oneshot::Receiver<PollResponse>>, Vec<Notice>) {
            self.ch.poll(Some(client), Some(role), Some(0), ClientInfo { ip: Some(client.into()), ..Default::default() })
        }
    }
    fn make() -> H {
        let t = Arc::new(StdMutex::new(0.0));
        let tc = t.clone();
        H { ch: TvChannel::new(1000, Arc::new(move || *tc.lock().unwrap()), false), t }
    }
    fn cmds(r: &PollResponse) -> Vec<String> {
        r.commands.iter().map(|c| c.cmd.cmd.clone()).collect()
    }

    #[test]
    fn web_alone_active_and_receives_commands() {
        let h = make();
        let (r, n) = h.poll("web1", "web");
        let r = r.unwrap();
        assert!(r.active);
        assert_eq!(cmds(&r), ["active"]);
        assert_eq!(n, [Notice::ActiveChanged(Some("web1".into()))]);
        assert!(h.ch.is_online());
        let (parked, _) = h.poll("web1", "web");
        let mut rx = parked.unwrap_err();
        h.ch.send(Command::new("play")).unwrap();
        let r2 = rx.try_recv().unwrap();
        assert_eq!(cmds(&r2), ["play"]);
    }

    #[test]
    fn tv_takes_over_then_hands_back() {
        let h = make();
        h.poll("web1", "web").0.unwrap();
        let mut w2 = h.poll("web1", "web").0.unwrap_err();
        let (tv, n) = h.poll("tv1", "tv");
        let tv = tv.unwrap();
        assert!(tv.active);
        assert_eq!(cmds(&tv), ["active"]);
        assert_eq!(n, [Notice::ActiveChanged(Some("tv1".into()))]);
        let w2 = w2.try_recv().unwrap();
        assert_eq!(cmds(&w2), ["standby"]);
        assert!(!w2.active);
        h.tick(600.0);
        let _ = h.poll("web1", "web");
        h.tick(600.0); // tv lastSeen now 1200 ago -> dead
        let n = h.ch.sweep();
        assert_eq!(h.ch.active_id().as_deref(), Some("web1"));
        assert_eq!(n, [Notice::ActiveChanged(Some("web1".into()))]);
    }

    #[test]
    fn reports_from_non_active_flagged_false() {
        let h = make();
        let _ = h.poll("tv1", "tv");
        let _ = h.poll("web1", "web");
        assert!(!h.ch.report(Some("web1"), Some("web"), ClientInfo::default()).0);
        assert!(h.ch.report(Some("tv1"), Some("tv"), ClientInfo::default()).0);
    }

    #[test]
    fn everyone_gone_offline_once() {
        let h = make();
        let _ = h.poll("web1", "web");
        h.tick(1200.0);
        let n = h.ch.sweep();
        assert!(!h.ch.is_online());
        assert_eq!(n, [Notice::ActiveChanged(None), Notice::Offline]);
        assert!(h.ch.sweep().is_empty());
    }

    #[test]
    fn new_poller_replaces_old() {
        let h = make();
        h.poll("tv1", "tv").0.unwrap();
        let mut a = h.poll("tv1", "tv").0.unwrap_err();
        let _b = h.poll("tv1", "tv").0.unwrap_err();
        let r = a.try_recv().unwrap();
        assert_eq!(r.replaced, Some(true));
        assert!(r.commands.is_empty());
    }

    #[test]
    fn load_clears_older_transport_commands() {
        let h = make();
        h.poll("tv1", "tv").0.unwrap();
        h.ch.send(Command::new("pause"));
        h.ch.send(Command::new("volume"));
        h.ch.send(Command::new("load"));
        let r = h.poll("tv1", "tv").0.unwrap();
        assert_eq!(cmds(&r), ["volume", "load"]);
    }
}
