// HoosierSDR remote desktop shim. Loaded ahead of app.js when the desktop
// page is served over HTTP (Settings → Remote access → Open, or /desktop/
// in any browser). It provides the two Tauri IPC calls app.js uses —
// `invoke` and `listen` — on top of the web API, so the page runs exactly
// as it does in the app: commands go to /api/command, events arrive on the
// SSE feed, and audio (live calls and library clips) plays in this browser
// instead of on the far machine's speakers.
(() => {
  "use strict";
  const KEY = "hs.web.token";
  try {
    const qs = new URLSearchParams(location.search);
    const t = qs.get("token");
    if (t) { localStorage.setItem(KEY, t); history.replaceState(null, "", location.pathname + location.hash); }
  } catch (_) { /* no URL or storage access */ }
  const token = () => { try { return localStorage.getItem(KEY) || ""; } catch (_) { return ""; } };
  const snake = (k) => k.replace(/[A-Z]/g, (c) => "_" + c.toLowerCase());
  const headers = (extra = {}) => ({ "x-token": token(), ...extra });

  /* ---------- login: a token prompt over the page until the server lets us in ---------- */
  let loginP = null;
  function login(reason) {
    if (loginP) return loginP;
    loginP = new Promise((resolve) => {
      const ov = document.createElement("div");
      ov.id = "hsRemoteLogin";
      ov.style.cssText = "position:fixed;inset:0;z-index:99999;display:flex;align-items:center;justify-content:center;background:rgba(0,0,0,.55);font-family:system-ui,sans-serif";
      ov.innerHTML = `<form style="background:#1c1f26;color:#e8e8ec;padding:22px 24px;border-radius:12px;min-width:320px;box-shadow:0 20px 60px rgba(0,0,0,.5)">
        <div style="font-weight:700;font-size:15px;margin-bottom:6px">HoosierSDR · remote</div>
        <div style="font-size:12px;opacity:.75;margin-bottom:12px">${reason || "Enter the access token from that machine's Settings → Remote access."}</div>
        <input id="hsRemoteTok" type="password" autocomplete="off" spellcheck="false" placeholder="access token" style="width:100%;box-sizing:border-box;padding:8px 10px;border-radius:8px;border:1px solid #444;background:#111;color:#eee;font:inherit" />
        <div style="display:flex;justify-content:flex-end;gap:8px;margin-top:12px"><button type="submit" style="padding:7px 14px;border-radius:8px;border:0;background:#3b82f6;color:#fff;font:inherit;cursor:pointer">Connect</button></div>
      </form>`;
      document.body.appendChild(ov);
      const inp = ov.querySelector("#hsRemoteTok");
      setTimeout(() => inp.focus(), 50);
      ov.querySelector("form").onsubmit = (e) => {
        e.preventDefault();
        const v = inp.value.trim();
        if (!v) return;
        try { localStorage.setItem(KEY, v); } catch (_) {}
        ov.remove(); loginP = null; resolve();
      };
    });
    return loginP;
  }

  /* ---------- invoke ---------- */
  async function post(cmd, args) {
    const body = {};
    for (const k of Object.keys(args || {})) body[snake(k)] = args[k];
    for (;;) {
      let res;
      try {
        res = await fetch("/api/command", { method: "POST", headers: headers({ "content-type": "application/json" }), body: JSON.stringify({ command: cmd, args: body }) });
      } catch (e) { throw `remote: ${e.message || e}`; }
      if (res.status === 401) { await login("The token was not accepted. Enter the access token shown on that machine's Settings → Remote access."); continue; }
      const text = await res.text();
      if (!res.ok) throw text; // Tauri rejects with the command's error string; app.js shows it as-is
      try { return JSON.parse(text); } catch (_) { return text; }
    }
  }

  /* ---------- audio in this browser ---------- */
  let ac = null, gain = null, nextAt = 0, pill = null;
  function ensureAudio() {
    if (!ac) {
      ac = new (window.AudioContext || window.webkitAudioContext)();
      gain = ac.createGain(); gain.connect(ac.destination);
    }
    if (ac.state === "suspended") ac.resume().catch(() => {});
    if (ac.state === "running" && pill) { pill.remove(); pill = null; }
  }
  function showPill() {
    if (pill || (ac && ac.state === "running")) return;
    pill = document.createElement("div");
    pill.textContent = "🔊 Click anywhere to hear this instance";
    pill.style.cssText = "position:fixed;left:50%;bottom:18px;transform:translateX(-50%);z-index:99998;padding:8px 14px;border-radius:999px;background:#111;color:#fff;font:600 12px system-ui,sans-serif;box-shadow:0 8px 30px rgba(0,0,0,.4);pointer-events:none";
    document.body.appendChild(pill);
  }
  function playPcm(b64, rate) {
    ensureAudio();
    const bin = atob(b64), n = bin.length >> 1;
    const buf = ac.createBuffer(1, n, rate || 8000), ch = buf.getChannelData(0);
    for (let i = 0; i < n; i++) { let v = bin.charCodeAt(2 * i) | (bin.charCodeAt(2 * i + 1) << 8); if (v >= 32768) v -= 65536; ch[i] = v / 32768; }
    const src = ac.createBufferSource(); src.buffer = buf; src.connect(gain);
    // Calls queue one after another, as the desktop's player does.
    const at = Math.max(ac.currentTime, nextAt); src.start(at); nextAt = at + buf.duration;
  }
  async function playUrl(url) {
    ensureAudio();
    const res = await fetch(url, { headers: headers() });
    if (res.status === 401) { await login(); return playUrl(url); }
    if (!res.ok) throw await res.text();
    const buf = await ac.decodeAudioData(await res.arrayBuffer());
    const src = ac.createBufferSource(); src.buffer = buf; src.connect(gain); src.start();
  }
  ["click", "keydown", "touchstart"].forEach((n) => document.addEventListener(n, ensureAudio, { capture: true, passive: true }));

  /* ---------- commands handled here rather than on the far machine ---------- */
  const LOCAL = {
    ui_log: async (a) => { console.log("[hs]", a && a.msg); return null; },
    library_play: async (a) => { await playUrl(`/api/audio/${encodeURIComponent(a.id)}`); return null; },
    play_wav: async (a) => { await playUrl(`/api/file?path=${encodeURIComponent(a.path)}`); return null; },
    remote_open: async (a) => { location.href = String(a.url || "").replace(/\/+$/, "") + "/desktop/"; return null; },
    set_volume: async (a) => { ensureAudio(); if (gain) gain.gain.value = Math.max(0, Math.min(2, +a.gain || +a.value || 1)); return post("set_volume", a); },
  };
  async function invoke(cmd, args) {
    const local = LOCAL[cmd];
    if (local) return local(args || {});
    return post(cmd, args || {});
  }

  /* ---------- listen: one SSE connection, fanned out by event name ---------- */
  const subs = {};
  const known = new Set();
  let es = null;
  function bind(name) {
    es.addEventListener(name, (e) => {
      let payload; try { payload = JSON.parse(e.data); } catch (_) { payload = e.data; }
      for (const cb of subs[name] || []) { try { cb({ event: name, payload }); } catch (err) { console.error(`[hs] ${name} handler:`, err); } }
    });
  }
  function connect() {
    if (es) es.close();
    es = new EventSource(`/api/events?token=${encodeURIComponent(token())}&spectrum=1`);
    for (const n of known) bind(n);
    es.addEventListener("audio", (e) => { try { const d = JSON.parse(e.data); playPcm(d.pcm_b64, d.rate); showPill(); } catch (err) { console.error("[hs] audio:", err); } });
    es.addEventListener("lagged", () => console.warn("[hs] feed lagged; some frames were skipped"));
    es.onerror = () => { /* the browser retries on its own; a dead token surfaces on the next invoke */ };
  }
  async function listen(name, cb) {
    (subs[name] = subs[name] || []).push(cb);
    if (!known.has(name)) { known.add(name); if (es) bind(name); }
    return () => { subs[name] = (subs[name] || []).filter((f) => f !== cb); };
  }

  window.__HS_REMOTE__ = true;
  window.__TAURI__ = { core: { invoke }, event: { listen } };
  document.title = "HoosierSDR · remote";

  // Prove the token (or the far end's tailnet trust) before opening the feed,
  // so a wrong token shows the prompt instead of a silently dead page.
  (async () => {
    for (;;) {
      let res;
      try { res = await fetch("/api/status", { headers: headers() }); } catch (_) { await new Promise((r) => setTimeout(r, 2000)); continue; }
      if (res.status === 401) { await login(); continue; }
      break;
    }
    // Catch up with the run in progress, then follow it live. The page's
    // own `applySnapshot` (app.js) sets the controls; the replayed frames
    // rebuild the panels through the same handlers the live feed uses.
    try {
      const res = await fetch("/api/snapshot", { headers: headers() });
      if (res.ok) {
        const snap = await res.json();
        if (typeof window.applySnapshot === "function") { try { window.applySnapshot(snap); } catch (e) { console.error("[hs] applySnapshot:", e); } }
        for (const ev of snap.events || []) {
          for (const cb of subs[ev.event] || []) { try { cb({ event: ev.event, payload: ev.data }); } catch (e) { console.error("[hs] replay:", e); } }
        }
      }
    } catch (e) { console.error("[hs] snapshot:", e); }
    connect();
    showPill();
  })();
})();
