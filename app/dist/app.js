// HoosierSDR front end. Drives the Tauri backend when present (start_follow /
// start_capture and the follow / grant / status / spectrum events), and runs a
// small demo driver when opened standalone so the layout can be previewed.
const $ = (id) => document.getElementById(id);
const TAURI = window.__TAURI__;
const invoke = TAURI ? TAURI.core.invoke : null;
const listen = TAURI ? TAURI.event.listen : null;

/* ---------- diagnostics: JS errors and key events go to the launching terminal ---------- */
function log(m) {
  try { console.log(m); } catch (_) {}
  if (TAURI) invoke("ui_log", { msg: String(m) }).catch(() => {});
}
window.onerror = (m, src, line, col) => log(`JS error: ${m} @ ${line}:${col}`);
window.onunhandledrejection = (e) => log(`unhandled rejection: ${e.reason}`);
log(`page loaded; tauri=${!!TAURI}`);

/* ---------- theme ---------- */
$("theme").onclick = () => {
  const root = document.documentElement;
  const cur = root.getAttribute("data-theme") || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
  root.setAttribute("data-theme", cur === "dark" ? "light" : "dark");
};

/* ---------- helpers ---------- */
function parseFreq(s) {
  s = String(s).trim();
  if (/[Mm]$/.test(s)) return parseFloat(s) * 1e6;
  if (/[kK]$/.test(s)) return parseFloat(s) * 1e3;
  return parseFloat(s);
}
const mhz = (hz) => (hz / 1e6).toFixed(4);
// Everything that came from outside (RadioReference, CSVs, whisper, files) is
// escaped before it meets innerHTML.
const esc = (v) => String(v ?? "").replace(/[&<>"']/g, (ch) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[ch]));
const now = () => new Date().toLocaleTimeString("en-US", { hour12: false });
function wireSeg(el, onPick) {
  el.querySelectorAll("button").forEach((b) => {
    b.onclick = () => { setSeg(el, b.dataset.v); onPick(b.dataset.v); };
  });
}
function setSeg(el, v) { el.querySelectorAll("button").forEach((x) => x.setAttribute("aria-pressed", String(x.dataset.v === v))); }

/* ---------- views ---------- */
function showView(v) {
  ["monitor", "library", "playlists", "aliases", "settings"].forEach((n) => { $("view-" + n).style.display = n === v ? "" : "none"; });
  if (v === "library" && typeof libOnShow === "function") libOnShow();
  if (v === "aliases" && typeof aliasesOnShow === "function") aliasesOnShow();
  setSeg($("navSeg"), v);
}
$("navSeg").querySelectorAll("button").forEach((b) => b.onclick = () => showView(b.dataset.v));
setTimeout(() => { if (["#playlists", "#settings", "#library", "#aliases"].includes(location.hash)) showView(location.hash.slice(1)); }, 0);

/* ---------- tuning state ---------- */
let modeSel = "follow", modSel = "cqpsk", eqSel = "cma";
function applyMode() {
  const follow = modeSel === "follow";
  $("centerField").style.display = follow ? "" : "none";
  $("modField").style.display = follow ? "none" : "";
  $("eqField").style.display = follow ? "none" : "";
  $("chanReadouts").style.display = follow ? "none" : "";
  $("followOpts").style.display = follow ? "" : "none";
  $("channelOpts").style.display = follow ? "none" : "";
  $("freqHint").textContent = follow ? "control channel" : "channel";
  $("emptyHint").textContent = follow ? "Pick a playlist or set a control channel, then press Start." : "Set a channel and press Start. One-channel mode decodes and counts voice but does not play it — use Follow site to listen.";
}
wireSeg($("modeSeg"), (v) => { modeSel = v; applyMode(); });
wireSeg($("modSeg"), (v) => { modSel = v; });
wireSeg($("eqSeg"), (v) => { eqSel = v; $("r-eq").textContent = v === "bypass" ? "BARE" : v.toUpperCase(); });
function syncRate() {
  const r = parseFloat($("rate").value);
  $("rateMeta").textContent = r >= 1e6 ? (r / 1e6).toFixed(1) + " MSPS" : (r / 1e3) + " kSPS";
}
$("rate").onchange = syncRate;
$("source").onchange = () => {
  const a = $("source").value === "airspy";
  $("rate").value = a ? (modeSel === "follow" ? "10000000" : "2500000") : "2400000";
  syncRate();
};
applyMode(); syncRate();

/* ---------- state pill ---------- */
function setState(s) {
  const p = $("pill");
  p.className = "pill" + (s === "standby" ? "" : s === "following" || s === "decoding" ? " locked" : " live");
  $("pillText").textContent = s;
  $("start").disabled = s !== "standby";
  $("stop").disabled = s === "standby";
  if (s === "standby") { activeClear(); $("activeMeta").textContent = "idle"; }
}

/* ---------- now playing ---------- */
const activeCalls = new Map();   // key → { el, start }
function activeKey(tg, f) { return `${tg}@${f.toFixed(4)}`; }
let lastTg = null;
function activeStart(ev) {
  const key = activeKey(ev.tg, ev.freq_mhz);
  lastTg = ev.tg; updateHoldBtn();
  if (activeCalls.has(key)) return;
  const el = document.createElement("div");
  el.className = "call" + (ev.priority && ev.priority < 50 ? " pri" : "");
  el.innerHTML = `<span class="tg">${esc(ev.name)}</span><span class="t">0:00</span><span class="sub">TG ${ev.tg} · ${ev.freq_mhz.toFixed(4)} MHz</span>`;
  $("active").prepend(el);
  activeCalls.set(key, { el, start: Date.now() });
  activeRefresh();
}
function activeEnd(ev) {
  const key = activeKey(ev.tg, ev.freq_mhz);
  const a = activeCalls.get(key);
  if (a) { a.el.remove(); activeCalls.delete(key); }
  activeRefresh();
}
function activeMarkEmergency(tg) {
  activeCalls.forEach((a, key) => { if (key.startsWith(tg + "@")) a.el.classList.add("emg"); });
}

/* ---------- events log ---------- */
const evlog = $("events");
function logEvent(text, cls) {
  const d = document.createElement("div");
  d.className = "ev" + (cls ? " " + cls : "");
  d.innerHTML = `<span class="t">${now()}</span><span>${esc(text)}</span>`;
  evlog.prepend(d);
  while (evlog.children.length > 150) evlog.removeChild(evlog.lastChild);
}
$("evClear").onclick = () => { evlog.innerHTML = ""; };

/* ---------- hold ---------- */
let holdTg = null;
function updateHoldBtn() {
  const b = $("holdBtn");
  b.classList.toggle("on", holdTg != null);
  b.textContent = holdTg != null ? `Hold TG ${holdTg}` : (lastTg != null ? `Hold TG ${lastTg}` : "Hold");
  b.disabled = holdTg == null && lastTg == null;
}
$("holdBtn").onclick = () => {
  holdTg = holdTg != null ? null : lastTg;
  updateHoldBtn();
  if (TAURI) invoke("set_hold", { tg: holdTg }).catch((e) => alert(e));
  logEvent(holdTg != null ? `hold on TG ${holdTg}` : "hold released");
};
updateHoldBtn();
function activeClear() { activeCalls.forEach((a) => a.el.remove()); activeCalls.clear(); activeRefresh(); }
function activeRefresh() {
  $("activeEmpty").style.display = activeCalls.size ? "none" : "";
  $("activeMeta").textContent = activeCalls.size ? `${activeCalls.size} on air` : ($("pillText").textContent === "following" ? "listening" : "idle");
}
setInterval(() => {
  const t = Date.now();
  activeCalls.forEach((a) => {
    const s = Math.floor((t - a.start) / 1000);
    a.el.querySelector(".t").textContent = `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
  });
}, 500);

/* ---------- call history ---------- */
const tbody = $("callBody");
const history = [];   // {el, text}
function addCall(g) {
  $("empty").style.display = "none";
  const tr = document.createElement("tr");
  tr.className = "new";
  const len = g.secs != null ? `${g.secs.toFixed(1)}s` : "";
  tr.innerHTML =
    `<td class="time">${now()}</td>` +
    `<td class="tg">${esc(g.name)}<span class="num">TG ${g.tg}</span></td>` +
    `<td class="src">${g.unit_name ? `${esc(g.unit_name)}<span class="num" style="display:block;font-size:10.5px;color:var(--ink-faint)">${g.source}</span>` : (g.source ? g.source : "—")}</td>` +
    `<td class="dl">${g.freq_mhz.toFixed(4)}</td>` +
    `<td class="len">${len}</td>` +
    `<td>${g.encrypted ? '<span class="badge enc">Encrypted</span>' : g.emergency ? '<span class="badge emg">EMERGENCY</span>' : `<span class="badge clear">${g.modulation || "clear"}</span>`}${g.patched_with && g.patched_with.length ? ` <span class="badge clear" title="patched with">⛓ ${g.patched_with.length}</span>` : ""}</td>` +
    `<td class="act">` +
      (g.wav ? `<button title="Replay" data-wav="${esc(g.wav)}">▶</button>` : "") +
      (g.id != null ? `<button title="Add to cart" data-cart="${g.id}" class="${cart.has(g.id) ? "on" : ""}">🛒</button>` : "") +
      `<button data-pri="${g.tg}">☆</button>` +
      `<button title="Alert tone for TG ${g.tg}" data-bell="${g.tg}">🔔</button>` +
      `<button title="Avoid TG ${g.tg} for a while" data-avoid="${g.tg}">⏱</button>` +
      `<button title="Lock out TG ${g.tg}" data-lock="${g.tg}">⊘</button>` +
    `</td>`;
  if (g.emergency) tr.classList.add("emg");
  tr.querySelectorAll("button[data-wav]").forEach((b) => b.onclick = () => replay(b.dataset.wav));
  tr.querySelectorAll("button[data-lock]").forEach((b) => b.onclick = () => toggleLock(+b.dataset.lock));
  tr.querySelectorAll("button[data-avoid]").forEach((b) => b.onclick = () => avoidFor(+b.dataset.avoid));
  tr.querySelectorAll("button[data-pri]").forEach((b) => b.onclick = () => cyclePriority(+b.dataset.pri));
  tr.querySelectorAll("button[data-bell]").forEach((b) => b.onclick = () => toggleBell(+b.dataset.bell));
  tr.querySelectorAll("button[data-cart]").forEach((b) => b.onclick = () => cartToggle(+b.dataset.cart, `${now()} ${g.name} · ${g.secs != null ? g.secs.toFixed(1) + "s" : ""}`));
  const text = `${g.name} ${g.tg} ${g.source || ""} ${g.freq_mhz.toFixed(4)}`.toLowerCase();
  history.unshift({ el: tr, text });
  tbody.prepend(tr);
  while (history.length > 500) history.pop().el.remove();
  applyHistFilter(); refreshRowButtons();
}
function applyHistFilter() {
  const q = $("histFilter").value.trim().toLowerCase();
  let shown = 0;
  history.forEach((h) => { const on = !q || h.text.includes(q); h.el.style.display = on ? "" : "none"; if (on) shown++; });
  $("histMeta").textContent = history.length ? (q ? `${shown} of ${history.length}` : `${history.length} calls`) : "";
}
$("histFilter").oninput = applyHistFilter;
$("clear").onclick = () => { tbody.innerHTML = ""; history.length = 0; $("empty").style.display = ""; applyHistFilter(); };

/* ---------- per-talkgroup settings (kept in localStorage) ---------- */
const store = (k, d) => { try { return JSON.parse(localStorage.getItem(k)) ?? d; } catch (_) { return d; } };
const save = (k, v) => localStorage.setItem(k, JSON.stringify(v));
const prio = new Map(Object.entries(store("hs.prio", {})).map(([k, v]) => [+k, +v]));   // tg → 10 | 90
const bells = new Set(store("hs.bells", []));
const avoidUntil = new Map(Object.entries(store("hs.avoid", {})).map(([k, v]) => [+k, +v])); // tg → epoch ms
function pushPriorities() { if (TAURI) invoke("set_priorities", { entries: [...prio] }).catch((e) => log(`set_priorities: ${e}`)); }
function cyclePriority(tg) {
  const cur = prio.get(tg) || 50, next = cur === 50 ? 10 : cur === 10 ? 90 : 50;
  if (next === 50) prio.delete(tg); else prio.set(tg, next);
  save("hs.prio", Object.fromEntries(prio)); pushPriorities(); refreshRowButtons();
}
function toggleBell(tg) { bells.has(tg) ? bells.delete(tg) : bells.add(tg); save("hs.bells", [...bells]); refreshRowButtons(); }

/* ---------- cart ---------- */
const cart = new Map(Object.entries(store("hs.cart", {})).map(([k, v]) => [+k, v]));  // id → label
function cartSave() { save("hs.cart", Object.fromEntries(cart)); renderCart(); }
function cartAdd(id, label) { if (id == null) return; cart.set(id, label); cartSave(); }
function cartToggle(id, label) { if (cart.has(id)) cart.delete(id); else cart.set(id, label); cartSave(); }
function renderCart() {
  $("cartMeta").textContent = cart.size ? `${cart.size} call${cart.size === 1 ? "" : "s"}` : "empty";
  $("cartList").innerHTML = [...cart].map(([id, label]) => `<div class="row"><span class="grow">${esc(label)}</span><button class="btn ghost" data-uncart="${id}">✕</button></div>`).join("");
  $("cartList").querySelectorAll("[data-uncart]").forEach((b) => b.onclick = () => { cart.delete(+b.dataset.uncart); cartSave(); });
  document.querySelectorAll("button[data-cart]").forEach((b) => b.classList.toggle("on", cart.has(+b.dataset.cart)));
}
$("cartClear").onclick = () => { cart.clear(); cartSave(); };
renderCart();

/* ---------- tones (WebAudio, no assets) ---------- */
let actx = null;
function tone(kind) {
  if (!$("tones").checked) return;
  try {
    actx = actx || new (window.AudioContext || window.webkitAudioContext)();
    const seq = kind === "emergency" ? [[880, 0], [660, .12], [880, .24], [660, .36]] : [[1320, 0], [1760, .1]];
    seq.forEach(([f, t]) => {
      const o = actx.createOscillator(), g = actx.createGain();
      o.frequency.value = f; o.type = "square"; g.gain.value = 0.06;
      o.connect(g).connect(actx.destination); o.start(actx.currentTime + t); o.stop(actx.currentTime + t + .09);
    });
  } catch (_) {}
}

/* ---------- lockout (permanent) + timed avoid ---------- */
const lockout = new Set(store("hs.lockout", []));
function effectiveLockout() {
  const now = Date.now();
  for (const [tg, until] of avoidUntil) if (until <= now) avoidUntil.delete(tg);
  save("hs.avoid", Object.fromEntries(avoidUntil));
  return [...new Set([...lockout, ...avoidUntil.keys()])];
}
function pushLockout() { if (TAURI) invoke("set_lockout", { tgs: effectiveLockout() }).catch((e) => alert(e)); }
function avoidFor(tg) {
  const min = +$("avoidMin").value || 60;
  if (avoidUntil.has(tg)) avoidUntil.delete(tg); else avoidUntil.set(tg, Date.now() + min * 60000);
  save("hs.avoid", Object.fromEntries(avoidUntil)); renderLockout(); pushLockout();
}
setInterval(() => { const before = avoidUntil.size; effectiveLockout(); renderLockout(); if (avoidUntil.size !== before) pushLockout(); }, 15000);
function renderLockout() {
  const chips = [...lockout].sort((a, b) => a - b).map((tg) => `<span class="chip" data-tg="${tg}" title="Unlock">TG ${tg} ✕</span>`)
    .concat([...avoidUntil].map(([tg, until]) => `<span class="chip" data-avoid="${tg}" title="Timed avoid — click to lift">TG ${tg} ⏱ ${Math.max(1, Math.round((until - Date.now()) / 60000))}m ✕</span>`));
  $("lockbar").style.display = chips.length ? "" : "none";
  $("lockchips").innerHTML = chips.join(" ");
  $("lockchips").querySelectorAll(".chip[data-tg]").forEach((c) => c.onclick = () => toggleLock(+c.dataset.tg));
  $("lockchips").querySelectorAll(".chip[data-avoid]").forEach((c) => c.onclick = () => avoidFor(+c.dataset.avoid));
  refreshRowButtons();
}
function refreshRowButtons() {
  tbody.querySelectorAll("button[data-lock]").forEach((b) => b.classList.toggle("on", lockout.has(+b.dataset.lock)));
  tbody.querySelectorAll("button[data-avoid]").forEach((b) => b.classList.toggle("on", avoidUntil.has(+b.dataset.avoid)));
  tbody.querySelectorAll("button[data-pri]").forEach((b) => { const p = prio.get(+b.dataset.pri) || 50; b.className = p === 10 ? "pri-h" : p === 90 ? "pri-l" : ""; b.textContent = p === 10 ? "★" : p === 90 ? "▽" : "☆"; b.title = `Priority: ${p === 10 ? "high" : p === 90 ? "low" : "normal"} — click to cycle`; });
  tbody.querySelectorAll("button[data-bell]").forEach((b) => b.classList.toggle("bell", bells.has(+b.dataset.bell)));
}
function toggleLock(tg) {
  if (lockout.has(tg)) lockout.delete(tg); else lockout.add(tg);
  save("hs.lockout", [...lockout]);
  renderLockout(); pushLockout();
}
function replay(path) { if (TAURI) invoke("play_wav", { path }).catch((e) => alert(e)); }
effectiveLockout(); renderLockout();

/* ---------- spectrum + waterfall (SDR++-style controls) ---------- */
const wf = $("waterfall"), wctx = wf.getContext("2d");
const sp = $("spectrum"), spctx = sp.getContext("2d");
wctx.fillStyle = "#05090a"; wctx.fillRect(0, 0, wf.width, wf.height);
const MAPS = {
  phosphor: [[6,14,16],[14,58,74],[26,140,150],[74,214,180],[150,240,120],[245,200,90],[255,246,225]],
  inferno:  [[0,0,4],[40,11,84],[101,21,110],[159,42,99],[212,72,66],[245,125,21],[250,193,39],[252,255,164]],
  viridis:  [[68,1,84],[72,40,120],[62,74,137],[49,104,142],[38,130,142],[31,158,137],[53,183,121],[109,205,89],[180,222,44],[253,231,37]],
  turbo:    [[48,18,59],[70,107,227],[40,187,213],[87,238,133],[189,247,57],[251,184,39],[240,97,16],[175,29,4]],
  grey:     [[0,0,0],[255,255,255]],
};
const wfCfg = store("hs.wf", { fft: 1024, avg: 4, map: "phosphor", min: -95, max: -20, line: true, peak: false });
let peakHold = null;
function colour(t) {
  const stops = MAPS[wfCfg.map] || MAPS.phosphor;
  t = Math.max(0, Math.min(1, t));
  const p = t * (stops.length - 1), i = Math.min(stops.length - 2, Math.floor(p)), f = p - i;
  const a = stops[i], b = stops[i + 1];
  return [a[0]+(b[0]-a[0])*f, a[1]+(b[1]-a[1])*f, a[2]+(b[2]-a[2])*f];
}
let pendingSpectrum = null, spectrumRaf = 0;
function pushSpectrum(db) {
  // Coalesce: draw at most once per animation frame, with the newest data.
  pendingSpectrum = db;
  if (!spectrumRaf) spectrumRaf = requestAnimationFrame(() => { spectrumRaf = 0; const d = pendingSpectrum; pendingSpectrum = null; if (d) drawSpectrum(d); });
}
function drawSpectrum(db) {
  const w = wf.width, h = wf.height, n = db.length, lo = wfCfg.min, hi = wfCfg.max;
  wctx.drawImage(wf, 0, 0, w, h - 1, 0, 1, w, h - 1);
  const row = wctx.createImageData(w, 1);
  for (let x = 0; x < w; x++) {
    const v = db[Math.floor((x / w) * n)];
    const [r, g, b] = colour((v - lo) / (hi - lo));
    const i = x * 4; row.data[i] = r; row.data[i+1] = g; row.data[i+2] = b; row.data[i+3] = 255;
  }
  wctx.putImageData(row, 0, 0);
  // spectrum line
  sp.style.display = wfCfg.line ? "" : "none";
  if (!wfCfg.line) return;
  const sw = sp.width, sh = sp.height;
  spctx.fillStyle = "#05090a"; spctx.fillRect(0, 0, sw, sh);
  spctx.strokeStyle = "rgba(46,120,112,.25)"; spctx.lineWidth = 1;
  for (let k = 1; k < 4; k++) { const y = (sh * k) / 4; spctx.beginPath(); spctx.moveTo(0, y); spctx.lineTo(sw, y); spctx.stroke(); }
  if (wfCfg.peak) { if (!peakHold || peakHold.length !== n) peakHold = db.slice(); else for (let i = 0; i < n; i++) peakHold[i] = Math.max(peakHold[i] - 0.05, db[i]); } else peakHold = null;
  const yOf = (v) => sh - ((v - lo) / (hi - lo)) * sh;
  if (peakHold) { spctx.strokeStyle = "rgba(245,181,68,.8)"; spctx.beginPath(); for (let x = 0; x < sw; x++) { const v = peakHold[Math.floor((x / sw) * n)]; x ? spctx.lineTo(x, yOf(v)) : spctx.moveTo(x, yOf(v)); } spctx.stroke(); }
  spctx.strokeStyle = "rgba(52,224,207,.95)"; spctx.lineWidth = 1.2; spctx.beginPath();
  for (let x = 0; x < sw; x++) { const v = db[Math.floor((x / sw) * n)]; x ? spctx.lineTo(x, yOf(v)) : spctx.moveTo(x, yOf(v)); }
  spctx.stroke();
  spctx.lineTo(sw, sh); spctx.lineTo(0, sh); spctx.closePath(); spctx.fillStyle = "rgba(52,224,207,.12)"; spctx.fill();
}
function wfApply() {
  $("wfFft").value = wfCfg.fft; $("wfAvg").value = wfCfg.avg; $("wfMap").value = wfCfg.map; $("wfMin").value = wfCfg.min; $("wfMax").value = wfCfg.max; $("wfLine").checked = wfCfg.line; $("wfPeak").checked = wfCfg.peak;
  sp.style.display = wfCfg.line ? "" : "none";
  save("hs.wf", wfCfg);
  if (TAURI) invoke("spectrum_set", { fft: +wfCfg.fft, average: +wfCfg.avg }).catch(() => {});
}
$("wfFft").onchange = () => { wfCfg.fft = +$("wfFft").value; wfApply(); };
$("wfAvg").onchange = () => { wfCfg.avg = +$("wfAvg").value; wfApply(); };
$("wfMap").onchange = () => { wfCfg.map = $("wfMap").value; wfApply(); };
$("wfMin").oninput = () => { wfCfg.min = Math.min(+$("wfMin").value, wfCfg.max - 10); };
$("wfMax").oninput = () => { wfCfg.max = Math.max(+$("wfMax").value, wfCfg.min + 10); };
$("wfMin").onchange = $("wfMax").onchange = wfApply;
$("wfLine").onchange = () => { wfCfg.line = $("wfLine").checked; wfApply(); };
$("wfPeak").onchange = () => { wfCfg.peak = $("wfPeak").checked; wfApply(); };
wfApply();

/* ---------- readouts shared by both modes ---------- */
let followVoice = 0;
function setStatus(s) {
  if (s.syncs != null) $("r-syncs").textContent = s.syncs;
  if (s.grants != null) $("r-grants").textContent = s.grants;
  if (s.voice_secs != null) $("r-voice").innerHTML = s.voice_secs.toFixed(1) + "<small>s</small>";
  if (s.modulation) $("tunedSub").textContent = s.modulation.toUpperCase();
  if (s.lock != null) {
    if (s.lock >= 0) { $("r-lock").textContent = s.lock.toFixed(2); $("r-lockbar").style.width = Math.max(0, Math.min(100, s.lock * 100)) + "%"; }
    else { $("r-lock").textContent = "—"; $("r-lockbar").style.width = "0%"; }
  }
  if (s.dropped != null) $("r-syncerr").textContent = s.dropped ? `${s.dropped}` : "0";
}

/* ---------- follow events (backend or demo) ---------- */
const evCounts = {};
function handleFollow(ev) {
  evCounts[ev.kind] = (evCounts[ev.kind] || 0) + 1;
  if (ev.kind !== "spectrum" && ev.kind !== "status") log(`follow ${ev.kind}: ${JSON.stringify(ev).slice(0, 160)}`);
  else if (evCounts[ev.kind] % 20 === 1) log(`follow ${ev.kind} #${evCounts[ev.kind]}`);
  switch (ev.kind) {
    case "measured":
      setState("following");
      logEvent(`control ${ev.control_mhz.toFixed(4)} MHz ${ev.modulation}, tuner ${ev.correction_hz >= 0 ? "+" : ""}${ev.correction_hz.toFixed(0)} Hz`);
      $("tunedHz").textContent = ev.control_mhz.toFixed(4);
      $("tunedSub").textContent = `${ev.modulation} · tuner ${ev.correction_hz >= 0 ? "+" : ""}${ev.correction_hz.toFixed(0)} Hz`;
      $("wfAxis").textContent = `${(ev.center_mhz ?? parseFreq($("center").value) / 1e6).toFixed(4)} MHz ± ${(ev.rate / 2e6).toFixed(2)} MHz`;
      if (ev.center_mhz != null) $("center").value = ev.center_mhz.toFixed(4) + "M";
      $("followMeta").textContent = "";
      activeRefresh();
      break;
    case "call_start":
      activeStart(ev);
      if (bells.has(ev.tg)) tone("bell");
      break;
    case "call":
      activeEnd(ev);
      followVoice += ev.secs;
      if (ev.emergency) { tone("emergency"); logEvent(`EMERGENCY · ${ev.name} · unit ${ev.unit_name || ev.source}`, "alarm"); }
      addCall({ tg: ev.tg, name: ev.name, source: ev.source, unit_name: ev.unit_name, freq_mhz: ev.freq_mhz, encrypted: false,
                secs: ev.secs, modulation: ev.modulation, wav: ev.wav, emergency: ev.emergency, patched_with: ev.patched_with, id: ev.id });
      if (typeof libLiveAdd === "function" && ev.id != null) libLiveAdd(ev.id);
      $("r-voice").innerHTML = followVoice.toFixed(1) + "<small>s</small>";
      break;
    case "site": {
      const nac = ev.nac != null ? "0x" + ev.nac.toString(16).toUpperCase().padStart(3, "0") : "—";
      const sys = ev.sys_id != null ? "0x" + ev.sys_id.toString(16).toUpperCase().padStart(3, "0") : "—";
      $("r-site").textContent = `${nac} · ${sys}`;
      $("siteSummary").textContent = `${ev.alternates_mhz.length} alt · ${ev.idens.length} plans · ${ev.patches.length} patches`;
      $("siteBody").textContent =
        `control   ${ev.control_mhz.toFixed(4)} MHz\n` +
        `NAC       ${nac}   system ${sys}   WACN ${ev.wacn != null ? "0x" + ev.wacn.toString(16).toUpperCase().padStart(5, "0") : "—"}\n` +
        `alternates ${ev.alternates_mhz.length ? ev.alternates_mhz.map((f) => f.toFixed(4)).join(", ") : "none announced"}\n` +
        `plans     ${ev.idens.length ? ev.idens.map(([id, b, sp]) => `#${id} ${b.toFixed(4)} MHz / ${sp.toFixed(2)} kHz`).join("; ") : "—"}\n` +
        `patches   ${ev.patches.length ? ev.patches.map(([sg, m]) => `${sg} ← ${m.join(",")}`).join("; ") : "none"}`;
      if (ev.patches.length) logEvent(`patches: ${ev.patches.map(([sg, m]) => `${sg}←${m.join(",")}`).join("; ")}`);
      break;
    }
    case "notice":
      $("followMeta").textContent = ev.text;
      logEvent(ev.text, /lost|moved|not followed|could not/.test(ev.text) ? "warn" : "");
      break;
    case "status":
      $("r-syncs").textContent = ev.control_syncs;
      $("r-grants").textContent = ev.calls;
      $("r-syncerr").textContent = ev.dropped ? `${ev.dropped}` : "0";
      $("r-stream").textContent = `${ev.msps.toFixed(2)}/${ev.want_msps.toFixed(2)}M · ${ev.dropped || 0}`;
      if (ev.locked) $("followMeta").textContent = `${ev.locked} locked-out call${ev.locked === 1 ? "" : "s"} skipped`;
      break;
    case "spectrum": pushSpectrum(ev.bins_db); break;
  }
}

/* ================================================================ */
if (TAURI) {
  listen("grant", (e) => addCall(e.payload));
  listen("status", (e) => setStatus(e.payload));
  listen("spectrum", (e) => pushSpectrum(e.payload.bins_db));
  listen("stopped", () => { setState("standby"); holdTg = null; updateHoldBtn(); invoke("set_hold", { tg: null }).catch(() => {}); });
  listen("error", (e) => { log(`backend error: ${e.payload}`); setState("standby"); alert("Capture error:\n" + e.payload); });
  listen("follow", (e) => handleFollow(e.payload));

  const opts = () => ({
    source: $("source").value,
    freq: parseFreq($("freq").value),
    rate: parseFloat($("rate").value),
    gain: $("gain").value.trim() === "" ? null : parseFloat($("gain").value),
    cqpsk: modSel === "cqpsk",
    eq: eqSel,
  });
  $("start").onclick = async () => {
    try {
      if (!Number.isFinite(parseFreq($("freq").value))) { alert("Enter a frequency like 851.5375M"); return; }
      if (modeSel === "follow" && !Number.isFinite(parseFreq($("center").value))) { alert("Enter a band centre like 855M"); return; }
      if (+$("rate").value < 1e6) { alert("The 48 kHz rate is for decoding files; pick 2.4 M (RTL-SDR) or 2.5 / 10 M (Airspy)."); return; }
      setState(modeSel === "follow" ? "measuring" : "capturing");
      log(`start: mode=${modeSel} source=${$("source").value} rate=${$("rate").value} freq=${$("freq").value} center=${$("center").value}`);
      followVoice = 0;
      if (modeSel === "follow") {
        const o = opts();
        $("followMeta").textContent = "measuring the control channel…";
        $("tunedHz").textContent = mhz(o.freq);
        const pl = playlists.find((p) => p.id === $("playlist").value);
        save("hs.prefs", { ...store("hs.prefs", {}), lastPlaylist: $("playlist").value });
        await invoke("start_follow", { source: o.source, freq: parseFreq($("center").value), rate: o.rate, gain: o.gain,
          control: o.freq, callsDir: $("callsdir").value.trim() || null, play: $("play").checked,
          hangMs: parseInt($("hangMs").value, 10) || null, systemName: pl ? pl.system_name : null });
      } else {
        $("tunedHz").textContent = mhz(opts().freq);
        $("wfAxis").textContent = `${(opts().freq / 1e6).toFixed(4)} MHz ± ${(opts().rate / 2e6).toFixed(2)} MHz`;
        await invoke("start_capture", { ...opts(), recordIq: $("reciq").value.trim() || null, recordLog: $("reclog").value.trim() || null });
      }
    } catch (err) { setState("standby"); alert(err); }
  };
  $("stop").onclick = () => invoke("stop_capture").catch((e) => alert(e));
  $("loadcat").onclick = async () => {
    const path = $("catalog").value.trim(); if (!path) return;
    try { const n = await invoke("load_catalog", { path }); $("loadcat").textContent = n + " TGs"; alert(`Loaded. ${n} talkgroups are now named.`); if (typeof aliasesOnShow === "function") aliasesOnShow(); } catch (err) { alert(err); }
  };
  $("decode").onclick = async () => {
    const path = $("decfile").value.trim(); if (!path) return;
    try { setState("decoding"); await invoke("decode_file", { path, rate: parseFloat($("rate").value), cqpsk: modSel === "cqpsk", eq: eqSel }); }
    catch (err) { alert(err); } finally { setState("standby"); }
  };
  pushLockout(); pushPriorities();
  $("skipBtn").onclick = () => invoke("skip_call").catch((e) => alert(e));
  $("replayBtn").onclick = () => invoke("replay_last").catch((e) => alert(e));
  setInterval(async () => { try { const n = await invoke("audio_queued"); $("queueMeta").textContent = n ? `${n} queued` : ""; } catch (_) {} }, 1000);

  /* ---------- settings persisted locally ---------- */
  const prefs = store("hs.prefs", {});
  $("hangMs").value = prefs.hangMs ?? ""; $("avoidMin").value = prefs.avoidMin ?? "60";
  $("autostart").checked = !!prefs.autostart; $("tones").checked = prefs.tones !== false;
  $("callsdir").value = prefs.callsdir ?? ""; $("play").checked = prefs.play !== false;
  const savePrefs = () => save("hs.prefs", { hangMs: $("hangMs").value, avoidMin: $("avoidMin").value, autostart: $("autostart").checked,
    tones: $("tones").checked, callsdir: $("callsdir").value, play: $("play").checked, lastPlaylist: $("playlist").value });
  ["hangMs", "avoidMin", "autostart", "tones", "callsdir", "play"].forEach((id) => $(id).onchange = savePrefs);

  /* ---------- radio-ID aliases ---------- */
  async function renderUnits() {
    try {
      const u = await invoke("units_list");
      $("unitsMeta").textContent = u.length ? `${u.length} named` : "";
      $("unitsBody").innerHTML = u.map((r) => `<tr><td class="mono">${r.id}</td><td>${esc(r.name)}</td></tr>`).join("");
    } catch (e) { log(`units: ${e}`); }
  }
  $("unitSave").onclick = async () => {
    const id = parseInt($("unitId").value, 10); if (!Number.isFinite(id)) return;
    try { await invoke("unit_set", { id, name: $("unitName").value }); $("unitId").value = ""; $("unitName").value = ""; renderUnits(); } catch (e) { alert(e); }
  };
  $("unitsImport").onclick = async () => {
    const path = $("unitsCsv").value.trim(); if (!path) return;
    try { const n = await invoke("units_import", { path }); $("unitsMeta").textContent = `${n} named`; renderUnits(); } catch (e) { alert(e); }
  };
  renderUnits();

  /* ---------- library: search, listen, detail, export ---------- */
  let libRows = [], libSel = null, listening = false, listenQueue = [], listenIdx = 0, listenTimer = null;
  const fmtT = (epoch) => new Date(epoch * 1000).toLocaleString("en-US", { hour12: false, month: "short", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit" });
  const localToEpoch = (v) => v ? Math.floor(new Date(v).getTime() / 1000) : null;
  function libQuery(extra) {
    return { text: $("qText").value.trim() || null, from: localToEpoch($("qFrom").value), to: localToEpoch($("qTo").value),
      tg: parseInt($("qTg").value, 10) || null, unit: parseInt($("qUnit").value, 10) || null,
      starred: $("qStar").checked || null, emergency: $("qEmg").checked || null, with_audio: $("qAudio").checked || null, limit: 200, ...extra };
  }
  function libRender(rows, append) {
    if (!append) { libRows = []; $("libBody").innerHTML = ""; }
    libRows = append ? libRows.concat(rows) : rows;
    const html = rows.map(libRowHtml).join("");
    if (append) $("libBody").insertAdjacentHTML("beforeend", html); else $("libBody").innerHTML = html;
    $("libEmpty").style.display = libRows.length ? "none" : "";
    $("libMeta").textContent = libRows.length ? `${libRows.length} calls` : "";
    wireLibRows();
  }
  function libRowHtml(r) {
    const t = r.transcript_edited || r.transcript || "";
    return `<tr data-id="${r.id}" class="${libSel === r.id ? "sel" : ""}"><td><input type="checkbox" data-sel="${r.id}" ${cart.has(r.id) ? "checked" : ""}></td>` +
      `<td class="time">${fmtT(r.start)}</td><td class="tg">${esc(r.tg_name)}<span class="num">TG ${r.tg}${r.emergency ? " · EMERGENCY" : ""}</span></td>` +
      `<td class="src">${esc(r.unit_name || r.unit || "—")}</td><td class="len">${r.secs.toFixed(1)}s</td>` +
      `<td class="tr ${r.transcript_edited ? "edited" : ""}" title="${esc(t)}">${esc(t) || (r.audio ? '<span class="faint">not transcribed</span>' : '<span class="faint">no audio</span>')}</td>` +
      `<td class="act">${r.audio ? `<button title="Play" data-lplay="${r.id}">▶</button>` : ""}<button title="Star" data-lstar="${r.id}" class="${r.starred ? "pri-h" : ""}">★</button>` +
      `<button title="Transcribe now" data-ltr="${r.id}">T</button></td></tr>`;
  }
  function libPrependHtml(r) { $("libBody").insertAdjacentHTML("afterbegin", libRowHtml(r)); libRows.unshift(r); $("libEmpty").style.display = "none"; wireLibRows(); }
  function wireLibRows() {
    $("libBody").querySelectorAll("tr[data-id]").forEach((tr) => tr.onclick = (e) => { if (e.target.closest("button,input")) return; libSelect(+tr.dataset.id); });
    $("libBody").querySelectorAll("input[data-sel]").forEach((c) => c.onchange = () => { const r = libRows.find((x) => x.id === +c.dataset.sel); cartToggle(r.id, `${fmtT(r.start)} ${r.tg_name} · ${r.secs.toFixed(1)}s`); });
    $("libBody").querySelectorAll("button[data-lplay]").forEach((b) => b.onclick = () => invoke("library_play", { id: +b.dataset.lplay }).catch((e) => alert(e)));
    $("libBody").querySelectorAll("button[data-lstar]").forEach((b) => b.onclick = async () => { const r = libRows.find((x) => x.id === +b.dataset.lstar); r.starred = !r.starred; try { await invoke("library_star", { id: r.id, on: r.starred }); b.classList.toggle("pri-h", r.starred); } catch (e) { alert(e); } });
    $("libBody").querySelectorAll("button[data-ltr]").forEach((b) => b.onclick = () => { b.textContent = "…"; invoke("transcribe_call", { id: +b.dataset.ltr }).catch((e) => { b.textContent = "T"; alert(e); }); });
  }
  async function libSearch(append) {
    try {
      const q = libQuery(append && libRows.length ? { before_id: libRows[libRows.length - 1].id } : {});
      const rows = await invoke("library_search", { query: q });
      libRender(rows, append);
    } catch (e) { alert(e); }
  }
  $("qGo").onclick = () => libSearch(false);
  $("qText").onkeydown = (e) => { if (e.key === "Enter") libSearch(false); };
  ["qFrom", "qTo", "qTg", "qUnit", "qStar", "qEmg", "qAudio"].forEach((id) => $(id).onchange = () => libSearch(false));
  $("qMore").onclick = () => libSearch(true);
  $("qAllCart").onclick = () => { libRows.forEach((r) => cart.set(r.id, `${fmtT(r.start)} ${r.tg_name} · ${r.secs.toFixed(1)}s`)); cartSave(); $("libBody").querySelectorAll("input[data-sel]").forEach((c) => c.checked = true); };
  let libShown = false;
  window.libOnShow = () => { if (!libShown) { libShown = true; libSearch(false); } };
  // New live calls appear at the top when no filter narrows them out.
  window.libLiveAdd = async (id) => {
    if (!libShown) return;
    try {
      const r = await invoke("library_get", { id }); if (!r) return;
      const q = libQuery({});
      const ok = (!q.tg || r.tg === q.tg) && (!q.unit || r.unit === q.unit) && (!q.starred || r.starred) && (!q.emergency || r.emergency) && (!q.with_audio || r.audio) && (!q.to || r.start <= q.to) && (!q.from || r.start >= q.from) && !q.text;
      if (!ok) return;
      libPrependHtml(r); if (listening && $("qLive").checked) listenQueue.push(r.id);
    } catch (_) {}
  };
  listen("transcript", (e) => {
    const { id, text } = e.payload;
    const r = libRows.find((x) => x.id === id); if (r) r.transcript = text;
    const tr = $("libBody").querySelector(`tr[data-id="${id}"]`);
    if (tr) { const td = tr.querySelector("td.tr"); if (td && !(r && r.transcript_edited)) { td.textContent = text; td.title = text; } const b = tr.querySelector("button[data-ltr]"); if (b) b.textContent = "T"; }
    if (libSel === id) { const m = document.querySelector("#detBody .machine"); if (m) m.textContent = text; }
  });
  listen("transcribe_error", (e) => logEvent(`transcription: ${e.payload}`, "warn"));
  listen("transcribe_ready", (e) => logEvent(`transcriber ready: ${e.payload}`));

  async function libSelect(id) {
    libSel = id;
    $("libBody").querySelectorAll("tr[data-id]").forEach((tr) => tr.classList.toggle("sel", +tr.dataset.id === id));
    try {
      const r = await invoke("library_get", { id }); if (!r) return;
      $("detMeta").textContent = `#${r.id} · ${r.sha256 ? "sha256 " + r.sha256.slice(0, 12) + "…" : "no audio"}`;
      $("detBody").innerHTML = `<div class="det">
        <div><b>${esc(r.tg_name)}</b> <span class="faint">TG ${r.tg}</span> · unit ${r.unit_name ? esc(r.unit_name) + " (" + r.unit + ")" : r.unit} · ${(r.freq_hz / 1e6).toFixed(4)} MHz · ${r.modulation} · ${r.secs.toFixed(1)}s${r.emergency ? ' · <span class="badge emg">EMERGENCY</span>' : ""}</div>
        <div class="faint">${fmtT(r.start)} · ${esc(r.system || "")} ${r.patched_with.length ? "· patched " + r.patched_with.join(",") : ""}</div>
        <div class="xport" style="margin:8px 0">${r.audio ? `<button class="btn sm" id="detPlay">▶ Play</button>` : ""}<button class="btn sm" id="detCart">${cart.has(r.id) ? "Remove from cart" : "Add to cart"}</button><button class="btn sm" id="detTr">Transcribe${r.transcript ? " again" : ""}</button>${r.audio ? `<button class="btn sm" id="detUp" title="Send to the enabled sharing services">Upload</button>` : ""}</div>
        <div class="k">Machine transcript ${r.transcript_model ? "· " + r.transcript_model : ""}</div>
        <div class="machine">${esc(r.transcript || "—")}</div>
        <div class="k">Edited transcript (kept separately; the machine text above is never changed)</div>
        <textarea id="detEdit" placeholder="Type a corrected transcript…">${esc(r.transcript_edited || "")}</textarea>
        <div class="xport" style="margin-top:6px"><button class="btn primary sm" id="detSave">Save edit</button><button class="btn ghost sm" id="detClearEdit">Clear edit</button><span class="meta" id="detSaved">${r.edited_at ? "edited " + fmtT(r.edited_at) : ""}</span></div>
      </div>`;
      const play = $("detPlay"); if (play) play.onclick = () => invoke("library_play", { id }).catch((e) => alert(e));
      $("detCart").onclick = () => { cartToggle(r.id, `${fmtT(r.start)} ${r.tg_name} · ${r.secs.toFixed(1)}s`); libSelect(id); };
      $("detTr").onclick = () => invoke("transcribe_call", { id }).then(() => $("detSaved").textContent = "transcribing…").catch((e) => alert(e));
      const up = $("detUp"); if (up) up.onclick = () => invoke("upload_call", { id }).then(() => $("detSaved").textContent = "queued for upload").catch((e) => alert(e));
      $("detSave").onclick = async () => { try { await invoke("library_set_edited", { id, text: $("detEdit").value }); $("detSaved").textContent = "saved"; libSearchRefreshRow(id); } catch (e) { alert(e); } };
      $("detClearEdit").onclick = async () => { $("detEdit").value = ""; await invoke("library_set_edited", { id, text: "" }); $("detSaved").textContent = "edit cleared"; libSearchRefreshRow(id); };
    } catch (e) { alert(e); }
  }
  async function libSearchRefreshRow(id) {
    const r = await invoke("library_get", { id }); const i = libRows.findIndex((x) => x.id === id);
    if (r && i >= 0) { libRows[i] = r; const tr = $("libBody").querySelector(`tr[data-id="${id}"]`); if (tr) { tr.outerHTML = libRowHtml(r); wireLibRows(); } }
  }

  /* listen mode: play results oldest → newest through the speaker, live calls muted meanwhile */
  $("listenBtn").onclick = async () => {
    if (!libRows.length) { alert("Search first, then listen."); return; }
    listening = true; listenQueue = libRows.map((r) => r.id).filter((id) => libRows.find((r) => r.id === id).audio).reverse(); listenIdx = 0;
    $("listenBtn").disabled = true; $("listenStop").disabled = false;
    await invoke("set_archive_mode", { on: true }).catch((e) => alert(e));
    listenNext();
  };
  $("listenStop").onclick = async () => { listening = false; clearTimeout(listenTimer); $("listenBtn").disabled = false; $("listenStop").disabled = true; $("listenMeta").textContent = ""; $("libBody").querySelectorAll("tr.playing").forEach((t) => t.classList.remove("playing")); try { await invoke("set_archive_mode", { on: false }); } catch (e) { log(`archive off: ${e}`); } };
  async function listenNext() {
    if (!listening) return;
    if (listenIdx >= listenQueue.length) {
      if (!$("qLive").checked) { $("listenStop").onclick(); $("listenMeta").textContent = "done — live audio resumed"; return; }
      $("listenMeta").textContent = "waiting for new calls…"; listenTimer = setTimeout(listenNext, 1500); return;
    }
    const id = listenQueue[listenIdx++]; const r = libRows.find((x) => x.id === id);
    $("libBody").querySelectorAll("tr.playing").forEach((t) => t.classList.remove("playing"));
    const tr = $("libBody").querySelector(`tr[data-id="${id}"]`); if (tr) { tr.classList.add("playing"); tr.scrollIntoView({ block: "nearest" }); }
    $("listenMeta").textContent = `${listenIdx}/${listenQueue.length} · ${r ? r.tg_name : ""}`;
    try { await invoke("library_play", { id }); } catch (_) {}
    listenTimer = setTimeout(listenNext, ((r ? r.secs : 3) + 0.4) * 1000);
  }

  /* export */
  $("exportBtn").onclick = async () => {
    const dest = $("exportDir").value.trim(); if (!dest) { alert("Choose a destination folder."); return; }
    if (!cart.size) { alert("The cart is empty."); return; }
    try { const m = await invoke("library_export", { ids: [...cart.keys()], dest }); $("exportResult").textContent = `exported ${cart.size} calls → ${m}`; }
    catch (e) { alert(e); }
  };

  /* settings: transcription + library */
  async function trRefresh() {
    try {
      const p = await invoke("transcribe_probe");
      $("trEnabled").checked = p.settings.enabled; $("trEngine").value = p.settings.engine; $("trModel").value = p.settings.model; $("trLang").value = p.settings.language; $("trDevice").value = p.settings.device;
      [...$("trEngine").options].forEach((o) => { o.disabled = !p.engines.includes(o.value); o.textContent = o.value + (p.engines.includes(o.value) ? "" : " (not installed)"); });
      $("trMeta").textContent = p.engines.length ? (p.running_model ? `running ${p.running_model}` : `available: ${p.engines.join(", ")}`) : "no whisper found — see below";
      if (p.last_error) $("trMeta").textContent = `error: ${p.last_error}`;
    } catch (e) { log(`transcribe_probe: ${e}`); }
  }
  $("trSave").onclick = async () => {
    try { await invoke("transcribe_configure", { settings: { enabled: $("trEnabled").checked, engine: $("trEngine").value, model: $("trModel").value, language: $("trLang").value.trim() || "en", device: $("trDevice").value } }); $("trMeta").textContent = "saved"; setTimeout(trRefresh, 800); }
    catch (e) { alert(e); }
  };
  $("trEnabled").onchange = $("trSave").onclick;
  async function libStatsRefresh() {
    try { const [n, secs, tr, dir] = await invoke("library_stats"); $("libStats").textContent = `${n} calls · ${(secs / 60).toFixed(0)} min · ${tr} transcribed`; $("libDir").textContent = dir; } catch (e) { log(`library_stats: ${e}`); }
  }
  $("pruneNow").onclick = async () => {
    const d = parseInt($("pruneDays").value, 10); if (!Number.isFinite(d)) { alert("Enter a number of days."); return; }
    if (!confirm(`Delete unstarred calls older than ${d} days?`)) return;
    try { const n = await invoke("library_prune", { days: d }); alert(`${n} calls deleted`); libStatsRefresh(); } catch (e) { alert(e); }
  };
  trRefresh(); libStatsRefresh();

  /* ---------- aliases tab ---------- */
  let alRows = [];
  function alRowHtml(r) {
    const p = prio.get(r.id) || 50;
    return `<tr class="${r.encrypted ? "enc" : ""}"><td class="mono">${r.id}</td><td>${r.alias}</td><td>${r.description}</td><td><small>${r.category}</small></td><td><small class="mono">${r.source}</small></td>` +
      `<td class="act"><button data-pri="${r.id}" class="${p === 10 ? "pri-h" : p === 90 ? "pri-l" : ""}">${p === 10 ? "★" : p === 90 ? "▽" : "☆"}</button>` +
      `<button data-bell="${r.id}" class="${bells.has(r.id) ? "bell" : ""}">🔔</button><button data-avoid="${r.id}" class="${avoidUntil.has(r.id) ? "on" : ""}">⏱</button><button data-lock="${r.id}" class="${lockout.has(r.id) ? "on" : ""}">⊘</button></td></tr>`;
  }
  function alRender() {
    const q = $("alFilter").value.trim().toLowerCase();
    const shown = q ? alRows.filter((r) => `${r.id} ${r.alias} ${r.description} ${r.category} ${r.source}`.toLowerCase().includes(q)) : alRows;
    $("alBody").innerHTML = shown.slice(0, 2000).map(alRowHtml).join("");
    $("alEmpty").style.display = alRows.length ? "none" : "";
    $("alMeta").textContent = alRows.length ? (q ? `${shown.length} of ${alRows.length}` : `${alRows.length} talkgroups`) : "";
    const tb = $("alBody");
    tb.querySelectorAll("button[data-pri]").forEach((b) => b.onclick = () => { cyclePriority(+b.dataset.pri); alRender(); });
    tb.querySelectorAll("button[data-bell]").forEach((b) => b.onclick = () => { toggleBell(+b.dataset.bell); alRender(); });
    tb.querySelectorAll("button[data-avoid]").forEach((b) => b.onclick = () => { avoidFor(+b.dataset.avoid); alRender(); });
    tb.querySelectorAll("button[data-lock]").forEach((b) => b.onclick = () => { toggleLock(+b.dataset.lock); alRender(); });
  }
  $("alFilter").oninput = alRender;
  async function srcRender() {
    try {
      const list = await invoke("catalogs_list");
      $("srcMeta").textContent = list.length ? `${list.length} source${list.length === 1 ? "" : "s"}` : "none";
      $("srcList").innerHTML = list.length ? list.map((s) => `<div class="row"><span class="grow"><b>${esc(s.name.replace(/^rr_/, "RadioReference sid ").replace(/^csv_/, "CSV: "))}</b><br><small>${s.talkgroups} talkgroups</small></span><button class="btn ghost" data-rmsrc="${esc(s.name)}">Remove</button></div>`).join("")
        : '<div class="row"><span class="grow" style="color:var(--ink-faint)">Nothing loaded yet.</span></div>';
      $("srcList").querySelectorAll("[data-rmsrc]").forEach((b) => b.onclick = async () => { if (!confirm(`Remove ${b.dataset.rmsrc}?`)) return; try { const n = await invoke("catalog_remove", { name: b.dataset.rmsrc }); $("loadcat").textContent = n ? n + " TGs" : "Load"; aliasesRefresh(); } catch (e) { alert(e); } });
    } catch (e) { log(`catalogs_list: ${e}`); }
  }
  async function aliasesRefresh() {
    try { alRows = await invoke("catalog_rows"); alRender(); srcRender(); $("r-names").textContent = new Set(alRows.map((r) => r.id)).size || "—"; } catch (e) { log(`catalog_rows: ${e}`); }
  }
  window.aliasesOnShow = aliasesRefresh;
  $("alCheckGo").onclick = async () => {
    const tg = parseInt($("alCheck").value, 10); if (!Number.isFinite(tg)) return;
    try {
      const hits = await invoke("catalog_lookup", { tg });
      $("alCheckResult").textContent = hits.length ? `✔ TG ${tg} = “${hits[hits.length - 1].alias}” (${hits[hits.length - 1].description || hits[hits.length - 1].category}) — from ${hits.map((h) => h.source).join(", ")}` : `✘ TG ${tg} is not in any loaded catalog`;
      $("alCheckResult").style.color = hits.length ? "var(--clear)" : "var(--enc)";
    } catch (e) { alert(e); }
  };
  $("alCheck").onkeydown = (e) => { if (e.key === "Enter") $("alCheckGo").onclick(); };
  aliasesRefresh();

  /* ---------- whisper models: what's downloaded, download ahead of time ---------- */
  const MODEL_SIZES = { tiny: "75 MB", base: "145 MB", small: "480 MB", medium: "1.5 GB", "large-v3": "3 GB", "distil-large-v3": "1.5 GB", turbo: "1.6 GB" };
  const downloading = new Set();
  async function trModelsRender() {
    try {
      const rows = await invoke("transcribe_models"); const names = [...new Set(rows.map((r) => r.model))];
      $("trModels").innerHTML = names.map((m) => {
        const cell = (eng) => { const r = rows.find((x) => x.model === m && x.engine === eng); const key = `${eng}/${m}`;
          return r && r.downloaded ? `<span class="badge clear">✔ downloaded</span>` : downloading.has(key) ? `<span class="meta">downloading…</span>` : `<button class="btn ghost sm" data-dl="${key}">Download</button>`; };
        return `<tr><td class="mono">${m} <small class="faint">${MODEL_SIZES[m] || ""}</small></td><td>${cell("faster-whisper")}</td><td>${cell("openai-whisper")}</td></tr>`;
      }).join("");
      $("trModels").querySelectorAll("[data-dl]").forEach((b) => b.onclick = () => { const [engine, model] = b.dataset.dl.split("/"); downloading.add(b.dataset.dl); trModelsRender(); invoke("transcribe_download", { engine, model }).catch((e) => { downloading.delete(b.dataset.dl); alert(e); trModelsRender(); }); });
    } catch (e) { log(`transcribe_models: ${e}`); }
  }
  listen("transcribe_download", (e) => {
    const { engine, model, state, detail } = e.payload; const key = `${engine}/${model}`;
    if (state !== "started") downloading.delete(key);
    if (state === "error") alert(`Model download failed (${key}): ${detail}`);
    if (state === "done") logEvent(`model ready: ${key}`);
    trModelsRender();
  });
  trModelsRender();

  /* ---------- stored audio format ---------- */
  async function fmtRefresh() {
    try {
      const f = await invoke("format_get");
      $("fmtCodec").value = f.format.codec; $("fmtKbps").value = String(f.format.bitrate_kbps); $("fmtMode").value = f.format.mode;
      $("fmtMeta").textContent = f.ffmpeg ? f.ffmpeg.replace(/ Copyright.*/, "") : "ffmpeg not found — WAV only";
      [...$("fmtCodec").options].forEach((o) => { if (o.value !== "wav") o.disabled = !f.ffmpeg; });
    } catch (e) { log(`format_get: ${e}`); }
  }
  const fmtSave = async () => {
    try { await invoke("format_set", { format: { codec: $("fmtCodec").value, bitrate_kbps: +$("fmtKbps").value, mode: $("fmtMode").value } }); $("fmtMeta").textContent = "saved"; setTimeout(fmtRefresh, 700); }
    catch (e) { alert(e); fmtRefresh(); }
  };
  $("fmtCodec").onchange = $("fmtKbps").onchange = $("fmtMode").onchange = fmtSave;
  fmtRefresh();

  /* ---------- call sharing ---------- */
  const upSettings = () => ({
    rdio: { enabled: $("upRdio").checked, url: $("upRdioUrl").value.trim(), key: $("upRdioKey").value, system: parseInt($("upRdioSys").value, 10) || 0, system_label: $("upRdioLabel").value.trim() },
    openmhz: { enabled: $("upOmhz").checked, url: $("upOmhzUrl").value.trim() || "https://api.openmhz.com", short_name: $("upOmhzName").value.trim(), api_key: $("upOmhzKey").value },
    broadcastify: { enabled: $("upBcfy").checked, api_key: $("upBcfyKey").value, system_id: parseInt($("upBcfySys").value, 10) || 0, format: $("upBcfyFmt").value },
    min_secs: parseFloat($("upMin").value) || 0,
  });
  async function upRefresh(fields = true) {
    try {
      const v = await invoke("uploads_get"); const s = v.settings;
      if (fields) {
      $("upRdio").checked = s.rdio.enabled; $("upRdioUrl").value = s.rdio.url; $("upRdioKey").value = s.rdio.key; $("upRdioSys").value = s.rdio.system || ""; $("upRdioLabel").value = s.rdio.system_label;
      $("upOmhz").checked = s.openmhz.enabled; $("upOmhzUrl").value = s.openmhz.url; $("upOmhzName").value = s.openmhz.short_name; $("upOmhzKey").value = s.openmhz.api_key;
      $("upBcfy").checked = s.broadcastify.enabled; $("upBcfyKey").value = s.broadcastify.api_key; $("upBcfySys").value = s.broadcastify.system_id || ""; $("upBcfyFmt").value = s.broadcastify.format || "m4a";
      $("upMin").value = s.min_secs;
      }
      const st = v.status;
      $("upMeta").textContent = st.last_error ? `error: ${st.last_error}` : st.sent || st.queued ? `${st.sent} sent · ${st.failed} failed · ${st.queued} queued` : (!v.ffmpeg ? "ffmpeg not found — OpenMHz/Broadcastify need it" : "");
      $("upMeta").style.color = st.last_error ? "var(--enc)" : "";
    } catch (e) { log(`uploads_get: ${e}`); }
  }
  $("upSave").onclick = async () => { try { await invoke("uploads_configure", { settings: upSettings() }); $("upMeta").textContent = "saved"; setTimeout(upRefresh, 800); } catch (e) { alert(e); } };
  document.querySelectorAll("[data-uptest]").forEach((b) => b.onclick = async () => {
    b.disabled = true; try { alert(await invoke("uploads_test", { service: b.dataset.uptest, settings: upSettings() })); } catch (e) { alert(`Test failed: ${e}`); } finally { b.disabled = false; }
  });
  upRefresh(); setInterval(() => { if ($("view-settings").style.display !== "none") upRefresh(false); }, 5000);

  /* ---------- live feed ---------- */
  async function stRefresh(fields = true) {
    try {
      const v = await invoke("stream_get"); const s = v.settings;
      if (fields) {
      $("stEnabled").checked = s.enabled; $("stHost").value = s.host; $("stPort").value = s.port; $("stMount").value = s.mount; $("stUser").value = s.user;
      $("stPass").value = s.password; $("stTls").checked = s.tls; $("stCodec").value = s.codec; $("stKbps").value = String(s.bitrate_kbps); $("stName").value = s.name; $("stDesc").value = s.description;
      }
      $("stMeta").textContent = !v.ffmpeg ? "ffmpeg not found" : v.status.last_error ? `error: ${v.status.last_error}` : v.status.running ? (v.status.connected ? `streaming · ${(v.status.bytes_sent / 1024).toFixed(0)} KB sent` : "connecting…") : "off";
      $("stMeta").style.color = v.status.last_error ? "var(--enc)" : v.status.connected ? "var(--clear)" : "";
    } catch (e) { log(`stream_get: ${e}`); }
  }
  $("stSave").onclick = async () => {
    try {
      await invoke("stream_configure", { settings: { enabled: $("stEnabled").checked, host: $("stHost").value.trim(), port: parseInt($("stPort").value, 10) || 80, mount: $("stMount").value.trim(),
        user: $("stUser").value.trim() || "source", password: $("stPass").value, codec: $("stCodec").value, bitrate_kbps: +$("stKbps").value, name: $("stName").value, description: $("stDesc").value, tls: $("stTls").checked } });
      setTimeout(stRefresh, 1500);
    } catch (e) { alert(e); }
  };
  stRefresh(); setInterval(() => { if ($("view-settings").style.display !== "none") stRefresh(false); }, 5000);

  /* ---------- RadioReference account ---------- */
  async function rrRefresh() {
    try {
      const st = await invoke("rr_settings");
      $("rrUser").value = st.username || "";
      if (st.sid && !$("rrSid").value) $("rrSid").value = st.sid;
      $("rrPass").placeholder = st.has_password ? "saved in Keychain" : "";
      $("rrKeyField").style.display = st.embedded_key ? "none" : "";
      $("rrKey").placeholder = st.has_app_key ? "saved in Keychain" : "";
      const missing = [];
      if (!st.has_app_key) missing.push("app key");
      if (!st.username) missing.push("username");
      if (st.username && !st.has_password) missing.push("password");
      $("rrMeta").textContent = missing.length ? `missing: ${missing.join(", ")}`
        : st.catalog_len ? `${st.catalog_len} talkgroups loaded${st.system_name ? " · " + st.system_name : ""}` : "signed in";
      $("rrMeta").style.color = missing.length ? "var(--enc)" : "";
      if (st.catalog_len) $("loadcat").textContent = st.catalog_len + " TGs";
      return st;
    } catch (e) { log(`rr_settings: ${e}`); }
  }
  async function rrSaveCreds() {
    await invoke("rr_save", { appKey: $("rrKey").value, username: $("rrUser").value, password: $("rrPass").value, sid: sidVal() });
    $("rrPass").value = ""; $("rrKey").value = "";
  }
  const sidVal = () => { const m = String($("rrSid").value).match(/(\d+)\s*$/); const v = m ? parseInt(m[1], 10) : NaN; return Number.isFinite(v) ? v : null; };
  $("rrSave").onclick = async () => { try { await rrSaveCreds(); $("rrMeta").textContent = "saved"; await rrRefresh(); } catch (e) { alert(e); } };

  /* ---------- find a system ---------- */
  let statesLoaded = false;
  async function loadStates() {
    if (statesLoaded) return;
    const st = await invoke("rr_states", {});
    $("bState").innerHTML = '<option value="">—</option>' + st.map((s) => `<option value="${s.stid}">${esc(s.name)}</option>`).join("");
    statesLoaded = true;
  }
  function renderSystems(list, label) {
    $("findMeta").textContent = label || "";
    $("sysList").innerHTML = list.length ? list.map((s) =>
      `<div class="row" data-sid="${s.sid}"><span class="grow">${esc(s.name)}${s.city ? ` <small>· ${esc(s.city)}</small>` : ""}</span><span class="mono">sid ${s.sid}</span></div>`).join("")
      : '<div class="row"><span class="grow" style="color:var(--ink-faint)">No trunked systems listed here.</span></div>';
    $("sysList").querySelectorAll(".row[data-sid]").forEach((r) => r.onclick = () => { $("rrSid").value = r.dataset.sid; loadSystem(+r.dataset.sid); });
  }
  $("bState").onchange = async () => {
    const stid = +$("bState").value; $("bCounty").innerHTML = '<option value="">— statewide —</option>'; if (!stid) return;
    try { $("findMeta").textContent = "loading…"; const v = await invoke("rr_state", { stid });
      $("bCounty").innerHTML += v.counties.map((c) => `<option value="${c.ctid}">${esc(c.name)}</option>`).join("");
      renderSystems(v.systems, `${v.systems.length} statewide system${v.systems.length === 1 ? "" : "s"}`);
    } catch (e) { $("findMeta").textContent = ""; alert(e); }
  };
  $("bCounty").onchange = async () => {
    const ctid = +$("bCounty").value; if (!ctid) { $("bState").onchange(); return; }
    try { $("findMeta").textContent = "loading…"; const v = await invoke("rr_county", { ctid });
      renderSystems(v, `${v.length} system${v.length === 1 ? "" : "s"} in this county`);
    } catch (e) { $("findMeta").textContent = ""; alert(e); }
  };
  $("bZipGo").onclick = async () => {
    const zip = parseInt($("bZip").value, 10); if (!Number.isFinite(zip)) return;
    try { $("findMeta").textContent = "looking up ZIP…"; await loadStates();
      const z = await invoke("rr_zip", { zip });
      $("bState").value = String(z.stid); await $("bState").onchange();
      $("bCounty").value = String(z.ctid); await $("bCounty").onchange();
      if (z.city) $("findMeta").textContent = `${z.city} · ` + $("findMeta").textContent;
    } catch (e) { $("findMeta").textContent = ""; alert(e); }
  };
  $("bState").onfocus = () => loadStates().catch((e) => alert(e));

  /* ---------- a loaded system → playlist ---------- */
  let sys = null, pickedSite = null, picked = new Set();
  async function loadSystem(sid) {
    try {
      $("rrDownload").disabled = true; $("findMeta").textContent = "downloading system…";
      if ($("rrPass").value || $("rrKey").value) await rrSaveCreds();
      sys = await invoke("rr_download", { sid });
      $("findMeta").textContent = "";
      $("sysPanel").style.display = "";
      $("sysName").textContent = sys.name;
      $("sysMeta").textContent = `sid ${sys.sid} · ${sys.talkgroups} talkgroups · ${sys.sites.length} sites`;
      $("loadcat").textContent = sys.talkgroups + " TGs";
      pickedSite = sys.sites[0] || null; picked = new Set();
      renderSites(); renderCats(); renderTgs();
      $("plName").value = sys.name;
      $("sysLoaded").textContent = `✔ Loaded ${sys.talkgroups} talkgroups from ${sys.name} — they now name calls; see the Aliases tab to check any talkgroup.`;
      logEvent(`loaded ${sys.talkgroups} talkgroups from ${sys.name}`);
      rrRefresh(); aliasesRefresh();
      $("sysPanel").scrollIntoView({ behavior: "smooth", block: "start" });
    } catch (e) { $("findMeta").textContent = ""; alert(e); }
    finally { $("rrDownload").disabled = false; }
  }
  $("rrDownload").onclick = () => { const sid = sidVal(); if (sid == null) { alert("Enter a system ID."); return; } loadSystem(sid); };
  const siteRate = (s) => ((s.span_mhz ? s.span_mhz[1] - s.span_mhz[0] : 0) <= 1.9 ? 2500000 : 10000000);
  function renderSites() {
    $("siteList").innerHTML = sys.sites.map((s) =>
      `<div class="row ${pickedSite && s.site_id === pickedSite.site_id ? "on" : ""}" data-site="${s.site_id}">` +
      `<span class="grow"><b>${s.site_id}</b> ${esc(s.name)}${s.tdma_control ? ' <small style="color:var(--enc)">TDMA CC — not decodable yet</small>' : ""}</span>` +
      (s.nac != null ? `<span class="mono">NAC 0x${s.nac.toString(16).toUpperCase().padStart(3, "0")}</span>` : "") +
      `<span class="mono">${s.control_mhz[0].toFixed(4)} MHz</span>` +
      (s.span_mhz ? `<span class="mono">${(s.span_mhz[1] - s.span_mhz[0]).toFixed(2)} MHz span</span>` : "") + `</div>`).join("");
    $("siteList").querySelectorAll(".row").forEach((r) => r.onclick = () => { pickedSite = sys.sites.find((s) => s.site_id === +r.dataset.site); renderSites(); });
  }
  function renderCats() {
    const cats = [...new Set(sys.tgs.map((t) => t.category).filter(Boolean))].sort();
    $("tgCat").innerHTML = '<option value="">all</option>' + cats.map((c) => `<option>${esc(c)}</option>`).join("");
  }
  function shownTgs() {
    const q = $("tgFilter").value.trim().toLowerCase(), cat = $("tgCat").value;
    return sys.tgs.filter((t) => (!cat || t.category === cat) && (!q || `${t.id} ${t.alias} ${t.description} ${t.category}`.toLowerCase().includes(q)));
  }
  function renderTgs() {
    $("tgBody").innerHTML = shownTgs().map((t) =>
      `<tr class="${t.encrypted ? "enc" : ""}"><td><input type="checkbox" data-tg="${t.id}" ${picked.has(t.id) ? "checked" : ""} ${t.encrypted ? "disabled" : ""}></td>` +
      `<td class="mono">${t.id}</td><td>${esc(t.alias)}</td><td>${esc(t.description)}</td><td><small>${esc(t.category)}</small></td>` +
      `<td>${t.encrypted ? '<span class="badge enc">Encrypted</span>' : ""}</td></tr>`).join("");
    $("tgBody").querySelectorAll("input[data-tg]").forEach((c) => c.onchange = () => { c.checked ? picked.add(+c.dataset.tg) : picked.delete(+c.dataset.tg); tgMeta(); });
    tgMeta();
  }
  function tgMeta() { $("tgMeta").textContent = picked.size ? `${picked.size} selected` : "none selected → the playlist follows every clear talkgroup"; }
  $("tgFilter").oninput = renderTgs; $("tgCat").onchange = renderTgs;
  $("tgAll").onclick = () => { shownTgs().filter((t) => !t.encrypted).forEach((t) => picked.add(t.id)); renderTgs(); };
  $("tgNone").onclick = () => { picked = new Set(); renderTgs(); };
  $("plSave").onclick = async () => {
    if (!sys || !pickedSite) { alert("Load a system and pick a site first."); return; }
    const lo = pickedSite.span_mhz ? pickedSite.span_mhz[0] : pickedSite.control_mhz[0];
    const hi = pickedSite.span_mhz ? pickedSite.span_mhz[1] : pickedSite.control_mhz[0];
    const playlist = { id: "", name: $("plName").value.trim(), sid: sys.sid, system_name: sys.name,
      site_id: pickedSite.site_id, site_name: pickedSite.name, nac: pickedSite.nac,
      control_mhz: pickedSite.control_mhz[0], center_mhz: +((lo + hi) / 2).toFixed(4),
      rate: siteRate(pickedSite), tgs: [...picked].sort((a, b) => a - b) };
    try { renderPlaylists(await invoke("playlist_save", { playlist })); $("plMeta").textContent = "saved"; } catch (e) { alert(e); }
  };

  /* ---------- playlists ---------- */
  let playlists = [];
  function renderPlaylists(list) {
    playlists = list;
    $("plEmpty").style.display = list.length ? "none" : "";
    $("plList").innerHTML = list.map((p) =>
      `<div class="row" data-id="${esc(p.id)}"><span class="grow"><b>${esc(p.name)}</b><br><small>${esc(p.system_name)} · site ${p.site_id} ${esc(p.site_name)} · ${p.control_mhz.toFixed(4)} MHz · ${p.tgs.length ? p.tgs.length + " TGs" : "all TGs"}</small></span>` +
      `<button class="btn primary" data-act="${p.id}">Use</button><button class="btn ghost" data-del="${p.id}">Delete</button></div>`).join("");
    $("plList").querySelectorAll("[data-act]").forEach((b) => b.onclick = () => activatePlaylist(b.dataset.act));
    $("plList").querySelectorAll("[data-del]").forEach((b) => b.onclick = async () => {
      if (!confirm("Delete this playlist?")) return;
      try { renderPlaylists(await invoke("playlist_delete", { id: b.dataset.del })); } catch (e) { alert(e); }
    });
    const cur = $("playlist").value;
    $("playlist").innerHTML = '<option value="">— every talkgroup —</option>' + list.map((p) => `<option value="${esc(p.id)}">${esc(p.name)}</option>`).join("");
    $("playlist").value = list.some((p) => p.id === cur) ? cur : "";
  }
  async function activatePlaylist(id) {
    try {
      const p = await invoke("playlist_activate", { id: id || null });
      $("playlist").value = p ? p.id : "";
      if (p) {
        modeSel = "follow"; setSeg($("modeSeg"), "follow"); applyMode();
        $("freq").value = p.control_mhz.toFixed(4) + "M"; $("center").value = p.center_mhz.toFixed(4) + "M";
        if ($("source").value === "rtlsdr" && p.rate > 2400000) {
          $("rate").value = "2400000"; $("center").value = p.control_mhz.toFixed(4) + "M";
          logEvent(`RTL-SDR covers ±1.2 MHz: centred on the control channel; calls outside that span will be skipped — pick Airspy R2 for the whole site`, "warn");
        } else { $("rate").value = String(p.rate); }
        syncRate();
        if ($("pillText").textContent !== "standby") logEvent("playlist changed — press Stop, then Start to retune", "warn");
        $("followMeta").textContent = `playlist: ${p.name} · ${p.tgs.length ? p.tgs.length + " talkgroups" : "all talkgroups"}`;
        showView("monitor");
      } else { $("followMeta").textContent = ""; }
    } catch (e) { alert(e); }
  }
  $("playlist").onchange = () => activatePlaylist($("playlist").value);
  invoke("playlists_list").then(async (list) => {
    renderPlaylists(list);
    // Auto-start: opt-in, and only if the last-used playlist still exists.
    const pr = store("hs.prefs", {});
    if (pr.autostart && pr.lastPlaylist && list.some((p) => p.id === pr.lastPlaylist) && !location.hash.startsWith("#autostart")) {
      await activatePlaylist(pr.lastPlaylist);
      logEvent(`auto-start: ${list.find((p) => p.id === pr.lastPlaylist).name}`);
      setTimeout(() => $("start").click(), 600);
    }
  }).catch((e) => log(`playlists: ${e}`));
  rrRefresh();

  // Dev hook: open with #autostart=airspy to press Start for a 10 MSPS site follow.
  if (location.hash.startsWith("#autostart")) {
    const hp = new URLSearchParams(location.hash.slice(1));
    $("source").value = hp.get("autostart") || "airspy"; $("source").onchange();
    if (hp.get("rrload")) invoke("rr_download", { sid: +hp.get("rrload") }).then((d) => log(`rrload ok: ${d.name} ${d.talkgroups} tgs ${d.sites.length} sites`)).catch((e) => log(`rrload error: ${e}`));
    setTimeout(() => $("start").click(), 800);
    setTimeout(() => $("stop").click(), +(hp.get("secs") || 40) * 1000);
  }
} else {
  /* ---------- demo driver: preview the layout without a backend ---------- */
  const TGS = [[10103,"IMPD Dispatch NW"],[10106,"IMPD Dispatch SE"],[10147,"IFD Fire Dispatch"],[10202,"Marion Co EMS"],[10308,"Sheriff Patrol"]];
  const DLS = [851.8125, 857.3625, 857.3875, 858.3375];
  const rnd = (a, b) => a + Math.random() * (b - a), pick = (a) => a[Math.floor(Math.random() * a.length)];
  let running = false, tick = 0, syncs = 0, calls = 0, raf = 0;
  const live = [];
  function specRow() {
    const row = new Float32Array(256);
    for (let i = 0; i < 256; i++) row[i] = -88 + rnd(-3, 3);
    [[100, -34], [160, -52], [190, -48]].forEach(([c, p]) => { for (let i = -6; i <= 6; i++) row[c + i] = Math.max(row[c + i], p - Math.abs(i) * 3 + rnd(-2, 2)); });
    live.forEach((l) => { for (let i = -5; i <= 5; i++) row[l.bin + i] = Math.max(row[l.bin + i], -40 - Math.abs(i) * 2.5 + rnd(-3, 3)); });
    return Array.from(row);
  }
  function loop() {
    if (!running) return;
    tick++;
    handleFollow({ kind: "spectrum", bins_db: specRow() });
    if (tick === 40) handleFollow({ kind: "measured", control_mhz: 851.5375, modulation: "C4FM", correction_hz: 0, rate: 9600000 });
    if (tick > 40 && tick % 6 === 0) syncs += 1;
    if (tick > 40 && Math.random() < 0.012 && live.length < 3) {
      const [tg, name] = pick(TGS), f = pick(DLS);
      const c = { tg, name, freq_mhz: f, bin: Math.floor(rnd(30, 226)), end: tick + Math.floor(rnd(60, 300)), src: Math.floor(rnd(4910000, 4914000)) };
      live.push(c); handleFollow({ kind: "call_start", tg, name, freq_mhz: f });
    }
    for (let i = live.length - 1; i >= 0; i--) if (tick >= live[i].end) {
      const c = live.splice(i, 1)[0]; calls++;
      handleFollow({ kind: "call", tg: c.tg, name: c.name, source: c.src, freq_mhz: c.freq_mhz, modulation: "CQPSK", secs: (c.end - tick + rnd(60, 300)) / 30, wav: null });
    }
    if (tick % 30 === 0) handleFollow({ kind: "status", control_syncs: syncs, calls, out_of_band: 0, encrypted: 0, locked: 0, msps: 9.6, want_msps: 9.6, dropped: 0, elapsed_secs: tick / 30 });
    raf = requestAnimationFrame(loop);
  }
  $("start").onclick = () => { if (running) return; running = true; setState("measuring"); $("tunedHz").textContent = "851.5375"; loop(); };
  $("stop").onclick = () => { running = false; cancelAnimationFrame(raf); setState("standby"); };
  $("loadcat").onclick = () => { $("loadcat").textContent = "406 TGs"; };
  $("decode").onclick = () => {};
  $("rrSave").onclick = () => { $("rrMeta").textContent = "saved (demo)"; };
  $("rrDownload").onclick = $("bZipGo").onclick = () => { $("findMeta").textContent = "demo: no backend"; };
  $("plSave").onclick = () => {};
  $("plEmpty").style.display = "";
}
