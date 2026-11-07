/* player.js — <video> wrapper + OSD. ES5 only.
 * Player.load({loadId,url,title,autoplay}) / play / pause / stop / seek / setVolume / setMute
 * Emits reports through Player.onReport(fn): {loadId,state,position,duration,volume,mute,error} */
var Player = (function () {
  'use strict';

  var video = document.getElementById('video');
  var idle = document.getElementById('idle');
  var loading = document.getElementById('loading');
  var loadingTxt = document.getElementById('loading-txt');
  var osd = document.getElementById('osd');
  var osdTitle = document.getElementById('osd-title');
  var osdCur = document.getElementById('osd-cur');
  var osdDur = document.getElementById('osd-dur');
  var osdFill = document.getElementById('osd-fill');
  var osdState = document.getElementById('osd-state');

  var current = null;      // {loadId,url,title}
  var reportFn = null;
  var osdTimer = null;
  var lastState = 'idle';
  var terminal = false;   // set after ended/error so the trailing pause event is ignored
  var stallTimer = null;
  var retried = false;
  var needsUnmute = false;

  function fmt(sec) {
    if (!isFinite(sec) || sec < 0) sec = 0;
    sec = Math.floor(sec);
    var m = Math.floor(sec / 60), s = sec % 60;
    var h = Math.floor(m / 60); m = m % 60;
    var ss = (s < 10 ? '0' : '') + s;
    return h ? h + ':' + (m < 10 ? '0' : '') + m + ':' + ss : m + ':' + ss;
  }

  function report(state, extra) {
    if (state) lastState = state;
    var r = {
      loadId: current ? current.loadId : 0,
      state: lastState,
      position: isFinite(video.currentTime) ? video.currentTime : 0,
      duration: isFinite(video.duration) ? video.duration : 0,
      volume: Math.round(video.volume * 100),
      mute: !!video.muted
    };
    if (extra) for (var k in extra) if (extra.hasOwnProperty(k)) r[k] = extra[k];
    if (reportFn) reportFn(r);
  }

  function showIdle(on) {
    if (on) { idle.classList.remove('hidden'); osd.classList.add('hidden'); loading.classList.add('hidden'); }
    else idle.classList.add('hidden');
  }

  function showLoading(on, txt) {
    if (txt) loadingTxt.textContent = txt;
    if (on) loading.classList.remove('hidden'); else loading.classList.add('hidden');
  }

  function updateOsd() {
    var cur = video.currentTime || 0, dur = video.duration;
    osdCur.textContent = fmt(cur);
    osdDur.textContent = isFinite(dur) ? fmt(dur) : '--:--';
    osdFill.style.width = (isFinite(dur) && dur > 0 ? Math.min(100, cur / dur * 100) : 0) + '%';
    osdState.textContent = video.paused ? '已暂停' : '';
  }

  function showOsd(ms) {
    if (!current) return;
    updateOsd();
    osd.classList.remove('hidden');
    if (osdTimer) clearTimeout(osdTimer);
    if (ms !== 0) osdTimer = setTimeout(function () { if (!video.paused) osd.classList.add('hidden'); }, ms || 3000);
  }

  function clearStall() { if (stallTimer) { clearTimeout(stallTimer); stallTimer = null; } }

  /* ---- video events ---- */
  video.addEventListener('loadstart', function () { report('loading'); });
  video.addEventListener('loadedmetadata', function () { report(null); updateOsd(); });
  video.addEventListener('playing', function () { if (waitTimer) { clearTimeout(waitTimer); waitTimer = null; } clearStall(); showLoading(false); showIdle(false); report('playing'); showOsd(3000); });
  video.addEventListener('play', function () { report(video.readyState >= 3 ? 'playing' : 'loading'); });
  /* readyState < 2 = the element never had data: that pause is the tail of a failed load, not the user. */
  video.addEventListener('pause', function () { if (!video.ended && !terminal && current && video.readyState >= 2) { report('paused'); showOsd(0); } });
  /* webOS fires 'waiting' liberally (seeks, tiny stalls). Only call it buffering if no
     progress for 1.5 s; a 'playing' event in between cancels it. */
  var waitTimer = null;
  video.addEventListener('waiting', function () {
    if (!current) return;
    if (waitTimer) clearTimeout(waitTimer);
    waitTimer = setTimeout(function () { waitTimer = null; if (current && video.readyState < 3) { showLoading(true, '缓冲中…'); report('loading'); } }, 1500);
  });
  video.addEventListener('canplay', function () { if (waitTimer) { clearTimeout(waitTimer); waitTimer = null; } showLoading(false); });
  video.addEventListener('timeupdate', function () { if (!osd.classList.contains('hidden')) updateOsd(); });
  video.addEventListener('ended', function () {
    clearStall();
    terminal = true;
    report('ended');
    App.toast('播放完毕');
    // Keep the last frame for a moment, then back to idle unless something new arrives.
    var id = current ? current.loadId : 0;
    setTimeout(function () { if (current && current.loadId === id) stop(true); }, 2500);
  });
  video.addEventListener('error', function () {
    clearStall();
    var err = video.error;
    var code = err ? err.code : 0;
    var msg = ({ 1: '已中止', 2: '网络错误', 3: '解码失败', 4: '格式不支持' })[code] || ('错误 ' + code);
    if (!current) return;
    // One retry for transient network errors (CDN hiccup / proxy redirect).
    if (!retried && (code === 2 || code === 4)) {
      retried = true;
      App.log('video error ' + code + ', retrying');
      var c = current;
      setTimeout(function () { if (current === c) { video.src = c.url + (c.url.indexOf('?') < 0 ? '?' : '&') + 'r=1'; video.load(); video.play(); } }, 600);
      return;
    }
    showLoading(false);
    terminal = true;
    App.toast('无法播放：' + msg, true);
    report('error', { error: msg + ' (' + code + ')' });
    var failed = current;
    setTimeout(function () { if (current === failed) stop(true); }, 2500);
  });
  video.addEventListener('volumechange', function () { report(null); });

  /* ---- commands ---- */
  function load(cmd) {
    /* Douyin sends Stop ~30 ms before the next SetAVTransportURI. Tearing the
       source down on Stop and again here makes webOS 4.4 release and re-acquire
       the hardware decoder (~0.4-0.8 s before the new source is even opened).
       So: Stop only pauses and arms a deferred teardown; a load arriving in
       time cancels it and simply swaps src. Measured on OLED55B8 via CDP:
       load -> first byte request 1.35 s -> ~0.8 s. */
    cancelTeardown();
    current = { loadId: cmd.loadId, url: cmd.url, title: cmd.title || '' };
    retried = false;
    terminal = false;
    osdTitle.textContent = current.title;
    showIdle(false);
    showLoading(true, '正在加载…');
    osd.classList.add('hidden');
    try { video.pause(); } catch (e) { /* ignore */ }
    video.src = cmd.url;
    video.load();
    report('loading');
    if (cmd.startAt && cmd.startAt > 1) {
      var startAt = cmd.startAt;
      var onMeta = function () { video.removeEventListener('loadedmetadata', onMeta); try { video.currentTime = startAt; } catch (e) { /* ignore */ } };
      video.addEventListener('loadedmetadata', onMeta);
    }
    if (cmd.autoplay !== false) {
      var p = video.play();
      if (p && p['catch']) p['catch'](function (e) {
        App.log('play() rejected: ' + e);
        /* Browser autoplay policy (no user gesture yet): fall back to muted
           autoplay and let the user click to unmute, so the clip at least shows. */
        if (API.role === 'web' && !video.muted) {
          video.muted = true;
          var p2 = video.play();
          if (p2 && p2['catch']) p2['catch'](function () {});
          needsUnmute = true;
          App.showUnmute(true);
        }
      });
    }
    // Watchdog: if nothing starts within 20 s, report an error so the phone
    // is not stuck in TRANSITIONING.
    clearStall();
    stallTimer = setTimeout(function () {
      if (current && current.loadId === cmd.loadId && video.readyState < 3) {
        App.toast('加载超时', true);
        report('error', { error: 'load timeout' });
        stop(true);
      }
    }, 20000);
  }

  function play() {
    if (!current) return;
    var p = video.play();
    if (p && p['catch']) p['catch'](function () {});
    osd.classList.add('hidden');
  }

  function pause() { if (!current) return; video.pause(); }

  var teardownTimer = null;
  var TEARDOWN_DELAY_MS = 400;
  function cancelTeardown() { if (teardownTimer) { clearTimeout(teardownTimer); teardownTimer = null; } }
  function teardown() {
    teardownTimer = null;
    video.removeAttribute('src');
    try { video.load(); } catch (e) { /* ignore */ }
  }

  function stop(silent) {
    clearStall();
    var had = !!current;
    current = null;
    try { video.pause(); } catch (e) { /* ignore */ }
    // Release the decoder only if no new load follows shortly (see load()).
    cancelTeardown();
    teardownTimer = setTimeout(teardown, TEARDOWN_DELAY_MS);
    if (needsUnmute) { needsUnmute = false; App.showUnmute(false); }
    showIdle(true);
    if (had && !silent) report('stopped');
  }

  function seek(sec) {
    if (!current) return;
    var d = video.duration;
    if (isFinite(d) && d > 0) sec = Math.min(sec, Math.max(0, d - 0.5));
    try { video.currentTime = Math.max(0, sec); } catch (e) { /* not seekable yet */ }
    showOsd(3000);
  }

  function seekBy(delta) { if (current) seek((video.currentTime || 0) + delta); }

  function togglePlay() {
    if (!current) return;
    if (needsUnmute) { unmute(); return; }
    if (video.paused) play(); else pause();
  }

  function setVolume(v) { video.volume = Math.max(0, Math.min(1, v / 100)); }
  function setMute(m) { video.muted = !!m; }

  /* Called from a real user gesture (click / key): lift the browser autoplay mute. */
  function unmute() {
    if (!needsUnmute) return false;
    needsUnmute = false;
    video.muted = false;
    App.showUnmute(false);
    App.toast('已开启声音');
    if (video.paused && current) play();
    return true;
  }

  /* Another screen (the TV) took over: drop whatever we are showing, silently. */
  function standby() { stop(true); }

  return {
    standby: standby, unmute: unmute, needsUnmute: function () { return needsUnmute; },
    load: load, play: play, pause: pause, stop: stop, seek: seek, seekBy: seekBy, togglePlay: togglePlay,
    setVolume: setVolume, setMute: setMute, showOsd: showOsd,
    isActive: function () { return !!current; },
    isPaused: function () { return video.paused; },
    report: function () { report(null); },
    onReport: function (fn) { reportFn = fn; }
  };
})();
