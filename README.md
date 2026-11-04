# webos-cast-receiver-rs

Rust port of the server side of [webos-cast-receiver](https://github.com/coomteng/webos-cast-receiver):
a DLNA **MediaRenderer** that lives on a LAN server and relays casts (the 抖音
"投屏 TV" button, or anything speaking UPnP AVTransport) to a tiny web app on an
LG webOS TV. Same protocol surface, same HTTP routes, same TV app, same
environment variables and `DATA_DIR/uuid` file as the Node version, so the two
are drop-in replacements for each other.

```
phone ──SSDP / SOAP AVTransport / GENA──▶ cast-receiver-rs (host network, :8790)
                                            ├─ /proxy/<id>   follows redirects, sets UA/Referer,
                                            │                passes Range through, rewrites HLS
                                            ├─ /tv/poll      screens long-poll for load/play/pause/seek/stop
                                            ├─ /tv/state     screens report state/position (→ GetPositionInfo, LastChange)
                                            └─ /tv/ /sim /   the TV app itself (served from the Node repo's app/ + sim/)
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
cargo test                                 # 47 tests: unit + HTTP end-to-end + proxy against a fake CDN
```

Static assets: by default the binary looks for `app/` and `sim/` next to the
executable, in the cwd, or in a sibling checkout `../webos-cast-receiver/`.
Override with `APP_DIR` / `SIM_DIR`.

Environment (identical to the Node version): `FRIENDLY_NAME` (default
`客厅电视`), `PORT` (8790), `BIND_ADDR` (pin the LAN interface on multi-NIC
hosts; also the advertised IP), `ADVERTISE_HOST`, `PROXY_MODE` = `always` |
`https-only` | `off`, `PROXY_UA`, `DATA_DIR` (persists the device UUID),
`TV_TIMEOUT_MS` (12000), `UUID`, `MANUFACTURER`, `MODEL_NAME`, `VERBOSE=1`.

Docker: `compose.portainer.yml` — **must** use `network_mode: host`
(multicast). The image build clones the Node repo to pick up `app/` and
`sim/` (pin with `--build-arg APP_REF=<tag>`).

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
HEAD); multi-screen channel (a live TV always wins over browser tabs, mid-clip
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
```

Dependencies: tokio, axum, reqwest (rustls + ring, no OpenSSL/cmake), socket2,
serde_json, uuid, regex, rand, url, if-addrs, httpdate.

## License

MIT, same as the original.
