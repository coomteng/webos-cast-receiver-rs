//! Renderer state machine: the single source of truth that SOAP reads and
//! writes, the TV feeds back into, and GENA observes. No I/O here.
//!
//! Transport states follow AVTransport:1: NO_MEDIA_PRESENT, STOPPED,
//! TRANSITIONING, PLAYING, PAUSED_PLAYBACK. The TV reports its real player
//! state; between reports the position is extrapolated while PLAYING so
//! GetPositionInfo stays smooth for phone progress bars.
//!
//! Events (LastChange variables) go out through an unbounded channel; the
//! GENA task moderates and delivers them. Commands to the active screen go
//! through the [`Screen`] trait (implemented by the TV channel).
use crate::soap::UpnpError;
use crate::util::Clock;
use crate::xml::{tag_value, xml_unescape};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TransportState {
    #[serde(rename = "NO_MEDIA_PRESENT")]
    NoMedia,
    #[serde(rename = "STOPPED")]
    Stopped,
    #[serde(rename = "TRANSITIONING")]
    Transitioning,
    #[serde(rename = "PLAYING")]
    Playing,
    #[serde(rename = "PAUSED_PLAYBACK")]
    Paused,
}

impl TransportState {
    pub fn as_str(self) -> &'static str {
        match self {
            TransportState::NoMedia => "NO_MEDIA_PRESENT",
            TransportState::Stopped => "STOPPED",
            TransportState::Transitioning => "TRANSITIONING",
            TransportState::Playing => "PLAYING",
            TransportState::Paused => "PAUSED_PLAYBACK",
        }
    }
}

/// A command for the active screen. Serialised into the poll response; the
/// TV channel adds `seq`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Command {
    pub cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// Cover art URL (DIDL `upnp:albumArtURI`), fetched by the screen itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub art: Option<String>,
    /// `audio` | `video`: which player face the screen shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autoplay: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mute: Option<bool>,
}

impl Command {
    pub fn new(cmd: &str) -> Self {
        Command { cmd: cmd.into(), load_id: None, url: None, title: None, artist: None, album: None, art: None, kind: None, autoplay: None, start_at: None, position: None, volume: None, mute: None }
    }
}

/// What the renderer needs from the screen side.
pub trait Screen: Send + Sync {
    fn is_online(&self) -> bool;
    fn send(&self, cmd: Command);
}

/// LastChange variables for one service. Values are JSON so GENA can format
/// numbers (durations, positions) consistently.
pub type Vars = Vec<(&'static str, Value)>;

#[derive(Debug, Clone)]
pub enum Event {
    Avt(Vars),
    Rcs(Vars),
    Ended,
}

/// What the TV app POSTs to /tv/state.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TvReport {
    pub client: Option<String>,
    pub role: Option<String>,
    pub name: Option<String>,
    pub load_id: Option<u64>,
    pub state: Option<String>,
    pub position: Option<f64>,
    pub duration: Option<f64>,
    pub volume: Option<f64>,
    pub mute: Option<bool>,
    pub error: Option<Value>,
}

pub fn title_from_metadata(meta: &str) -> String {
    track_meta(meta).title
}

/// What the screen needs to know about the pushed item, from its DIDL-Lite.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackMeta {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art: String,
    /// True for `object.item.audioItem*`, or an `audio/*` res when no class is given.
    pub audio: bool,
}

pub fn track_meta(meta: &str) -> TrackMeta {
    if meta.is_empty() {
        return TrackMeta::default();
    }
    let field = |names: &[&str]| {
        names
            .iter()
            .filter_map(|n| tag_value(meta, n))
            .map(|v| xml_unescape(&v).trim().to_string())
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    };
    let class = field(&["class"]);
    let audio = if class.is_empty() { meta.contains("http-get:*:audio/") } else { class.contains("audioItem") };
    TrackMeta { title: field(&["title"]), artist: field(&["artist", "creator"]), album: field(&["album"]), art: field(&["albumArtURI"]), audio }
}

