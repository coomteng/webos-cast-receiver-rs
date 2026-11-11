/* keepawake.js — keep the TV's screen saver away while something is playing.
 * ES5 only (Chrome 53).
 *
 * webOS starts its screen saver after a few minutes without remote input.
 * Music has no video picture, so the system does not hold it off by itself.
 * The documented way for an app: subscribe to
 *   luna://com.webos.service.tvpower/power/registerScreenSaverRequest
 * The TV then asks (a message carrying `timestamp`) each time it is about to
 * start the screen saver, and the app may refuse with
 *   .../power/responseScreenSaverRequest  {clientName, ack:false, timestamp}
 *
 * We refuse only while playing, and for a short while after (the gap between
 * two tracks). Paused, stopped or idle: the screen saver is allowed, so a
 * still picture is never pinned on the panel.
 * Browser screens have no PalmServiceBridge: this file then does nothing. */
var KeepAwake = (function () {
  'use strict';

  var SERVICE = 'luna://com.webos.service.tvpower/power/';
  /* A fresh name per page load. A fixed one is refused with "The client is
     already registered" when the app is relaunched while the TV still holds
     the previous page's registration (seen on OLED55B8, webOS 4.4) -- and an
     unregistered app is simply never asked. */
  var NAME = 'com.ting.cast-' + Math.random().toString(36).slice(2, 10);
  var RETRY_MS = 15000;
  var registered = false;
  var GRACE_MS = 30000;
  var log = [];            // last few events, for remote inspection
  var lastPlaying = 0;
  var sub = null;          // keep the bridges referenced or they are collected
  var replies = [];

  function note(what, detail) {
    log.push({ at: new Date().toISOString(), what: what, detail: detail === undefined ? '' : String(detail).slice(0, 200) });
    if (log.length > 20) log.shift();
  }

  function playing() { return Player.isActive() && !Player.isPaused(); }
  function wanted() { return playing() || (Date.now() - lastPlaying) < GRACE_MS; }

  function refuse(timestamp) {
    var b = new PalmServiceBridge();
    replies.push(b);
    if (replies.length > 4) replies.shift();
    b.onservicecallback = function (msg) { note('reply-result', msg); };
    b.call(SERVICE + 'responseScreenSaverRequest', JSON.stringify({ clientName: NAME, ack: false, timestamp: timestamp }));
  }

  function onMessage(msg) {
    var r;
    try { r = JSON.parse(msg); } catch (e) { note('bad-message', msg); return; }
    if (r.timestamp === undefined) {
      registered = r.returnValue !== false;
      note(registered ? 'registered' : 'register-failed', msg);
      if (!registered) setTimeout(register, RETRY_MS);
      return;
    }
    if (wanted()) { note('refused', r.timestamp); refuse(r.timestamp); }
    else note('allowed', r.timestamp);
  }

  function register() {
    sub = new PalmServiceBridge();
    sub.onservicecallback = onMessage;
    sub.call(SERVICE + 'registerScreenSaverRequest', JSON.stringify({ subscribe: true, clientName: NAME }));
  }

  function start() {
    if (typeof PalmServiceBridge !== 'function') return false;
    setInterval(function () { if (playing()) lastPlaying = Date.now(); }, 1000);
    register();
    return true;
  }

  var active = false;
  try { active = start(); } catch (e) { note('start-failed', e && e.message); }

  return { active: active, log: log, wanted: wanted, name: NAME, registered: function () { return registered; } };
})();
