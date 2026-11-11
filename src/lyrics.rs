//! Lyrics for the audio face of the screen.
//!
//! The screen (a `file://` page on the TV) cannot call other origins, so it
//! asks us (`GET /tv/lyric`) and we ask whoever serves the track. Today that
//! is one kind of source: an LX Music web player server, recognised by its stream URL
//! (`/api/stream/track?source=&songmid=`), whose lyric endpoint lives next to
//! it (`/api/lyric`, answering `{lyric, tlyric, from}` with LRC text).
//! Anything else simply has no lyrics.
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// The lyric endpoint for a pushed stream URI, if it is one we know.
/// `title`/`artist` come from the DIDL; the source's own keyword fallback
/// needs them when the track's platform has no lyric.
pub fn lyric_url(uri: &str, title: &str, artist: &str) -> Option<String> {
    let u = url::Url::parse(uri).ok()?;
    if !matches!(u.scheme(), "http" | "https") || u.path() != "/api/stream/track" {
        return None;
    }
    let q: HashMap<String, String> = u.query_pairs().into_owned().collect();
    let get = |k: &str| q.get(k).filter(|v| !v.is_empty()).cloned();
    let (source, songmid) = (get("source")?, get("songmid")?);
    let mut out = u.clone();
    out.set_path("/api/lyric");
    out.set_query(None);
    {
        let mut qp = out.query_pairs_mut();
        qp.append_pair("source", &source).append_pair("songmid", &songmid);
        if let Some(name) = get("name").or_else(|| Some(title.to_string()).filter(|t| !t.is_empty())) {
            qp.append_pair("name", &name);
        }
        if !artist.is_empty() {
            qp.append_pair("singer", artist);
        }
        for k in ["hash", "interval"] {
            if let Some(v) = get(k) {
                qp.append_pair(k, &v);
            }
        }
    }
    Some(out.into())
}

pub struct Lyrics {
    client: reqwest::Client,
    /// The last answer, keyed by load id: the screen may ask again (retry,
    /// relaunch, a second screen taking over) for the same item.
    last: Mutex<Option<(u64, Value)>>,
    verbose: bool,
}

fn answer(load_id: u64, lyric: &str, tlyric: &str) -> Value {
    json!({ "loadId": load_id, "lyric": lyric, "tlyric": tlyric })
}

impl Lyrics {
    pub fn new(verbose: bool) -> Self {
        crate::util::install_crypto();
        // Lyric sources are plain `http://` servers on the LAN. Like the GENA
        // and proxy clients, do not load the system CA store: the release
        // image is `scratch` and has none, and building a verifying client
        // there fails at startup.
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .connect_timeout(Duration::from_secs(3))
            // A first-time lyric makes the source search its upstreams.
            .timeout(Duration::from_secs(10))
            .no_proxy()
            .build()
            .expect("reqwest client");
        Lyrics { client, last: Mutex::new(None), verbose }
    }

    /// Lyrics for the item with this load id. `url` is [`lyric_url`] of that
    /// item. No source, no lyric and a failed fetch all answer with empty
    /// text; only a completed fetch is remembered, so a failure can be retried.
    pub async fn get(&self, load_id: u64, url: Option<String>) -> Value {
        let Some(url) = url else { return answer(load_id, "", "") };
        if let Some((id, v)) = self.last.lock().unwrap().as_ref() {
            if *id == load_id {
                return v.clone();
            }
        }
        match self.fetch(&url).await {
            Ok((lyric, tlyric)) => {
                if self.verbose {
                    crate::logln!("[lyric] load {} -> {} bytes", load_id, lyric.len());
                }
                let v = answer(load_id, &lyric, &tlyric);
                *self.last.lock().unwrap() = Some((load_id, v.clone()));
                v
            }
            Err(e) => {
                crate::logln!("[lyric] load {} failed: {}", load_id, e);
                let mut v = answer(load_id, "", "");
                v["error"] = json!(e);
                v
            }
        }
    }

    async fn fetch(&self, url: &str) -> Result<(String, String), String> {
        let res = self.client.get(url).send().await.map_err(|e| e.to_string())?;
        let status = res.status();
        let body = res.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("HTTP {} {}", status.as_u16(), body.chars().take(120).collect::<String>()));
        }
        let v: Value = serde_json::from_str(&body).map_err(|_| "bad json".to_string())?;
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        Ok((s("lyric"), s("tlyric")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lx_stream_url_maps_to_its_lyric_endpoint() {
        let u = lyric_url(
            "http://lx.lan:3939/api/stream/track?source=kw&songmid=61988&quality=320k&name=%E4%B8%80%E4%B8%AA&interval=03%3A51",
            "标题",
            "任贤齐 & 朋友",
        )
        .unwrap();
        let p = url::Url::parse(&u).unwrap();
        assert_eq!(format!("{}://{}:{}{}", p.scheme(), p.host_str().unwrap(), p.port().unwrap(), p.path()), "http://lx.lan:3939/api/lyric");
        let q: HashMap<String, String> = p.query_pairs().into_owned().collect();
        assert_eq!(q["source"], "kw");
        assert_eq!(q["songmid"], "61988");
        assert_eq!(q["name"], "一个", "the URL's own name wins over the DIDL title");
        assert_eq!(q["singer"], "任贤齐 & 朋友");
        assert_eq!(q["interval"], "03:51");
        assert!(!q.contains_key("quality") && !q.contains_key("hash"));
    }

    #[test]
    fn title_fills_in_a_missing_name_and_hash_is_kept() {
        let u = lyric_url("http://h:1/api/stream/track?source=kg&songmid=7&quality=128k&hash=ABC", "歌名", "").unwrap();
        let q: HashMap<String, String> = url::Url::parse(&u).unwrap().query_pairs().into_owned().collect();
        assert_eq!(q["name"], "歌名");
        assert_eq!(q["hash"], "ABC");
        assert!(!q.contains_key("singer"));
    }

    #[test]
    fn other_urls_have_no_lyric_source() {
        for u in [
            "http://cdn/a.mp4",
            "http://h:1/api/stream?u=x",
            "http://h:1/api/stream/track?source=kw",
            "http://h:1/api/stream/track?songmid=1&source=",
            "ftp://h/api/stream/track?source=kw&songmid=1",
            "not a url",
            "",
        ] {
            assert_eq!(lyric_url(u, "t", "a"), None, "{u}");
        }
    }
}