pub struct Renderer {
    screen: Arc<dyn Screen>,
    to_playable: Box<dyn Fn(&str) -> String + Send + Sync>,
    now: Clock,
    events: mpsc::UnboundedSender<Event>,
    verbose: bool,
    pub state: TransportState,
    pub status: &'static str,
    pub uri: String,
    pub metadata: String,
    pub title: String,
    pub track: TrackMeta,
    pub playable_uri: String,
    pub next_uri: String,
    pub next_metadata: String,
    pub duration: f64,
    pub volume: u8,
    pub mute: bool,
    pub load_id: u64,
    pos: f64,
    pos_at: f64,
}

impl Renderer {
    pub fn new(
        screen: Arc<dyn Screen>,
        to_playable: Box<dyn Fn(&str) -> String + Send + Sync>,
        now: Clock,
        events: mpsc::UnboundedSender<Event>,
        verbose: bool,
    ) -> Self {
        let t = now();
        Renderer {
            screen,
            to_playable,
            now,
            events,
            verbose,
            state: TransportState::NoMedia,
            status: "OK",
            uri: String::new(),
            metadata: String::new(),
            title: String::new(),
            track: TrackMeta::default(),
            playable_uri: String::new(),
            next_uri: String::new(),
            next_metadata: String::new(),
            duration: 0.0,
            volume: 50,
            mute: false,
            load_id: 0,
            pos: 0.0,
            pos_at: t,
        }
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    // ---- derived -----------------------------------------------------------
    pub fn position(&self) -> f64 {
        if self.state == TransportState::Playing {
            let p = self.pos + ((self.now)() - self.pos_at) / 1000.0;
            if self.duration > 0.0 { p.min(self.duration) } else { p }
        } else {
            self.pos
        }
    }

    pub fn kind(&self) -> &'static str {
        if self.track.audio { "audio" } else { "video" }
    }

