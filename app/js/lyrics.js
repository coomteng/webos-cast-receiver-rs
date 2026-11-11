/* lyrics.js — LRC parsing and position lookup for the audio face. Pure
 * functions, no DOM. ES5 only (Chrome 53). parseLrc / activeLineIndex read
 * standard LRC; build() adds the row model the TV shows.
 * Also loadable from Node (module.exports) so it can be checked off-TV. */
var Lyrics = (function () {
  'use strict';

  /* One or more leading time tags: [mm:ss], [mm:ss.x], [mm:ss.xx], [mm:ss.xxx]. */
  var TIME_TAG = /\[(\d{1,3}):(\d{1,2})(?:\.(\d{1,3}))?\]/g;
  /* [offset:+500] in ms; positive = lyrics earlier = subtract from every time. */
  var OFFSET_TAG = /\[offset:\s*([+-]?\d+)\s*\]/i;
  var ANY_TAG = /^\s*\[[^\]]*\]\s*/;

  /* -> [{timeSec, text}] sorted by time (stable). Metadata tags, untagged
     lines and empty lines are skipped. Never throws. */
  function offsetOf(text) {
    var om = typeof text === 'string' ? OFFSET_TAG.exec(text) : null;
    if (!om) return null;
    var ms = parseInt(om[1], 10);
    return isNaN(ms) ? null : ms / 1000;
  }

  /* `fallbackOffsetSec` applies when the text carries no [offset:] of its own. */
  function parseLrc(text, fallbackOffsetSec) {
    if (typeof text !== 'string' || text === '') return [];
    var offsetSec = offsetOf(text);
    if (offsetSec === null) offsetSec = fallbackOffsetSec || 0;
    var out = [];
    var lines = text.split(/\r\n|\r|\n/);
    for (var i = 0; i < lines.length; i++) {
      var line = lines[i];
      TIME_TAG.lastIndex = 0;
      var times = [], m, end = 0;
      while ((m = TIME_TAG.exec(line)) !== null) {
        if (m.index !== end) break; /* tags must be contiguous from the line start */
        var frac = m[3] ? parseInt(m[3], 10) / Math.pow(10, m[3].length) : 0;
        times.push(parseInt(m[1], 10) * 60 + parseInt(m[2], 10) + frac);
        end = TIME_TAG.lastIndex;
      }
      if (!times.length) continue;
      var t = line.slice(end).replace(/^\s+|\s+$/g, '');
      if (t === '') continue;
      for (var k = 0; k < times.length; k++) {
        var sec = times[k] - offsetSec;
        out.push({ timeSec: sec > 0 ? sec : 0, text: t, seq: out.length });
      }
    }
    out.sort(function (a, b) { return a.timeSec - b.timeSec || a.seq - b.seq; });
    return out;
  }

  /* Index of the last row whose timeSec <= positionSec, -1 before the first. */
  function activeLineIndex(rows, positionSec) {
    if (!rows || !rows.length) return -1;
    var lo = 0, hi = rows.length - 1, ans = -1;
    while (lo <= hi) {
      var mid = (lo + hi) >> 1;
      if (rows[mid].timeSec <= positionSec) { ans = mid; lo = mid + 1; } else { hi = mid - 1; }
    }
    return ans;
  }

  /* build(lyric, tlyric) -> {timed, rows:[{timeSec, text, sub}]}.
     One row per timestamp: a second line at the same time (some sources put
     the translation there) and the matching tlyric line become `sub`.
     Text without any timestamp gives timed:false rows (shown, not followed). */
  function build(lyric, tlyric) {
    var main = parseLrc(lyric);
    var rows = [], i;
    if (!main.length) {
      var plain = typeof lyric === 'string' ? lyric.split(/\r\n|\r|\n/) : [];
      for (i = 0; i < plain.length; i++) {
        var p = plain[i];
        while (ANY_TAG.test(p)) p = p.replace(ANY_TAG, '');
        p = p.replace(/^\s+|\s+$/g, '');
        if (p) rows.push({ timeSec: 0, text: p, sub: '' });
      }
      return { timed: false, rows: rows };
    }
    for (i = 0; i < main.length; i++) {
      var last = rows.length ? rows[rows.length - 1] : null;
      if (last && Math.abs(last.timeSec - main[i].timeSec) < 0.005) {
        if (main[i].text !== last.text) last.sub = last.sub ? last.sub + ' ' + main[i].text : main[i].text;
      } else {
        rows.push({ timeSec: main[i].timeSec, text: main[i].text, sub: '' });
      }
    }
    /* A translation mirrors the original's raw timestamps: shift it the same way. */
    var tr = parseLrc(tlyric, offsetOf(lyric) || 0);
    for (i = 0; i < tr.length; i++) {
      var idx = activeLineIndex(rows, tr[i].timeSec + 0.005);
      if (idx < 0 || Math.abs(rows[idx].timeSec - tr[i].timeSec) > 0.05) continue;
      if (tr[i].text === rows[idx].text) continue;
      rows[idx].sub = rows[idx].sub ? rows[idx].sub + ' ' + tr[i].text : tr[i].text;
    }
    return { timed: true, rows: rows };
  }

  return { parseLrc: parseLrc, activeLineIndex: activeLineIndex, build: build };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = Lyrics;
