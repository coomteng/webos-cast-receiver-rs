/* api.js — XHR channel to the cast-receiver server. ES5 only (Chrome 53).
 * On the TV the app is loaded from file:// and needs an explicit server address
 * (BRIDGE_HOST placeholder is replaced by tools/build_ipk.sh). Served by the
 * server itself (http://<server>:8790/tv/) it uses the same origin. */
var API = (function () {
  'use strict';

  var BASE = (window.location.protocol === 'http:' || window.location.protocol === 'https:')
    ? window.location.protocol + '//' + window.location.host
    : 'http://BRIDGE_HOST:8790';

  /* Screen identity: the LG app is role 'tv' (always wins); a browser tab is role 'web'.
   * A random per-tab id lets several browser tabs coexist without stealing each other's queue. */
  var ROLE = /Web0S|WebAppManager/.test(navigator.userAgent) || window.location.protocol === 'file:' ? 'tv' : 'web';
  var CLIENT = ROLE + '-' + Math.random().toString(36).slice(2, 10);

  function request(method, path, body, timeoutMs) {
    return new Promise(function (resolve, reject) {
      var xhr = new XMLHttpRequest();
      var done = false;
      function fail(msg) { if (done) return; done = true; reject(new Error(msg)); }
      try { xhr.open(method, BASE + path, true); } catch (e) { return fail('open: ' + e.message); }
      xhr.timeout = timeoutMs || 15000;
      if (body !== undefined && body !== null) xhr.setRequestHeader('Content-Type', 'application/json');
      xhr.onreadystatechange = function () {
        if (xhr.readyState !== 4 || done) return;
        if (xhr.status >= 200 && xhr.status < 300) {
          var data = null;
          try { data = xhr.responseText ? JSON.parse(xhr.responseText) : null; } catch (e) { return fail('bad json'); }
          done = true; resolve(data);
        } else {
          fail('HTTP ' + xhr.status + ' ' + path);
        }
      };
      xhr.ontimeout = function () { fail('timeout ' + path); };
      xhr.onerror = function () { fail('network ' + path); };
      try { xhr.send(body === undefined || body === null ? null : JSON.stringify(body)); } catch (e2) { fail('send: ' + e2.message); }
    });
  }

  /* This app's own version (tools/build_ipk.sh checks it against appinfo.json).
     The server reports its version separately in /tv/info. */
  var APP_VERSION = '0.3.2';

  return {
    base: BASE,
    version: APP_VERSION,
    info: function () { return request('GET', '/tv/info', null, 8000); },
    /* Long poll: server holds up to 25 s; our timeout is a bit longer. */
    role: ROLE,
    client: CLIENT,
    poll: function (after) { return request('GET', '/tv/poll?client=' + CLIENT + '&role=' + ROLE + '&after=' + encodeURIComponent(after), null, 32000); },
    /* Lyrics of the item with this load id: {loadId, lyric, tlyric[, stale][, error]} */
    lyric: function (loadId) { return request('GET', '/tv/lyric?loadId=' + encodeURIComponent(loadId), null, 15000); },
    state: function (report) { report.client = CLIENT; report.role = ROLE; return request('POST', '/tv/state', report, 8000); }
  };
})();
