/* app.js — command loop (long poll), state heartbeat, remote keys. ES5 only. */
var App = (function () {
  'use strict';

  var idleDot = document.getElementById('idle-dot');
  var idleName = document.getElementById('idle-name');
  var idleHint = document.getElementById('idle-hint');
  var idleMeta = document.getElementById('idle-meta');
  var footVer = document.getElementById('foot-ver');
  var toastEl = document.getElementById('toast');

  var connected = false;
  var lastSeq = 0;
  var toastTimer = null;
  var backoff = 1000;
  var deviceName = '';
  var reportTimer = null;
  var pendingReport = null;

  function log(msg) { try { console.log('[cast] ' + msg); } catch (e) { /* ignore */ } }

  var unmuteEl = document.getElementById('unmute');
  function showUnmute(on) { if (on) unmuteEl.classList.remove('hidden'); else unmuteEl.classList.add('hidden'); }

  function toast(msg, isErr) {
    toastEl.textContent = msg;
    toastEl.className = isErr ? 'err' : '';
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { toastEl.classList.add('hidden'); }, 3000);
  }

  var isActive = false;
  function setConnected(on, why) {
    connected = on;
    if (on) { idleDot.classList.add('on'); refreshHint(); }
    else { idleDot.classList.remove('on'); idleHint.textContent = why || '接收服务未连接'; }
  }
  function refreshHint() {
    if (!connected) return;
    if (isActive) idleHint.textContent = API.role === 'web' ? '等待投屏（电视未连接，由此浏览器播放）' : '等待投屏';
    else idleHint.textContent = API.role === 'web' ? '待机：电视在线，投屏会在电视上播放' : '待机：另一块屏幕正在接收';
  }

  /* ---- reports: coalesce to at most one POST in flight, keep latest ---- */
  function sendReport(r) {
    r.name = deviceName;
    pendingReport = r;
    if (reportTimer) return;
    reportTimer = setTimeout(flushReport, 50);
  }
  function flushReport() {
    reportTimer = null;
    var r = pendingReport; pendingReport = null;
    if (!r) return;
    API.state(r).then(function () {
      if (!connected) setConnected(true);
    }, function (e) { log('state POST failed: ' + e.message); });
  }
  Player.onReport(sendReport);
  // Heartbeat: once a second while playing, every 4 s when idle.
  setInterval(function () { if (Player.isActive()) Player.report(); }, 1000);
  setInterval(function () { if (!Player.isActive()) Player.report(); }, 4000);

  /* ---- command loop ---- */
  function handle(cmd) {
    if (cmd.seq && cmd.seq <= lastSeq && cmd.cmd !== 'load') return;
    if (cmd.seq) lastSeq = cmd.seq;
    switch (cmd.cmd) {
      case 'load': Player.load(cmd); break;
      case 'play': Player.play(); break;
      case 'pause': Player.pause(); break;
      case 'stop': Player.stop(); break;
      case 'seek': Player.seek(cmd.position || 0); break;
      case 'volume': Player.setVolume(cmd.volume); break;
      case 'mute': Player.setMute(cmd.mute); break;
      case 'active': isActive = true; refreshHint(); break;
      case 'standby': isActive = false; Player.standby(); refreshHint(); break;
      default: log('unknown cmd ' + cmd.cmd);
    }
  }

  function loop() {
    API.poll(lastSeq).then(function (res) {
      backoff = 1000;
      if (!connected) setConnected(true);
      var cmds = (res && res.commands) || [];
      for (var i = 0; i < cmds.length; i++) handle(cmds[i]);
      if (res && typeof res.active === 'boolean' && res.active !== isActive) { isActive = res.active; if (!isActive) Player.standby(); refreshHint(); }
      if (res && res.replaced) { log('poll replaced by another client'); }
      setTimeout(loop, 0);
    }, function (e) {
      if (connected) { setConnected(false, '接收服务断开，重连中…'); }
      log('poll failed: ' + e.message + ', retry in ' + backoff);
      setTimeout(loop, backoff);
      backoff = Math.min(backoff * 2, 15000);
    });
  }

  function fetchInfo() {
    API.info().then(function (info) {
      deviceName = info.name || '';
      idleName.textContent = info.name || '投屏接收';
      idleMeta.textContent = '接收服务 ' + info.host + ':' + info.port + ' · 服务版本 ' + info.version;
      footVer.textContent = 'App v' + API.version;
    }, function () {
      idleMeta.textContent = '接收服务 ' + API.base.replace(/^https?:\/\//, '');
      footVer.textContent = 'App v' + API.version;
      setTimeout(fetchInfo, 5000);
    });
  }

  /* ---- remote control ---- */
  document.addEventListener('keydown', function (e) {
    var code = e.keyCode || e.which;
    /* Any key is a user gesture: lift the browser autoplay mute first. */
    if (Player.needsUnmute() && (code === 13 || code === 32 || code === 415)) { Player.unmute(); e.preventDefault(); return; }
    var handled = true;
    switch (code) {
      case 13:  /* OK */
      case 415: /* Play */
      case 19:  /* Pause */
      case 32:  /* Space (sim) */
        if (Player.isActive()) Player.togglePlay(); else handled = false; break;
      case 413: /* Stop */
        if (Player.isActive()) Player.stop(); else handled = false; break;
      case 37: Player.seekBy(-10); break;
      case 39: Player.seekBy(10); break;
      case 412: Player.seekBy(-30); break;
      case 417: Player.seekBy(30); break;
      case 38: case 40: /* Up / Down */
      case 405:         /* Yellow */
        if (Player.isAudio()) toggleLyrics(); else if (code !== 405) Player.showOsd(3000); else handled = false;
        break;
      case 461: /* Back */
      case 27:
        if (Player.isActive()) Player.stop(); else window.close();
        break;
      default: handled = false;
    }
    if (handled) e.preventDefault();
  });

  /* Lyrics on/off (audio face). The choice sticks until it is changed again. */
  function toggleLyrics() { toast(AudioFace.toggleLyrics() ? '歌词已打开' : '歌词已关闭'); }
  document.getElementById('au-lyrbtn').addEventListener('click', function (e) { e.stopPropagation(); toggleLyrics(); });
  document.getElementById('au-track').addEventListener('click', function (e) {
    e.stopPropagation();
    var r = this.getBoundingClientRect();
    var v = document.getElementById('video');
    if (isFinite(v.duration)) Player.seek((e.clientX - r.left) / r.width * v.duration);
  });

  /* Magic Remote pointer: click toggles play/pause; moving the pointer shows the OSD. */
  document.addEventListener('click', function () { if (Player.unmute()) return; if (Player.isActive()) Player.togglePlay(); });
  var moveT = 0;
  document.addEventListener('mousemove', function () {
    var t = Date.now();
    if (t - moveT < 500) return;
    moveT = t;
    if (Player.isActive()) Player.showOsd(3000);
  });

  // Click on the progress track seeks.
  document.getElementById('osd-track').addEventListener('click', function (e) {
    e.stopPropagation();
    var r = this.getBoundingClientRect();
    var frac = (e.clientX - r.left) / r.width;
    var v = document.getElementById('video');
    if (isFinite(v.duration)) Player.seek(frac * v.duration);
  });

  // webOS: when the app is relaunched / regains visibility, nudge a report.
  document.addEventListener('visibilitychange', function () { if (!document.hidden) Player.report(); });

  if (API.role === 'web') {
    document.title = '投屏接收 — 浏览器屏幕';
    /* Pressing keys on a 1920x1080 page in a browser: fit it to the window. */
    var fit = function () {
      var s = Math.min(window.innerWidth / 1920, window.innerHeight / 1080);
      document.body.style.transformOrigin = '0 0';
      document.body.style.transform = 'scale(' + s + ')';
      document.documentElement.style.background = '#000';
      document.documentElement.style.overflow = 'hidden';
    };
    window.addEventListener('resize', fit); fit();
  }
  fetchInfo();
  loop();
  Player.report();

  return { toast: toast, log: log, showUnmute: showUnmute };
})();
