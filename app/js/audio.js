/* audio.js — the audio face: spinning record (cover as its label), track
 * info, progress and lyrics. ES5 only (Chrome 53).
 *
 * AudioFace.show(track)   track = {loadId, title, artist, art}
 * AudioFace.update(s)     s = {cur, dur, playing, note}   (called on player events)
 * AudioFace.hide()
 * AudioFace.toggleLyrics()
 *
 * Lyrics are ON by default. The choice is remembered (localStorage), so once
 * switched it stays that way for every following track until switched back. */
var AudioFace = (function () {
  'use strict';

  var root = document.getElementById('audio');
  var bg = document.getElementById('au-bg');
  var label = document.getElementById('au-label');
  var chr = document.getElementById('au-char');
  var art = document.getElementById('au-art');
  var titleEl = document.getElementById('au-title');
  var artistEl = document.getElementById('au-artist');
  var curEl = document.getElementById('au-cur');
  var durEl = document.getElementById('au-dur');
  var fill = document.getElementById('au-fill');
  var stateEl = document.getElementById('au-state');
  var box = document.getElementById('au-lyr');
  var list = document.getElementById('au-list');
  var empty = document.getElementById('au-empty');

  var STORE_KEY = 'cast.lyrics';
  var track = null;        // what is on the face
  var model = null;        // Lyrics.build() result for `track`, null until it arrives
  var rowEls = [];
  var activeIdx = -2;      // -2 = nothing placed yet
  var last = { cur: 0, dur: 0 };
  var retryTimer = null;

  function readOn() { try { return window.localStorage.getItem(STORE_KEY) !== '0'; } catch (e) { return true; } }
  var lyricsOn = readOn();

  function fmt(sec) {
    if (!isFinite(sec) || sec < 0) sec = 0;
    sec = Math.floor(sec);
    var m = Math.floor(sec / 60), s = sec % 60;
    return m + ':' + (s < 10 ? '0' : '') + s;
  }

  /* Stable colour per title for the label while (or if never) the cover loads. */
  function colorOf(s) {
    var h = 0;
    for (var i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) % 360;
    return 'hsl(' + h + ',38%,34%)';
  }

  function applyLyricsOn() {
    if (lyricsOn) root.classList.remove('nolyr'); else root.classList.add('nolyr');
  }

  /* ---- backdrop ----
     A soft, dark wash of the cover's colours, drawn once per track:
     shrink the cover to 12x7 pixels, scale that up (the scaling is the blur),
     darken it, and add faint noise so the 8-bit gradient does not band. */
  var BG_W = 480, BG_H = 270;
  function makeBackdrop(img) {
    var small = document.createElement('canvas');
    small.width = 12; small.height = 7;
    small.getContext('2d').drawImage(img, 0, 0, 12, 7);
    var c = document.createElement('canvas');
    c.width = BG_W; c.height = BG_H;
    var x = c.getContext('2d');
    x.imageSmoothingEnabled = true;
    x.drawImage(small, 0, 0, BG_W, BG_H);
    x.fillStyle = 'rgba(10,11,16,0.72)';
    x.fillRect(0, 0, BG_W, BG_H);
    var d = x.getImageData(0, 0, BG_W, BG_H), p = d.data;
    for (var i = 0; i < p.length; i += 4) {
      var n = (Math.random() + Math.random() - 1) * 3;
      p[i] += n; p[i + 1] += n; p[i + 2] += n;
    }
    x.putImageData(d, 0, 0);
    return c.toDataURL('image/png');
  }

  /* Needs a CORS-readable copy of the cover; if the art server does not allow
     that (or anything else fails) the plain dark background simply stays. */
  function loadBackdrop(t) {
    bg.classList.add('off');
    if (!t.art) return;
    var im = new Image();
    im.crossOrigin = 'anonymous';
    im.onload = function () {
      if (track !== t) return;
      try {
        bg.style.backgroundImage = 'url(' + makeBackdrop(im) + ')';
        bg.classList.remove('off');
      } catch (e) { /* tainted canvas: no backdrop */ }
    };
    im.src = t.art;
  }

  /* ---- lyrics ---- */
  function setEmpty(msg) { empty.textContent = msg || ''; }

  function clearRows() {
    while (list.firstChild) list.removeChild(list.firstChild);
    rowEls = [];
    activeIdx = -2;
    list.className = 'list jump';
    list.style.webkitTransform = list.style.transform = 'translateY(0px)';
  }

  function renderRows() {
    clearRows();
    if (!model || !model.rows.length) { setEmpty(model ? '暂无歌词' : '歌词加载中…'); return; }
    setEmpty('');
    if (!model.timed) list.className = 'list jump untimed';
    for (var i = 0; i < model.rows.length; i++) {
      var r = model.rows[i];
      var el = document.createElement('div');
      el.className = 'row';
      var main = document.createElement('div');
      main.textContent = r.text;
      el.appendChild(main);
      if (r.sub) {
        var sub = document.createElement('div');
        sub.className = 'sub';
        sub.textContent = r.sub;
        el.appendChild(sub);
      }
      list.appendChild(el);
      rowEls.push(el);
    }
    place(true);
  }

  function moveTo(y, jump) {
    if (jump) list.classList.add('jump'); else list.classList.remove('jump');
    list.style.webkitTransform = list.style.transform = 'translateY(' + Math.round(y) + 'px)';
  }

  /* Highlight the row for the current position and bring it to the middle. */
  function place(jump) {
    if (!lyricsOn || !model || !rowEls.length) return;
    var h = box.clientHeight;
    if (!model.timed) {
      /* No timestamps: nothing to follow, drift through the text with the song. */
      var over = list.offsetHeight - h;
      var frac = last.dur > 0 ? Math.min(1, last.cur / last.dur) : 0;
      moveTo(over > 0 ? -over * frac : 0, jump);
      return;
    }
    var idx = Lyrics.activeLineIndex(model.rows, last.cur);
    if (idx === activeIdx && !jump) return;
    if (activeIdx >= 0 && rowEls[activeIdx]) rowEls[activeIdx].classList.remove('on');
    if (idx >= 0) rowEls[idx].classList.add('on');
    /* A jump of more than one row is a seek: do not animate across the song. */
    var far = activeIdx < -1 || Math.abs(idx - activeIdx) > 1;
    activeIdx = idx;
    var el = rowEls[idx < 0 ? 0 : idx];
    moveTo(h / 2 - (el.offsetTop + el.offsetHeight / 2), jump || far);
  }

  function fetchLyric(t, attempt) {
    API.lyric(t.loadId).then(function (res) {
      if (track !== t) return;
      if (res && res.error && attempt < 2) { retryLater(t, attempt); return; }
      model = Lyrics.build(res ? res.lyric : '', res ? res.tlyric : '');
      renderRows();
    }, function () {
      if (track !== t) return;
      if (attempt < 2) { retryLater(t, attempt); return; }
      model = Lyrics.build('', '');
      renderRows();
    });
  }
  function retryLater(t, attempt) {
    retryTimer = setTimeout(function () { retryTimer = null; if (track === t) fetchLyric(t, attempt + 1); }, 4000);
  }

  /* ---- face ---- */
  function show(t) {
    if (retryTimer) { clearTimeout(retryTimer); retryTimer = null; }
    track = t;
    model = null;
    last = { cur: 0, dur: 0 };
    titleEl.textContent = t.title || '';
    artistEl.textContent = t.artist || '';
    chr.textContent = (t.title || '♪').charAt(0);
    label.style.backgroundColor = colorOf(t.title || '');
    art.style.display = 'none';
    art.removeAttribute('src');
    if (t.art) {
      art.onload = function () { if (track === t) art.style.display = ''; };
      art.onerror = function () { art.style.display = 'none'; };
      art.src = t.art;
    }
    loadBackdrop(t);
    curEl.textContent = '0:00'; durEl.textContent = '--:--'; fill.style.width = '0%';
    stateEl.textContent = '';
    root.classList.remove('playing');
    applyLyricsOn();
    root.classList.remove('hidden');
    renderRows();          // "歌词加载中…"
    fetchLyric(t, 1);
  }

  function hide() {
    if (retryTimer) { clearTimeout(retryTimer); retryTimer = null; }
    track = null;
    root.classList.add('hidden');
    root.classList.remove('playing');
  }

  function update(s) {
    if (!track) return;
    last = { cur: s.cur || 0, dur: s.dur || 0 };
    if (s.playing) root.classList.add('playing'); else root.classList.remove('playing');
    curEl.textContent = fmt(last.cur);
    durEl.textContent = last.dur > 0 ? fmt(last.dur) : '--:--';
    fill.style.width = (last.dur > 0 ? Math.min(100, last.cur / last.dur * 100) : 0) + '%';
    stateEl.textContent = s.note || '';
    place(false);
  }

  function toggleLyrics() {
    lyricsOn = !lyricsOn;
    try { window.localStorage.setItem(STORE_KEY, lyricsOn ? '1' : '0'); } catch (e) { /* not persisted */ }
    applyLyricsOn();
    if (lyricsOn) place(true);   // offsets were 0 while the panel was hidden
    return lyricsOn;
  }

  applyLyricsOn();

  return {
    show: show, hide: hide, update: update, toggleLyrics: toggleLyrics,
    visible: function () { return !!track; },
    lyricsOn: function () { return lyricsOn; }
  };
})();
