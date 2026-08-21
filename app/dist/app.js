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
/* ---------- in-app dialogs: the webview has no native alert/confirm ---------- */
function uiToast(msg, kind) {
  let host = $("toasts"); if (!host) { host = document.createElement("div"); host.id = "toasts"; document.body.appendChild(host); }
  const d = document.createElement("div"); d.className = "toast" + (kind ? " " + kind : ""); d.textContent = String(msg);
  host.appendChild(d); setTimeout(() => d.classList.add("show"), 10);
  const ttl = kind === "err" ? 9000 : 4500;
  setTimeout(() => { d.classList.remove("show"); setTimeout(() => d.remove(), 300); }, ttl);
  d.onclick = () => d.remove();
}
function uiConfirm(msg, okLabel) {
  return new Promise((resolve) => {
    const wrap = document.createElement("div"); wrap.className = "modal-wrap";
    wrap.innerHTML = `<div class="modal"><div class="msg"></div><div class="xport" style="justify-content:flex-end;margin:12px 0 0"><button class="btn ghost" data-no>Cancel</button><button class="btn primary" data-yes>${esc(okLabel || "OK")}</button></div></div>`;
    wrap.querySelector(".msg").textContent = String(msg);
    const done = (v) => { wrap.remove(); resolve(v); };
    wrap.querySelector("[data-no]").onclick = () => done(false);
    wrap.querySelector("[data-yes]").onclick = () => done(true);
    wrap.onclick = (e) => { if (e.target === wrap) done(false); };
    wrap.onkeydown = (e) => { if (e.key === "Escape") done(false); if (e.key === "Enter") done(true); };
    document.body.appendChild(wrap); wrap.querySelector("[data-yes]").focus();
  });
}
window.alert = (m) => uiToast(m, /error|fail|could not|not found|invalid|enter |choose |no /i.test(String(m)) ? "err" : "");
function wireSeg(el, onPick) {
  el.querySelectorAll("button").forEach((b) => {
    b.onclick = () => { setSeg(el, b.dataset.v); onPick(b.dataset.v); };
  });
}
function setSeg(el, v) { el.querySelectorAll("button").forEach((x) => x.setAttribute("aria-pressed", String(x.dataset.v === v))); }

/* ---------- views ---------- */
function showView(v) {
  ["monitor", "library", "playlists", "aliases", "discovery", "alerts", "devices", "settings"].forEach((n) => { $("view-" + n).style.display = n === v ? "" : "none"; });
  if (v === "alerts" && typeof alertsOnShow === "function") alertsOnShow();
  if (v === "devices" && typeof devicesOnShow === "function") devicesOnShow();
  if (v === "library" && typeof libOnShow === "function") libOnShow();
  if (v === "aliases" && typeof aliasesOnShow === "function") aliasesOnShow();
  if (v === "discovery" && typeof discoveryOnShow === "function") discoveryOnShow();
  setSeg($("navSeg"), v);
}
$("navSeg").querySelectorAll("button").forEach((b) => b.onclick = () => showView(b.dataset.v));
setTimeout(() => { if (["#playlists", "#settings", "#library", "#aliases", "#discovery", "#alerts", "#devices"].includes(location.hash)) showView(location.hash.slice(1)); }, 0);

/* ---------- tuning state ---------- */
let modeSel = "follow", modSel = "cqpsk", eqSel = "cma";
let measuredPpm = null;
const ppmVal = () => { const v = parseFloat($("ppm").value); return Number.isFinite(v) ? v : null; };
function applyMode() {
  const follow = modeSel === "follow";
  $("centerField").style.display = follow ? "" : "none";
  $("tmodField").style.display = follow ? "" : "none";
  $("modField").style.display = follow ? "none" : "";
  $("eqField").style.display = follow ? "none" : "";
  $("chanReadouts").style.display = follow ? "none" : "";
  $("followOpts").style.display = follow ? "" : "none";
  $("channelOpts").style.display = follow ? "none" : "";
  $("freqHint").textContent = follow ? "control channel" : "channel";
  $("emptyHint").textContent = follow ? "Pick a playlist or set a control channel, then press Start." : "Set a channel and press Start. One-channel mode decodes one channel and counts voice but does not play it; on a control channel it only announces grants (Discovery, Events) — use Follow site to listen.";
}
wireSeg($("modeSeg"), (v) => { modeSel = v; applyMode(); });
wireSeg($("modSeg"), (v) => { modSel = v; });
wireSeg($("eqSeg"), (v) => { eqSel = v; $("r-eq").textContent = v === "bypass" ? "BARE" : v.toUpperCase(); });
function syncRate() {
  const r = parseFloat($("rate").value);
  $("rateMeta").textContent = r >= 1e6 ? (r / 1e6).toFixed(1) + " MSPS" : (r / 1e3) + " kSPS";
}
$("rate").onchange = syncRate;
const srcKind = () => $("source").value.split("|")[0];
const srcId = () => $("source").value.split("|")[1] || "";
$("source").onchange = () => {
  if ($("pillText").textContent !== "standby" && TAURI) { invoke("stop_capture").catch(() => {}); uiToast("Stopped — the radio is switched; press Start to run on the new one."); }
  const a = srcKind() === "airspy";
  save("hs.device", $("source").value);
  if (typeof devLoadSelected === "function") devLoadSelected();
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
  el.innerHTML = `<span class="tg">${esc(ev.name)}</span><span class="t">0:00</span><span class="sub">TG ${ev.tg} · ${ev.freq_mhz.toFixed(4)}</span>`;
  el.dataset.tg = ev.tg; applyColor(el, ev.tg);
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
let noAudioCount = 0;
function addCall(g) {
  $("empty").style.display = "none";
  const tr = document.createElement("tr");
  tr.className = "new";
  const len = g.secs != null ? `${g.secs.toFixed(1)}s` : "";
  tr.innerHTML =
    `<td class="time">${now()}</td>` +
    `<td class="tg">${esc(g.name)}<span class="num">TG ${g.tg}</span></td>` +
    `<td class="src">${g.unit_name ? `${esc(g.unit_name)}<span class="num" style="display:block;font-size:10.5px;color:var(--ink-faint)">${g.source}</span>` : (g.source ? g.source : "—")}${g.talker_alias ? `<span class="alias" style="display:block" title="alias broadcast over the air">“${esc(g.talker_alias)}”</span>` : ""}</td>` +
    `<td class="tr" data-trid="${g.id != null ? g.id : ""}" title="${esc(g.transcript || "")}">${g.transcript ? esc(g.transcript) : `<span class="faint">${g.id != null ? "…" : ""}</span>`}</td>` +
    `<td class="dl">${g.freq_mhz.toFixed(4)}</td>` +
    `<td class="len">${len}</td>` +
    `<td>${g.encrypted ? '<span class="badge enc">Encrypted</span>' : g.emergency ? '<span class="badge emg">EMERGENCY</span>' : (g.secs === 0 || g.modulation === "?") ? `<span class="badge" title="granted, but no voice frame decoded — syncs C4FM ${g.syncs_c4fm ?? 0} / CQPSK ${g.syncs_cqpsk ?? 0}">no audio · ${g.syncs_c4fm ?? 0}/${g.syncs_cqpsk ?? 0}</span>` : `<span class="badge clear">${g.modulation || "clear"}</span>`}${g.patched_with && g.patched_with.length ? ` <span class="badge clear" title="patched with">⛓ ${g.patched_with.length}</span>` : ""}</td>` +
    `<td class="act">` +
      (g.wav ? `<button title="Replay" data-wav="${esc(g.wav)}">▶</button>` : "") +
      (g.id != null ? `<button title="Add to cart" data-cart="${g.id}" class="${cart.has(g.id) ? "on" : ""}">🛒</button>` : "") +
      `<button data-pri="${g.tg}">☆</button>` +
      `<button title="Alert tone for TG ${g.tg}" data-bell="${g.tg}">🔔</button>` +
      `<button title="Avoid TG ${g.tg} for a while" data-avoid="${g.tg}">⏱</button>` +
      `<button title="Lock out TG ${g.tg}" data-lock="${g.tg}">⊘</button>` +
    `</td>`;
  if (g.emergency) tr.classList.add("emg");
  if (g.secs === 0 || g.modulation === "?") tr.classList.add("noaudio");
  applyColor(tr, g.tg);
  tr.querySelectorAll("button[data-wav]").forEach((b) => b.onclick = () => replay(b.dataset.wav));
  tr.querySelectorAll("button[data-lock]").forEach((b) => b.onclick = () => toggleLock(+b.dataset.lock));
  tr.querySelectorAll("button[data-avoid]").forEach((b) => b.onclick = () => avoidFor(+b.dataset.avoid));
  tr.querySelectorAll("button[data-pri]").forEach((b) => b.onclick = () => cyclePriority(+b.dataset.pri));
  tr.querySelectorAll("button[data-bell]").forEach((b) => b.onclick = () => toggleBell(+b.dataset.bell));
  tr.querySelectorAll("button[data-cart]").forEach((b) => b.onclick = () => cartToggle(+b.dataset.cart, `${now()} ${g.name} · ${g.secs != null ? g.secs.toFixed(1) + "s" : ""}`));
  const text = `${g.name} ${g.tg} ${g.source || ""} ${g.unit_name || ""} ${g.talker_alias || ""} ${g.freq_mhz.toFixed(4)} ${g.transcript || ""}`.toLowerCase();
  history.unshift({ el: tr, text, id: g.id });
  tbody.prepend(tr);
  while (history.length > 500) history.pop().el.remove();
  applyHistFilter(); refreshRowButtons();
}
function applyHistFilter() {
  const q = $("histFilter").value.trim().toLowerCase();
  let shown = 0;
  const hideNa = $("histHideNa").checked;
  history.forEach((h) => { const on = (!q || h.text.includes(q)) && !(hideNa && h.el.classList.contains("noaudio")); h.el.style.display = on ? "" : "none"; if (on) shown++; });
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

/* ---------- per-talkgroup colours, and range rules (ranges/wildcards) ---------- */
const PALETTE = ["#34e0cf", "#f5b544", "#5fd39a", "#e97387", "#7aa2ff", "#c77dff", "#ff9f43", "#9aa5a3"];
const colors = new Map(Object.entries(store("hs.colors", {})).map(([k, v]) => [+k, v]));   // tg → css colour
// Range rules: { lo, hi, name, pri: 10|90|null, lock: bool, bell: bool, color: "" }
const tgRules = store("hs.tgrules", []);
function ruleFor(tg) { return tgRules.find((r) => tg >= r.lo && tg <= r.hi); }
function colorOf(tg) { return colors.get(tg) || (ruleFor(tg) || {}).color || null; }
function bellFor(tg) { return bells.has(tg) || !!(ruleFor(tg) || {}).bell; }
function applyColor(el, tg) {
  const c = colorOf(tg);
  if (c) { el.dataset.color = c; el.style.setProperty("--tgc", c); } else { delete el.dataset.color; el.style.removeProperty("--tgc"); }
}
function cycleColor(tg) {
  const cur = colors.get(tg), i = PALETTE.indexOf(cur);
  if (cur && i === PALETTE.length - 1) colors.delete(tg); else colors.set(tg, PALETTE[(i + 1) % PALETTE.length]);
  save("hs.colors", Object.fromEntries(colors));
  document.querySelectorAll(`tr[data-tg="${tg}"], .call[data-tg="${tg}"]`).forEach((el) => applyColor(el, tg));
  tbody.querySelectorAll("button[data-lock]").forEach((b) => { if (+b.dataset.lock === tg) applyColor(b.closest("tr"), tg); });
  refreshRowButtons();
}
function pushRanges() {
  if (!TAURI) return;
  invoke("set_lockout_ranges", { ranges: tgRules.filter((r) => r.lock).map((r) => [r.lo, r.hi]) }).catch((e) => log(`ranges: ${e}`));
  invoke("set_priority_ranges", { ranges: tgRules.filter((r) => r.pri).map((r) => [r.lo, r.hi, r.pri]) }).catch((e) => log(`ranges: ${e}`));
}
function saveRules() { save("hs.tgrules", tgRules); pushRanges(); if (typeof renderRules === "function") renderRules(); }

/* ---------- accordions: the left column's panels collapse into each other ---------- */
const accOpen = store("hs.acc", { tuning: true, groups: true, control: true, playing: true, events: true });
function accApply() {
  document.querySelectorAll(".panel.acc").forEach((p) => { const k = p.dataset.acc, open = accOpen[k] !== false; p.classList.toggle("closed", !open); });
}
document.querySelectorAll(".panel.acc > .head").forEach((h) => h.onclick = (e) => {
  if (e.target.closest("button,input,select")) return;
  const k = h.parentElement.dataset.acc; accOpen[k] = accOpen[k] === false; save("hs.acc", accOpen); accApply();
});
accApply();

/* ---------- listen groups: named sets of talkgroups you can mute or unmute in one click ---------- */
const groups = store("hs.groups", []);   // [{id, name, tgs:[], listen:true}]
function groupsSave() { save("hs.groups", groups); renderGroupChips(); if (typeof renderGroupList === "function") renderGroupList(); pushLockout(); }
// Listening wins: a talkgroup in any group you are listening to stays
// audible even if a muted group also contains it; only talkgroups found
// solely in muted groups are silenced.
function mutedByGroups() {
  const heard = new Set(); groups.forEach((g) => { if (g.listen) g.tgs.forEach((t) => heard.add(t)); });
  const s = new Set(); groups.forEach((g) => { if (!g.listen) g.tgs.forEach((t) => { if (!heard.has(t)) s.add(t); }); });
  return s;
}
function renderGroupChips() {
  const el = $("grpChips"); if (!el) return;
  el.innerHTML = groups.length ? groups.map((g) => `<span class="chip ${g.listen ? "on" : "muted"}" data-grp="${esc(g.id)}" title="${g.tgs.length} talkgroups — click to ${g.listen ? "mute" : "listen"}">${g.listen ? "🔊" : "🔇"} ${esc(g.name)} <small>${g.tgs.length}</small></span>`).join("") : '<span class="faint">no groups yet — tick talkgroups in Aliases and make one</span>';
  el.querySelectorAll(".chip[data-grp]").forEach((c) => c.onclick = () => { const g = groups.find((x) => x.id === c.dataset.grp); g.listen = !g.listen; groupsSave(); logEvent(`${g.listen ? "listening to" : "muted"} group “${g.name}” (${g.tgs.length} talkgroups)`); });
  $("grpSummary").textContent = groups.length ? `${groups.filter((g) => g.listen).length} of ${groups.length} on` : "none";
}

/* ---------- record / stream / upload policy: a default plus per-talkgroup exceptions ---------- */
const POLICIES = ["record", "stream", "upload"];
const policy = store("hs.policy", { record: { all: true, except: [] }, stream: { all: true, except: [] }, upload: { all: true, except: [] } });
const polAllows = (k, tg) => policy[k].all !== policy[k].except.includes(tg);
function polSet(k, tg, on) {
  const ex = new Set(policy[k].except);
  if (on === policy[k].all) ex.delete(tg); else ex.add(tg);
  policy[k].except = [...ex];
}
function pushPolicies() {
  save("hs.policy", policy);
  if (TAURI) invoke("set_policies", { record: [policy.record.all, policy.record.except], stream: [policy.stream.all, policy.stream.except], upload: [policy.upload.all, policy.upload.except] }).catch((e) => log(`policies: ${e}`));
  const m = POLICIES.map((k) => `${k}: ${policy[k].all ? "all" : "none"}${policy[k].except.length ? ` (${policy[k].except.length} exceptions)` : ""}`).join(" · ");
  const el = $("polMeta"); if (el) el.textContent = m;
}

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
  return [...new Set([...lockout, ...avoidUntil.keys(), ...mutedByGroups()])];
}
function pushLockout() { if (TAURI) invoke("set_lockout", { tgs: effectiveLockout() }).catch((e) => alert(e)); }
function avoidFor(tg) {
  const min = +$("avoidMin").value || 60;
  if (avoidUntil.has(tg)) avoidUntil.delete(tg); else avoidUntil.set(tg, Date.now() + min * 60000);
  save("hs.avoid", Object.fromEntries(avoidUntil)); renderLockout(); pushLockout();
}
setInterval(() => { const before = avoidUntil.size; effectiveLockout(); renderLockout(); renderGroupChips(); if (avoidUntil.size !== before) pushLockout(); }, 15000);
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
effectiveLockout(); renderLockout(); renderGroupChips();
$("histHideNa").checked = !!store("hs.hidena", false); $("histHideNa").onchange = () => { save("hs.hidena", $("histHideNa").checked); applyHistFilter(); };

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
      if (ev.bands && ev.bands.length > 1) logEvent(`decoding ${ev.bands.length} bands: ${ev.bands.map(([c, r]) => `${c.toFixed(3)} ±${(r / 2e6).toFixed(2)}`).join(" · ")} MHz`);
      if (ev.ppm != null && Number.isFinite(ev.ppm)) {
        measuredPpm = ev.ppm;
        const applied = parseFloat($("ppm").value) || 0;
        $("ppmMeasured").textContent = `measured: ${ev.correction_hz >= 0 ? "+" : ""}${ev.correction_hz.toFixed(0)} Hz at ${ev.control_mhz.toFixed(4)} MHz ≈ ${(applied + ev.ppm).toFixed(1)} ppm total (${applied} set + ${ev.ppm.toFixed(1)} residual) — see Devices`;
        $("ppmUse").disabled = false;
      }
      $("wfAxis").textContent = `${(ev.center_mhz ?? parseFreq($("center").value) / 1e6).toFixed(4)} MHz ± ${(ev.rate / 2e6).toFixed(2)} MHz`;
      if (ev.center_mhz != null) $("center").value = ev.center_mhz.toFixed(4) + "M";
      $("followMeta").textContent = "";
      activeRefresh();
      break;
    case "call_start":
      activeStart(ev);
      if (bellFor(ev.tg)) tone("bell");
      break;
    case "call":
      activeEnd(ev);
      followVoice += ev.secs;
      if (ev.emergency) { tone("emergency"); logEvent(`EMERGENCY · ${ev.name} · unit ${ev.unit_name || ev.source}`, "alarm"); }
      addCall({ tg: ev.tg, name: ev.name, source: ev.source, unit_name: ev.unit_name, talker_alias: ev.talker_alias, freq_mhz: ev.freq_mhz, encrypted: false,
                secs: ev.secs, modulation: ev.modulation, wav: ev.wav, emergency: ev.emergency, patched_with: ev.patched_with, id: ev.id, syncs_c4fm: ev.syncs_c4fm, syncs_cqpsk: ev.syncs_cqpsk });
      if (ev.secs === 0) { noAudioCount++; $("histMeta").title = `${noAudioCount} granted calls produced no audio`; }
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
        `patches   ${ev.patches.length ? ev.patches.map(([sg, m]) => `${sg} ← ${m.join(",")}`).join("; ") : "none"}\n` +
        `RFSS/site ${ev.rfss != null ? ev.rfss : "—"} / ${ev.site != null ? ev.site : "—"}\n` +
        `neighbours ${ev.neighbours && ev.neighbours.length ? ev.neighbours.map(([sys, r, st, mhz]) => `sys 0x${sys.toString(16).toUpperCase()} rfss ${r} site ${st}${mhz != null ? " @ " + mhz.toFixed(4) : ""}`).join("; ") : "none announced"}`;
      $("siteSummary").textContent = `${ev.alternates_mhz.length} alt · ${ev.idens.length} plans · ${ev.patches.length} patches · ${(ev.neighbours || []).length} neighbours`;
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
      lastStream = `${ev.msps.toFixed(2)} / ${ev.want_msps.toFixed(2)} MSPS · ${ev.dropped || 0} dropped`;
      if (ev.locked) $("followMeta").textContent = `${ev.locked} locked-out call${ev.locked === 1 ? "" : "s"} skipped`;
      break;
    case "spectrum": pushSpectrum(ev.bins_db); break;
    case "constellation": drawConstellation(ev); break;
    case "grant": discoveryGrant(ev); break;
    case "mobility": affiliationEvent(ev); break;
    case "location": locationEvent(ev); break;
    case "talker_alias": logEvent(`${ev.name}: radio alias “${ev.alias}”`); break;
  }
}
function alertFired(p) {
  logEvent(`ALERT ${p.name}: ${p.message.split("\n")[0]}`, "alarm");
  uiToast(`🚨 ${p.name} — ${p.message.split("\n").slice(0, 2).join(" · ")}`);
  if (p.tone) tone("emergency");
}

