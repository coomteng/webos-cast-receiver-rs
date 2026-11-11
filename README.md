# webos-cast-receiver-rs

A DLNA **MediaRenderer** that lives on a LAN server and relays casts (the 抖音
"投屏 TV" button, or anything speaking UPnP AVTransport) to a tiny web app on an
LG webOS TV. Server in Rust, TV app in plain ES5 (`app/`), browser simulator
in `sim/`. Successor of the Node implementation
[webos-cast-receiver](https://github.com/coomteng/webos-cast-receiver)
(archived): same protocol surface, same HTTP routes, same TV app, same
environment variables and `DATA_DIR/uuid` file, so it was a drop-in
replacement.

```
phone ──SSDP / SOAP AVTransport / GENA──▶ cast-receiver-rs (host network, :8790)
                                            ├─ /proxy/<id>   follows redirects, sets UA/Referer,
                                            │                passes Range through, rewrites HLS
                                            ├─ /tv/poll      screens long-poll for load/play/pause/seek/stop
                                            ├─ /tv/state     screens report state/position (→ GetPositionInfo, LastChange)
                                            ├─ /tv/lyric     lyrics of the current item, fetched from the track's own server
                                            └─ /tv/ /sim /   the TV app itself (app/ + sim/)
LG TV app (com.ting.cast) or any browser at / ──XHR──▶ /tv/*;  <video src=http://server:8790/proxy/…>
```

## Why a port

The Node version works and is small (1.6 k lines, zero deps). The Rust one
exists for the cases where the runtime matters: a 4.7 MB static binary, ~5 MB
RSS instead of ~44 MB, a `scratch` image of ~6 MB instead of 172 MB, and a
single file you can drop on a router or a tiny box without installing Node.
Performance is not the point: one phone, one TV, one stream — the bottleneck is
Wi-Fi and the CDN, in either language.

Measured on the dev box (x86_64, release build): binary 4.7 MB, RSS 9.6 MB
after a full cast through the proxy (idle ~5 MB).

## Run

```sh
cargo run --release                        # http://<lan-ip>:8790, advertises on SSDP
cargo test                                 # 57 tests: unit + HTTP end-to-end + proxy against a fake CDN
```

Static assets: by default the binary looks for `app/` and `sim/` in the cwd,
next to the executable, or in this repository (`cargo run` from anywhere).
Override with `APP_DIR` / `SIM_DIR`.

Environment (identical to the Node version): `FRIENDLY_NAME` (default
`客厅电视`), `PORT` (8790), `BIND_ADDR` (pin the LAN interface on multi-NIC
hosts; also the advertised IP), `ADVERTISE_HOST`, `PROXY_MODE` = `always` |
`https-only` | `off`, `PROXY_UA`, `DATA_DIR` (persists the device UUID),
`TV_TIMEOUT_MS` (12000), `UUID`, `MANUFACTURER`, `MODEL_NAME`, `VERBOSE=1`
(per-request proxy timing lines: TTFB, bytes, rate, prefetch hits).

Docker: `compose.portainer.yml` pulls `ghcr.io/coomteng/webos-cast-receiver-rs`
(built by GitHub Actions on every push/tag; `TAG` env selects a version,
default `latest`) — **must** use `network_mode: host` (multicast). `Dockerfile`
builds the same image locally; `app/` and `sim/` are copied in from this repo.

## TV app

A prebuilt ipk for the author's LAN (server at 192.168.0.9:8790) is attached to
each [GitHub release](https://github.com/coomteng/webos-cast-receiver-rs/releases);
for any other network build your own as below.

`app/` is plain ES5 + flexbox (webOS 4.4 = Chrome 53; one ES2015 token kills the
script). `tools/es5check.sh` gates it with acorn. Keys: OK/Play/Pause toggle,
←/→ ±10 s, ⏪/⏩ ±30 s, Stop/Back stop, Back again exits. Magic Remote: click
toggles, moving shows the OSD, clicking the bar seeks.

### Music

An item pushed as `object.item.audioItem*` (or with an `audio/*` res) gets the
audio face instead of a black picture: a spinning record with the cover
(`upnp:albumArtURI`) as its label, title / artist / progress, and lyrics that
follow playback. ↑ / ↓ or the yellow key (or the 歌词 button with the pointer)
switch the lyrics off and on; they are on by default and the choice is kept for
every following track until it is switched again.

Lyrics come from the server the track is streamed from. One kind of source is
known today, an LX Music web player server: a stream URL of the form
`/api/stream/track?source=&songmid=` is mapped to `/api/lyric` on the same
host, which answers `{lyric, tlyric}` as LRC text. Any other audio plays with
the same face and no lyrics.

```sh
echo SERVER_BASE=http://192.168.0.9:8790 > local.env
tools/build_ipk.sh                         # substitutes the server address, packages dist/*.ipk
ares-install --device <tv> dist/com.ting.cast_*.ipk
ares-launch  --device <tv> com.ting.cast
```

The TV app only talks to the server over HTTP (`/tv/poll`, `/tv/state`), so a
server-only release does not require reinstalling the ipk; rebuild it only when
`app/` changes. Developer Mode setup for old LG TVs: see the notes in
[coomteng/moontv-webos](https://github.com/coomteng/moontv-webos).

## Testing without a TV or phone

- `http://<server>:8790/sim/` — scaled TV + remote + a "fake phone" that pushes a URL via SOAP.
- `node tools/cast.js` lists MediaRenderers on the LAN; `node tools/cast.js <url> [name]`
  casts to one, subscribes to events and prints position every second
  (zero-dependency Node script, dev tool only; not part of the image).

## What is implemented

Device description with `DMR-1.50`; AVTransport (SetAVTransportURI,
SetNextAVTransportURI, Play, Pause, Stop, Seek, Next, Previous,
Get{Media,Transport,Position}Info, GetDeviceCapabilities, GetTransportSettings,
GetCurrentTransportActions, SetPlayMode); RenderingControl (Get/SetVolume,
Get/SetMute, presets); ConnectionManager (GetProtocolInfo, connection IDs);
GENA SUBSCRIBE/renew/UNSUBSCRIBE with 200 ms-moderated `LastChange` NOTIFY and
dead-subscriber eviction after 3 failed deliveries; SSDP alive/byebye/M-SEARCH
with MX spreading; media proxy (≤5 redirects, UA/Referer, Range and If-Range
both ways, hop-by-hop header filtering, HLS segment and `URI=` rewriting,
HEAD); **prefetch**: on SetAVTransportURI the server opens the upstream
connection immediately and registers it as soon as upstream headers arrive (the
body keeps streaming into a bounded buffer), so the screen's first
`bytes=0-` request (which arrives ~1.3 s later on webOS) is answered from
memory and continues on the same upstream stream — removes the CDN TTFB from
the start-up path; multi-screen channel (a live TV always wins over browser tabs, mid-clip
handover with `startAt`, 12 s offline → UPnP 701).

Behavioural details carried over from the Node version's real-TV debugging:
Play right after SetAVTransportURI while TRANSITIONING does not queue a second
load; a `paused` report arriving after `ended`/`error` is not a user pause;
reports with a stale `loadId` are ignored; an upstream reset mid-body ends the
stream without touching already-sent headers or the process.

## Layout

```
src/
  main.rs        bind, SSDP task, graceful shutdown (SIGINT/SIGTERM → byebye)
  server.rs      axum routes; wires renderer ↔ tv channel ↔ gena ↔ proxy
  renderer.rs    AVTransport state machine (no I/O)
  actions.rs     SOAP action dispatch (AVT 15 / RCS 6 / CMS 3)
  tvchannel.rs   screens, long-poll waiters, active-screen election
  gena.rs        subscriptions, LastChange moderation, NOTIFY delivery
  proxy.rs       media relay
  ssdp.rs        UDP 1900 advertiser / responder
  device_xml.rs  device description + 3 SCPDs
  soap.rs xml.rs config.rs util.rs
tests/
  e2e.rs         fake phone (SOAP + GENA callback server) + fake screens over real HTTP
  proxy.rs       fake CDN: redirect, Range, HEAD, 403/502, HLS rewrite, mid-body RST
app/             the webOS TV app (ES5): appinfo.json, index.html, js/{api,app,player}.js, css/
sim/             browser simulator (TV + remote + fake phone) served at /sim/
tools/
  build_ipk.sh   ares-package the TV app with SERVER_BASE from local.env substituted
  es5check.sh    acorn ecmaVersion:5 parse gate for app/js/*.js
  cast.js        fake phone / DLNA control point for the command line
```

Dependencies: tokio, axum, reqwest (rustls + ring, no OpenSSL/cmake), socket2,
serde_json, uuid, regex, rand, url, if-addrs, httpdate.

## License

MIT.