    pub fn current_actions(&self) -> &'static str {
        match self.state {
            TransportState::Playing => "Pause,Stop,Seek,Next",
            TransportState::Paused => "Play,Stop,Seek,Next",
            TransportState::Stopped => if self.uri.is_empty() { "" } else { "Play,Seek,Next" },
            TransportState::Transitioning => "Stop",
            TransportState::NoMedia => "",
        }
    }

    pub fn snapshot(&self) -> Value {
        json!({
            "state": self.state.as_str(), "status": self.status, "uri": self.uri, "title": self.title,
            "artist": self.track.artist, "album": self.track.album, "art": self.track.art, "kind": self.kind(),
            "position": self.position(), "duration": self.duration, "volume": self.volume, "mute": self.mute,
            "loadId": self.load_id, "nextUri": self.next_uri,
        })
    }

    fn set_state(&mut self, state: TransportState) {
        if self.state == state {
            return;
        }
        // Freeze the extrapolated position when leaving PLAYING.
        if self.state == TransportState::Playing {
            self.pos = self.position();
            self.pos_at = (self.now)();
        }
        self.state = state;
        if self.verbose {
            crate::logln!("[avt] {} {}", state.as_str(), self.uri.chars().take(80).collect::<String>());
        }
        self.emit(Event::Avt(vec![
            ("TransportState", json!(state.as_str())),
            ("CurrentTransportActions", json!(self.current_actions())),
        ]));
    }

    fn require_tv(&self) -> Result<(), UpnpError> {
        if self.screen.is_online() { Ok(()) } else { Err(UpnpError::new(701, "TV app is not connected")) }
    }

    fn load_cmd(&self, autoplay: bool) -> Command {
        let mut c = Command::new("load");
        c.load_id = Some(self.load_id);
        c.url = Some(self.playable_uri.clone());
        c.title = Some(self.title.clone());
        let some = |s: &String| if s.is_empty() { None } else { Some(s.clone()) };
        c.artist = some(&self.track.artist);
        c.album = some(&self.track.album);
        c.art = some(&self.track.art);
        c.kind = Some(self.kind().into());
        c.autoplay = Some(autoplay);
        c
    }

    fn cmd_with_load(&self, name: &str) -> Command {
        let mut c = Command::new(name);
        c.load_id = Some(self.load_id);
        c
    }

    // ---- control point side (SOAP) ----------------------------------------
    pub fn set_uri(&mut self, uri: &str, metadata: &str) -> Result<(), UpnpError> {
        if uri.is_empty() {
            return Err(UpnpError::new(402, "CurrentURI missing"));
        }
        self.load_id += 1;
        self.uri = uri.to_string();
        self.metadata = metadata.to_string();
        self.track = track_meta(metadata);
        self.title = self.track.title.clone();
        self.playable_uri = (self.to_playable)(uri);
        self.duration = 0.0;
        self.pos = 0.0;
        self.pos_at = (self.now)();
        self.status = "OK";
        self.emit(Event::Avt(vec![
            ("AVTransportURI", json!(uri)),
            ("AVTransportURIMetaData", json!(self.metadata)),
            ("CurrentTrackURI", json!(uri)),
            ("CurrentTrackMetaData", json!(self.metadata)),
            ("CurrentTrackDuration", json!("0:00:00")),
            ("CurrentMediaDuration", json!("0:00:00")),
            ("CurrentTrack", json!(1)),
            ("NumberOfTracks", json!(1)),
        ]));
        // Many control points (Douyin included) call SetAVTransportURI while a
        // previous item is still playing and expect the new one to start
        // immediately, before/without an explicit Play. Load now if the TV is
        // there; otherwise just arm STOPPED and let Play do it.
        if self.screen.is_online() {
            self.set_state(TransportState::Transitioning);
            self.screen.send(self.load_cmd(true));
        } else {
            self.set_state(TransportState::Stopped);
        }
        Ok(())
    }

    pub fn set_next_uri(&mut self, uri: &str, metadata: &str) {
        self.next_uri = uri.to_string();
        self.next_metadata = metadata.to_string();
        self.emit(Event::Avt(vec![
            ("NextAVTransportURI", json!(self.next_uri)),
            ("NextAVTransportURIMetaData", json!(self.next_metadata)),
        ]));
    }

    pub fn play(&mut self) -> Result<(), UpnpError> {
        if self.uri.is_empty() {
            return Err(UpnpError::new(701, "no media"));
        }
        self.require_tv()?;
        match self.state {
            TransportState::Playing => Ok(()),
            // A load with autoplay is already in flight; Play right after
            // SetURI (Douyin does this) must not queue a second command.
            TransportState::Transitioning => Ok(()),
            TransportState::Stopped | TransportState::NoMedia => {
                self.set_state(TransportState::Transitioning);
                self.screen.send(self.load_cmd(true));
                Ok(())
            }
            TransportState::Paused => {
                self.screen.send(self.cmd_with_load("play"));
                // Optimistic: report PLAYING right away so the phone's UI
                // flips; the TV report will correct it if it could not start.
                self.pos_at = (self.now)();
                self.set_state(TransportState::Playing);
                Ok(())
            }
        }
    }

    pub fn pause(&mut self) -> Result<(), UpnpError> {
        match self.state {
            TransportState::Playing | TransportState::Transitioning => {}
            TransportState::Paused => return Ok(()),
            _ => return Err(UpnpError::new(701, "not playing")),
        }
        self.require_tv()?;
        self.screen.send(self.cmd_with_load("pause"));
        self.set_state(TransportState::Paused);
        Ok(())
    }

    pub fn stop(&mut self) {
        if self.state == TransportState::NoMedia {
            return;
        }
        if self.screen.is_online() {
            self.screen.send(self.cmd_with_load("stop"));
        }
        self.pos = 0.0;
        self.pos_at = (self.now)();
        self.set_state(TransportState::Stopped);
    }

    pub fn seek(&mut self, mut seconds: f64) -> Result<(), UpnpError> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(UpnpError::new(711, "illegal seek target"));
        }
        if self.state == TransportState::NoMedia {
            return Err(UpnpError::new(701, "no media"));
        }
        self.require_tv()?;
        if self.duration > 0.0 {
            seconds = seconds.min(self.duration);
        }
        self.pos = seconds;
        self.pos_at = (self.now)();
        let mut c = self.cmd_with_load("seek");
        c.position = Some(seconds);
        self.screen.send(c);
        self.emit(Event::Avt(vec![("RelativeTimePosition", json!(seconds))]));
        Ok(())
    }

    pub fn set_volume(&mut self, v: &str) -> Result<(), UpnpError> {
        let n: f64 = v.trim().parse().map_err(|_| UpnpError::new(402, "bad volume"))?;
        if !n.is_finite() {
            return Err(UpnpError::new(402, "bad volume"));
        }
        self.volume = n.round().clamp(0.0, 100.0) as u8;
        if self.screen.is_online() {
            let mut c = Command::new("volume");
            c.volume = Some(self.volume);
            self.screen.send(c);
        }
        self.emit(Event::Rcs(vec![("Volume", json!(self.volume))]));
        Ok(())
    }

    pub fn set_mute(&mut self, m: bool) {
        self.mute = m;
        if self.screen.is_online() {
            let mut c = Command::new("mute");
            c.mute = Some(m);
            self.screen.send(c);
        }
        self.emit(Event::Rcs(vec![("Mute", json!(if m { 1 } else { 0 }))]));
    }

    // ---- TV side -----------------------------------------------------------
    pub fn on_tv_report(&mut self, r: &TvReport) {
        if let Some(v) = r.volume {
            let v = v.round().clamp(0.0, 100.0) as u8;
            if v != self.volume {
                self.volume = v;
                self.emit(Event::Rcs(vec![("Volume", json!(v))]));
            }
        }
        if let Some(m) = r.mute {
            if m != self.mute {
                self.mute = m;
                self.emit(Event::Rcs(vec![("Mute", json!(if m { 1 } else { 0 }))]));
            }
        }
        // Reports about an older load are stale: ignore their transport info.
        if let Some(id) = r.load_id {
            if id != self.load_id {
                return;
            }
        }
        if let Some(d) = r.duration {
            if d.is_finite() && d > 0.0 && (d - self.duration).abs() > 0.5 {
                self.duration = d;
                self.emit(Event::Avt(vec![("CurrentTrackDuration", json!(d)), ("CurrentMediaDuration", json!(d))]));
            }
        }
        if let Some(p) = r.position {
            if p.is_finite() {
                self.pos = p;
                self.pos_at = (self.now)();
            }
        }
        match r.state.as_deref() {
            Some("loading") => {
                if !self.uri.is_empty() {
                    self.set_state(TransportState::Transitioning);
                }
            }
            Some("playing") => {
                self.status = "OK";
                self.set_state(TransportState::Playing);
            }
            // A pause arriving in STOPPED/NO_MEDIA (e.g. the element's trailing
            // pause after error/ended) is not a user pause.
            Some("paused") => {
                if matches!(self.state, TransportState::Playing | TransportState::Transitioning) {
                    self.set_state(TransportState::Paused);
                }
            }
            // Natural end: position = duration, STOPPED. This LastChange is
            // what tells the phone the clip is over.
            Some("ended") => {
                if self.duration > 0.0 {
                    self.pos = self.duration;
                }
                self.pos_at = (self.now)();
                self.set_state(TransportState::Stopped);
                self.emit(Event::Ended);
            }
            Some("stopped") => self.set_state(TransportState::Stopped),
            Some("error") => {
                self.status = "ERROR_OCCURRED";
                if self.verbose {
                    crate::logln!("[avt] tv error: {:?}", r.error);
                }
                self.emit(Event::Avt(vec![("TransportStatus", json!("ERROR_OCCURRED"))]));
                self.set_state(TransportState::Stopped);
            }
            _ => {}
        }
    }

    /// The active screen changed (TV appeared / browser took over). If we had
    /// something going, push it to the new screen and resume near the old
    /// position; the new screen's reports then take over.
    pub fn on_screen_changed(&mut self) {
        if self.uri.is_empty() || !self.screen.is_online() || self.state == TransportState::NoMedia {
            return;
        }
        let resume_at = self.position();
        let should_play = matches!(self.state, TransportState::Playing | TransportState::Transitioning);
        let paused = self.state == TransportState::Paused;
        if !should_play && !paused {
            return; // STOPPED: nothing to hand over
        }
        let mut c = self.load_cmd(should_play);
        c.start_at = Some(resume_at);
        self.screen.send(c);
        if should_play {
            self.set_state(TransportState::Transitioning);
        }
    }

    /// Active screen went away (poll timeout): whatever it was doing is no longer true.
    pub fn on_tv_offline(&mut self) {
        if matches!(self.state, TransportState::Playing | TransportState::Transitioning | TransportState::Paused) {
            self.set_state(TransportState::Stopped);
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    pub struct FakeScreen {
        pub online: AtomicBool,
        pub sent: Mutex<Vec<Command>>,
    }
    impl Screen for FakeScreen {
        fn is_online(&self) -> bool {
            self.online.load(Ordering::SeqCst)
        }
        fn send(&self, cmd: Command) {
            self.sent.lock().unwrap().push(cmd);
        }
    }

    pub struct H {
        pub r: Renderer,
        pub tv: Arc<FakeScreen>,
        pub rx: mpsc::UnboundedReceiver<Event>,
        pub t: Arc<Mutex<f64>>,
    }
    impl H {
        pub fn tick(&self, ms: f64) {
            *self.t.lock().unwrap() += ms;
        }
        pub fn last(&self) -> Command {
            self.tv.sent.lock().unwrap().last().cloned().unwrap()
        }
        pub fn drain(&mut self) -> Vec<Event> {
            let mut v = vec![];
            while let Ok(e) = self.rx.try_recv() {
                v.push(e);
            }
            v
        }
        pub fn avt_has(&mut self, name: &str, val: &Value) -> bool {
            self.drain().iter().any(|e| matches!(e, Event::Avt(v) if v.iter().any(|(k, x)| *k == name && x == val)))
        }
    }

    pub fn make(online: bool) -> H {
        let t = Arc::new(Mutex::new(1000.0));
        let tc = t.clone();
        let tv = Arc::new(FakeScreen { online: AtomicBool::new(online), sent: Mutex::new(vec![]) });
        let (tx, rx) = mpsc::unbounded_channel();
        let r = Renderer::new(
            tv.clone(),
            Box::new(|u| format!("http://proxy/{}", u.replace("://", "%3A%2F%2F"))),
            Arc::new(move || *tc.lock().unwrap()),
            tx,
            false,
        );
        H { r, tv, rx, t }
    }

    const DIDL: &str = "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><item id=\"1\"><dc:title>猫猫视频</dc:title></item></DIDL-Lite>";

    /// What BubbleUPnP pushes for a track browsed from the LX DLNA server.
    pub const MUSIC_DIDL: &str = "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><item id=\"t/kw/61988/320k\" parentID=\"0\" restricted=\"1\"><upnp:class>object.item.audioItem.musicTrack</upnp:class><dc:title>一个男人的眼泪</dc:title><dc:creator>任贤齐</dc:creator><upnp:artist>任贤齐</upnp:artist><upnp:albumArtURI>http://lx:3940/art?source=kw&amp;songmid=61988</upnp:albumArtURI><upnp:album>认真 精选辑</upnp:album><res protocolInfo=\"http-get:*:audio/mpeg:DLNA.ORG_OP=01\" duration=\"0:03:51.000\">http://lx:3939/api/stream/track?source=kw&amp;songmid=61988&amp;quality=320k</res></item></DIDL-Lite>";

    #[test]
    fn track_meta_from_didl() {
        let m = track_meta(MUSIC_DIDL);
        assert_eq!(m.title, "一个男人的眼泪");
        assert_eq!(m.artist, "任贤齐");
        assert_eq!(m.album, "认真 精选辑", "album must not be confused with albumArtURI");
        assert_eq!(m.art, "http://lx:3940/art?source=kw&songmid=61988");
        assert!(m.audio);
        // video item / no metadata / class-less audio res
        assert!(!track_meta(DIDL).audio);
        assert_eq!(track_meta(""), TrackMeta::default());
        assert!(track_meta("<DIDL-Lite><item><dc:title>x</dc:title><res protocolInfo=\"http-get:*:audio/mpeg:*\">u</res></item></DIDL-Lite>").audio);
        assert!(!track_meta("<DIDL-Lite><item><upnp:class>object.item.videoItem</upnp:class><res protocolInfo=\"http-get:*:audio/mpeg:*\">u</res></item></DIDL-Lite>").audio, "class wins over res");
        // creator is the fallback when there is no artist
        assert_eq!(track_meta("<x><dc:creator>甲</dc:creator></x>").artist, "甲");
    }

    #[test]
    fn load_command_carries_track_info() {
        let mut h = make(true);
        h.r.set_uri("http://lx:3939/api/stream/track?source=kw&songmid=61988&quality=320k", MUSIC_DIDL).unwrap();
        let c = h.last();
        assert_eq!(c.kind.as_deref(), Some("audio"));
        assert_eq!(c.artist.as_deref(), Some("任贤齐"));
        assert_eq!(c.album.as_deref(), Some("认真 精选辑"));
        assert_eq!(c.art.as_deref(), Some("http://lx:3940/art?source=kw&songmid=61988"));
        assert_eq!(h.r.snapshot()["kind"], "audio");
        // the next item has no metadata: nothing may leak over from the previous one
        h.r.set_uri("http://cdn/a.mp4", "").unwrap();
        let c = h.last();
        assert_eq!(c.kind.as_deref(), Some("video"));
        assert_eq!((c.artist, c.album, c.art), (None, None, None));
    }

    #[test]
    fn initial_state() {
        let h = make(true);
        assert_eq!(h.r.state, TransportState::NoMedia);
        assert_eq!(h.r.current_actions(), "");
    }

    #[test]
    fn set_uri_online_loads_immediately() {
        let mut h = make(true);
        h.r.set_uri("https://cdn/a.mp4", DIDL).unwrap();
        assert_eq!(h.r.state, TransportState::Transitioning);
        assert_eq!(h.r.title, "猫猫视频");
        let sent = h.tv.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].cmd, "load");
        assert_eq!(sent[0].url.as_deref(), Some("http://proxy/https%3A%2F%2Fcdn/a.mp4"));
        assert_eq!(sent[0].load_id, Some(1));
        assert!(h.avt_has("AVTransportURI", &json!("https://cdn/a.mp4")));
    }

    #[test]
    fn set_uri_offline_arms_stopped_play_701() {
        let mut h = make(false);
        h.r.set_uri("http://x/a.mp4", "").unwrap();
        assert_eq!(h.r.state, TransportState::Stopped);
        assert!(h.tv.sent.lock().unwrap().is_empty());
        assert_eq!(h.r.play().unwrap_err().code, 701);
    }

    #[test]
    fn tv_reports_drive_states() {
        let mut h = make(true);
        h.r.set_uri("http://x/a.mp4", "").unwrap();
        h.r.on_tv_report(&TvReport { load_id: Some(1), state: Some("playing".into()), position: Some(0.0), duration: Some(30.0), ..Default::default() });
        assert_eq!(h.r.state, TransportState::Playing);
        assert_eq!(h.r.duration, 30.0);
        h.tick(5000.0);
        assert!((h.r.position() - 5.0).abs() < 0.01);
        h.r.on_tv_report(&TvReport { load_id: Some(1), state: Some("paused".into()), position: Some(5.2), ..Default::default() });
        assert_eq!(h.r.state, TransportState::Paused);
        h.tick(5000.0);
        assert!((h.r.position() - 5.2).abs() < 0.01, "paused does not advance");
        h.r.on_tv_report(&TvReport { load_id: Some(0), state: Some("playing".into()), position: Some(20.0), ..Default::default() });
        assert_eq!(h.r.state, TransportState::Paused, "stale report ignored");
        h.drain();
        h.r.on_tv_report(&TvReport { load_id: Some(1), state: Some("ended".into()), position: Some(30.0), ..Default::default() });
        assert_eq!(h.r.state, TransportState::Stopped);
        assert_eq!(h.r.position(), 30.0);
        assert!(h.avt_has("TransportState", &json!("STOPPED")));
        assert_eq!(h.tv.sent.lock().unwrap().len(), 1, "ended sends nothing to TV");
    }

    #[test]
    fn play_pause_stop_seek_round_trip() {
        let mut h = make(true);
        h.r.set_uri("http://x/a.mp4", "").unwrap();
        h.r.on_tv_report(&TvReport { load_id: Some(1), state: Some("ended".into()), position: Some(10.0), duration: Some(10.0), ..Default::default() });
        h.r.play().unwrap();
        assert_eq!(h.last().cmd, "load");
        assert_eq!(h.r.state, TransportState::Transitioning);
        h.r.on_tv_report(&TvReport { load_id: Some(1), state: Some("playing".into()), position: Some(0.0), duration: Some(10.0), ..Default::default() });
        h.r.pause().unwrap();
        assert_eq!(h.last().cmd, "pause");
        assert_eq!(h.r.state, TransportState::Paused);
        h.r.play().unwrap();
        assert_eq!(h.last().cmd, "play");
        assert_eq!(h.r.state, TransportState::Playing);
        h.r.seek(7.0).unwrap();
        assert_eq!(h.last().cmd, "seek");
        assert_eq!(h.last().position, Some(7.0));
        assert!((h.r.position() - 7.0).abs() < 0.01);
        h.r.stop();
        assert_eq!(h.last().cmd, "stop");
        assert_eq!(h.r.state, TransportState::Stopped);
        assert_eq!(h.r.position(), 0.0);
    }

    #[test]
    fn play_while_transitioning_is_noop() {
        let mut h = make(true);
        h.r.set_uri("http://x/a.mp4", "").unwrap();
        h.r.play().unwrap();
        assert_eq!(h.tv.sent.lock().unwrap().len(), 1, "no second load queued");
    }

    #[test]
    fn second_set_uri_supersedes() {
        let mut h = make(true);
        h.r.set_uri("http://x/a.mp4", "").unwrap();
        h.r.on_tv_report(&TvReport { load_id: Some(1), state: Some("playing".into()), position: Some(3.0), duration: Some(10.0), ..Default::default() });
        h.r.set_uri("http://x/b.mp4", "").unwrap();
        assert_eq!(h.r.load_id, 2);
        assert_eq!(h.r.state, TransportState::Transitioning);
        assert_eq!(h.r.position(), 0.0);
        assert_eq!(h.last().load_id, Some(2));
    }

    #[test]
    fn volume_mute_both_ways() {
        let mut h = make(true);
        h.r.set_volume("30").unwrap();
        assert_eq!(h.r.volume, 30);
        assert_eq!(h.last().cmd, "volume");
        h.r.set_mute(true);
        assert_eq!(h.last().cmd, "mute");
        h.drain();
        h.r.on_tv_report(&TvReport { volume: Some(55.0), mute: Some(false), ..Default::default() });
        assert_eq!(h.r.volume, 55);
        assert!(!h.r.mute);
        assert!(h.drain().iter().any(|e| matches!(e, Event::Rcs(v) if v.iter().any(|(k, x)| *k == "Volume" && *x == json!(55)))));
    }

    #[test]
    fn offline_while_playing_stops() {
        let mut h = make(true);
        h.r.set_uri("http://x/a.mp4", "").unwrap();
        h.r.on_tv_report(&TvReport { load_id: Some(1), state: Some("playing".into()), position: Some(0.0), duration: Some(30.0), ..Default::default() });
        h.r.on_tv_offline();
        assert_eq!(h.r.state, TransportState::Stopped);
    }

    #[test]
    fn trailing_pause_after_ended_is_not_user_pause() {
        let mut h = make(true);
        h.r.set_uri("http://x/a.mp4", "").unwrap();
        h.r.on_tv_report(&TvReport { load_id: Some(1), state: Some("ended".into()), ..Default::default() });
        h.r.on_tv_report(&TvReport { load_id: Some(1), state: Some("paused".into()), ..Default::default() });
        assert_eq!(h.r.state, TransportState::Stopped);
    }

    #[test]
    fn screen_handover_carries_position() {
        let mut h = make(true);
        h.r.set_uri("http://x/a.mp4", "").unwrap();
        h.r.on_tv_report(&TvReport { load_id: Some(1), state: Some("playing".into()), position: Some(7.0), duration: Some(60.0), ..Default::default() });
        h.tick(500.0);
        h.r.on_screen_changed();
        let c = h.last();
        assert_eq!(c.cmd, "load");
        assert!(c.start_at.unwrap() >= 7.0 && c.start_at.unwrap() < 9.0);
        assert_eq!(c.autoplay, Some(true));
        assert_eq!(h.r.state, TransportState::Transitioning);
    }
}