/* ---------- constellation (control channel symbols) ---------- */
const cc = $("constellation"), cctx = cc.getContext("2d");
let lastConst = null;
function drawConstellation(ev) {
  lastConst = ev;
  $("constBox").style.display = wfCfg.constellation === false ? "none" : "";
  if (wfCfg.constellation === false) return;
  const w = cc.width, h = cc.height, cx = w / 2, cy = h / 2;
  cctx.fillStyle = "#05090a"; cctx.fillRect(0, 0, w, h);
  cctx.strokeStyle = "rgba(46,120,112,.35)"; cctx.lineWidth = 1;
  cctx.beginPath(); cctx.moveTo(0, cy); cctx.lineTo(w, cy); cctx.moveTo(cx, 0); cctx.lineTo(cx, h); cctx.stroke();
  const pts = ev.points || [];
  const cq = ev.modulation === "CQPSK";
  if (cq) { cctx.beginPath(); cctx.arc(cx, cy, w * 0.33, 0, Math.PI * 2); cctx.stroke(); }
  else { [-3, -1, 1, 3].forEach((l) => { const x = cx + l * w / 8, y = cy - l * h / 8; cctx.beginPath(); cctx.moveTo(x, 0); cctx.lineTo(x, h); cctx.moveTo(0, y); cctx.lineTo(w, y); cctx.stroke(); }); }
  // Scale: CQPSK symbols are unit-ish after AGC; C4FM levels sit at ±1/±3.
  let scale = cq ? w * 0.33 : w / 8;
  if (cq && pts.length) { let m = 0; for (const [x, y] of pts) m = Math.max(m, Math.hypot(x, y)); if (m > 0) scale = (w * 0.33) / m; }
  const n = pts.length;
  pts.forEach(([x, y], i) => {
    const a = 0.25 + 0.75 * (i / Math.max(1, n - 1));
    cctx.fillStyle = `rgba(52,224,207,${a.toFixed(2)})`;
    cctx.fillRect(cx + x * scale - 1.2, cy - y * scale - 1.2, 2.4, 2.4);
  });
  $("constLabel").textContent = cq ? "CQPSK · I/Q after equalizer" : "C4FM · level vs previous level";
}
$("wfConst").onchange = () => { wfCfg.constellation = $("wfConst").checked; save("hs.wf", wfCfg); $("constBox").style.display = wfCfg.constellation ? "" : "none"; };
$("wfConst").checked = wfCfg.constellation !== false; $("constBox").style.display = wfCfg.constellation === false ? "none" : "";

/* ---------- discovery: talkgroups and channels the control channel actually uses ---------- */
const disc = store("hs.discovery", { tgs: {}, freqs: {} });   // tg → {name, named, n, freq, unit, first, last}; freq → {n, tgs:{}, last}
let discDirty = false;
function discoveryGrant(ev) {
  const t = disc.tgs[ev.tg] || (disc.tgs[ev.tg] = { first: Date.now(), n: 0 });
  t.n++; t.last = Date.now(); t.name = ev.name; t.named = ev.named; t.freq = ev.freq_mhz; if (ev.unit) t.unit = ev.unit; t.enc = ev.encrypted;
  const k = ev.freq_mhz.toFixed(4), f = disc.freqs[k] || (disc.freqs[k] = { n: 0, tgs: {} });
  f.n++; f.last = Date.now(); f.tgs[ev.tg] = 1;
  discDirty = true;
}
setInterval(() => { if (discDirty) { discDirty = false; const keys = Object.keys(disc.tgs); if (keys.length > 3000) keys.sort((a, b) => disc.tgs[a].last - disc.tgs[b].last).slice(0, keys.length - 3000).forEach((k) => delete disc.tgs[k]); save("hs.discovery", disc); if ($("view-discovery").style.display !== "none") renderDiscovery(); } }, 2000);
const ago = (t) => { const s = Math.max(0, Math.round((Date.now() - t) / 1000)); return s < 60 ? `${s}s ago` : s < 3600 ? `${Math.floor(s / 60)}m ago` : `${Math.floor(s / 3600)}h ${Math.floor(s % 3600 / 60)}m ago`; };
let bandLo = 0, bandHi = 0;
function renderDiscovery() {
  const q = $("dcFilter").value.trim().toLowerCase(), un = $("dcUnnamed").checked;
  const rows = Object.entries(disc.tgs).map(([tg, t]) => ({ tg: +tg, ...t })).filter((t) => (!un || !t.named) && (!q || `${t.tg} ${t.name || ""} ${t.freq || ""}`.toLowerCase().includes(q))).sort((a, b) => b.last - a.last);
  $("dcBody").innerHTML = rows.slice(0, 1000).map((t) => `<tr data-tg="${t.tg}"><td class="mono">${t.tg}</td><td>${t.named ? esc(t.name) : `<span class="faint">unnamed</span>`}${t.enc ? ' <span class="badge enc">enc</span>' : ""}</td><td class="mono">${t.n}</td><td class="mono">${t.freq != null ? t.freq.toFixed(4) : "—"}</td><td class="mono">${t.unit || "—"}</td><td class="mono">${ago(t.last)}</td>` +
    `<td class="act"><button data-dcplay="${t.tg}" title="Play the newest recorded call on this talkgroup">▶</button><input type="text" data-name="${t.tg}" placeholder="${t.named ? "rename" : "name it"}" style="width:120px;padding:2px 6px;font-size:11px" /><button data-namego="${t.tg}">✔</button>` +
    `<button data-pri="${t.tg}">${(prio.get(t.tg) || 50) === 10 ? "★" : (prio.get(t.tg) || 50) === 90 ? "▽" : "☆"}</button><button data-lock="${t.tg}" class="${lockout.has(t.tg) ? "on" : ""}">⊘</button></td></tr>`).join("");
  $("dcEmpty").style.display = rows.length ? "none" : "";
  const all = Object.keys(disc.tgs).length, unnamed = Object.values(disc.tgs).filter((t) => !t.named).length;
  $("dcMeta").textContent = all ? `${all} talkgroups · ${unnamed} unnamed` : "";
  const tb = $("dcBody");
  tb.querySelectorAll("[data-namego]").forEach((b) => b.onclick = async () => {
    const tg = +b.dataset.namego, inp = tb.querySelector(`input[data-name="${tg}"]`), name = inp.value.trim(); if (!name || !TAURI) return;
    try { await invoke("catalog_user_set", { tg, alias: name, category: "Discovered" }); disc.tgs[tg].name = name; disc.tgs[tg].named = true; save("hs.discovery", disc); renderDiscovery(); if (typeof aliasesRefresh === "function") aliasesRefresh(); logEvent(`named TG ${tg} “${name}”`); } catch (e) { alert(e); }
  });
  tb.querySelectorAll("button[data-dcplay]").forEach((b) => b.onclick = async () => {
    if (!TAURI) return;
    try { const r = await invoke("tg_latest_call", { tg: +b.dataset.dcplay }); if (!r) { uiToast("No recorded audio on that talkgroup yet — it may be locked out, not recorded, or only heard as grants.", "err"); return; }
      await invoke("library_play", { id: r.id }); uiToast(`Playing ${r.tg_name} (TG ${r.tg}) · ${r.secs.toFixed(1)} s${r.transcript ? " — “" + r.transcript.slice(0, 80) + "”" : ""}`); }
    catch (e) { uiToast(`${e}`, "err"); }
  });
  tb.querySelectorAll("input[data-name]").forEach((i) => i.onkeydown = (e) => { if (e.key === "Enter") tb.querySelector(`[data-namego="${i.dataset.name}"]`).click(); });
  tb.querySelectorAll("button[data-pri]").forEach((b) => b.onclick = () => { cyclePriority(+b.dataset.pri); renderDiscovery(); });
  tb.querySelectorAll("button[data-lock]").forEach((b) => b.onclick = () => { toggleLock(+b.dataset.lock); renderDiscovery(); });
  const fr = Object.entries(disc.freqs).map(([f, v]) => ({ f: +f, ...v })).sort((a, b) => b.n - a.n);
  $("dfBody").innerHTML = fr.slice(0, 300).map((r) => `<tr><td class="mono">${r.f.toFixed(4)}</td><td class="mono">${r.n}</td><td class="mono">${Object.keys(r.tgs).length}</td><td>${bandHi ? (r.f >= bandLo && r.f <= bandHi ? '<span class="badge clear">yes</span>' : '<span class="badge enc">no</span>') : "—"}</td><td class="mono">${ago(r.last)}</td></tr>`).join("");
  $("dfMeta").textContent = fr.length ? `${fr.length} channels` : "";
  renderAffiliations(); renderMap();
}
$("dcFilter").oninput = renderDiscovery; $("dcUnnamed").onchange = renderDiscovery;
$("dcClear").onclick = async () => { if (!(await uiConfirm("Forget everything discovered so far?", "Clear"))) return; disc.tgs = {}; disc.freqs = {}; save("hs.discovery", disc); renderDiscovery(); };
$("dcExportGo").onclick = async () => {
  const path = $("dcExport").value.trim(); if (!path) { alert("Choose a file to write, e.g. ~/Desktop/discovered.csv"); return; }
  const rows = Object.entries(disc.tgs).map(([tg, t]) => ({ tg: +tg, ...t })).sort((a, b) => a.tg - b.tg);
  const csvq = (v) => `"${String(v ?? "").replace(/"/g, '""')}"`;
  const text = "Decimal,Hex,Alpha Tag,Mode,Description,Tag,Category,Priority,Grants,Last Frequency,Last Radio,First Heard,Last Heard\n" +
    rows.map((t) => [t.tg, t.tg.toString(16).toUpperCase(), csvq(t.named ? t.name : ""), t.enc ? "DE" : "D", csvq(t.named ? t.name : ""), "", csvq(t.named ? "" : "Discovered"), "", t.n, t.freq != null ? t.freq.toFixed(4) : "", t.unit || "", new Date(t.first).toISOString(), new Date(t.last).toISOString()].join(",")).join("\n") + "\n";
  try { if (TAURI) { const p = await invoke("save_text", { path, text }); $("dcMeta").textContent = `exported → ${p}`; } } catch (e) { alert(e); }
};
window.discoveryOnShow = renderDiscovery;

