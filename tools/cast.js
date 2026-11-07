#!/usr/bin/env node
// Fake phone: DLNA control point. Discovers MediaRenderers via SSDP, pushes a
// URL with SetAVTransportURI + Play, subscribes to AVTransport events, and
// prints GetPositionInfo every second. Zero dependencies.
//
//   node tools/cast.js                         # list renderers
//   node tools/cast.js <url> [name-substring]  # cast to first renderer matching name
//   node tools/cast.js --location http://host:8790/dlna/device.xml <url>   # skip SSDP
'use strict'

const dgram = require('node:dgram')
const http = require('node:http')
const os = require('node:os')
const { URL } = require('node:url')

const args = process.argv.slice(2)
let location = null
if (args[0] === '--location') { location = args[1]; args.splice(0, 2) }
const mediaUrl = args[0]
const nameFilter = args[1] || ''

function discover(ms = 2500) {
  return new Promise(resolve => {
    const found = new Map()
    const sock = dgram.createSocket({ type: 'udp4', reuseAddr: true })
    sock.on('message', msg => {
      const t = msg.toString()
      const loc = (t.match(/^LOCATION:\s*(.+?)\s*$/im) || [])[1]
      const st = (t.match(/^ST:\s*(.+?)\s*$/im) || [])[1]
      if (loc && /MediaRenderer/.test(st || '')) found.set(loc, true)
    })
    sock.bind(0, () => {
      const q = 'M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: "ssdp:discover"\r\nMX: 2\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n'
      sock.send(q, 1900, '239.255.255.250')
      setTimeout(() => sock.send(q, 1900, '239.255.255.250'), 500)
    })
    setTimeout(() => { sock.close(); resolve([...found.keys()]) }, ms)
  })
}

function get(url) {
  return new Promise((resolve, reject) => {
    http.get(url, res => { let b = ''; res.on('data', c => b += c); res.on('end', () => resolve(b)) }).on('error', reject)
  })
}

function tag(xml, name) { const m = xml.match(new RegExp(`<(?:\\w+:)?${name}[^>]*>([\\s\\S]*?)</(?:\\w+:)?${name}>`, 'i')); return m ? m[1] : '' }
function esc(s) { return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;') }

async function describe(loc) {
  const xml = await get(loc)
  const name = tag(xml, 'friendlyName')
  const svcBlock = xml.split('<service>').find(s => /AVTransport:1/.test(s)) || ''
  const control = new URL(tag(svcBlock, 'controlURL'), loc).toString()
  const event = new URL(tag(svcBlock, 'eventSubURL'), loc).toString()
  return { loc, name, control, event }
}

function soap(control, action, argsXml = '') {
  const body = `<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:${action} xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID>${argsXml}</u:${action}></s:Body></s:Envelope>`
  const u = new URL(control)
  return new Promise((resolve, reject) => {
    const req = http.request({ method: 'POST', hostname: u.hostname, port: u.port, path: u.pathname, headers: { 'content-type': 'text/xml; charset="utf-8"', 'content-length': Buffer.byteLength(body), soapaction: `"urn:schemas-upnp-org:service:AVTransport:1#${action}"` } }, res => {
      let b = ''; res.on('data', c => b += c); res.on('end', () => resolve({ status: res.statusCode, body: b }))
    })
    req.on('error', reject); req.end(body)
  })
}

function localIpFor(host) {
  for (const list of Object.values(os.networkInterfaces())) for (const i of list) {
    if (i.family === 'IPv4' && !i.internal) return i.address
  }
  return '127.0.0.1'
}

async function main() {
  let devs
  if (location) devs = [await describe(location)]
  else {
    const locs = await discover()
    devs = await Promise.all(locs.map(l => describe(l).catch(() => null)))
    devs = devs.filter(Boolean)
  }
  if (!mediaUrl) {
    console.log(devs.length ? 'MediaRenderers:' : 'no MediaRenderer found')
    for (const d of devs) console.log(`  ${d.name}  ${d.loc}`)
    return
  }
  const dev = devs.find(d => d.name.includes(nameFilter)) || devs[0]
  if (!dev) { console.error('no renderer'); process.exit(1) }
  console.log(`casting to "${dev.name}" (${dev.control})`)

  // GENA subscriber
  const cb = http.createServer((req, res) => {
    let b = ''; req.on('data', c => b += c); req.on('end', () => {
      const lc = tag(b, 'LastChange').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&amp;/g, '&')
      const ts = (lc.match(/TransportState val="([^"]+)"/) || [])[1]
      console.log(`  [event seq=${req.headers.seq}] ${ts ? 'TransportState=' + ts : lc.slice(0, 120)}`)
      res.writeHead(200); res.end()
    })
  })
  await new Promise(r => cb.listen(0, r))
  const ev = new URL(dev.event)
  await new Promise((resolve) => {
    const req = http.request({ method: 'SUBSCRIBE', hostname: ev.hostname, port: ev.port, path: ev.pathname, headers: { NT: 'upnp:event', CALLBACK: `<http://${localIpFor(ev.hostname)}:${cb.address().port}/>`, TIMEOUT: 'Second-300' } }, res => { console.log(`  subscribed: ${res.statusCode} SID=${res.headers.sid}`); res.resume(); resolve() })
    req.on('error', e => { console.log('  subscribe failed', e.message); resolve() }); req.end()
  })

  const didl = `<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="0" parentID="-1" restricted="1"><dc:title>cast.js 测试</dc:title><upnp:class>object.item.videoItem</upnp:class><res protocolInfo="http-get:*:video/mp4:*">${esc(mediaUrl)}</res></item></DIDL-Lite>`
  let r = await soap(dev.control, 'SetAVTransportURI', `<CurrentURI>${esc(mediaUrl)}</CurrentURI><CurrentURIMetaData>${esc(didl)}</CurrentURIMetaData>`)
  console.log(`  SetAVTransportURI -> ${r.status}${r.status !== 200 ? ' ' + tag(r.body, 'errorDescription') : ''}`)
  r = await soap(dev.control, 'Play', '<Speed>1</Speed>')
  console.log(`  Play -> ${r.status}${r.status !== 200 ? ' ' + tag(r.body, 'errorDescription') : ''}`)

  let n = 0
  const t = setInterval(async () => {
    const [ti, pi] = await Promise.all([soap(dev.control, 'GetTransportInfo'), soap(dev.control, 'GetPositionInfo')])
    console.log(`  ${tag(ti.body, 'CurrentTransportState')} ${tag(pi.body, 'RelTime')}/${tag(pi.body, 'TrackDuration')}`)
    n += 1
    if (n === 8) { console.log('  -> Pause'); await soap(dev.control, 'Pause') }
    if (n === 11) { console.log('  -> Seek 0:00:30'); await soap(dev.control, 'Seek', '<Unit>REL_TIME</Unit><Target>0:00:30</Target>'); await soap(dev.control, 'Play', '<Speed>1</Speed>') }
    if (n >= 20 || tag(ti.body, 'CurrentTransportState') === 'STOPPED' && n > 3) {
      clearInterval(t); cb.close(); console.log('done'); process.exit(0)
    }
  }, 1000)
}

main().catch(e => { console.error(e); process.exit(1) })