/* ---------- affiliations (who is on which talkgroup) ---------- */
const affil = new Map();   // unit → {tg, name, unit_name, status, last}
function affiliationEvent(ev) {
  if (ev.what === "deregistered") { affil.delete(ev.unit); }
  else {
    const a = affil.get(ev.unit) || {};
    a.last = Date.now(); a.unit_name = ev.unit_name || a.unit_name; a.status = ev.what;
    if (ev.tg != null && ev.what !== "refused") { a.tg = ev.tg; a.name = ev.name; }
    affil.set(ev.unit, a);
    if (affil.size > 4096) affil.delete(affil.keys().next().value);
  }
  if ($("view-discovery").style.display !== "none") renderAffiliations();
}
function renderAffiliations() {
  const q = $("afFilter").value.trim().toLowerCase();
  const rows = [...affil].map(([unit, a]) => ({ unit, ...a })).filter((a) => !q || `${a.unit} ${a.unit_name || ""} ${a.tg || ""} ${a.name || ""}`.toLowerCase().includes(q)).sort((a, b) => b.last - a.last);
  $("afBody").innerHTML = rows.slice(0, 500).map((a) => `<tr><td class="mono">${a.unit_name ? `${esc(a.unit_name)} <span class="faint">${a.unit}</span>` : a.unit}</td><td>${a.tg != null ? `${esc(a.name || "")} <span class="faint mono">TG ${a.tg}</span>` : "—"}</td><td><span class="badge ${a.status === "refused" ? "enc" : "clear"}">${a.status}</span></td><td class="mono">${ago(a.last)}</td></tr>`).join("");
  $("afEmpty").style.display = affil.size ? "none" : "";
  $("afMeta").textContent = affil.size ? `${affil.size} radios` : "";
}
$("afFilter").oninput = renderAffiliations;

/* ---------- map: radio positions on CARTO/OpenStreetMap raster tiles (no library) ---------- */
const fixes = new Map();   // unit → {lat, lon, name, t}
const tiles = new Map();   // "z/x/y" → Image
let mapView = null;        // {lat, lon, z}
let mapDrag = null;
const mc = $("map"), mctx = mc.getContext("2d");
function locationEvent(ev) {
  fixes.set(ev.unit, { lat: ev.lat, lon: ev.lon, name: ev.unit_name, t: Date.now() });
  if (!mapView) mapView = { lat: ev.lat, lon: ev.lon, z: 12 };
  logEvent(`position: ${ev.unit_name || ev.unit} at ${ev.lat.toFixed(5)}, ${ev.lon.toFixed(5)}`);
  if ($("view-discovery").style.display !== "none") renderMap();
}
const lon2x = (lon, z) => (lon + 180) / 360 * Math.pow(2, z);
const lat2y = (lat, z) => (1 - Math.log(Math.tan(lat * Math.PI / 180) + 1 / Math.cos(lat * Math.PI / 180)) / Math.PI) / 2 * Math.pow(2, z);
const x2lon = (x, z) => x / Math.pow(2, z) * 360 - 180;
const y2lat = (y, z) => { const n = Math.PI - 2 * Math.PI * y / Math.pow(2, z); return 180 / Math.PI * Math.atan(0.5 * (Math.exp(n) - Math.exp(-n))); };
function tile(z, x, y) {
  const n = Math.pow(2, z); x = ((x % n) + n) % n; if (y < 0 || y >= n) return null;
  const k = `${z}/${x}/${y}`; let im = tiles.get(k);
  if (!im) { im = new Image(); im.crossOrigin = "anonymous"; im.onload = () => renderMap(); im.src = `https://${"abcd"[(x + y) % 4]}.basemaps.cartocdn.com/rastertiles/voyager/${z}/${x}/${y}.png`; tiles.set(k, im); if (tiles.size > 400) tiles.delete(tiles.keys().next().value); }
  return im.complete && im.naturalWidth ? im : null;
}
function renderMap() {
  const w = mc.width, h = mc.height;
  mctx.fillStyle = "#0b1416"; mctx.fillRect(0, 0, w, h);
  $("mapMeta").textContent = fixes.size ? `${fixes.size} radio${fixes.size === 1 ? "" : "s"} with a fix` : "";
  $("mapBody").innerHTML = [...fixes].sort((a, b) => b[1].t - a[1].t).map(([u, f]) => `<tr><td class="mono">${f.name ? `${esc(f.name)} <span class="faint">${u}</span>` : u}</td><td class="mono">${f.lat.toFixed(5)}</td><td class="mono">${f.lon.toFixed(5)}</td><td class="mono">${ago(f.t)}</td></tr>`).join("");
  if (!mapView) { mctx.fillStyle = "rgba(180,220,214,.5)"; mctx.font = "12px IBM Plex Mono, monospace"; mctx.textAlign = "center"; mctx.fillText("no position reports yet", w / 2, h / 2); return; }
  const { lat, lon, z } = mapView, cx = lon2x(lon, z), cy = lat2y(lat, z);
  const x0 = cx - w / 512, y0 = cy - h / 512;   // tile units (256 px per tile)
  for (let tx = Math.floor(x0); tx <= Math.floor(x0 + w / 256); tx++) for (let ty = Math.floor(y0); ty <= Math.floor(y0 + h / 256); ty++) {
    const im = tile(z, tx, ty), px = (tx - x0) * 256, py = (ty - y0) * 256;
    if (im) mctx.drawImage(im, px, py, 256, 256); else { mctx.fillStyle = "#10191b"; mctx.fillRect(px, py, 256, 256); }
  }
  for (const [u, f] of fixes) {
    const px = (lon2x(f.lon, z) - x0) * 256, py = (lat2y(f.lat, z) - y0) * 256;
    mctx.fillStyle = "#e97387"; mctx.beginPath(); mctx.arc(px, py, 6, 0, Math.PI * 2); mctx.fill();
    mctx.strokeStyle = "#fff"; mctx.lineWidth = 1.5; mctx.stroke();
    const label = f.name || String(u); mctx.font = "bold 11px IBM Plex Sans, sans-serif"; mctx.textAlign = "left";
    const tw = mctx.measureText(label).width; mctx.fillStyle = "rgba(0,0,0,.6)"; mctx.fillRect(px + 8, py - 9, tw + 6, 14); mctx.fillStyle = "#fff"; mctx.fillText(label, px + 11, py + 2);
  }
}
mc.onwheel = (e) => { if (!mapView) return; e.preventDefault(); mapView.z = Math.max(3, Math.min(18, mapView.z + (e.deltaY < 0 ? 1 : -1))); renderMap(); };
mc.onmousedown = (e) => { if (mapView) mapDrag = { x: e.clientX, y: e.clientY, lat: mapView.lat, lon: mapView.lon }; };
window.addEventListener("mousemove", (e) => { if (!mapDrag) return; const z = mapView.z, s = mc.width / mc.getBoundingClientRect().width; const dx = (e.clientX - mapDrag.x) * s / 256, dy = (e.clientY - mapDrag.y) * s / 256; mapView.lon = x2lon(lon2x(mapDrag.lon, z) - dx, z); mapView.lat = y2lat(lat2y(mapDrag.lat, z) - dy, z); renderMap(); });
window.addEventListener("mouseup", () => { mapDrag = null; });
mc.ondblclick = () => { if (fixes.size) { const f = [...fixes.values()].pop(); mapView = { lat: f.lat, lon: f.lon, z: Math.max(mapView ? mapView.z : 12, 12) }; renderMap(); } };

/* ================================================================ */
if (TAURI) {
  // One-channel mode parked on a control channel hears every grant the site
  // issues — repeated several times a second for each call. Those are
  // announcements, not calls: they count toward Discovery and get one event
  // line per talkgroup/channel every few seconds, never a call-history row.
  const grantSeen = new Map();
  listen("grant", (e) => {
    const g = e.payload, key = `${g.tg}@${g.freq_mhz.toFixed(4)}`, now = Date.now();
    discoveryGrant({ tg: g.tg, name: g.name, named: !/^TG \d+$/.test(g.name), freq_mhz: g.freq_mhz, unit: g.source, encrypted: g.encrypted });
    if ((grantSeen.get(key) || 0) + 5000 < now) { grantSeen.set(key, now); logEvent(`grant: ${g.name} on ${g.freq_mhz.toFixed(4)} MHz${g.encrypted ? " (encrypted)" : ""} — one-channel mode only announces; use Follow site to hear it`); }
  });
  listen("status", (e) => setStatus(e.payload));
  listen("spectrum", (e) => pushSpectrum(e.payload.bins_db));
  listen("stopped", () => { setState("standby"); holdTg = null; updateHoldBtn(); invoke("set_hold", { tg: null }).catch(() => {}); });
  listen("error", (e) => { log(`backend error: ${e.payload}`); setState("standby"); alert("Capture error:\n" + e.payload); });
  listen("follow", (e) => handleFollow(e.payload));

  const opts = () => ({
    source: srcKind(),
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
        bandLo = (parseFreq($("center").value) - o.rate * 0.4) / 1e6; bandHi = (parseFreq($("center").value) + o.rate * 0.4) / 1e6;
        await invoke("start_follow", { source: o.source, freq: parseFreq($("center").value), rate: o.rate, gain: o.gain,
          control: o.freq, callsDir: $("callsdir").value.trim() || null, play: $("play").checked,
          hangMs: parseInt($("hangMs").value, 10) || null, systemName: pl ? pl.system_name : null, siteName: pl ? pl.site_name : null,
          ppm: ppmVal(), device: srcId() || null, modulation: $("tmod").value, extra: coverageExtras() });
      } else {
        $("tunedHz").textContent = mhz(opts().freq);
        $("wfAxis").textContent = `${(opts().freq / 1e6).toFixed(4)} MHz ± ${(opts().rate / 2e6).toFixed(2)} MHz`;
        await invoke("start_capture", { ...opts(), recordIq: $("reciq").value.trim() || null, recordLog: $("reclog").value.trim() || null, ppm: ppmVal(), device: srcId() || null });
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
  pushLockout(); pushPriorities(); pushRanges(); pushPolicies();
  POLICIES.forEach((k) => { const sel = $("pol" + k[0].toUpperCase() + k.slice(1)); sel.value = policy[k].all ? "all" : "none"; sel.onchange = () => { policy[k].all = sel.value === "all"; policy[k].except = []; pushPolicies(); if (typeof alRender === "function") alRender(); }; });
  $("skipBtn").onclick = () => invoke("skip_call").catch((e) => alert(e));

  /* ---------- volume ---------- */
  let volBefore = 100;
  const volApply = (v) => { $("volume").value = v; $("volMeta").textContent = `${v}%`; $("muteBtn").textContent = v === 0 ? "🔇" : v < 50 ? "🔉" : "🔊"; $("muteBtn").classList.toggle("on", v === 0); invoke("set_volume", { gain: v / 100 }).catch((e) => log(`volume: ${e}`)); save("hs.volume", v); };
  $("volume").oninput = () => volApply(+$("volume").value);
  $("muteBtn").onclick = () => { const v = +$("volume").value; if (v === 0) volApply(volBefore || 100); else { volBefore = v; volApply(0); } };
  volApply(+store("hs.volume", 100));
  $("replayBtn").onclick = () => invoke("replay_last").catch((e) => alert(e));
  setInterval(async () => { try { const q = await invoke("audio_queued"); $("queueMeta").textContent = q.clips ? `${q.clips} queued · ${q.secs.toFixed(0)} s behind${q.dropped ? ` · ${q.dropped} dropped` : ""}` : (q.dropped ? `${q.dropped} dropped as stale` : ""); } catch (_) {} }, 1000);

  /* ---------- settings persisted locally ---------- */
  const prefs = store("hs.prefs", {});
  $("hangMs").value = prefs.hangMs ?? ""; $("avoidMin").value = prefs.avoidMin ?? "60";
  $("autostart").checked = !!prefs.autostart; $("tones").checked = prefs.tones !== false;
  $("callsdir").value = prefs.callsdir ?? ""; $("play").checked = prefs.play !== false;
  $("learnAliases").checked = !!prefs.learnAliases; $("tmod").value = prefs.tmod ?? "auto";
  $("maxCalls").value = String(prefs.maxCalls ?? 12); $("queueLimit").value = prefs.queueLimit ?? "45"; $("chanMode").value = prefs.chanMode ?? "channelizer"; $("uvQuality").value = String(prefs.uvQuality ?? 16);
  const pushScan = () => { invoke("set_max_calls", { n: parseInt($("maxCalls").value, 10) || 12 }).catch(() => {}); invoke("set_queue_limit", { secs: parseFloat($("queueLimit").value) || 0 }).catch(() => {}); invoke("set_channelizer", { on: $("chanMode").value !== "classic" }).catch(() => {}); invoke("set_uv_quality", { q: parseInt($("uvQuality").value, 10) || 16 }).catch(() => {}); };
  pushScan();
  const savePrefs = () => save("hs.prefs", { ...store("hs.prefs", {}), hangMs: $("hangMs").value, avoidMin: $("avoidMin").value, autostart: $("autostart").checked,
    tones: $("tones").checked, callsdir: $("callsdir").value, play: $("play").checked, lastPlaylist: $("playlist").value, learnAliases: $("learnAliases").checked, maxCalls: $("maxCalls").value, queueLimit: $("queueLimit").value, chanMode: $("chanMode").value, tmod: $("tmod").value, uvQuality: $("uvQuality").value });
  ["hangMs", "avoidMin", "autostart", "tones", "callsdir", "play", "tmod"].forEach((id) => $(id).onchange = savePrefs);
  $("maxCalls").onchange = $("queueLimit").onchange = $("chanMode").onchange = $("uvQuality").onchange = () => { savePrefs(); pushScan(); if ($("pillText").textContent !== "standby") uiToast("Calls-at-once applies on the next Start; the queue limit applies now."); };
  $("learnAliases").onchange = () => { savePrefs(); invoke("set_learn_aliases", { on: $("learnAliases").checked }).catch((e) => log(`learn: ${e}`)); };
  invoke("set_learn_aliases", { on: $("learnAliases").checked }).catch(() => {});
  $("ppmUse").onclick = () => { if (measuredPpm == null) return; const applied = parseFloat($("ppm").value) || 0; $("ppm").value = (applied + measuredPpm).toFixed(1); devSave(); $("ppmUse").disabled = true; $("ppmMeasured").textContent = `set to ${$("ppm").value} ppm for this radio — applies on the next start`; };

  /* ---------- devices: what is attached, and each radio's own settings ---------- */
  let devView = { devices: [], settings: {} };
  const devKey = () => srcId() ? `${srcKind()}|${srcId()}` : srcKind();
  function devRenderSource() {
    const cur = $("source").value, saved = store("hs.device", "");
    const opts = devView.devices.map((d) => ({ v: `${d.kind}|${d.id}`, t: (devView.settings[`${d.kind}|${d.id}`] || {}).nickname || d.label }));
    if (!opts.length) opts.push({ v: "rtlsdr", t: "RTL-SDR (not detected)" }, { v: "airspy", t: "Airspy R2 (not detected)" });
    $("source").innerHTML = opts.map((o) => `<option value="${esc(o.v)}">${esc(o.t)}</option>`).join("");
    const vals = opts.map((o) => o.v);
    // Prefer what was in use, then the saved radio, then the first Airspy, then anything.
    $("source").value = vals.includes(cur) && cur.includes("|") ? cur : vals.includes(saved) ? saved : (vals.find((v) => v.startsWith("airspy|")) || vals[0]);
    if ($("source").value !== cur) { const a = srcKind() === "airspy"; if ($("pillText").textContent === "standby") { $("rate").value = a ? (modeSel === "follow" ? "10000000" : "2500000") : "2400000"; syncRate(); } }
    devLoadSelected();
  }
  /* ---------- gain controls, SDRTrunk-style ---------- */
  const RTL_GAINS = () => (devView.rtl_gains_db && devView.rtl_gains_db.length ? devView.rtl_gains_db : [0, 0.9, 1.4, 2.7, 3.7, 7.7, 8.7, 12.5, 14.4, 15.7, 16.6, 19.7, 20.7, 22.9, 25.4, 28.0, 29.7, 32.8, 33.8, 36.4, 37.2, 38.6, 40.2, 42.1, 43.4, 43.9, 44.5, 48.0, 49.6]);
  function gainUi() {
    const airspy = srcKind() === "airspy";
    $("gnRtl").style.display = airspy ? "none" : ""; $("gnAirspy").style.display = airspy ? "" : "none";
    const g = RTL_GAINS(); $("gnRtlGain").max = g.length - 1; $("gnRtlGainVal").textContent = (g[+$("gnRtlGain").value] ?? 0).toFixed(1) + " dB";
    $("gnRtlGain").disabled = $("gnRtlAgc").checked;
    const mode = $("gnAsMode").value, on = $("gnAsEnabled").checked;
    $("gnAsPresetField").style.display = mode === "linearity" || mode === "sensitivity" ? "" : "none"; $("gnAsManual").style.display = mode === "manual" ? "" : "none";
    ["gnAsMode", "gnAsPreset", "gnAsLna", "gnAsMixer", "gnAsVga", "gnAsLnaAgc", "gnAsMixerAgc"].forEach((id) => $(id).disabled = !on);
    $("gnAsPresetVal").textContent = $("gnAsPreset").value; $("gnAsLnaVal").textContent = $("gnAsLna").value; $("gnAsMixerVal").textContent = $("gnAsMixer").value; $("gnAsVgaVal").textContent = $("gnAsVga").value;
    $("gnAsLna").disabled = !on || $("gnAsLnaAgc").checked; $("gnAsMixer").disabled = !on || $("gnAsMixerAgc").checked;
    // Mirror the RTL gain into the legacy field the start command still reads.
    $("gain").value = !airspy && !$("gnRtlAgc").checked ? String(g[+$("gnRtlGain").value] ?? "") : "";
  }
  ["gnRtlAgc", "gnRtlGain", "gnAsEnabled", "gnAsMode", "gnAsPreset", "gnAsLna", "gnAsMixer", "gnAsVga", "gnAsLnaAgc", "gnAsMixerAgc"].forEach((id) => { $(id).oninput = gainUi; $(id).onchange = gainUi; });
  function gainFromUi(s) {
    const g = RTL_GAINS();
    s.gain = $("gnRtlAgc").checked ? null : (g[+$("gnRtlGain").value] ?? null);
    s.airspy_gain = $("gnAsEnabled").checked; s.airspy_mode = $("gnAsMode").value; s.airspy_preset = +$("gnAsPreset").value;
    s.airspy_lna = +$("gnAsLna").value; s.airspy_mixer = +$("gnAsMixer").value; s.airspy_vga = +$("gnAsVga").value; s.airspy_lna_agc = $("gnAsLnaAgc").checked; s.airspy_mixer_agc = $("gnAsMixerAgc").checked;
    return s;
  }
  function gainToUi(s) {
    const g = RTL_GAINS();
    $("gnRtlAgc").checked = s.gain == null;
    if (s.gain != null) { let best = 0; g.forEach((v, i) => { if (Math.abs(v - s.gain) < Math.abs(g[best] - s.gain)) best = i; }); $("gnRtlGain").value = best; } else $("gnRtlGain").value = g.length - 1;
    $("gnAsEnabled").checked = !!s.airspy_gain; $("gnAsMode").value = s.airspy_mode || "agc"; $("gnAsPreset").value = s.airspy_preset ?? 12;
    $("gnAsLna").value = s.airspy_lna ?? 8; $("gnAsMixer").value = s.airspy_mixer ?? 8; $("gnAsVga").value = s.airspy_vga ?? 8; $("gnAsLnaAgc").checked = !!s.airspy_lna_agc; $("gnAsMixerAgc").checked = !!s.airspy_mixer_agc;
    gainUi();
  }
  $("gnApply").onclick = async () => {
    if ($("pillText").textContent === "standby") { uiToast("Start first — live gain changes apply to the running radio.", "err"); return; }
    const s = gainFromUi({ ...(devView.settings[devKey()] || {}), nickname: $("dvNick").value.trim(), ppm: parseFloat($("ppm").value) || 0, rate: parseFloat($("dvRate").value) || 0 });
    try { $("gnMeta").textContent = await invoke("gain_live", { key: devKey(), settings: s }); devView.settings[devKey()] = s; devRenderList(); } catch (e) { uiToast(`${e}`, "err"); $("gnMeta").textContent = ""; }
  };

  window.devLoadSelected = function devLoadSelected() {
    const d = devView.devices.find((x) => `${x.kind}|${x.id}` === $("source").value);
    const s = devView.settings[devKey()] || {};
    $("dvSelMeta").textContent = d ? d.label : ($("source").value.includes("|") ? "not attached right now" : "no radio detected");
    $("dvNick").value = s.nickname || ""; $("ppm").value = s.ppm != null && s.ppm !== 0 ? String(s.ppm) : ""; gainToUi(s);
    const rates = d ? d.rates : (srcKind() === "airspy" ? [10000000, 2500000] : [2400000]);
    $("dvRate").innerHTML = `<option value="0">default for the mode</option>` + rates.map((r) => `<option value="${r}">${r >= 1e6 ? (r / 1e6).toFixed(1) + " M" : (r / 1e3) + " k"}</option>`).join("");
    $("dvRate").value = String(s.rate || 0);
    if (s.rate && $("pillText").textContent === "standby") { const o = [...$("rate").options].find((x) => +x.value === +s.rate); if (o) { $("rate").value = o.value; syncRate(); } }
  };
  async function devSave() {
    const id = devKey();
    const settings = gainFromUi({ ...(devView.settings[id] || {}), nickname: $("dvNick").value.trim(), ppm: parseFloat($("ppm").value) || 0, rate: parseFloat($("dvRate").value) || 0 });
    try { await invoke("devices_set", { id, settings }); devView.settings[id] = settings; devRenderList(); devRenderSource(); uiToast("Radio settings saved"); } catch (e) { uiToast(`${e}`, "err"); }
  }
  $("dvSave").onclick = devSave;
  function devRenderList() {
    $("dvEmpty").style.display = devView.devices.length ? "none" : "";
    $("dvMeta").textContent = devView.devices.length ? `${devView.devices.length} attached` : "";
    $("dvList").innerHTML = devView.devices.map((d) => { const k = `${d.kind}|${d.id}`, s = devView.settings[k] || {}; return `<div class="row ${$("source").value === k ? "on" : ""}" data-dev="${esc(k)}"><span class="grow"><b>${esc(s.nickname || d.label)}</b> <small>${d.kind === "airspy" ? "Airspy" : "RTL-SDR"} · ${esc(d.kind === "airspy" ? d.id.replace(/^0+/, "") : d.id)}</small><br><small>${s.ppm ? `${s.ppm} ppm` : "0 ppm"} · ${d.kind === "airspy" ? (s.airspy_gain ? `gain: ${s.airspy_mode || "agc"}${/linearity|sensitivity/.test(s.airspy_mode || "") ? " " + s.airspy_preset : ""}` : "gain: firmware default") : (s.gain != null ? s.gain + " dB" : "AGC")} · ${s.rate ? (s.rate / 1e6).toFixed(1) + " M" : "default rate"}</small></span><button class="btn ghost sm" data-devuse="${esc(k)}">Use</button></div>`; }).join("");
    $("dvList").querySelectorAll("[data-devuse]").forEach((b) => b.onclick = () => { $("source").value = b.dataset.devuse; $("source").onchange(); devRenderList(); showView("monitor"); });
  }
  async function devRefresh() {
    try { devView = await invoke("devices_list"); devRenderSource(); devRenderList(); if (typeof coveragePlan === "function") coveragePlan(); }
    catch (e) { log(`devices_list: ${e}`); }
  }
  $("dvRescan").onclick = devRefresh;
  window.devicesOnShow = () => { devRenderList(); devLoadSelected(); coveragePlan(); };
  devRefresh();

  /* ---------- band coverage: park the other radios over the rest of the site ---------- */
  const roles = store("hs.roles", {});   // "kind|id" → "cover" | "off"
  const usable = (d) => { const r = (devView.settings[`${d.kind}|${d.id}`] || {}).rate || d.rates[0]; return { rate: r, width: r * 0.8 / 1e6 }; };
  { const sp = store("hs.span", null); if (sp) { $("cpLo").value = sp[0]; $("cpHi").value = sp[1]; } $("cpEnabled").checked = store("hs.coverage", true) !== false; }
  $("cpLo").onchange = $("cpHi").onchange = () => { const lo = parseFloat($("cpLo").value), hi = parseFloat($("cpHi").value); if (Number.isFinite(lo) && Number.isFinite(hi)) save("hs.span", [lo, hi]); coveragePlan(); };
  $("cpEnabled").onchange = () => { save("hs.coverage", $("cpEnabled").checked); coveragePlan(); };
  // Greedy: the primary covers its slice; each covering radio, widest first,
  // takes the largest uncovered stretch it can, centred on it (or on the
  // stretch's start when the stretch is wider than the radio).
  window.coveragePlan = function coveragePlan() {
    const lo = parseFloat($("cpLo").value), hi = parseFloat($("cpHi").value);
    const primaryKey = $("source").value, pc = parseFreq($("center").value) / 1e6, pr = parseFloat($("rate").value);
    const others = devView.devices.filter((d) => `${d.kind}|${d.id}` !== primaryKey);
    $("cpRoles").innerHTML = others.length ? others.map((d) => { const k = `${d.kind}|${d.id}`, s = devView.settings[k] || {}; return `<div class="row"><span class="grow"><b>${esc(s.nickname || d.label)}</b> <small>${usable(d).width.toFixed(2)} MHz usable at ${(usable(d).rate / 1e6).toFixed(1)} M</small></span><select data-role="${esc(k)}" style="width:auto"><option value="cover" ${roles[k] !== "off" ? "selected" : ""}>cover</option><option value="off" ${roles[k] === "off" ? "selected" : ""}>off</option></select></div>`; }).join("") : '<div class="row"><span class="grow" style="color:var(--ink-faint)">No other radio attached.</span></div>';
    $("cpRoles").querySelectorAll("select[data-role]").forEach((sel) => sel.onchange = () => { roles[sel.dataset.role] = sel.value; save("hs.roles", roles); coveragePlan(); });
    const plan = [];
    if ($("cpEnabled").checked && Number.isFinite(lo) && Number.isFinite(hi) && hi > lo && Number.isFinite(pc) && Number.isFinite(pr)) {
      let gaps = [[lo, hi]];
      const cut = (a, b) => { gaps = gaps.flatMap(([x, y]) => (b <= x || a >= y) ? [[x, y]] : [[x, Math.min(y, a)], [Math.max(x, b), y]].filter(([p, q]) => q - p > 0.02)); };
      cut(pc - pr * 0.4 / 1e6, pc + pr * 0.4 / 1e6);
      const radios = others.filter((d) => roles[`${d.kind}|${d.id}`] !== "off").sort((a, b) => usable(b).width - usable(a).width);
      for (const d of radios) {
        if (!gaps.length) break;
        gaps.sort((a, b) => (b[1] - b[0]) - (a[1] - a[0]));
        const [x, y] = gaps[0], w = usable(d).width;
        const centre = (y - x) <= w ? (x + y) / 2 : x + w / 2;
        const k = `${d.kind}|${d.id}`, s = devView.settings[k] || {};
        plan.push({ source: d.kind, device: d.id, center: +(centre * 1e6).toFixed(0), rate: usable(d).rate, gain: s.gain ?? null, ppm: s.ppm || null, label: s.nickname || d.label, lo: centre - w / 2, hi: centre + w / 2 });
        cut(centre - w / 2, centre + w / 2);
      }
      $("cpPlan").innerHTML = [`primary ${(pc - pr * 0.4 / 1e6).toFixed(3)}–${(pc + pr * 0.4 / 1e6).toFixed(3)} MHz (control channel)`]
        .concat(plan.map((p) => `${esc(p.label)}: ${p.lo.toFixed(3)}–${p.hi.toFixed(3)} MHz (centre ${(p.center / 1e6).toFixed(4)})`))
        .concat(gaps.length ? [`<span style="color:var(--amber)">uncovered: ${gaps.map(([x, y]) => `${x.toFixed(3)}–${y.toFixed(3)}`).join(", ")} MHz — calls there are reported as out of band</span>`] : ["<span style=\"color:var(--clear)\">the whole span is covered</span>"]).join("<br>");
    } else { $("cpPlan").textContent = $("cpEnabled").checked ? "Pick a playlist (or type the site span) to plan coverage." : "Coverage off — only the primary radio's band is decoded."; }
    $("cpMeta").textContent = plan.length ? `${plan.length} extra radio${plan.length === 1 ? "" : "s"}` : "";
    return plan;
  };
  window.coverageExtras = () => coveragePlan().map(({ source, device, center, rate, gain, ppm, label }) => ({ source, device, center, rate, gain, ppm, label }));
  $("source").addEventListener("change", () => setTimeout(coveragePlan, 0));
  $("rate").addEventListener("change", () => setTimeout(coveragePlan, 0));

  /* ---------- alerts: editor, Telegram, Ollama, log ---------- */
  listen("alert", (e) => alertFired(e.payload));
  listen("alert_error", (e) => { logEvent(`alert failed: ${e.payload}`, "warn"); uiToast(`Alert failed: ${e.payload}`, "err"); });
  let akSettings = null, akSel = null;
  const akKindLabel = { keywords: "keywords", emergency: "emergency", talkgroup: "any call", unit: "radio" };
  function akRenderList() {
    const list = akSettings ? akSettings.alerts : [];
    $("akEmpty").style.display = list.length ? "none" : "";
    $("akMeta").textContent = list.length ? `${list.filter((a) => a.enabled).length} of ${list.length} enabled` : "";
    $("akList").innerHTML = list.map((a) => `<div class="row ${akSel === a.id ? "on" : ""}" data-ak="${esc(a.id)}"><span class="grow"><b>${esc(a.name)}</b> ${a.enabled ? "" : '<span class="badge enc">off</span>'}<br><small>${akKindLabel[a.trigger.kind] || a.trigger.kind}${a.trigger.keywords.length ? ": " + esc(a.trigger.keywords.slice(0, 4).join(", ")) + (a.trigger.keywords.length > 4 ? "…" : "") : ""} · TG ${a.trigger.tgs.length ? a.trigger.tgs.join(",") : "any"}${a.telegram ? " · Telegram" : ""}${a.ai_gate ? " · AI" : ""}</small></span><label class="check" style="margin:0" title="enabled"><input type="checkbox" data-aken="${esc(a.id)}" ${a.enabled ? "checked" : ""}></label></div>`).join("");
    $("akList").querySelectorAll(".row[data-ak]").forEach((r) => r.onclick = (e) => { if (e.target.closest("input")) return; akEdit(r.dataset.ak); });
    $("akList").querySelectorAll("input[data-aken]").forEach((c) => c.onchange = async () => { const a = akSettings.alerts.find((x) => x.id === c.dataset.aken); a.enabled = c.checked; await akPersist(); });
  }
  function akEdit(id) {
    const a = akSettings.alerts.find((x) => x.id === id); if (!a) return;
    akSel = id; akRenderList(); $("akEditor").style.display = "";
    $("akName").value = a.name; $("akKind").value = a.trigger.kind; $("akEnabled").checked = a.enabled;
    $("akTgs").value = a.trigger.tgs.join(", "); $("akKeywords").value = a.trigger.keywords.join("\n"); $("akUnits").value = a.trigger.units.join(", ");
    $("akMessage").value = a.message; $("akCooldown").value = a.cooldown_secs; $("akPrev").value = a.combine_prev; $("akWindow").value = a.combine_window_secs;
    $("akTelegram").checked = a.telegram; $("akAudio").checked = a.attach_audio; $("akTone").checked = a.tone; $("akAi").checked = a.ai_gate; $("akAiPrompt").value = a.ai_prompt;
    $("akEdMeta").textContent = a.trigger.kind === "keywords" ? "fires when the transcript arrives" : "fires when the call completes";
    akKindUi();
  }
  function akKindUi() { const k = $("akKind").value; $("akKwField").style.display = k === "keywords" ? "" : "none"; $("akUnitField").style.display = k === "unit" ? "" : "none"; $("akAiField").style.display = $("akAi").checked ? "" : "none"; }
  $("akKind").onchange = akKindUi; $("akAi").onchange = akKindUi;
  const nums = (v) => v.split(/[\s,;]+/).map((x) => parseInt(x, 10)).filter(Number.isFinite);
  function akRead() {
    const a = akSettings.alerts.find((x) => x.id === akSel); if (!a) return null;
    a.name = $("akName").value.trim(); a.enabled = $("akEnabled").checked;
    a.trigger = { kind: $("akKind").value, tgs: nums($("akTgs").value), units: nums($("akUnits").value), keywords: $("akKeywords").value.split(/[\n,;]+/).map((x) => x.trim()).filter(Boolean) };
    a.message = $("akMessage").value; a.cooldown_secs = parseInt($("akCooldown").value, 10) || 0; a.combine_prev = parseInt($("akPrev").value, 10) || 0; a.combine_window_secs = parseInt($("akWindow").value, 10) || 120;
    a.telegram = $("akTelegram").checked; a.attach_audio = $("akAudio").checked; a.tone = $("akTone").checked; a.ai_gate = $("akAi").checked; a.ai_prompt = $("akAiPrompt").value;
    return a;
  }
  async function akPersist() {
    akSettings.telegram.chat_id = $("tgChat").value.trim();
    akSettings.ollama = { url: $("olUrl").value.trim() || "http://localhost:11434", model: $("olModel").value, timeout_secs: parseInt($("olTimeout").value, 10) || 60, fail_open: $("olFailOpen").checked };
    try { await invoke("alerts_set", { settings: akSettings }); const v = await invoke("alerts_get"); akSettings = v.settings; akRenderList(); return true; } catch (e) { log(`alerts_set failed: ${e}`); uiToast(`Could not save alerts: ${e}`, "err"); return false; }
  }
  $("akNew").onclick = () => {
    const id = `a${Date.now()}`;
    akSettings.alerts.push({ id, name: "New alert", enabled: true, trigger: { kind: "keywords", keywords: [], tgs: [], units: [] }, message: "🚨 {alert}\n{tgname} (TG {tg}) · {unitname} · {time}\n{transcript}", cooldown_secs: 300, telegram: true, tone: true, attach_audio: true, combine_prev: 0, combine_window_secs: 120, ai_gate: false, ai_prompt: "" });
    akRenderList(); akEdit(id);
  };
  $("akSave").onclick = async () => { const a = akRead(); if (!a) return; if (await akPersist()) { uiToast("Alert saved"); akEdit(akSel); if (a.trigger.kind === "keywords" && !$("trEnabled").checked) uiToast("Keyword alerts need transcription — enable it in Settings → Transcription", "err"); } };
  $("akDelete").onclick = async () => { if (!(await uiConfirm("Delete this alert?", "Delete"))) return; akSettings.alerts = akSettings.alerts.filter((x) => x.id !== akSel); akSel = null; $("akEditor").style.display = "none"; await akPersist(); };
  $("akTest").onclick = async () => { if (!akRead()) return; if (!(await akPersist())) return; try { uiToast(await invoke("alerts_test", { id: akSel })); setTimeout(akLogRefresh, 4000); setTimeout(akLogRefresh, 15000); } catch (e) { uiToast(`Test failed: ${e}`, "err"); } };
  $("akPickTg").onclick = () => { const box = $("akTgPick"); if (box.style.display === "none") { const cur = new Set(nums($("akTgs").value)); box.innerHTML = alRows.length ? alRows.slice(0, 1500).map((r) => `<span class="chip ${cur.has(r.id) ? "on" : ""}" data-tg="${r.id}" title="${esc(r.description)}">${r.id} ${esc(r.alias)}</span>`).join("") : '<span class="faint">load a catalog first</span>'; box.querySelectorAll(".chip").forEach((c) => c.onclick = () => { const set = new Set(nums($("akTgs").value)); set.has(+c.dataset.tg) ? set.delete(+c.dataset.tg) : set.add(+c.dataset.tg); $("akTgs").value = [...set].sort((a, b) => a - b).join(", "); c.classList.toggle("on"); }); box.style.display = ""; } else box.style.display = "none"; };
  async function akLogRefresh() {
    try { const rows = await invoke("alerts_log"); $("akLogMeta").textContent = rows.length ? `${rows.length} recent` : "nothing fired yet"; $("akLog").innerHTML = rows.map((r) => `<tr><td class="mono">${new Date(r.at * 1000).toLocaleTimeString("en-US", { hour12: false })}</td><td>${esc(r.alert)}</td><td>${esc(r.tg_name)} <span class="faint mono">${r.tg}</span></td><td><span class="badge ${r.ok ? "clear" : "enc"}">${r.ok ? "sent" : "failed"}</span> <small>${esc(r.detail)}</small></td></tr>`).join(""); } catch (e) { log(`alerts_log: ${e}`); }
  }
  $("akLogRefresh").onclick = akLogRefresh;
  async function akRefresh() {
    try {
      const v = await invoke("alerts_get"); akSettings = v.settings;
      $("tgChat").value = v.settings.telegram.chat_id; $("tgToken").placeholder = v.has_token ? "saved in Keychain" : "123456:ABC-DEF…"; $("tgMeta2").textContent = v.has_token ? (v.settings.telegram.chat_id ? "configured" : "token saved — add a chat id") : "no token";
      $("olUrl").value = v.settings.ollama.url; $("olTimeout").value = v.settings.ollama.timeout_secs; $("olFailOpen").checked = v.settings.ollama.fail_open;
      if (v.settings.ollama.model) $("olModel").innerHTML = `<option value="${esc(v.settings.ollama.model)}">${esc(v.settings.ollama.model)}</option>`;
      akRenderList(); akLogRefresh(); olRefresh(true);
    } catch (e) { log(`alerts_get: ${e}`); }
  }
  async function olRefresh(quiet) {
    try { const models = await invoke("ollama_models", { url: $("olUrl").value.trim() || "http://localhost:11434" }); const cur = akSettings.ollama.model || $("olModel").value; $("olModel").innerHTML = '<option value="">—</option>' + models.map((m) => `<option value="${esc(m)}">${esc(m)}</option>`).join(""); $("olModel").value = models.includes(cur) ? cur : ""; $("olMeta").textContent = `${models.length} models`; }
    catch (e) { $("olMeta").textContent = "not reachable"; if (!quiet) uiToast(`Ollama: ${e}`, "err"); }
  }
  $("olRefresh").onclick = () => olRefresh(false);
  ["olModel", "olTimeout", "olFailOpen", "olUrl"].forEach((id) => $(id).onchange = akPersist);
  $("tgSave").onclick = async () => { try { if ($("tgToken").value.trim()) { await invoke("telegram_save", { token: $("tgToken").value.trim() }); $("tgToken").value = ""; } if (await akPersist()) { uiToast("Telegram settings saved"); akRefresh(); } } catch (e) { uiToast(`${e}`, "err"); } };
  $("tgTest").onclick = async () => {
    await $("tgSave").onclick();
    const id = `t${Date.now()}`;
    akSettings.alerts.push({ id, name: "Telegram test", enabled: true, trigger: { kind: "talkgroup", keywords: [], tgs: [], units: [] }, message: "✅ HoosierSDR can reach this chat. Last call: {tgname} (TG {tg}) · {time}", cooldown_secs: 0, telegram: true, tone: false, attach_audio: true, combine_prev: 0, combine_window_secs: 0, ai_gate: false, ai_prompt: "" });
    try { await invoke("alerts_set", { settings: akSettings }); uiToast(await invoke("alerts_test", { id })); } catch (e) { uiToast(`${e}`, "err"); }
    akSettings.alerts = akSettings.alerts.filter((a) => a.id !== id); await invoke("alerts_set", { settings: akSettings }).catch(() => {}); setTimeout(akLogRefresh, 5000);
  };
  window.alertsOnShow = () => { if (!akSettings) akRefresh(); else akLogRefresh(); cvRefresh(); cvStateRefresh(); };
  akRefresh();

  /* ---------- conversation rules: stitch, summarise, send ---------- */
  let cvView = null, cvSel = null;
  const CV_DEFAULT = { id: "", name: "", enabled: true, tgs: [], fixed_units: [], learn_fixed: true, end_gap_secs: 90, late_window_secs: 180, max_secs: 900, min_calls: 1,
    summary_prompt: "Summarise this EMS-to-hospital radio report for a clinician in two or three sentences: unit, patient age/sex, chief complaint, vitals or interventions mentioned, and ETA. Use only what was said; mark anything unclear as unclear.",
    message: "🏥 {rule} · {tgname}\n{summary}\n\n{unitnames} · {calls} transmissions · {duration} · {started}{revision}", chat_id: "", attach_audio: true, send_without_transcript: false };
  function cvRenderList() {
    const rules = cvView ? cvView.settings.rules : [];
    $("cvEmpty").style.display = rules.length ? "none" : ""; $("cvMeta").textContent = rules.length ? `${rules.filter((r) => r.enabled).length} of ${rules.length} enabled` : "";
    $("cvList").innerHTML = rules.map((r) => `<div class="row ${cvSel === r.id ? "on" : ""}" data-cv="${esc(r.id)}"><span class="grow"><b>${esc(r.name)}</b> ${r.enabled ? "" : '<span class="badge enc">off</span>'}<br><small>TG ${r.tgs.join(",") || "—"} · fixed ${r.fixed_units.length ? r.fixed_units.join(",") : (r.learn_fixed ? "learned" : "none")} · quiet ${r.end_gap_secs}s</small></span><label class="check" style="margin:0"><input type="checkbox" data-cven="${esc(r.id)}" ${r.enabled ? "checked" : ""}></label></div>`).join("");
    $("cvList").querySelectorAll(".row[data-cv]").forEach((row) => row.onclick = (e) => { if (e.target.closest("input")) return; cvEdit(row.dataset.cv); });
    $("cvList").querySelectorAll("input[data-cven]").forEach((c) => c.onchange = async () => { const r = cvView.settings.rules.find((x) => x.id === c.dataset.cven); r.enabled = c.checked; await cvPersist(); });
  }
  function cvEdit(id) {
    const r = cvView.settings.rules.find((x) => x.id === id); if (!r) return;
    cvSel = id; cvRenderList(); $("cvEditor").style.display = "";
    $("cvName").value = r.name; $("cvTgs").value = r.tgs.join(", "); $("cvEnabled").checked = r.enabled; $("cvFixed").value = r.fixed_units.join(", "); $("cvLearn").checked = r.learn_fixed;
    $("cvGap").value = r.end_gap_secs; $("cvLate").value = r.late_window_secs; $("cvMax").value = r.max_secs; $("cvPrompt").value = r.summary_prompt; $("cvMessage").value = r.message;
    $("cvChat").value = r.chat_id; $("cvMin").value = r.min_calls; $("cvAudio").checked = r.attach_audio; $("cvNoTr").checked = r.send_without_transcript;
    const proposed = Object.entries(cvView.proposed_fixed || {}).filter(([k]) => k.startsWith(id + ":")).flatMap(([k, units]) => units.map((u) => ({ tg: k.split(":")[1], u })));
    $("cvProposed").innerHTML = proposed.length ? "learned fixed IDs: " + proposed.map((p) => `<button class="btn ghost sm" data-cvadopt="${p.u}" title="TG ${p.tg}">${p.u} ✔ adopt</button>`).join(" ") : "";
    $("cvProposed").querySelectorAll("[data-cvadopt]").forEach((b) => b.onclick = () => { const set = new Set(nums($("cvFixed").value)); set.add(+b.dataset.cvadopt); $("cvFixed").value = [...set].join(", "); });
    $("cvEdMeta").textContent = `${r.tgs.length} talkgroups`;
  }
  function cvRead() {
    const r = cvView.settings.rules.find((x) => x.id === cvSel); if (!r) return null;
    r.name = $("cvName").value.trim(); r.tgs = nums($("cvTgs").value); r.enabled = $("cvEnabled").checked; r.fixed_units = nums($("cvFixed").value); r.learn_fixed = $("cvLearn").checked;
    r.end_gap_secs = parseInt($("cvGap").value, 10) || 90; r.late_window_secs = parseInt($("cvLate").value, 10) || 0; r.max_secs = parseInt($("cvMax").value, 10) || 900; r.summary_prompt = $("cvPrompt").value; r.message = $("cvMessage").value;
    r.chat_id = $("cvChat").value.trim(); r.min_calls = parseInt($("cvMin").value, 10) || 1; r.attach_audio = $("cvAudio").checked; r.send_without_transcript = $("cvNoTr").checked;
    return r;
  }
  async function cvPersist() {
    if (!cvView) { uiToast("Conversation rules did not load — reopen the Alerts tab", "err"); return false; }
    try { await invoke("conversations_set", { rules: cvView.settings.rules }); }
    catch (e) { log(`conversations_set failed: ${e} · payload ${JSON.stringify(cvView.settings.rules).slice(0, 300)}`); uiToast(`Could not save the conversation rule: ${e}`, "err"); return false; }
    try { cvView = await invoke("conversations_get"); } catch (e) { log(`conversations_get failed: ${e}`); }
    cvRenderList(); return true;
  }
  async function cvRefresh() { try { cvView = await invoke("conversations_get"); cvRenderList(); } catch (e) { log(`conversations_get: ${e}`); } }
  async function cvStateRefresh() {
    try {
      const st = await invoke("conversations_state");
      $("cvLiveEmpty").style.display = st.open.length ? "none" : ""; $("cvLiveMeta").textContent = st.open.length ? `${st.open.length} open` : "";
      $("cvLive").innerHTML = st.open.map((c) => { const units = [...new Set(c.pieces.filter((p) => !p.fixed).map((p) => p.unit_name || p.unit))].join(", "); const age = Math.max(0, Math.round(Date.now() / 1000 - c.last_at)); return `<div class="row"><span class="grow"><b>${esc(c.rule_name)}</b> · ${esc(c.tg_name)} <small>${esc(units) || "fixed party only"}</small><br><small>${c.pieces.length} transmissions · quiet ${age}s · ${c.busy ? "summarising…" : c.sent_at ? (c.dirty ? "reopened — will revise" : `sent${c.revision ? " (rev " + c.revision + ")" : ""}`) : "open"}${c.last_error ? ` · <span style="color:var(--enc)">${esc(c.last_error)}</span>` : ""}</small>${c.last_summary ? `<br><small>${esc(c.last_summary.slice(0, 160))}</small>` : ""}</span>${c.sent_at && !c.busy ? `<button class="btn ghost sm" data-cvresend="${c.key}" title="Summarise again and replace the Telegram message">resend</button>` : ""}</div>`; }).join("");
      $("cvLive").querySelectorAll("[data-cvresend]").forEach((b) => b.onclick = async () => { try { await invoke("conversation_resend", { key: +b.dataset.cvresend }); uiToast("Re-summarising…"); } catch (e) { uiToast(`${e}`, "err"); } });
      $("cvLog").innerHTML = st.log.map((l) => `<tr><td class="mono">${new Date(l.at * 1000).toLocaleTimeString("en-US", { hour12: false })}</td><td>${esc(l.rule)}<br><small>${esc(l.tg_name)}</small></td><td>${esc(l.units)} <small>· ${l.calls}${l.revision ? " · rev " + l.revision : ""}</small></td><td><span class="badge ${l.ok ? "clear" : "enc"}">${l.ok ? "sent" : "failed"}</span> <small title="${esc(l.summary)}">${esc(l.detail)}</small></td></tr>`).join("");
    } catch (e) { log(`conversations_state: ${e}`); }
  }
  listen("conversations", () => { if ($("view-alerts").style.display !== "none") cvStateRefresh(); });
  setInterval(() => { if ($("view-alerts").style.display !== "none") cvStateRefresh(); }, 10000);
  $("cvNew").onclick = () => { const id = `c${Date.now()}`; cvView.settings.rules.push({ ...CV_DEFAULT, id, name: "Hospitals" }); cvRenderList(); cvEdit(id); };
  $("cvSave").onclick = async () => { if (!cvRead()) return; if (await cvPersist()) { uiToast("Conversation rule saved"); cvEdit(cvSel); if (!$("trEnabled").checked) uiToast("Summaries need transcription — enable it in Settings → Transcription", "err"); if (!$("olModel").value) uiToast("Pick an Ollama model above for the summaries", "err"); } };
  $("cvDelete").onclick = async () => { if (!(await uiConfirm("Delete this conversation rule?", "Delete"))) return; cvView.settings.rules = cvView.settings.rules.filter((x) => x.id !== cvSel); cvSel = null; $("cvEditor").style.display = "none"; await cvPersist(); };
  $("cvTest").onclick = async () => { if (!cvRead()) return; if (!(await cvPersist())) return; try { uiToast(await invoke("conversation_test", { id: cvSel })); setTimeout(cvStateRefresh, 5000); setTimeout(cvStateRefresh, 30000); } catch (e) { uiToast(`Test failed: ${e}`, "err"); } };
  cvRefresh();

  /* ---------- file name template ---------- */
  async function fnRefresh() {
    try { const v = await invoke("names_get"); $("fnTemplate").value = v.settings.template; $("fnExample").textContent = v.example; $("fnTokens").innerHTML = v.tokens.map(([t, d]) => `<b>${esc(t)}</b> ${esc(d)}`).join(" · "); } catch (e) { log(`names_get: ${e}`); }
  }
  $("fnTemplate").oninput = async () => { try { $("fnExample").textContent = await invoke("names_preview", { template: $("fnTemplate").value }); } catch (_) {} };
  $("fnSave").onclick = async () => { try { $("fnExample").textContent = await invoke("names_set", { template: $("fnTemplate").value }); logEvent("file name template saved"); } catch (e) { alert(e); } };
  fnRefresh();

  /* ---------- script hook ---------- */
  const hkSettings = () => ({ enabled: $("hkEnabled").checked, command: $("hkCmd").value.trim(), timeout_secs: parseInt($("hkTimeout").value, 10) || 20, min_secs: parseFloat($("hkMin").value) || 0, emergency_only: $("hkEmg").checked });
  async function hkRefresh(fields = true) {
    try {
      const v = await invoke("hook_get"); const s = v.settings;
      if (fields) { $("hkEnabled").checked = s.enabled; $("hkCmd").value = s.command; $("hkTimeout").value = s.timeout_secs; $("hkMin").value = s.min_secs; $("hkEmg").checked = s.emergency_only; }
      const st = v.status;
      $("hkMeta").textContent = st.last_error ? `error: ${st.last_error}` : st.runs ? `${st.runs} run${st.runs === 1 ? "" : "s"} · ${st.failures} failed` : (s.enabled ? "armed" : "off");
      $("hkMeta").style.color = st.last_error ? "var(--enc)" : "";
      if (st.last_output && !st.last_error) $("hkOut").textContent = `last output: ${st.last_output}`;
    } catch (e) { log(`hook_get: ${e}`); }
  }
  $("hkSave").onclick = async () => { try { await invoke("hook_configure", { settings: hkSettings() }); $("hkMeta").textContent = "saved"; setTimeout(hkRefresh, 500); } catch (e) { alert(e); } };
  $("hkTest").onclick = async () => { $("hkOut").textContent = "running…"; try { $("hkOut").textContent = `test output: ${await invoke("hook_test", { settings: hkSettings() })}`; } catch (e) { $("hkOut").textContent = `test failed: ${e}`; } };
  listen("hook_error", (e) => logEvent(`script hook: ${e.payload}`, "warn"));
  hkRefresh(); setInterval(() => { if ($("view-settings").style.display !== "none") hkRefresh(false); }, 5000);

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
    // The Monitor's call history shows it too, and becomes searchable by it.
    const h = history.find((x) => x.id === id);
    if (h) { const td = h.el.querySelector("td.tr"); if (td) { td.textContent = text; td.title = text; } h.text += " " + text.toLowerCase(); applyHistFilter(); }
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
    let dest = $("exportDir").value.trim();
    if (!dest) { dest = `~/Desktop/hoosier-export-${new Date().toISOString().slice(0, 19).replace(/[:T]/g, "-")}`; $("exportDir").value = dest; }
    if (!cart.size) { uiToast("The cart is empty — tick calls in the Library or use 🛒 on a call.", "err"); return; }
    $("exportResult").textContent = "exporting…";
    try { const m = await invoke("library_export", { ids: [...cart.keys()], dest }); $("exportResult").textContent = `exported ${cart.size} calls → ${m}`; uiToast(`Exported ${cart.size} calls to ${dest}`); }
    catch (e) { $("exportResult").textContent = ""; uiToast(`Export failed: ${e}`, "err"); }
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
    if (!(await uiConfirm(`Delete unstarred calls older than ${d} days?`, "Delete"))) return;
    try { const n = await invoke("library_prune", { days: d }); alert(`${n} calls deleted`); libStatsRefresh(); } catch (e) { alert(e); }
  };
  trRefresh(); libStatsRefresh();

  /* ---------- bottom status strip ---------- */
  let lastStream = "—";
  async function sbTick() {
    try {
      const s = await invoke("sys_status");
      $("sb-cpu").textContent = `${s.cpu_app.toFixed(0)}% · ${s.cpu_total.toFixed(0)}% of ${s.cores}`;
      $("sb-cpu").classList.toggle("hot", s.cpu_app > 90 * s.cores || s.cpu_total > 90);
      $("sb-mem").textContent = `${(s.mem_app_mb / 1024).toFixed(2)} GB · ${(s.mem_used_mb / 1024).toFixed(1)} / ${(s.mem_total_mb / 1024).toFixed(0)} GB`;
      $("sb-disk").textContent = `${s.disk_free_gb.toFixed(1)} GB free of ${s.disk_total_gb.toFixed(0)}`;
      $("sb-disk").classList.toggle("hot", s.disk_free_gb < 5);
      $("sb-lib").textContent = `${s.library_calls} calls · ${s.library_minutes.toFixed(0)} min`;
      const up = s.uptime_secs; $("sb-up").textContent = `${Math.floor(up / 3600)}:${String(Math.floor(up / 60) % 60).padStart(2, "0")}:${String(up % 60).padStart(2, "0")}`;
      $("sb-state").textContent = $("pillText").textContent;
      $("sb-stream").textContent = lastStream;
    } catch (e) { log(`sys_status: ${e}`); }
  }
  sbTick(); setInterval(sbTick, 2000);

  /* ---------- RadioReference load progress ---------- */
  listen("rr_progress", (e) => {
    const p = e.payload, box = $("rrProg");
    if (p.step === "done" || p.step === "failed") { setTimeout(() => { box.style.display = "none"; }, p.step === "done" ? 800 : 0); $("rrProgBar").style.width = p.step === "done" ? "100%" : "0%"; $("rrProgText").textContent = p.step === "done" ? "loaded" : "failed"; return; }
    box.style.display = "";
    const pct = p.total ? Math.round((p.done / p.total) * 100) : 0;
    $("rrProgBar").style.width = `${Math.max(3, pct)}%`;
    $("rrProgText").textContent = `${p.step}${p.total ? ` (${p.done + 1}/${p.total})` : ""}`;
  });

  /* ---------- aliases tab ---------- */
  let alRows = [];
  let alSort = store("hs.alsort", { key: "id", dir: 1 });
  const alTicked = new Set();
  function alRowHtml(r) {
    const p = prio.get(r.id) || 50, c = colorOf(r.id), rule = ruleFor(r.id);
    const pol = (k, glyph, title) => `<button data-pol="${k}:${r.id}" class="${polAllows(k, r.id) ? "on-ok" : "off"}" title="${title}: ${polAllows(k, r.id) ? "yes" : "no"} — click to toggle">${glyph}</button>`;
    return `<tr class="${r.encrypted ? "enc" : ""}" data-tg="${r.id}" ${c ? `data-color="${c}" style="--tgc:${c}"` : ""}><td><input type="checkbox" data-tick="${r.id}" ${alTicked.has(r.id) ? "checked" : ""}></td><td class="mono">${r.id}</td><td>${esc(r.alias)}</td><td>${esc(r.description)}${rule ? ` <small class="faint" title="range rule">▸ ${esc(rule.name || rule.lo + "–" + rule.hi)}</small>` : ""}</td><td><small>${esc(r.category)}</small></td><td><small class="mono">${esc(srcLabel(r.source))}</small></td>` +
      `<td class="act">${pol("record", "●", "Record audio")}${pol("stream", "▶", "Stream live")}${pol("upload", "↑", "Upload to sharing services")}</td>` +
      `<td class="act"><button class="swatch" data-color="${r.id}" title="Colour — click to cycle" style="background:${c || "transparent"}"></button><button data-pri="${r.id}" class="${p === 10 ? "pri-h" : p === 90 ? "pri-l" : ""}">${p === 10 ? "★" : p === 90 ? "▽" : "☆"}</button>` +
      `<button data-bell="${r.id}" class="${bells.has(r.id) ? "bell" : ""}">🔔</button><button data-avoid="${r.id}" class="${avoidUntil.has(r.id) ? "on" : ""}">⏱</button><button data-lock="${r.id}" class="${lockout.has(r.id) || (rule && rule.lock) ? "on" : ""}">⊘</button></td></tr>`;
  }
  const srcLabel = (src) => src.replace(/^rr_(\d+)$/, (m, sid) => { const pl = (typeof playlists !== "undefined" ? playlists : []).find((p) => String(p.sid) === sid); return pl ? `${pl.system_name}` : `RR sid ${sid}`; }).replace(/^csv_user$/, "named by you").replace(/^csv_/, "CSV ");
  let alShown = [];
  function alRender() {
    const q = $("alFilter").value.trim().toLowerCase(), src = $("alSource").value;
    const shown = alRows.filter((r) => (!src || r.source === src) && (!q || `${r.id} ${r.alias} ${r.description} ${r.category} ${r.source}`.toLowerCase().includes(q)));
    const k = alSort.key, d = alSort.dir;
    shown.sort((a, b) => (typeof a[k] === "number" ? a[k] - b[k] : String(a[k]).localeCompare(String(b[k]), undefined, { numeric: true, sensitivity: "base" })) * d || a.id - b.id);
    alShown = shown;
    $("alBody").innerHTML = shown.slice(0, 3000).map(alRowHtml).join("");
    $("alEmpty").style.display = alRows.length ? "none" : "";
    $("alMeta").textContent = alRows.length ? (q || src ? `${shown.length} of ${alRows.length}` : `${alRows.length} talkgroups`) : "";
    document.querySelectorAll("#view-aliases th[data-sort]").forEach((th) => { th.classList.toggle("asc", th.dataset.sort === k && d > 0); th.classList.toggle("desc", th.dataset.sort === k && d < 0); });
    const tb = $("alBody");
    tb.querySelectorAll("input[data-tick]").forEach((c) => c.onchange = () => { c.checked ? alTicked.add(+c.dataset.tick) : alTicked.delete(+c.dataset.tick); $("grpMeta").textContent = alTicked.size ? `${alTicked.size} ticked` : ""; });
    tb.querySelectorAll("button[data-pol]").forEach((b) => b.onclick = () => { const [k, tg] = b.dataset.pol.split(":"); polSet(k, +tg, !polAllows(k, +tg)); pushPolicies(); alRender(); });
    tb.querySelectorAll("button[data-pri]").forEach((b) => b.onclick = () => { cyclePriority(+b.dataset.pri); alRender(); });
    tb.querySelectorAll("button[data-bell]").forEach((b) => b.onclick = () => { toggleBell(+b.dataset.bell); alRender(); });
    tb.querySelectorAll("button[data-avoid]").forEach((b) => b.onclick = () => { avoidFor(+b.dataset.avoid); alRender(); });
    tb.querySelectorAll("button[data-lock]").forEach((b) => b.onclick = () => { toggleLock(+b.dataset.lock); alRender(); });
    tb.querySelectorAll("button[data-color]").forEach((b) => b.onclick = () => { cycleColor(+b.dataset.color); alRender(); });
    renderCategoryChips();
  }
  $("alFilter").oninput = alRender;
  $("alSource").onchange = alRender;
  document.querySelectorAll("#view-aliases th[data-sort]").forEach((th) => th.onclick = () => { alSort = { key: th.dataset.sort, dir: alSort.key === th.dataset.sort ? -alSort.dir : 1 }; save("hs.alsort", alSort); alRender(); });
  document.querySelectorAll("[data-bulk]").forEach((b) => b.onclick = () => { const [k, v] = b.dataset.bulk.split(":"); alShown.forEach((r) => polSet(k, r.id, v === "on")); pushPolicies(); alRender(); uiToast(`${alShown.length} talkgroups: ${k} ${v}`); });
  $("alTickAll").onchange = () => { alShown.forEach((r) => $("alTickAll").checked ? alTicked.add(r.id) : alTicked.delete(r.id)); alRender(); $("grpMeta").textContent = alTicked.size ? `${alTicked.size} ticked` : ""; };
  window.renderGroupList = function renderGroupList() {
    $("grpList").innerHTML = groups.length ? groups.map((g) => `<div class="row"><span class="grow"><b>${esc(g.name)}</b> <small>${g.tgs.length} talkgroups · ${g.listen ? "listening" : "muted"}</small></span><button class="btn ghost sm" data-grptoggle="${esc(g.id)}">${g.listen ? "mute" : "listen"}</button><button class="btn ghost sm" data-grpshow="${esc(g.id)}" title="tick its members">show</button><button class="btn ghost sm" data-grpdel="${esc(g.id)}">✕</button></div>`).join("") : "";
    $("grpList").querySelectorAll("[data-grptoggle]").forEach((b) => b.onclick = () => { const g = groups.find((x) => x.id === b.dataset.grptoggle); g.listen = !g.listen; groupsSave(); });
    $("grpList").querySelectorAll("[data-grpshow]").forEach((b) => b.onclick = () => { const g = groups.find((x) => x.id === b.dataset.grpshow); alTicked.clear(); g.tgs.forEach((t) => alTicked.add(t)); $("alFilter").value = ""; alRender(); $("grpMeta").textContent = `${alTicked.size} ticked`; });
    $("grpList").querySelectorAll("[data-grpdel]").forEach((b) => b.onclick = async () => { if (!(await uiConfirm("Delete this group? (Talkgroups are not affected.)", "Delete"))) return; const i = groups.findIndex((x) => x.id === b.dataset.grpdel); groups.splice(i, 1); groupsSave(); });
  };
  $("grpMake").onclick = () => {
    const name = $("grpName").value.trim(); if (!name) { uiToast("Give the group a name (Hospitals, EMS/Fire…)", "err"); return; }
    if (!alTicked.size) { uiToast("Tick the talkgroups first.", "err"); return; }
    let g = groups.find((x) => x.name.toLowerCase() === name.toLowerCase());
    if (!g) { g = { id: `g${Date.now()}`, name, tgs: [], listen: true }; groups.push(g); }
    g.tgs = [...new Set([...g.tgs, ...alTicked])].sort((a, b) => a - b);
    alTicked.clear(); $("grpName").value = ""; groupsSave(); alRender(); uiToast(`Group “${g.name}”: ${g.tgs.length} talkgroups — mute or unmute it from the Monitor tab.`); $("grpMeta").textContent = "";
  };
  renderGroupList();
  function alSourcesRender() {
    const cur = $("alSource").value, srcs = [...new Set(alRows.map((r) => r.source))].sort();
    $("alSource").innerHTML = '<option value="">all sources</option>' + srcs.map((x) => `<option value="${esc(x)}">${esc(srcLabel(x))}</option>`).join("");
    $("alSource").value = srcs.includes(cur) ? cur : "";
  }

  /* ---------- service-type (category) filter for the live follow ---------- */
  const catSel = new Set(store("hs.cats", []));
  function renderCategoryChips() {
    const cats = [...new Set(alRows.map((r) => r.category).filter(Boolean))].sort();
    [...catSel].forEach((c) => { if (!cats.includes(c)) catSel.delete(c); });
    $("catChips").innerHTML = cats.length ? cats.map((c) => `<span class="chip ${catSel.has(c) ? "on" : ""}" data-cat="${esc(c)}">${esc(c)}</span>`).join("") : '<span class="faint">load a catalog to filter by service type</span>';
    $("catChips").querySelectorAll(".chip").forEach((ch) => ch.onclick = () => { const c = ch.dataset.cat; catSel.has(c) ? catSel.delete(c) : catSel.add(c); save("hs.cats", [...catSel]); renderCategoryChips(); pushCategoryAllowlist(); });
    $("catSummary").textContent = catSel.size ? `${catSel.size} of ${cats.length}` : "all";
  }
  // The effective allowlist is the playlist's talkgroups (if any) intersected
  // with the chosen categories. Re-pushed after every playlist activation,
  // since activation sets the backend allowlist to the playlist alone.
  async function pushCategoryAllowlist() {
    const pl = playlists.find((p) => p.id === $("playlist").value);
    const plSet = pl && pl.tgs.length ? new Set(pl.tgs) : null;
    if (!catSel.size) { await invoke("set_allowlist", { tgs: plSet ? [...plSet] : null }).catch((e) => log(`allowlist: ${e}`)); return; }
    const inCats = new Set(alRows.filter((r) => catSel.has(r.category)).map((r) => r.id));
    const tgs = plSet ? [...plSet].filter((t) => inCats.has(t)) : [...inCats];
    await invoke("set_allowlist", { tgs }).catch((e) => log(`allowlist: ${e}`));
    $("followMeta").textContent = `service types: ${[...catSel].join(", ")} → ${tgs.length} talkgroups`;
  }
  window.pushCategoryAllowlist = pushCategoryAllowlist;

  /* ---------- talkgroup range rules ---------- */
  $("rgColor").innerHTML = '<option value="">none</option>' + PALETTE.map((c) => `<option value="${c}" style="color:${c}">${c}</option>`).join("");
  window.renderRules = function renderRules() {
    $("rgList").innerHTML = tgRules.length ? tgRules.map((r, i) => `<div class="row"><span class="swatch" style="background:${r.color || "transparent"}"></span><span class="grow"><b>${esc(r.name || "")}</b> <span class="mono">${r.lo}–${r.hi}</span><br><small>${[r.pri === 10 ? "high priority" : r.pri === 90 ? "low priority" : "", r.lock ? "locked out" : "", r.bell ? "alert" : ""].filter(Boolean).join(" · ") || "colour only"}</small></span><button class="btn ghost" data-rgdel="${i}">✕</button></div>`).join("") : '<div class="row"><span class="grow" style="color:var(--ink-faint)">No range rules.</span></div>';
    $("rgList").querySelectorAll("[data-rgdel]").forEach((b) => b.onclick = () => { tgRules.splice(+b.dataset.rgdel, 1); saveRules(); alRender(); });
    $("rgMeta").textContent = tgRules.length ? `${tgRules.length} rule${tgRules.length === 1 ? "" : "s"}` : "";
  };
  $("rgAdd").onclick = () => {
    const lo = parseInt($("rgLo").value, 10), hi = parseInt($("rgHi").value, 10);
    if (!Number.isFinite(lo) || !Number.isFinite(hi) || lo < 0 || hi > 65535) { alert("Enter a talkgroup range, e.g. 10000 to 10999."); return; }
    tgRules.push({ lo: Math.min(lo, hi), hi: Math.max(lo, hi), name: $("rgName").value.trim(), pri: +$("rgPri").value || null, color: $("rgColor").value || "", lock: $("rgLock").checked, bell: $("rgBell").checked });
    ["rgLo", "rgHi", "rgName"].forEach((id) => $(id).value = ""); $("rgLock").checked = $("rgBell").checked = false;
    saveRules(); alRender();
  };
  renderRules();

  /* ---------- radio-ID wildcard rules ---------- */
  let unitRules = [];
  async function urRender() {
    try { unitRules = await invoke("unit_rules_list"); } catch (e) { log(`unit_rules_list: ${e}`); }
    $("urList").innerHTML = unitRules.length ? unitRules.map((r, i) => `<div class="row"><span class="grow"><span class="mono">${esc(r.pattern)}</span> → ${esc(r.name)}</span><button class="btn ghost" data-urdel="${i}">✕</button></div>`).join("") : '<div class="row"><span class="grow" style="color:var(--ink-faint)">No wildcard rules. Regex rows in an imported CSV land here too.</span></div>';
    $("urList").querySelectorAll("[data-urdel]").forEach((b) => b.onclick = async () => { unitRules.splice(+b.dataset.urdel, 1); try { await invoke("unit_rules_set", { rules: unitRules }); } catch (e) { alert(e); } urRender(); });
  }
  $("urAdd").onclick = async () => {
    const pattern = $("urPat").value.trim(), name = $("urName").value.trim(); if (!pattern || !name) return;
    try { await invoke("unit_rules_set", { rules: [...unitRules, { pattern, name }] }); $("urPat").value = ""; $("urName").value = ""; urRender(); } catch (e) { alert(e); }
  };
  $("urTry").oninput = async () => { const id = parseInt($("urTry").value, 10); if (!Number.isFinite(id)) { $("urTryOut").textContent = ""; return; } try { const n = await invoke("unit_resolve", { id }); $("urTryOut").textContent = n ? `${id} → ${n}` : `${id} → no alias or rule matches`; } catch (_) {} };
  urRender();
  async function srcRender() {
    try {
      const list = await invoke("catalogs_list");
      $("srcMeta").textContent = list.length ? `${list.length} source${list.length === 1 ? "" : "s"}` : "none";
      $("srcList").innerHTML = list.length ? list.map((s) => `<div class="row"><span class="grow"><b>${esc(s.name.replace(/^rr_/, "RadioReference sid ").replace(/^csv_/, "CSV: "))}</b><br><small>${s.talkgroups} talkgroups</small></span><button class="btn ghost" data-rmsrc="${esc(s.name)}">Remove</button></div>`).join("")
        : '<div class="row"><span class="grow" style="color:var(--ink-faint)">Nothing loaded yet.</span></div>';
      $("srcList").querySelectorAll("[data-rmsrc]").forEach((b) => b.onclick = async () => { if (!(await uiConfirm(`Remove ${b.dataset.rmsrc}?`, "Remove"))) return; try { const n = await invoke("catalog_remove", { name: b.dataset.rmsrc }); $("loadcat").textContent = n ? n + " TGs" : "Load"; aliasesRefresh(); } catch (e) { alert(e); } });
    } catch (e) { log(`catalogs_list: ${e}`); }
  }
  async function aliasesRefresh() {
    try { alRows = await invoke("catalog_rows"); alSourcesRender(); alRender(); srcRender(); $("r-names").textContent = new Set(alRows.map((r) => r.id)).size || "—"; } catch (e) { log(`catalog_rows: ${e}`); }
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
  $("rrDownload").onclick = () => { const sid = sidVal(); if (sid == null) { alert("Enter a system ID."); return; } $("rrProg").style.display = ""; $("rrProgBar").style.width = "3%"; $("rrProgText").textContent = "connecting…"; loadSystem(sid); };
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
    if (!sys) return [];
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
  $("tgNone").onclick = () => { picked = new Set(); if (sys) renderTgs(); };
  $("plSave").onclick = async () => {
    if (!sys || !pickedSite) { alert("Load a system and pick a site first."); return; }
    const lo = pickedSite.span_mhz ? pickedSite.span_mhz[0] : pickedSite.control_mhz[0];
    const hi = pickedSite.span_mhz ? pickedSite.span_mhz[1] : pickedSite.control_mhz[0];
    const playlist = { id: "", name: $("plName").value.trim(), sid: sys.sid, system_name: sys.name,
      site_id: pickedSite.site_id, site_name: pickedSite.name, nac: pickedSite.nac,
      control_mhz: pickedSite.control_mhz[0], center_mhz: +((lo + hi) / 2).toFixed(4),
      rate: siteRate(pickedSite), tgs: [...picked].sort((a, b) => a - b), span_mhz: pickedSite.span_mhz ? [pickedSite.span_mhz[0], pickedSite.span_mhz[1]] : null };
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
      if (!(await uiConfirm("Delete this playlist?", "Delete"))) return;
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
      if (typeof pushCategoryAllowlist === "function") pushCategoryAllowlist();
      if (p) {
        modeSel = "follow"; setSeg($("modeSeg"), "follow"); applyMode();
        $("freq").value = p.control_mhz.toFixed(4) + "M"; $("center").value = p.center_mhz.toFixed(4) + "M";
        if (p.span_mhz) { $("cpLo").value = p.span_mhz[0].toFixed(4); $("cpHi").value = p.span_mhz[1].toFixed(4); save("hs.span", p.span_mhz); if (typeof coveragePlan === "function") coveragePlan(); }
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
    if (tick > 40 && tick % 15 === 0) {
      const pts = []; for (let i = 0; i < 300; i++) { const k = Math.floor(rnd(0, 8)), a = k * Math.PI / 4 + rnd(-0.12, 0.12), r = 1 + rnd(-0.08, 0.08); pts.push([r * Math.cos(a), r * Math.sin(a)]); }
      handleFollow({ kind: "constellation", modulation: "CQPSK", points: pts });
    }
    if (tick > 40 && tick % 20 === 0) { const [tg, name] = pick(TGS.concat([[10999, "TG 10999"]])); handleFollow({ kind: "grant", tg, name, named: tg !== 10999, freq_mhz: pick(DLS), unit: Math.floor(rnd(4910000, 4914000)), encrypted: false }); }
    if (tick > 40 && tick % 45 === 0) { const [tg, name] = pick(TGS); handleFollow({ kind: "mobility", what: pick(["affiliated", "registered", "located"]), unit: Math.floor(rnd(4910000, 4910020)), unit_name: null, tg, name }); }
    if (tick === 120) handleFollow({ kind: "location", unit: 4910007, unit_name: "Car 12", lat: 39.7684 + rnd(-0.02, 0.02), lon: -86.1581 + rnd(-0.02, 0.02) });
    if (tick === 200) handleFollow({ kind: "talker_alias", tg: 10147, name: "IFD Fire Dispatch", alias: "ENG 21" });
    if (tick === 41) handleFollow({ kind: "site", nac: 0x260, wacn: 0xBEE00, sys_id: 0x6BD, control_mhz: 851.5375, alternates_mhz: [851.2125], idens: [[1, 851.00625, 6.25]], patches: [], rfss: 1, site: 12, neighbours: [[0x6BD, 1, 13, 856.2375], [0x6BD, 1, 14, null]] });
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
  // `index.html?demo` seeds every panel synchronously (for screenshots and
  // layout checks without a backend).
  if (location.search.includes("demo")) {
    setState("following");
    handleFollow({ kind: "measured", control_mhz: 851.5375, modulation: "CQPSK", correction_hz: -412, rate: 9600000, center_mhz: 855, ppm: 0.48 });
    handleFollow({ kind: "site", nac: 0x260, wacn: 0xBEE00, sys_id: 0x6BD, control_mhz: 851.5375, alternates_mhz: [851.2125], idens: [[1, 851.00625, 6.25]], patches: [[957, [10203, 10204]]], rfss: 1, site: 12, neighbours: [[0x6BD, 1, 13, 856.2375], [0x6BD, 1, 14, null]] });
    handleFollow({ kind: "spectrum", bins_db: specRow() });
    const pts = []; for (let i = 0; i < 400; i++) { const k = Math.floor(rnd(0, 8)), a = k * Math.PI / 4 + rnd(-0.1, 0.1), r = 1 + rnd(-0.06, 0.06); pts.push([r * Math.cos(a), r * Math.sin(a)]); }
    handleFollow({ kind: "constellation", modulation: "CQPSK", points: pts });
    TGS.forEach(([tg, name], i) => { handleFollow({ kind: "grant", tg, name, named: true, freq_mhz: DLS[i % DLS.length], unit: 4910000 + i, encrypted: false }); });
    [10999, 11042].forEach((tg) => handleFollow({ kind: "grant", tg, name: `TG ${tg}`, named: false, freq_mhz: 857.3625, unit: 4911111, encrypted: tg === 11042 }));
    handleFollow({ kind: "call_start", tg: 10147, name: "IFD Fire Dispatch", freq_mhz: 857.3875, priority: 10 });
    handleFollow({ kind: "call", tg: 10103, name: "IMPD Dispatch NW", source: 4910003, unit_name: "Car 12", talker_alias: "ENG 21", freq_mhz: 851.8125, modulation: "CQPSK", secs: 6.4, wav: null, emergency: false, patched_with: [] });
    handleFollow({ kind: "call", tg: 10308, name: "Sheriff Patrol", source: 4910008, freq_mhz: 858.3375, modulation: "CQPSK", secs: 3.1, wav: null, emergency: true, patched_with: [10204] });
    [["affiliated", 4910003, 10103], ["registered", 4910008, null], ["located", 4910011, 10147], ["refused", 4910012, 10202]].forEach(([what, unit, tg]) => handleFollow({ kind: "mobility", what, unit, unit_name: unit === 4910003 ? "Car 12" : null, tg, name: tg ? (TGS.find((t) => t[0] === tg) || [])[1] : null }));
    handleFollow({ kind: "location", unit: 4910003, unit_name: "Car 12", lat: 39.7684, lon: -86.1581 });
    handleFollow({ kind: "location", unit: 4910011, unit_name: null, lat: 39.79, lon: -86.17 });
    handleFollow({ kind: "talker_alias", tg: 10147, name: "IFD Fire Dispatch", alias: "ENG 21" });
    handleFollow({ kind: "status", control_syncs: 412, calls: 2, out_of_band: 3, encrypted: 1, locked: 0, msps: 9.6, want_msps: 9.6, dropped: 0, elapsed_secs: 42 });
    if (!groups.length) { groups.push({ id: "gdemo1", name: "Hospitals", tgs: [10202], listen: true }, { id: "gdemo2", name: "EMS / Fire", tgs: [10147, 10202], listen: false }); renderGroupChips(); }
    { const h = history[0]; if (h) { const td = h.el.querySelector("td.tr"); td.textContent = "Engine 21 on scene, working structure fire, requesting second alarm."; } }
    colors.set(10147, "#f5b544"); tgRules.push({ lo: 10100, hi: 10199, name: "IMPD", pri: 10, color: "#7aa2ff", lock: false, bell: false });
    if (location.hash === "#discovery") renderDiscovery();
  }
}
