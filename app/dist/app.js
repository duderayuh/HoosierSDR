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
// Schemes that read as light: the map tiles, the ◐ toggle and anything
// else that asks "is this dark?" go by this, not by the name "light".
const LIGHT_THEMES = new Set(["light", "sepia", "snow", "valentine"]);
const isLightTheme = (t) => t ? LIGHT_THEMES.has(t) : !matchMedia("(prefers-color-scheme: dark)").matches;
$("theme").onclick = () => {
  const root = document.documentElement;
  const next = isLightTheme(root.getAttribute("data-theme")) ? "dark" : "light";
  if (typeof applyTheme === "function") applyTheme(next); else root.setAttribute("data-theme", next);
  document.querySelectorAll("#themeChips [data-th]").forEach((x) => x.classList.toggle("on", x.dataset.th === next));
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
// A modal with arbitrary content. Returns the wrapper; call close() to dismiss.
function uiModal(html, opts) {
  const wrap = document.createElement("div"); wrap.className = "modal-wrap";
  wrap.innerHTML = `<div class="modal ${opts && opts.wide ? "wide" : ""}"></div>`;
  wrap.firstChild.innerHTML = html;
  wrap.close = () => wrap.remove();
  wrap.onclick = (e) => { if (e.target === wrap) wrap.close(); };
  wrap.onkeydown = (e) => { if (e.key === "Escape") wrap.close(); };
  document.body.appendChild(wrap);
  return wrap;
}
window.alert = (m) => uiToast(m, /error|fail|could not|not found|invalid|enter |choose |no /i.test(String(m)) ? "err" : "");
function wireSeg(el, onPick) {
  el.querySelectorAll("button").forEach((b) => {
    b.onclick = () => { setSeg(el, b.dataset.v); onPick(b.dataset.v); };
  });
}
function setSeg(el, v) { el.querySelectorAll("button").forEach((x) => x.setAttribute("aria-pressed", String(x.dataset.v === v))); }

/* ---------- views ---------- */
const VIEWS = ["monitor", "library", "tripwires", "conversations", "dispatch", "settings"];
function showView(v) {
  VIEWS.forEach((n) => { $("view-" + n).style.display = n === v ? "" : "none"; });
  if (v === "dispatch" && typeof dispatchOnShow === "function") dispatchOnShow();
  if (v === "library" && typeof libOnShow === "function") libOnShow();
  if (v === "conversations" && typeof conversationsOnShow === "function") conversationsOnShow();
  if (v === "tripwires" && typeof window.tripwiresOnShow === "function") window.tripwiresOnShow();
  // Coming back to Settings refreshes whichever page was left open.
  if (v === "settings") setPage(curPage);
  setSeg($("navSeg"), v);
}
$("navSeg").querySelectorAll("button").forEach((b) => b.onclick = () => showView(b.dataset.v));

/* ---------- settings sidebar (left-bar pages) ---------- */
// Pages that load something when shown. Playlists, Aliases, Devices and
// Discovery were top-level tabs once; their code
// still refreshes only while on screen, via settingsPageVisible().
const PAGE_HOOKS = { discovery: "discoveryOnShow", connections: "connectionsOnShow", library: "libraryOnShow", devices: "devicesOnShow", aliases: "aliasesOnShow", remote: "remoteOnShow" };
let curPage = "appearance";
function setPage(p) {
  const nav = $("setNav");
  if (!nav) return;
  curPage = p;
  nav.querySelectorAll("button").forEach((b) => b.classList.toggle("on", b.dataset.p === p));
  document.querySelectorAll("#setPages .settings-page").forEach((pg) => pg.style.display = pg.id === "set-" + p ? "" : "none");
  const h = PAGE_HOOKS[p]; if (h && typeof window[h] === "function") window[h]();
}
// A Settings page is on screen only when Settings is and it is the open page.
function settingsPageVisible(p) { const pg = $("set-" + p); return $("view-settings").style.display !== "none" && !!pg && pg.style.display !== "none"; }
function discoveryVisible() { return settingsPageVisible("discovery"); }
$("setNav").querySelectorAll("button").forEach((b) => b.onclick = () => setPage(b.dataset.p));
setPage("appearance");

setTimeout(() => {
  const m = /^#settings(?:\/(\w+))?/.exec(location.hash);
  if (m) { showView("settings"); if (m[1]) setPage(m[1]); return; }
  const page = location.hash.slice(1);
  if (["discovery", "playlists", "aliases", "connections", "devices"].includes(page)) { showView("settings"); setPage(page); return; }
  // The old Alerts and Analyzers pages are Tripwires now.
  if (["alerts", "analyzers", "tripwires"].includes(page)) { showView("tripwires"); return; }
  if (["library", "conversations", "dispatch"].includes(page)) showView(page);
}, 0);

/* ---------- tuning state ---------- */
let modeSel = "follow", modSel = "cqpsk", eqSel = "bypass", decoderSel = "p25", squelchVal = 0.3;
let measuredPpm = null;
const ppmVal = () => { const v = parseFloat($("ppm").value); return Number.isFinite(v) ? v : null; };
function applyMode() {
  const follow = modeSel === "follow" || modeSel === "dual";
  $("centerField").style.display = follow ? "" : "none";
  $("tmodField").style.display = follow ? "" : "none";
  $("modField").style.display = follow ? "none" : "";
  $("eqField").style.display = follow ? "none" : "";
  $("chanReadouts").style.display = follow ? "none" : "";
  $("followOpts").style.display = follow ? "" : "none";
  $("channelOpts").style.display = follow ? "none" : "";
  $("freqHint").textContent = follow ? "control channel" : "channel";
  const dual = modeSel === "dual";
  if (dual) { $("centerField").style.display = "none"; $("tmodField").style.display = "none"; $("modField").style.display = ""; $("eqField").style.display = "none"; }
  $("voiceSrcField").style.display = dual ? "" : "none";
  $("emptyHint").textContent = follow ? "Pick a playlist or set a control channel, then press Start." : "Set a channel and press Start. One-channel mode decodes one channel and counts voice but does not play it; on a control channel it only announces grants (Discovery, Events) — use Follow site to listen.";
  applyDecoder();
}
// The Decoder picker only applies to one-channel mode. Choosing a non-P25
// analog decoder hides the P25-only modulation/equalizer selectors and, for the
// FM decoders, reveals the squelch slider.
function applyDecoder() {
  const channel = modeSel === "channel";
  const analog = decoderSel !== "p25";
  $("decoderField").style.display = channel ? "" : "none";
  if (channel && analog) {
    $("modField").style.display = "none";
    $("eqField").style.display = "none";
  }
  // Squelch applies to the FM-based analog decoders (nbfm, dcs).
  const fm = analog && decoderSel !== "am";
  $("squelchField").style.display = channel && fm ? "" : "none";
}
wireSeg($("modeSeg"), (v) => { modeSel = v; applyMode(); });
wireSeg($("modSeg"), (v) => { modSel = v; });
wireSeg($("eqSeg"), (v) => { eqSel = v; $("r-eq").textContent = v === "bypass" ? "BARE" : v.toUpperCase(); });
$("decoder").onchange = () => { decoderSel = $("decoder").value; applyDecoder(); };
$("squelch").oninput = () => { squelchVal = parseFloat($("squelch").value); $("squelchMeta").textContent = `${Math.round(squelchVal * 100)}%`; };
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
let lastTg = null, lastPl = "";   // the talkgroup on the air and the playlist its run follows
function activeStart(ev) {
  const key = activeKey(ev.tg, ev.freq_mhz);
  lastTg = ev.tg; lastPl = runPl(ev); updateHoldBtn();
  if (activeCalls.has(key)) return;
  const el = document.createElement("div");
  el.className = "call" + (ev.priority && ev.priority < 50 ? " pri" : "");
  el.innerHTML = `<span class="tg">${esc(ev.name)}</span><span class="t">0:00</span><span class="sub">${ev.desc ? esc(ev.desc) + " · " : ""}TG ${ev.tg} · ${ev.freq_mhz.toFixed(4)}</span>${runLabel(ev) ? `<span class="sys">${esc(runLabel(ev))}</span>` : ""}`;
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

/* ---------- hold: one talkgroup, on the playlist it was heard on (every site following that playlist narrows; other systems carry on) ---------- */
let holdTg = null, holdPl = "";
function updateHoldBtn() {
  const b = $("holdBtn");
  b.classList.toggle("on", holdTg != null);
  b.textContent = holdTg != null ? `Hold TG ${holdTg}` : (lastTg != null ? `Hold TG ${lastTg}` : "Hold");
  b.title = holdTg != null && plName(holdPl) ? `Holding TG ${holdTg} on ${plName(holdPl)}` : "Follow only the talkgroup on the air / last heard";
  b.disabled = holdTg == null && lastTg == null;
}
$("holdBtn").onclick = () => {
  const releasing = holdTg != null, pl = releasing ? holdPl : lastPl;
  holdTg = releasing ? null : lastTg; holdPl = pl;
  updateHoldBtn();
  if (TAURI) invoke("set_hold", { tg: holdTg, playlist: pl || null }).catch((e) => alert(e));
  logEvent(holdTg != null ? `hold on TG ${holdTg}${plName(pl) ? " · " + plName(pl) : ""}` : "hold released");
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
    `<td class="time">${g.at ? new Date(g.at * 1000).toLocaleTimeString("en-US", { hour12: false }) : now()}</td>` +
    `<td class="tg">${esc(g.name)}<span class="num">TG ${g.tg}${g.site_name ? ` · ${esc(g.site_name)}` : ""}</span>${g.service ? `<span class="svc">${esc(g.service)}</span>` : ""}${g.category ? `<span class="cat">${esc(g.category)}</span>` : ""}${g.desc ? `<span class="desc">${esc(g.desc)}</span>` : ""}</td>` +
    `<td class="src">${g.unit_name ? `${esc(g.unit_name)}<span class="num" style="display:block;font-size:10.5px;color:var(--ink-faint)">${g.source}</span>` : (g.source ? g.source : "—")}${g.talker_alias ? `<span class="alias" style="display:block" title="alias broadcast over the air">“${esc(g.talker_alias)}”</span>` : ""}</td>` +
    `<td class="tr" data-trid="${g.id != null ? g.id : ""}" title="${esc(g.transcript || "")}">${g.transcript ? esc(g.transcript) : `<span class="faint">${g.id != null ? "…" : ""}</span>`}</td>` +
    `<td class="dl">${g.freq_mhz.toFixed(4)}</td>` +
    `<td class="len">${len}</td>` +
    `<td>${g.encrypted ? '<span class="badge enc">Encrypted</span>' : g.emergency ? '<span class="badge emg">EMERGENCY</span>' : (g.secs === 0 || g.modulation === "?") ? `<span class="badge" title="granted, but no voice frame decoded — syncs C4FM ${g.syncs_c4fm ?? 0} / CQPSK ${g.syncs_cqpsk ?? 0}">no audio · ${g.syncs_c4fm ?? 0}/${g.syncs_cqpsk ?? 0}</span>` : `<span class="badge clear">${g.modulation || "clear"}</span>`}${g.patched_with && g.patched_with.length ? ` <span class="badge clear" title="patched with">⛓ ${g.patched_with.length}</span>` : ""}</td>` +
    `<td class="act">` +
      (g.wav ? `<button title="Replay" data-wav="${esc(g.wav)}">▶</button>` : "") +
      (g.id != null ? `<button title="Add to cart" data-cart="${g.id}" class="${cart.has(g.id) ? "on" : ""}">🛒</button>` : "") +
      `<input type="number" class="priin" data-pri="${g.tg}" data-pl="${esc(g.pl || "")}" min="1" max="99" value="${priOf(g.tg, g.pl)}" title="Priority 1–99 (1 = highest)">` +
      `<button title="Alert tone for TG ${g.tg}" data-bell="${g.tg}">🔔</button>` +
      `<button title="Avoid TG ${g.tg} for a while" data-avoid="${g.tg}" data-pl="${esc(g.pl || "")}">⏱</button>` +
      `<button title="Lock out TG ${g.tg}${plName(g.pl) ? " on " + esc(plName(g.pl)) : ""}" data-lock="${g.tg}" data-pl="${esc(g.pl || "")}">⊘</button>` +
    `</td>`;
  if (g.emergency) tr.classList.add("emg");
  if (g.secs === 0 || g.modulation === "?") tr.classList.add("noaudio");
  applyColor(tr, g.tg);
  tr.querySelectorAll("button[data-wav]").forEach((b) => b.onclick = () => replay(b.dataset.wav));
  tr.querySelectorAll("button[data-lock]").forEach((b) => b.onclick = () => toggleLock(+b.dataset.lock, b.dataset.pl));
  tr.querySelectorAll("button[data-avoid]").forEach((b) => b.onclick = () => avoidFor(+b.dataset.avoid, b.dataset.pl));
  tr.querySelectorAll("input[data-pri]").forEach((b) => b.onchange = () => { setPriority(+b.dataset.pri, b.value, b.dataset.pl); });
  tr.querySelectorAll("button[data-bell]").forEach((b) => b.onclick = () => toggleBell(+b.dataset.bell));
  tr.querySelectorAll("button[data-cart]").forEach((b) => b.onclick = () => cartToggle(+b.dataset.cart, `${now()} ${g.name} · ${g.secs != null ? g.secs.toFixed(1) + "s" : ""}`));
  const text = `${g.name} ${g.desc || ""} ${g.service || ""} ${g.category || ""} ${g.system || ""} ${g.site_name || ""} ${g.tg} ${g.source || ""} ${g.unit_name || ""} ${g.talker_alias || ""} ${g.freq_mhz.toFixed(4)} ${g.transcript || ""}`.toLowerCase();
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

/* ---------- theme ---------- */
// [id, label, swatch colour]. The seasonal ones are picked by hand, not by
// the calendar; the last two are for the Hoosier in HoosierSDR.
const THEMES = [
  ["dark", "Slate", "#34e0cf"], ["light", "Paper", "#0b988c"], ["amber", "Amber", "#ffb347"], ["terminal", "Terminal", "#4dff88"],
  ["midnight", "Midnight", "#7c9bff"], ["solarized", "Solarized", "#2aa198"], ["nord", "Nord", "#88c0d0"], ["dracula", "Dracula", "#bd93f9"],
  ["gruvbox", "Gruvbox", "#fabd2f"], ["sepia", "Sepia", "#8a5a1b"],
  ["snow", "❄ Snow", "#1f6fb2"], ["valentine", "♥ Valentine", "#d0325f"], ["clover", "☘ Clover", "#3ddc84"], ["harvest", "🍂 Harvest", "#e3762e"],
  ["haunt", "🎃 Haunt", "#ff7a1a"], ["yuletide", "🎄 Yuletide", "#e63946"],
  ["indycar", "🏎 IndyCar", "#e4002b"], ["brickyard", "🏁 Brickyard", "#d4a24c"],
];
function applyTheme(name) { document.documentElement.setAttribute("data-theme", name); save("hs.theme", name); }
(function initTheme() {
  const saved = store("hs.theme", "dark");
  document.documentElement.setAttribute("data-theme", saved);
  const chips = $("themeChips");
  if (!chips) return;
  chips.innerHTML = THEMES.map(([v, label, sw]) => `<span class="chip ${saved === v ? "on" : ""}" data-th="${v}"><i class="sw" style="background:${sw}"></i>${label}</span>`).join("");
  chips.querySelectorAll("[data-th]").forEach((c) => c.onclick = () => { applyTheme(c.dataset.th); chips.querySelectorAll("[data-th]").forEach((x) => x.classList.toggle("on", x.dataset.th === c.dataset.th)); });
  const help = $("themeHelp"); if (help) help.textContent = `${THEMES.length} colour schemes, including the seasons and the Speedway; Amber and Terminal switch to a monospace type throughout for a phosphor-CRT look. Changes apply instantly.`;
})();

/* ---------- fonts (override the scheme's type) ---------- */
// Set as inline custom properties on <html>, which beat the scheme blocks
// in style.css, so a choice here holds across every theme; "Theme default"
// removes it and the scheme's own type returns.
const FONT_STACKS = {
  body: { plex: '"IBM Plex Sans", system-ui, sans-serif', inter: '"Inter", system-ui, sans-serif', grotesk: '"Space Grotesk", system-ui, sans-serif', serif: '"Source Serif 4", Georgia, serif', system: 'system-ui, -apple-system, "Segoe UI", sans-serif', mono: '"IBM Plex Mono", ui-monospace, monospace' },
  mono: { plex: '"IBM Plex Mono", ui-monospace, monospace', jetbrains: '"JetBrains Mono", ui-monospace, monospace', system: 'ui-monospace, Menlo, Consolas, monospace' },
  display: { chakra: '"Chakra Petch", sans-serif', grotesk: '"Space Grotesk", system-ui, sans-serif', body: 'var(--font)', mono: 'var(--mono)' },
};
const FONT_CHOICES = {
  body: [["", "Theme default"], ["plex", "IBM Plex Sans"], ["inter", "Inter"], ["grotesk", "Space Grotesk"], ["serif", "Source Serif"], ["system", "System"], ["mono", "Monospace"]],
  mono: [["", "Theme default"], ["plex", "IBM Plex Mono"], ["jetbrains", "JetBrains Mono"], ["system", "System mono"]],
  display: [["", "Theme default"], ["chakra", "Chakra Petch"], ["grotesk", "Space Grotesk"], ["body", "Same as body"], ["mono", "Monospace"]],
};
const FONT_VARS = { body: "--font", mono: "--mono", display: "--display" };
function applyFonts(sel) {
  for (const k of Object.keys(FONT_VARS)) {
    const stack = FONT_STACKS[k][sel[k]];
    if (stack) document.documentElement.style.setProperty(FONT_VARS[k], stack); else document.documentElement.style.removeProperty(FONT_VARS[k]);
  }
  save("hs.fonts", sel);
}
(function initFonts() {
  const sel = Object.assign({ body: "", mono: "", display: "" }, store("hs.fonts", {}));
  applyFonts(sel);
  for (const [k, id] of [["body", "fontBodyChips"], ["mono", "fontMonoChips"], ["display", "fontDisplayChips"]]) {
    const chips = $(id); if (!chips) continue;
    chips.innerHTML = FONT_CHOICES[k].map(([v, label]) => `<span class="chip ${sel[k] === v ? "on" : ""}" data-font="${v}" style="${FONT_STACKS[k][v] ? "font-family:" + FONT_STACKS[k][v].replace(/"/g, "'") : ""}">${label}</span>`).join("");
    chips.querySelectorAll("[data-font]").forEach((c) => c.onclick = () => { sel[k] = c.dataset.font; applyFonts(sel); chips.querySelectorAll("[data-font]").forEach((x) => x.classList.toggle("on", x.dataset.font === c.dataset.font)); });
  }
})();

/* ---------- text size (UI scale) ---------- */
const FS_STEPS = [["s", "S"], ["m", "M"], ["l", "L"], ["xl", "XL"]];
const FS_ZOOM = { s: 0.9, m: 1.0, l: 1.15, xl: 1.3 };
// The whole UI scales with CSS zoom. WebKit does not scale vh/vw or media
// queries with it, and mouse coordinates stay in window pixels, so the
// zoom is also published as --z (style.css divides vh/vw by it), the
// width breakpoints are toggled here from innerWidth / zoom, and pointer
// maths below divide by uiZoom().
const uiZoom = () => parseFloat(document.documentElement.style.zoom) || 1;
const UI_BREAKS = [900, 940, 980, 1100, 1250];
function applyBreaks() {
  const w = innerWidth / uiZoom();
  for (const b of UI_BREAKS) document.documentElement.classList.toggle(`lt${b}`, w <= b);
}
window.addEventListener("resize", applyBreaks);
function applyFs(k) {
  const z = FS_ZOOM[k] ?? 1;
  document.documentElement.style.zoom = z;
  document.documentElement.style.setProperty("--z", z);
  save("hs.fs", k);
  applyBreaks();
  // Anything that sized itself to the window (the Leaflet map) sees no
  // resize event when only the zoom changed.
  requestAnimationFrame(() => window.dispatchEvent(new Event("resize")));
}
(function initFs() {
  const saved = store("hs.fs", "m");
  applyFs(saved);
  const chips = $("fsChips");
  if (!chips) return;
  chips.innerHTML = FS_STEPS.map(([v, label]) => `<span class="chip ${saved === v ? "on" : ""}" data-fs="${v}">${label}</span>`).join("");
  chips.querySelectorAll("[data-fs]").forEach((c) => c.onclick = () => { applyFs(c.dataset.fs); chips.querySelectorAll("[data-fs]").forEach((x) => x.classList.toggle("on", x.dataset.fs === c.dataset.fs)); });
})();
/* ---------- nav order: drag a top tab to move it ---------- */
(function navOrder() {
  const seg = $("navSeg");
  const apply = (order) => {
    const btns = [...seg.querySelectorAll("button")]; const byV = new Map(btns.map((b) => [b.dataset.v, b])); const seen = new Set();
    (order || []).forEach((v) => { const b = byV.get(v); if (b) { seg.appendChild(b); seen.add(v); } });
    btns.forEach((b) => { if (!seen.has(b.dataset.v)) seg.appendChild(b); });   // tabs added later keep their default place
  };
  apply(store("hs.navorder", []));
  const persist = () => save("hs.navorder", [...seg.querySelectorAll("button")].map((b) => b.dataset.v));
  let dragging = null;
  seg.querySelectorAll("button").forEach((b) => {
    b.draggable = true;
    b.addEventListener("dragstart", (e) => { dragging = b; b.classList.add("dragging"); e.dataTransfer.effectAllowed = "move"; e.dataTransfer.setData("text/plain", b.dataset.v); });
    b.addEventListener("dragend", () => { b.classList.remove("dragging"); dragging = null; persist(); });
    b.addEventListener("dragover", (e) => {
      if (!dragging || dragging === b) return;
      e.preventDefault(); e.dataTransfer.dropEffect = "move";
      const r = b.getBoundingClientRect();
      seg.insertBefore(dragging, e.clientX < r.left + r.width / 2 ? b : b.nextSibling);
    });
    b.addEventListener("drop", (e) => e.preventDefault());
  });
  seg.addEventListener("dragover", (e) => { if (dragging) e.preventDefault(); });
  seg.addEventListener("drop", (e) => e.preventDefault());
  window.navOrderReset = () => { localStorage.removeItem("hs.navorder"); apply(VIEWS); uiToast("Tab order reset"); };
  const reset = $("navOrderReset"); if (reset) reset.onclick = () => window.navOrderReset();
})();

/* ---------- per-playlist filters ----------
   A talkgroup number only means something within one system, so lockouts,
   priorities and timed avoids are kept per playlist ("" = runs started
   without one). A playlist's lockout and priorities live in the backend,
   shared by every site following it and every open page; the unscoped set
   and the timed avoids stay in this browser. */
const filt = new Map();   // playlist id → { lockout:Set, prio:Map, avoid:Map(tg → epoch ms) }
function filtFor(pl) {
  pl = pl || "";
  let f = filt.get(pl);
  if (!f) {
    f = { lockout: new Set(pl ? [] : store("hs.lockout", [])),
      prio: new Map(pl ? [] : Object.entries(store("hs.prio", {})).map(([k, v]) => [+k, +v])),
      avoid: new Map(Object.entries(store(pl ? `hs.avoid.${pl}` : "hs.avoid", {})).map(([k, v]) => [+k, +v])) };
    filt.set(pl, f);
  }
  return f;
}
const knownPlaylistIds = () => (window.playlistsAll || []).map((p) => p.id);
const plName = (pl) => { if (!pl) return ""; const p = (window.playlistsAll || []).find((x) => x.id === pl); return p ? p.name : pl; };
// The playlist a talkgroup of system `sid` belongs to: a live run's, else the first saved for that system.
function plForSid(sid) {
  if (sid == null) return "";
  const live = (typeof runsNow !== "undefined" ? runsNow : []).find((r) => r.sid === sid && r.playlist);
  if (live) return live.playlist;
  const p = (window.playlistsAll || []).find((x) => x.sid === sid);
  return p ? p.id : "";
}
// Take a playlist's saved lockout and priorities from the backend's copy.
function loadPlaylistFilters(list) {
  window.playlistsAll = list;
  for (const p of list) { const f = filtFor(p.id); f.lockout = new Set(p.lockout || []); f.prio = new Map((p.priorities || []).map(([t, v]) => [+t, +v])); }
  for (const id of [...filt.keys()]) if (id && !list.some((p) => p.id === id)) filt.delete(id);
  renderLockout();
}
const bells = new Set(store("hs.bells", []));
function pushPriorities(pl) {
  const f = filtFor(pl);
  if (!pl) save("hs.prio", Object.fromEntries(f.prio));
  if (TAURI) invoke("set_priorities", { entries: [...f.prio], playlist: pl || null }).catch((e) => log(`set_priorities: ${e}`));
}
const priOf = (tg, pl) => filtFor(pl).prio.get(tg) || 50;
function setPriority(tg, val, pl) {
  const n = parseInt(val, 10), f = filtFor(pl);
  if (Number.isFinite(n) && n >= 1 && n <= 99 && n !== 50) f.prio.set(tg, n); else f.prio.delete(tg);
  pushPriorities(pl); refreshRowButtons();
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
/* Make every panel collapsible. Panels that already carry an explicit data-acc
   key (the Monitor column) are left as-is; every other panel with a head+body
   gets a key from its eyebrow label, a caret, and defaults to open. On-demand
   editor panels (inline display:none) are skipped — they appear when summoned.
   So are panels marked `nocollapse`: a page whose whole purpose is one long
   table has nothing to gain from folding it away, and everything to lose —
   a mis-click leaves an empty box the size of the table, which reads as a
   freeze rather than as a fold. */
(function panelify() {
  const seen = {};
  document.querySelectorAll(".panel").forEach((p) => {
    if (p.classList.contains("acc") || p.classList.contains("nocollapse") || p.style.display === "none") return;
    const head = Array.from(p.children).find((c) => c.classList.contains("head"));
    if (!head) return;
    const eb = head.querySelector(".eyebrow");
    let key = (eb ? eb.textContent : "").trim().replace(/\s+/g, " ").toLowerCase() || "panel";
    if (seen[key]) key = `${key}-${++seen[key]}`; else seen[key] = 1;
    p.dataset.acc = key;
    p.classList.add("acc");
    if (!head.querySelector(".caret")) { const c = document.createElement("span"); c.className = "caret"; c.textContent = "▾"; head.appendChild(c); }
    if (accOpen[key] === undefined) accOpen[key] = true;
  });
})();
function accApply() {
  document.querySelectorAll(".panel.acc").forEach((p) => { const k = p.dataset.acc, open = accOpen[k] !== false; p.classList.toggle("closed", !open); });
}
document.querySelectorAll(".panel.acc > .head").forEach((h) => h.onclick = (e) => {
  if (e.target.closest("button,input,select,textarea,label,a,summary")) return;
  const k = h.parentElement.dataset.acc; accOpen[k] = accOpen[k] === false; save("hs.acc", accOpen); accApply();
});
accApply();

/* ---------- listen groups: named sets of talkgroups you can mute or unmute in one click ---------- */
const groups = store("hs.groups", []);   // [{id, name, tgs:[], listen:true}]
function groupsSave() { save("hs.groups", groups); renderGroupChips(); if (typeof renderGroupList === "function") renderGroupList(); pushMuted(); }
// Muting is about the room, not the library: a muted talkgroup is still
// followed, recorded, transcribed and matched by tripwires — it just does
// not come out of the speakers. Refusing a call outright is what lockout is
// for, and it has its own list.
function pushMuted() {
  if (!TAURI) return;
  invoke("set_muted", { tgs: [...mutedByGroups()] }).catch((err) => log(`set_muted: ${err}`));
}
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

/* ---------- lockout (permanent) + timed avoid, per playlist ---------- */
// What one playlist's runs should refuse right now: the saved lockout, plus
// (for now only) timed avoids. Muted groups are not here: they silence the
// speaker, they do not refuse the call.
function effectiveLockout(pl) {
  const f = filtFor(pl), now = Date.now();
  for (const [tg, until] of f.avoid) if (until <= now) f.avoid.delete(tg);
  save(pl ? `hs.avoid.${pl}` : "hs.avoid", Object.fromEntries(f.avoid));
  return { tgs: [...f.lockout].sort((a, b) => a - b), extra: [...f.avoid.keys()] };
}
// Push one playlist's lockout (or, with no argument, every known one).
function pushLockout(pl) {
  if (!TAURI) return;
  const ids = pl === undefined ? [...new Set(["", ...filt.keys(), ...knownPlaylistIds()])] : [pl || ""];
  for (const id of ids) {
    const e = effectiveLockout(id);
    if (!id) save("hs.lockout", e.tgs);
    invoke("set_lockout", { tgs: e.tgs, playlist: id || null, extra: e.extra }).catch((err) => log(`set_lockout: ${err}`));
  }
}
function avoidFor(tg, pl) {
  const min = +$("avoidMin").value || 60, f = filtFor(pl);
  if (f.avoid.has(tg)) f.avoid.delete(tg); else f.avoid.set(tg, Date.now() + min * 60000);
  renderLockout(); pushLockout(pl);
}
setInterval(() => {
  for (const [pl, f] of filt) { const before = f.avoid.size; effectiveLockout(pl); if (f.avoid.size !== before) pushLockout(pl); }
  renderLockout(); renderGroupChips();
}, 15000);
function renderLockout() {
  const withChips = [...filt].filter(([, f]) => f.lockout.size || f.avoid.size);
  const multi = withChips.length > 1;
  const chips = withChips.flatMap(([pl, f]) => {
    const lab = multi ? `<span class="chip lab" title="playlist">${esc(plName(pl) || "no playlist")}</span>` : "";
    return [lab,
      ...[...f.lockout].sort((a, b) => a - b).map((tg) => `<span class="chip" data-tg="${tg}" data-pl="${esc(pl)}" title="Unlock${plName(pl) ? " on " + esc(plName(pl)) : ""}">TG ${tg} ✕</span>`),
      ...[...f.avoid].map(([tg, until]) => `<span class="chip" data-avoid="${tg}" data-pl="${esc(pl)}" title="Timed avoid — click to lift">TG ${tg} ⏱ ${Math.max(1, Math.round((until - Date.now()) / 60000))}m ✕</span>`)];
  }).filter(Boolean);
  $("lockbar").style.display = chips.length ? "" : "none";
  $("lockchips").innerHTML = chips.join(" ");
  $("lockchips").querySelectorAll(".chip[data-tg]").forEach((c) => c.onclick = () => toggleLock(+c.dataset.tg, c.dataset.pl));
  $("lockchips").querySelectorAll(".chip[data-avoid]").forEach((c) => c.onclick = () => avoidFor(+c.dataset.avoid, c.dataset.pl));
  refreshRowButtons();
}
function refreshRowButtons() {
  tbody.querySelectorAll("button[data-lock]").forEach((b) => b.classList.toggle("on", filtFor(b.dataset.pl).lockout.has(+b.dataset.lock)));
  tbody.querySelectorAll("button[data-avoid]").forEach((b) => b.classList.toggle("on", filtFor(b.dataset.pl).avoid.has(+b.dataset.avoid)));
  tbody.querySelectorAll("input[data-pri]").forEach((b) => { const p = priOf(+b.dataset.pri, b.dataset.pl); b.className = "priin" + (p < 50 ? " pri-h" : p > 50 ? " pri-l" : ""); b.value = p; b.title = `Priority ${p < 50 ? "(high)" : p > 50 ? "(low)" : "(default)"} — type 1–99`; });
  tbody.querySelectorAll("button[data-bell]").forEach((b) => b.classList.toggle("bell", bells.has(+b.dataset.bell)));
}
function toggleLock(tg, pl) {
  const f = filtFor(pl);
  if (f.lockout.has(tg)) f.lockout.delete(tg); else f.lockout.add(tg);
  renderLockout(); pushLockout(pl);
}
function replay(path) { if (TAURI) invoke("play_wav", { path }).catch((e) => alert(e)); }
filtFor(""); renderLockout(); renderGroupChips(); pushLockout(""); pushMuted();
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
const wfCfg = store("hs.wf", { fft: 1024, avg: 4, map: "phosphor", min: -95, max: -20, line: true, peak: false, auto: true, constellation: true });
let peakHold = null;
let specCenter = null, specRate = null;   // live SDR passband (MHz, Hz)
const channelCounts = new Map();            // active voice freq MHz → refcount
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
  // If auto-range: scene the bins dbRange so "how much is actually on" is honest.
  // quantiles at 5/95 over the most recent frame; -120 dB floor avoids clipping on quiet ones.
  if (wfCfg.auto) {
    let mn = Infinity, mx = -Infinity;
    for (const v of db) { if (v < mn) mn = v; if (v > mx) mx = v; }
    // pad a little so weak channels still draw visible above the noise floor
    wfCfg.min = Math.max(-120, mn - 5); wfCfg.max = Math.min(20, mx + 5);
  }
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
  drawSpectrumOverlay();
}
function drawSpectrumOverlay() {
  const sw = sp.width, sh = sp.height;
  if (!specCenter || !specRate) return;
  const lo = specCenter - specRate / 2e6, hi = specCenter + specRate / 2e6;
  const xOf = (mhz) => ((mhz - lo) / (hi - lo)) * sw;
  // Full-bandwidth frequency axis.
  const step = (hi - lo) > 6 ? 1 : (hi - lo) > 3 ? 0.5 : 0.25;
  spctx.textAlign = "center";
  spctx.font = "10px 'IBM Plex Mono', monospace";
  spctx.fillStyle = "rgba(180,220,214,.6)";
  spctx.strokeStyle = "rgba(180,220,214,.28)";
  spctx.lineWidth = 1;
  for (let mhz = Math.ceil(lo / step) * step; mhz <= hi; mhz += step) {
    const x = xOf(mhz);
    spctx.beginPath(); spctx.moveTo(x, sh - 14); spctx.lineTo(x, sh - 6); spctx.stroke();
    spctx.fillText(mhz.toFixed(step < 1 ? 2 : 1), x, sh - 17);
  }
  // Active voice-channel ribbons (the ranges being followed).
  for (const freq of channelCounts.keys()) {
    const x = xOf(freq);
    if (x < 0 || x > sw) continue;
    spctx.fillStyle = "rgba(52,224,207,.10)";
    spctx.fillRect(x - 4, 0, 8, sh);
    spctx.strokeStyle = "rgba(52,224,207,.4)";
    spctx.beginPath(); spctx.moveTo(x, 0); spctx.lineTo(x, sh); spctx.stroke();
  }
}
function wfApply() {
  $("wfFft").value = wfCfg.fft; $("wfAvg").value = wfCfg.avg; $("wfMap").value = wfCfg.map; $("wfMin").value = wfCfg.min; $("wfMax").value = wfCfg.max; $("wfLine").checked = wfCfg.line; $("wfPeak").checked = wfCfg.peak; $("wfAuto").checked = wfCfg.auto; $("wfConst").checked = wfCfg.constellation !== false;
  $("wfMin").disabled = wfCfg.auto;
  $("wfMax").disabled = wfCfg.auto;
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

/* ---------- waterfall height (drag the divider to resize) ---------- */
let wfH = store("hs.wfHeight", null);   // null = untouched (CSS default)
function resizeWaterfallBuffer(rows) {
  rows = Math.max(60, Math.min(900, Math.round(rows)));
  const cw = wf.width, ch = wf.height;
  if (ch === rows) return;
  // Preserve the existing waterfall: copy it off, resize, then paste back
  // anchored to the top (newest row at the top; history accrues downward).
  const tmp = document.createElement("canvas");
  tmp.width = cw; tmp.height = ch;
  tmp.getContext("2d").drawImage(wf, 0, 0);
  wf.height = rows;
  wctx.fillStyle = "#05090a"; wctx.fillRect(0, 0, cw, rows);
  const copyH = Math.min(ch, rows);
  wctx.drawImage(tmp, 0, 0, cw, copyH, 0, 0, cw, copyH);
}
function applyWfHeight() {
  if (wfH == null) return;
  resizeWaterfallBuffer(wfH);
  wf.style.height = wfH + "px";
}
const wfResize = document.getElementById("wfResize");
let wfDrag = null;
wfResize.addEventListener("pointerdown", (e) => {
  wfDrag = { startY: e.clientY, startH: wf.getBoundingClientRect().height / uiZoom(), cur: wf.getBoundingClientRect().height / uiZoom() };
  wfResize.classList.add("dragging");
  document.body.style.cursor = "ns-resize";
  document.body.style.userSelect = "none";
  e.preventDefault();
});
window.addEventListener("pointermove", (e) => {
  if (!wfDrag) return;
  wfDrag.cur = Math.max(60, Math.min(900, Math.round(wfDrag.startH + (e.clientY - wfDrag.startY) / uiZoom())));
  wf.style.height = wfDrag.cur + "px";   // live preview — cheap, scaled by the GPU
});
window.addEventListener("pointerup", () => {
  if (!wfDrag) return;
  wfResize.classList.remove("dragging");
  document.body.style.cursor = "";
  document.body.style.userSelect = "";
  wfH = wfDrag.cur;
  save("hs.wfHeight", wfH);
  resizeWaterfallBuffer(wfH);   // one crisp 1:1 resize on release
  wfDrag = null;
});
applyWfHeight();   // restore a saved height on launch

/* ---------- transcript tooltip (full text on hover, when truncated) ---------- */
const tip = document.createElement("div");
tip.className = "tip";
document.body.appendChild(tip);
function positionTip(x, y) {
  const z = uiZoom(), pad = 14, w = tip.offsetWidth, h = tip.offsetHeight;
  const vw = innerWidth / z, vh = innerHeight / z;   // window px → zoomed CSS px
  let left = x / z + pad, top = y / z + pad;
  if (left + w > vw - 8) left = Math.max(8, vw - w - 8);
  if (top + h > vh - 8) top = Math.max(8, vh - h - 8);
  tip.style.left = left + "px";
  tip.style.top = top + "px";
}
function showTip(text, x, y) {
  tip.textContent = text;
  tip.style.display = "block";
  positionTip(x, y);
}
function hideTip() { tip.style.display = "none"; }
function tdTruncated(td) { return td.scrollWidth > td.clientWidth + 1; }
document.addEventListener("mouseover", (e) => {
  const td = e.target.closest && e.target.closest("td.tr");
  if (!td || td.classList.contains("editing")) { hideTip(); return; }
  const full = (td.getAttribute("title") || "").trim();
  if (!full || !tdTruncated(td)) { hideTip(); return; }
  showTip(full, e.clientX, e.clientY);
});
document.addEventListener("mousemove", (e) => {
  if (tip.style.display !== "block") return;
  if (!(e.target.closest && e.target.closest("td.tr"))) return;
  positionTip(e.clientX, e.clientY);
});
document.addEventListener("mouseout", (e) => {
  if (e.target.closest && e.target.closest("td.tr") && !(e.relatedTarget && e.relatedTarget.closest && e.relatedTarget.closest("td.tr"))) hideTip();
});

/* ---------- collapse always-visible help paragraphs into ⓘ info buttons ---------- */
function showTipHtml(html, x, y) { tip.innerHTML = html; tip.style.display = "block"; positionTip(x, y); }
(function helpify() {
  document.querySelectorAll("p.help").forEach((p) => {
    const label = (p.textContent || "").trim();
    const html = p.innerHTML.trim();
    if (!html) { p.remove(); return; }
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "info";
    btn.setAttribute("aria-label", label);
    btn.dataset.help = html;
    p.replaceWith(btn);
  });
})();
document.addEventListener("mouseover", (e) => {
  const b = e.target.closest && e.target.closest("button.info");
  if (b) showTipHtml(b.dataset.help, e.clientX, e.clientY);
});
document.addEventListener("mousemove", (e) => {
  if (tip.style.display !== "block") return;
  const b = e.target.closest && e.target.closest("button.info");
  if (b) positionTip(e.clientX, e.clientY);
});
document.addEventListener("mouseout", (e) => {
  const b = e.target.closest && e.target.closest("button.info");
  if (b && !(e.relatedTarget && e.relatedTarget.closest && e.relatedTarget.closest("button.info"))) hideTip();
});
document.addEventListener("focusin", (e) => {
  const b = e.target.closest && e.target.closest("button.info");
  if (b) { const r = b.getBoundingClientRect(); showTipHtml(b.dataset.help, r.left, r.bottom); }
});
document.addEventListener("focusout", (e) => {
  if (e.target.closest && e.target.closest("button.info")) hideTip();
});

/* ---------- readouts shared by both modes ---------- */
let followVoice = 0;
// Receiver DSP health: carrier lock, the simulcast-multipath meter (echo
// fraction + delay spread read off the equalizer taps), and the ADC-overload
// flag. null / negative = no reading (C4FM, equalizer bypassed, unacquired).
function setDspHealth(lock, echoFrac, spreadUs, clipPct, voiceQuality) {
  if (lock != null) {
    if (lock >= 0) { $("r-lock").textContent = lock.toFixed(2); $("r-lockbar").style.width = Math.max(0, Math.min(100, lock * 100)) + "%"; }
    else { $("r-lock").textContent = "—"; $("r-lockbar").style.width = "0%"; }
  }
  if (voiceQuality != null) {
    if (voiceQuality >= 0) { $("r-vq").textContent = voiceQuality.toFixed(2); $("r-vqbar").style.width = Math.max(0, Math.min(100, voiceQuality * 100)) + "%"; }
    else { $("r-vq").textContent = "—"; $("r-vqbar").style.width = "0%"; }
  }
  if (echoFrac != null && echoFrac >= 0) {
    $("r-echo").textContent = `${(echoFrac * 100).toFixed(1)}%` + (spreadUs != null && spreadUs >= 0 ? ` · ${spreadUs.toFixed(0)} µs` : "");
    // The converged equalizer keeps most energy on the cursor even on heavy
    // simulcast (measured: ~0.9% mild echo, ~6.4% heavy), so ×10 spreads the
    // useful range over the bar.
    $("r-echobar").style.width = Math.max(0, Math.min(100, echoFrac * 1000)) + "%";
  } else {
    $("r-echo").textContent = "—"; $("r-echobar").style.width = "0%";
  }
  const sig = $("r-signal");
  if (clipPct != null && clipPct >= 0.1) {
    sig.classList.add("clip");
    sig.title = `${clipPct.toFixed(1)}% of samples at the ADC rails — front-end overload garbles decode; reduce gain`;
  } else {
    sig.classList.remove("clip"); sig.title = "";
  }
}
function setStatus(s) {
  if (s.syncs != null) $("r-syncs").textContent = s.syncs;
  if (s.grants != null) $("r-grants").textContent = s.grants;
  if (s.voice_secs != null) $("r-voice").innerHTML = s.voice_secs.toFixed(1) + "<small>s</small>";
  if (s.modulation) $("tunedSub").textContent = s.modulation.toUpperCase();
  setDspHealth(s.lock, s.echo_frac, s.echo_spread_us, s.clip_pct, s.voice_quality);
  if (s.dropped != null) $("r-syncerr").textContent = s.dropped ? `${s.dropped}` : "0";
}

/* ---------- follow events (backend or demo) ---------- */
const evCounts = {};
/* ---------- several systems at once ----------
   Each playlist being followed is a run on the backend; every follow event
   carries `run` (id) and `system` (label). Calls, grants and notices from all
   runs show together; the single-system panels (tuning, site, status,
   waterfall, constellation) show the run picked in the bar under the top bar. */
let runsNow = [];
let viewRun = null;
const DIAG_KINDS = new Set(["measured", "site", "status", "spectrum", "constellation"]);
function renderRuns() {
  const bar = $("runsBar");
  if (!bar) return;
  if (!runsNow.some((r) => r.id === viewRun)) viewRun = runsNow.length ? runsNow[0].id : null;
  bar.hidden = runsNow.length < 2;
  bar.innerHTML = runsNow.length < 2 ? "" : `<span class="lab">Systems</span>` + runsNow.map((r) =>
    `<span class="runchip${r.id === viewRun ? " on" : ""}" data-run="${r.id}" title="Show this system's tuning, site and waterfall panels">${esc(r.label)} <span class="f">${r.control_mhz.toFixed(4)}</span><button class="x" data-stoprun="${r.id}" title="Stop this system">✕</button></span>`).join("");
  bar.querySelectorAll("[data-run]").forEach((el) => el.onclick = (e) => { if (e && e.target && e.target.closest && e.target.closest("[data-stoprun]")) return; viewRun = +el.dataset.run; renderRuns(); showRunDiag(viewRun); });
  bar.querySelectorAll("[data-stoprun]").forEach((b) => b.onclick = () => invoke("stop_run", { id: +b.dataset.stoprun }).catch((e) => alert(e)));
}
function setRuns(list) {
  runsNow = Array.isArray(list) ? list : [];
  for (const id of Object.keys(runDiag)) if (!runsNow.some((r) => r.id === +id)) delete runDiag[id];
  const before = viewRun; renderRuns();
  if (viewRun !== before) showRunDiag(viewRun);
}
// The last measured/site frame per run, replayed into the panels when the
// picked chip changes.
const runDiag = {};
function showRunDiag(id) { const d = runDiag[id]; if (!d) return; for (const k of ["measured", "site"]) if (d[k]) handleFollow(d[k]); }
const runLabel = (ev) => runsNow.length > 1 && ev.system ? ev.system : "";
// The RadioReference system behind an event's run (runs started from a
// playlist know theirs), so names and rosters stay apart per system.
const runSid = (ev) => { const r = ev && ev.run != null ? runsNow.find((x) => x.id === ev.run) : null; return r && r.sid != null ? r.sid : null; };
// The playlist an event's run follows ("" when it follows every talkgroup).
const runPl = (ev) => { const r = ev && ev.run != null ? runsNow.find((x) => x.id === ev.run) : null; return r && r.playlist ? r.playlist : ""; };

function handleFollow(ev) {
  evCounts[ev.kind] = (evCounts[ev.kind] || 0) + 1;
  // Another system's diagnostics: keep the panels on the picked run.
  if (ev.run != null && (ev.kind === "measured" || ev.kind === "site")) (runDiag[ev.run] = runDiag[ev.run] || {})[ev.kind] = ev;
  if (ev.run != null && viewRun != null && ev.run !== viewRun && DIAG_KINDS.has(ev.kind)) return;
  if (ev.kind === "notice" && runsNow.length > 1 && ev.system) ev = { ...ev, text: `${ev.system}: ${ev.text}` };
  // Spectrum, status and constellation stream continuously — a counter line
  // once in 20 proves they flow without drowning the terminal (each log() is
  // also an IPC round-trip). Everything else is rare enough to dump.
  const chatty = ev.kind === "spectrum" || ev.kind === "status" || ev.kind === "constellation";
  if (!chatty) log(`follow ${ev.kind}: ${JSON.stringify(ev).slice(0, 160)}`);
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
      specCenter = ev.center_mhz ?? parseFreq($("center").value) / 1e6;
      specRate = ev.rate;
      $("followMeta").textContent = "";
      activeRefresh();
      break;
    case "call_start":
      activeStart(ev);
      channelCounts.set(ev.freq_mhz, (channelCounts.get(ev.freq_mhz) || 0) + 1);
      if (bellFor(ev.tg)) tone("bell");
      break;
    case "call":
      activeEnd(ev);
      { const c = (channelCounts.get(ev.freq_mhz) || 1) - 1; c > 0 ? channelCounts.set(ev.freq_mhz, c) : channelCounts.delete(ev.freq_mhz); }
      if (!ev.encrypted) followVoice += ev.secs;
      if (ev.emergency && !ev.replayed) { tone("emergency"); logEvent(`EMERGENCY · ${ev.name} · unit ${ev.unit_name || ev.source}`, "alarm"); }
      addCall({ at: ev.replayed ? ev.start : null, tg: ev.tg, name: ev.name, desc: ev.desc, service: ev.service, category: ev.category, source: ev.source, unit_name: ev.unit_name, talker_alias: ev.talker_alias, freq_mhz: ev.freq_mhz, encrypted: ev.encrypted,
                secs: ev.secs, modulation: ev.modulation, wav: ev.wav, emergency: ev.emergency, patched_with: ev.patched_with, id: ev.id, syncs_c4fm: ev.syncs_c4fm, syncs_cqpsk: ev.syncs_cqpsk, system: ev.system, site_name: ev.site_name, pl: runPl(ev) });
      if (ev.secs === 0) { noAudioCount++; $("histMeta").title = `${noAudioCount} granted calls produced no audio`; }
      // Say when a call's audio has holes, and why: stream drops mean the
      // decoder fell behind (CPU/USB); poor frames mean the signal itself.
      // Concealed frames are worth a warning when they add up to something
      // audible: a twentieth of the call *and* at least 200 ms (10 frames).
      // Every keyup starts with a few frames the decoder is still acquiring,
      // and on a one-second clip four of those already pass 5%.
      if (!ev.replayed && ev.secs > 0 && (ev.dropped_blocks > 0 || (ev.poor_frames >= 10 && ev.poor_frames > ev.secs * 50 * 0.05))) logEvent(`${ev.name} ${ev.secs.toFixed(1)}s: ${ev.dropped_blocks ? ev.dropped_blocks + " stream drop(s)" : ""}${ev.dropped_blocks && ev.poor_frames ? ", " : ""}${ev.poor_frames ? ev.poor_frames + " of " + Math.round(ev.secs * 50) + " frames concealed" : ""} — audio has holes`, "warn");
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
      if (ev.signal_dbfs != null) {
        $("r-signal").textContent = ev.signal_dbfs.toFixed(1) + " dBFS";
      } else {
        $("r-signal").textContent = "— dBFS";
      }
      // Follow mode's status event doesn't carry a per-call voice_quality yet
      // (it aggregates several simultaneous calls, each with its own), so
      // this shows "—" here rather than a single-channel number that would
      // misrepresent a multi-call site.
      setDspHealth(ev.lock ?? -1, ev.echo_frac ?? -1, ev.echo_spread_us ?? -1, ev.clip_pct, ev.voice_quality ?? -1);
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
  const first = String(p.message || "").split("\n");
  logEvent(`${p.follow ? "FOLLOW-UP" : "TRIPWIRE"} ${p.name}: ${first[0]}`, p.follow ? "" : "alarm");
  if (!p.follow) uiToast(`🚨 ${p.name} — ${first.slice(0, 2).join(" · ")}`);
  if (p.tone) tone("emergency");
  markFired(p.call, { rule_name: p.name, status: "sent", source: "tripwire", at: Math.floor(Date.now() / 1000) });
}
/* ---------- tripwire badges: which rules fired about a call ---------- */
// `fired` rows come from the tripwire history (Rust `events::Fired`). Sent is
// the signal colour, failed red, quiet (looked, stayed quiet) faint.
function firedBadges(list) {
  return (list || []).map((f) => `<span class="badge tw ${esc(f.status)}" title="${esc(`${f.rule_name} · ${f.status === "quiet" ? "looked, stayed quiet" : f.status}${f.at ? " · " + new Date(f.at * 1000).toLocaleTimeString("en-US", { hour12: false }) : ""}`)}">⚡ ${esc(f.rule_name)}</span>`).join("");
}
// A live call just fired a rule: badge its row in the Monitor history.
function markFired(id, f) {
  if (id == null) return;
  const h = history.find((x) => x.id === id); if (!h) return;
  const cell = h.el.querySelector("td.tg"); if (!cell) return;
  let box = cell.querySelector(".twbox"); if (!box) { box = document.createElement("span"); box.className = "twbox"; cell.appendChild(box); }
  box.insertAdjacentHTML("beforeend", firedBadges([f]));
  if (typeof window.libMarkFired === "function") window.libMarkFired(id, f);
}

/* ---------- constellation (control channel symbols) ---------- */
const cc = $("constellation"), cctx = cc.getContext("2d");
let lastConst = null;
function phaseScatter(pts) {
  if (!pts.length) return "—";
  let e = 0, n = 0;
  for (const [x, y] of pts) {
    const ang = Math.atan2(y, x);
    let best = Infinity;
    for (const t of [Math.PI / 4, 3 * Math.PI / 4, -Math.PI / 4, -3 * Math.PI / 4]) {
      let d = Math.abs(ang - t); if (d > Math.PI) d = 2 * Math.PI - d;
      if (d < best) best = d;
    }
    e += best; n++;
  }
  return ((e / n) * 180 / Math.PI).toFixed(1) + "°";
}
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
  $("constLabel").textContent = cq ? "CQPSK · phase scatter " + phaseScatter(pts) : "C4FM · level vs previous level";
}
$("wfAuto").onchange = () => { wfCfg.auto = $("wfAuto").checked; wfApply(); };
$("wfConst").onchange = () => { wfCfg.constellation = $("wfConst").checked; save("hs.wf", wfCfg); $("constBox").style.display = wfCfg.constellation ? "" : "none"; };
$("wfConst").checked = wfCfg.constellation !== false; $("constBox").style.display = wfCfg.constellation === false ? "none" : "";
$("wfAuto").title = "Scopes min/max to the bins range — cleaned-up = cleaned-up, fudge-proof.";
$("constLabel").title = "Mean distance of your symbol points from the four ideal CQPSK phases (±45°/±135°). Low scatter → you'll hear it clean.";
$("wfConst").onchange = () => { wfCfg.constellation = $("wfConst").checked; save("hs.wf", wfCfg); $("constBox").style.display = wfCfg.constellation ? "" : "none"; };
$("wfConst").checked = wfCfg.constellation !== false; $("constBox").style.display = wfCfg.constellation === false ? "none" : "";

/* ---------- discovery: talkgroups and channels the control channel actually uses ---------- */
// tgs: "<tg>" or "<sid>:<tg>" → {tg, sid, system, name, named, n, freq, unit, first, last}; freqs: freq → {n, tgs:{}, last}.
// The same talkgroup number on two systems is two talkgroups, so a grant
// from a run that knows its system is filed under that system.
const disc = store("hs.discovery", { tgs: {}, freqs: {} });
let discDirty = false;
function discoveryGrant(ev) {
  const sid = runSid(ev), key = sid != null ? `${sid}:${ev.tg}` : String(ev.tg);
  const t = disc.tgs[key] || (disc.tgs[key] = { first: Date.now(), n: 0, tg: ev.tg });
  if (sid != null) {
    t.sid = sid; if (ev.system) t.system = ev.system; const rp = runPl(ev); if (rp) t.pl = rp;
    // An entry filed by number alone before the system was known: fold its
    // counts in, so the talkgroup does not appear twice with a stale name.
    const legacy = disc.tgs[String(ev.tg)];
    if (legacy) { t.n += legacy.n || 0; if (legacy.first && legacy.first < t.first) t.first = legacy.first; delete disc.tgs[String(ev.tg)]; }
  }
  t.n++; t.last = Date.now(); t.name = ev.name; t.named = ev.named; t.freq = ev.freq_mhz; if (ev.unit) t.unit = ev.unit; t.enc = ev.encrypted;
  const k = ev.freq_mhz.toFixed(4), f = disc.freqs[k] || (disc.freqs[k] = { n: 0, tgs: {} });
  f.n++; f.last = Date.now(); f.tgs[ev.tg] = 1;
  discDirty = true;
}
setInterval(() => { if (discDirty) { discDirty = false; const keys = Object.keys(disc.tgs); if (keys.length > 3000) keys.sort((a, b) => disc.tgs[a].last - disc.tgs[b].last).slice(0, keys.length - 3000).forEach((k) => delete disc.tgs[k]); save("hs.discovery", disc); if (discoveryVisible()) renderDiscovery(); } }, 2000);
const ago = (t) => { const s = Math.max(0, Math.round((Date.now() - t) / 1000)); return s < 60 ? `${s}s ago` : s < 3600 ? `${Math.floor(s / 60)}m ago` : `${Math.floor(s / 3600)}h ${Math.floor(s % 3600 / 60)}m ago`; };
let bandLo = 0, bandHi = 0;
function renderDiscovery() {
  const q = $("dcFilter").value.trim().toLowerCase(), un = $("dcUnnamed").checked;
  const rows = Object.entries(disc.tgs).map(([key, t]) => ({ tg: +key, ...t, key })).filter((t) => (!un || !t.named) && (!q || `${t.tg} ${t.name || ""} ${t.system || ""} ${t.freq || ""}`.toLowerCase().includes(q))).sort((a, b) => b.last - a.last);
  const multi = new Set(rows.map((t) => t.sid ?? "")).size > 1;
  $("dcBody").innerHTML = rows.slice(0, 1000).map((t) => `<tr data-tg="${t.tg}"><td class="mono">${t.tg}${multi && t.system ? `<span class="faint" style="display:block;font-size:10.5px">${esc(t.system)}</span>` : ""}</td><td>${t.named ? esc(t.name) : `<span class="faint">unnamed</span>`}${t.enc ? ' <span class="badge enc">enc</span>' : ""}</td><td class="mono">${t.n}</td><td class="mono">${t.freq != null ? t.freq.toFixed(4) : "—"}</td><td class="mono">${t.unit || "—"}</td><td class="mono">${ago(t.last)}</td>` +
    `<td class="act"><button data-dcplay="${t.tg}" title="Play the newest recorded call on this talkgroup">▶</button><input type="text" data-name="${esc(t.key)}" placeholder="${t.named ? "rename" : "name it"}" style="width:120px;padding:2px 6px;font-size:11px" /><button data-namego="${esc(t.key)}">✔</button>` +
    `<input type="number" class="priin" data-pri="${t.tg}" data-pl="${esc(t.pl || plForSid(t.sid))}" min="1" max="99" value="${priOf(t.tg, t.pl || plForSid(t.sid))}" title="Priority 1–99 (1 = highest)"><button data-lock="${t.tg}" data-pl="${esc(t.pl || plForSid(t.sid))}" class="${filtFor(t.pl || plForSid(t.sid)).lockout.has(t.tg) ? "on" : ""}">⊘</button></td></tr>`).join("");
  $("dcEmpty").style.display = rows.length ? "none" : "";
  const all = Object.keys(disc.tgs).length, unnamed = Object.values(disc.tgs).filter((t) => !t.named).length;
  $("dcMeta").textContent = all ? `${all} talkgroups · ${unnamed} unnamed` : "";
  const tb = $("dcBody");
  tb.querySelectorAll("[data-namego]").forEach((b) => b.onclick = async () => {
    const key = b.dataset.namego, t = disc.tgs[key]; if (!t) return;
    const tg = t.tg != null ? t.tg : +key, sid = t.sid != null ? t.sid : null;
    const inp = tb.querySelector(`input[data-name="${key.replace(/[^0-9:]/g, "")}"]`), name = inp ? inp.value.trim() : ""; if (!name || !TAURI) return;
    try { await invoke("catalog_user_set", { tg, alias: name, category: "Discovered", sid }); t.name = name; t.named = true; save("hs.discovery", disc); renderDiscovery(); if (typeof aliasesRefresh === "function") aliasesRefresh(); logEvent(`named TG ${tg}${t.system ? " on " + t.system : ""} “${name}”`); } catch (e) { alert(e); }
  });
  tb.querySelectorAll("button[data-dcplay]").forEach((b) => b.onclick = async () => {
    if (!TAURI) return;
    try { const r = await invoke("tg_latest_call", { tg: +b.dataset.dcplay }); if (!r) { uiToast("No recorded audio on that talkgroup yet — it may be locked out, not recorded, or only heard as grants.", "err"); return; }
      await invoke("library_play", { id: r.id }); uiToast(`Playing ${r.tg_name} (TG ${r.tg}) · ${r.secs.toFixed(1)} s${r.transcript ? " — “" + r.transcript.slice(0, 80) + "”" : ""}`); }
    catch (e) { uiToast(`${e}`, "err"); }
  });
  tb.querySelectorAll("input[data-name]").forEach((i) => i.onkeydown = (e) => { if (e.key === "Enter") tb.querySelector(`[data-namego="${i.dataset.name}"]`).click(); });
  tb.querySelectorAll("input[data-pri]").forEach((b) => b.onchange = () => { setPriority(+b.dataset.pri, b.value, b.dataset.pl); renderDiscovery(); });
  tb.querySelectorAll("button[data-lock]").forEach((b) => b.onclick = () => { toggleLock(+b.dataset.lock, b.dataset.pl); renderDiscovery(); });
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
// "<run>:<unit>" → {unit, run, system, tg, name, unit_name, status, last}.
// Radio IDs are only unique within a system, so each run keeps its own roster.
const affil = new Map();
function affiliationEvent(ev) {
  const key = `${ev.run != null ? ev.run : ""}:${ev.unit}`;
  if (ev.what === "deregistered") { affil.delete(key); }
  else {
    const a = affil.get(key) || { unit: ev.unit, run: ev.run };
    a.last = Date.now(); a.unit_name = ev.unit_name || a.unit_name; a.status = ev.what; if (ev.system) a.system = ev.system;
    if (ev.tg != null && ev.what !== "refused") { a.tg = ev.tg; a.name = ev.name; }
    affil.set(key, a);
    if (affil.size > 4096) affil.delete(affil.keys().next().value);
  }
  if (discoveryVisible()) renderAffiliations();
}
function renderAffiliations() {
  const q = $("afFilter").value.trim().toLowerCase();
  const rows = [...affil.values()].filter((a) => !q || `${a.unit} ${a.unit_name || ""} ${a.tg || ""} ${a.name || ""} ${a.system || ""}`.toLowerCase().includes(q)).sort((a, b) => b.last - a.last);
  const multi = new Set(rows.map((a) => a.run)).size > 1;
  $("afBody").innerHTML = rows.slice(0, 500).map((a) => `<tr><td class="mono">${a.unit_name ? `${esc(a.unit_name)} <span class="faint">${a.unit}</span>` : a.unit}${multi && a.system ? `<span class="faint" style="display:block;font-size:10.5px">${esc(a.system)}</span>` : ""}</td><td>${a.tg != null ? `${esc(a.name || "")} <span class="faint mono">TG ${a.tg}</span>` : "—"}</td><td><span class="badge ${a.status === "refused" ? "enc" : "clear"}">${a.status}</span></td><td class="mono">${ago(a.last)}</td></tr>`).join("");
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
  if (discoveryVisible()) renderMap();
}
const lon2x = (lon, z) => (lon + 180) / 360 * Math.pow(2, z);
const lat2y = (lat, z) => (1 - Math.log(Math.tan(lat * Math.PI / 180) + 1 / Math.cos(lat * Math.PI / 180)) / Math.PI) / 2 * Math.pow(2, z);
const x2lon = (x, z) => x / Math.pow(2, z) * 360 - 180;
const y2lat = (y, z) => { const n = Math.PI - 2 * Math.PI * y / Math.pow(2, z); return 180 / Math.PI * Math.atan(0.5 * (Math.exp(n) - Math.exp(-n))); };
const tileBase = () => window.__HS_REMOTE__ ? "/tiles" : TAURI ? (navigator.userAgent.includes("Windows") ? "http://tiles.localhost" : "tiles://localhost") : "https://tile.openstreetmap.org";
function tile(z, x, y) {
  const n = Math.pow(2, z); x = ((x % n) + n) % n; if (y < 0 || y >= n) return null;
  const k = `${z}/${x}/${y}`; let im = tiles.get(k);
  if (!im) { im = new Image(); im.crossOrigin = "anonymous"; im.onload = () => renderMap(); im.src = `${tileBase()}/${z}/${x}/${y}.png`; tiles.set(k, im); if (tiles.size > 400) tiles.delete(tiles.keys().next().value); }
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
  // Survey pins — your receiver locations — amber squares, distinct from the
  // pink radio fixes.
  for (const p of surveyPins) {
    const px = (lon2x(p.lon, z) - x0) * 256, py = (lat2y(p.lat, z) - y0) * 256;
    mctx.fillStyle = "#e6a23c"; mctx.fillRect(px - 5, py - 5, 10, 10);
    mctx.strokeStyle = "#fff"; mctx.lineWidth = 1.5; mctx.strokeRect(px - 5, py - 5, 10, 10);
    const label = p.label || p.id;
    mctx.font = "bold 11px IBM Plex Sans, sans-serif"; mctx.textAlign = "left";
    const tw = mctx.measureText(label).width;
    mctx.fillStyle = "rgba(0,0,0,.6)"; mctx.fillRect(px + 8, py - 9, tw + 6, 14);
    mctx.fillStyle = "#e6a23c"; mctx.fillText(label, px + 11, py + 2);
  }
  // Pending pin — hollow amber square where the next capture will happen.
  if (surveyPin) {
    const px = (lon2x(surveyPin.lon, z) - x0) * 256, py = (lat2y(surveyPin.lat, z) - y0) * 256;
    mctx.strokeStyle = "#e6a23c"; mctx.lineWidth = 2.5; mctx.strokeRect(px - 7, py - 7, 14, 14);
  }
}
mc.onwheel = (e) => { if (!mapView) return; e.preventDefault(); mapView.z = Math.max(3, Math.min(18, mapView.z + (e.deltaY < 0 ? 1 : -1))); renderMap(); };
mc.onmousedown = (e) => { surveyDown = { x: e.clientX, y: e.clientY }; if (mapView) mapDrag = { x: e.clientX, y: e.clientY, lat: mapView.lat, lon: mapView.lon }; };
mc.onmouseup = (e) => { if (surveyDown && surveyMode) { const dx = e.clientX - surveyDown.x, dy = e.clientY - surveyDown.y; if (dx * dx + dy * dy < 25) surveyClick(e); } surveyDown = null; };
window.addEventListener("mousemove", (e) => { if (!mapDrag) return; const z = mapView.z, s = mc.width / mc.getBoundingClientRect().width; const dx = (e.clientX - mapDrag.x) * s / 256, dy = (e.clientY - mapDrag.y) * s / 256; mapView.lon = x2lon(lon2x(mapDrag.lon, z) - dx, z); mapView.lat = y2lat(lat2y(mapDrag.lat, z) - dy, z); renderMap(); });
window.addEventListener("mouseup", () => { mapDrag = null; });
mc.ondblclick = () => { if (fixes.size) { const f = [...fixes.values()].pop(); mapView = { lat: f.lat, lon: f.lon, z: Math.max(mapView ? mapView.z : 12, 12) }; renderMap(); } };

/* ---------- survey: pin your location, capture timed IQ + log ---------- */
let surveyMode = false;                      // armed: a map tap drops a pin
let surveyDown = null;                       // mousedown point, to tell tap from drag
let surveyPin = null;                        // { lat, lon } pending capture
const surveyPins = store("hs.survey", []);   // completed pins (mirror of corpus survey.json)

function surveyArmed(on) {
  surveyMode = on;
  mc.style.cursor = on ? "crosshair" : "grab";
  $("surveyToggle").textContent = on ? "Survey: on" : "Survey: off";
  $("surveyToggle").setAttribute("aria-pressed", String(on));
  if (on && !mapView) { mapView = { lat: 39.7684, lon: -86.1581, z: 12 }; renderMap(); }
  if (!on) { surveyPin = null; $("surveyForm").style.display = "none"; $("surveyPos").textContent = ""; }
}
$("surveyToggle").onclick = () => surveyArmed(!surveyMode);

function surveyClick(e) {
  if (!mapView) return;
  const rect = mc.getBoundingClientRect();
  const s = mc.width / rect.width;
  const px = (e.clientX - rect.left) * s, py = (e.clientY - rect.top) * s;
  const z = mapView.z, cx = lon2x(mapView.lon, z), cy = lat2y(mapView.lat, z);
  const x0 = cx - mc.width / 512, y0 = cy - mc.height / 512;
  surveyPin = { lat: y2lat(y0 + py / 256, z), lon: x2lon(x0 + px / 256, z) };
  $("surveyPos").textContent = `${surveyPin.lat.toFixed(5)}, ${surveyPin.lon.toFixed(5)}`;
  $("surveyForm").style.display = "";
  renderMap();
}

$("surveyCancel").onclick = () => { surveyPin = null; $("surveyForm").style.display = "none"; $("surveyPos").textContent = ""; renderMap(); };

$("surveyCapture").onclick = async () => {
  if (!surveyPin) return;
  const freq = parseFreq($("freq").value);
  if (!Number.isFinite(freq)) { uiToast("Set a channel frequency first — tune the control/voice channel you want to survey.", "err"); return; }
  const label = $("surveyLabel").value.trim() || "pin";
  const seconds = Math.max(5, parseFloat($("surveySecs").value) || 60);
  const spec = {
    source: srcKind(), freq, rate: parseFloat($("rate").value),
    gain: $("gain").value.trim() === "" ? null : parseFloat($("gain").value),
    cqpsk: modSel === "cqpsk", eq: eqSel, ppm: ppmVal(), device: srcId() || null,
    lat: surveyPin.lat, lon: surveyPin.lon, label, seconds,
    corpus: $("surveyCorpus").value.trim() || "~/hoosier-field/survey", format: "cs16",
  };
  const pin = surveyPin;
  surveyPin = null; $("surveyForm").style.display = "none"; $("surveyLabel").value = "";
  // A survey capture needs the radio to itself. If a follow/capture is live,
  // stop it first — the backend joins the old run before opening the radio,
  // so this is safe even back-to-back.
  if (TAURI && $("pillText").textContent !== "standby") {
    try { await invoke("stop_capture"); } catch (_) {}
    uiToast("Stopping current capture, then surveying…");
  }
  try {
    const entry = await invoke("survey_capture", { spec });
    logEvent(`survey: recording "${entry.label}" at ${entry.lat.toFixed(5)}, ${entry.lon.toFixed(5)} for ${entry.seconds}s → ${entry.iq.split("/").pop()}`);
  } catch (err) {
    surveyPin = pin; $("surveyForm").style.display = "";
    uiToast("survey: " + err, "err");
  }
};

function renderSurveyTable() {
  const body = $("surveyBody");
  body.innerHTML = [...surveyPins].reverse().map((p) =>
    `<tr data-lat="${p.lat}" data-lon="${p.lon}" data-id="${esc(p.id || "")}"><td>${esc(p.label || p.id)}</td><td class="mono">${p.lat.toFixed(5)}</td><td class="mono">${p.lon.toFixed(5)}</td><td class="mono">${esc((p.iq || "").split("/").pop())}</td><td class="mono">${ago(p.t * 1000)}</td><td><button class="btn ghost sm" data-survey-del title="Delete this position and its collected data">✕</button></td></tr>`
  ).join("");
  body.querySelectorAll("tr").forEach((tr) => {
    tr.style.cursor = "pointer";
    tr.onclick = () => { mapView = { lat: parseFloat(tr.dataset.lat), lon: parseFloat(tr.dataset.lon), z: Math.max(mapView ? mapView.z : 12, 15) }; renderMap(); };
  });
  body.querySelectorAll("button[data-survey-del]").forEach((b) => {
    b.onclick = (e) => {
      e.stopPropagation();
      const id = b.closest("tr").dataset.id;
      const pin = surveyPins.find((p) => (p.id || "") === id);
      if (pin) surveyDeletePin(pin);
    };
  });
}

async function surveyDeletePin(pin) {
  const name = pin.label || pin.id;
  if (!(await uiConfirm(`Delete position "${name}" and its captured IQ + log?`, "Delete"))) return;
  try {
    await invoke("survey_delete", { spec: { id: pin.id, iq: pin.iq, log: pin.log } });
  } catch (e) { alert("survey_delete: " + e); return; }
  surveyPins.splice(surveyPins.indexOf(pin), 1);
  save("hs.survey", surveyPins);
  renderSurveyTable(); renderMap();
  logEvent(`survey: deleted position "${name}"`);
}
renderSurveyTable();

if (TAURI) listen("survey_done", (e) => {
  const en = e.payload;
  surveyPins.push(en);
  save("hs.survey", surveyPins);
  renderSurveyTable();
  renderMap();
  uiToast(`Survey pin saved: ${en.label} (${en.seconds}s)`);
});

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
  listen("decoderevent", (e) => { const d = e.payload; logEvent(`${d.kind}: ${d.text}`); });
  listen("decoderdone", (e) => {
    const d = e.payload;
    logEvent(`${d.decoder}: ${d.audio_secs.toFixed(1)} s audio, ${d.events} event(s)${d.audio ? " — playing" : ""}`);
    if (d.audio) replay(d.audio);
  });
  listen("stopped", () => { setState("standby"); holdTg = null; holdPl = ""; updateHoldBtn(); });
  // A remote desktop page opening mid-run: show the same controls and state
  // as the local one (the shim then replays the run's key frames).
  window.applySnapshot = (snap) => {
    if (snap && Array.isArray(snap.runs)) setRuns(snap.runs);
    const st = snap && snap.start;
    if (st) {
      const key = `${st.source}|${st.device || ""}`;
      const vals = [...$("source").options].map((o) => o.value);
      if (vals.includes(key)) $("source").value = key; else if (vals.includes(st.source)) $("source").value = st.source;
      if (st.rate) $("rate").value = String(st.rate);
      if (st.freq) $("center").value = (st.freq / 1e6).toFixed(4) + "M";
      const firstRun = (snap.runs || [])[0];
      if (firstRun && firstRun.control_mhz) $("freq").value = firstRun.control_mhz.toFixed(4) + "M";
      else if (st.control) $("freq").value = (st.control / 1e6).toFixed(4) + "M"; else if (st.freq) $("freq").value = (st.freq / 1e6).toFixed(4) + "M";
      if (st.modulation && $("tmod")) $("tmod").value = st.modulation;
      if (typeof st.play === "boolean") $("play").checked = st.play;
      if (st.hang_ms) $("hangMs").value = st.hang_ms;
      modeSel = st.mode === "capture" ? "capture" : st.mode === "dual" ? "dual" : "follow";
      setSeg($("modeSeg"), modeSel); applyMode();
      const first = (snap.runs || [])[0];
      if (first && first.control_mhz) invoke("sites_list").then((list) => {
        const st = (list || []).find((x) => Math.abs(x.control_mhz - first.control_mhz) < 1e-6);
        if (st) { $("playlist").value = st.id; $("followMeta").textContent = siteMeta(st); }
      }).catch(() => {});
    }
    setState(snap && snap.running ? (st && st.mode === "capture" ? "capturing" : "following") : "standby");
  };
  listen("error", (e) => { log(`backend error: ${e.payload}`); if (!runsNow.length) setState("standby"); alert("Capture error:\n" + e.payload); });
  listen("runs", (e) => setRuns(e.payload));
  invoke("runs_list").then((list) => { setRuns(list); if (runsNow.length && $("pillText").textContent === "standby") setState("following"); }).catch(() => {});
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
      if (modeSel === "channel" && decoderSel !== "p25") { alert("Live capture with the non-P25 decoders isn't wired up yet — record IQ, then use “Decode a recording” below with the decoder selected."); return; }
      if (modeSel === "follow" && !Number.isFinite(parseFreq($("center").value))) { alert("Enter a band centre like 855M"); return; }
      if (+$("rate").value < 1e6) { alert("The 48 kHz rate is for decoding files; pick 2.4 M (RTL-SDR) or 2.5 / 10 M (Airspy)."); return; }
      setState(modeSel === "follow" ? "measuring" : "capturing");
      log(`start: mode=${modeSel} source=${$("source").value} rate=${$("rate").value} freq=${$("freq").value} center=${$("center").value}`);
      followVoice = 0;
      if (modeSel === "dual") {
        const vk = $("voiceSrc").value.split("|")[0];
        const vid = $("voiceSrc").value.split("|")[1] || null;
        await invoke("dual_start", {
          controlSource: srcKind(), controlDevice: srcId() || null,
          controlRate: parseFloat($("rate").value),
          voiceSource: vk, voiceDevice: vid, voiceRate: parseFloat($("rate").value),
          gain: opts().gain, control: parseFreq($("freq").value),
          cqpsk: modSel === "cqpsk", play: $("play").checked,
        });
      } else if (modeSel === "follow") {
        const o = opts();
        $("followMeta").textContent = "measuring the control channel…";
        $("tunedHz").textContent = mhz(o.freq);
        const pl = sites.find((p) => p.id === $("playlist").value);
        save("hs.prefs", { ...store("hs.prefs", {}), lastPlaylist: $("playlist").value });
        // Several sites at once: one run each, all reading the same radio,
        // so the band centre has to reach every control channel.
        const also = pl ? plAlsoSelected().filter((p) => p.id !== pl.id) : [];
        const group = pl ? [pl, ...also] : [];
        let center = parseFreq($("center").value);
        const common = { source: o.source, freq: center, rate: o.rate, gain: o.gain, callsDir: $("callsdir").value.trim() || null, play: $("play").checked,
          hangMs: parseInt($("hangMs").value, 10) || null, ppm: ppmVal(), device: srcId() || null, modulation: $("tmod").value };
        if (group.length > 1) {
          // Several sites: spread their control channels over the radios,
          // each run on the radio that holds its control channel, reading
          // every other radio for voice (the backend shares them).
          const radios = radiosForStart();
          const plan = planRadios(group, radios);
          if (!plan.ok) {
            setState("standby");
            const fit = commonCentre(group.map((p) => p.control_mhz * 1e6), o.rate);
            alert(`These sites' control channels don't fit on the ${radios.length} radio${radios.length === 1 ? "" : "s"} available (${radios.map((r) => `${r.label} ±${(r.width / 2).toFixed(2)} MHz`).join(", ")}) — they span ${(fit.span / 1e6).toFixed(2)} MHz:\n\n${group.map((p) => `${p.name} · ${p.control_mhz.toFixed(4)} MHz`).join("\n")}\n\nUntick one in the + list, plug in another radio, or switch a radio on for coverage under Settings → Devices.`);
            return;
          }
          const hosts = plan.radios.filter((r) => r.sites.length);
          logEvent(`following ${group.length} sites on ${hosts.length} radio${hosts.length === 1 ? "" : "s"}: ${hosts.map((r) => `${r.label} @ ${r.center.toFixed(4)} MHz (${r.sites.map((p) => p.name).join(", ")})`).join(" · ")}${plan.gaps.length ? ` — uncovered ${plan.gaps.map(([x, y]) => `${x.toFixed(3)}–${y.toFixed(3)}`).join(", ")} MHz` : " — the whole span is covered"}`);
          const primary = hosts[0];
          center = primary.center * 1e6;
          $("center").value = primary.center.toFixed(4) + "M";
          bandLo = primary.lo; bandHi = primary.hi;
          const spec = (r) => ({ source: r.source, device: r.device, center: Math.round(r.center * 1e6), rate: r.rate, gain: r.gain, ppm: r.ppm, label: r.label });
          for (const r of hosts) {
            const extra = plan.radios.filter((x) => x.key !== r.key).map(spec);
            for (const p of r.sites) {
              await invoke("start_follow", { ...common, source: r.source, device: r.device, freq: Math.round(r.center * 1e6), rate: r.rate, gain: r.gain, ppm: r.ppm,
                control: p.control_mhz * 1e6, systemName: p.system_name, siteName: p.name, extra, playlist: p.playlist || null });
            }
          }
        } else {
          bandLo = (center - o.rate * 0.4) / 1e6; bandHi = (center + o.rate * 0.4) / 1e6;
          if (!group.length) {
            await invoke("start_follow", { ...common, control: o.freq, systemName: null, siteName: null, extra: coverageExtras(), playlist: null });
          } else {
            const p = group[0];
            await invoke("start_follow", { ...common, control: p.control_mhz * 1e6, systemName: p.system_name, siteName: p.name, extra: coverageExtras(), playlist: p.playlist || null });
          }
        }
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
    const rate = parseFloat($("rate").value);
    try {
      setState("decoding");
      if (decoderSel !== "p25") {
        logEvent(`decoding ${path.split("/").pop()} with ${$("decoder").selectedOptions[0].textContent.replace(/ \(default\)/, "")}…`);
        await invoke("decode_file_analog", { path, rate, decoder: decoderSel, squelch: squelchVal });
      } else {
        await invoke("decode_file", { path, rate, cqpsk: modSel === "cqpsk", eq: eqSel });
      }
    }
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
    $("voiceSrc").innerHTML = opts.map((o) => `<option value="${esc(o.v)}">${esc(o.t)}</option>`).join("");
    $("voiceSrc").value = vals[1] || vals[0] || "";
    devLoadSelected();
  }
  /* ---------- gain controls, SDRTrunk-style ---------- */
  const E4000_GAINS = () => (devView.e4000_gains_db && devView.e4000_gains_db.length ? devView.e4000_gains_db : [-1.0, 1.5, 4.0, 6.5, 9.0, 11.5, 14.0, 16.5, 19.0, 21.5, 24.0, 29.0, 34.0, 42.0]);
  const GAINS = () => (srcKind() === "soapy" ? E4000_GAINS() : RTL_GAINS());
  const RTL_GAINS = () => (devView.rtl_gains_db && devView.rtl_gains_db.length ? devView.rtl_gains_db : [0, 0.9, 1.4, 2.7, 3.7, 7.7, 8.7, 12.5, 14.4, 15.7, 16.6, 19.7, 20.7, 22.9, 25.4, 28.0, 29.7, 32.8, 33.8, 36.4, 37.2, 38.6, 40.2, 42.1, 43.4, 43.9, 44.5, 48.0, 49.6]);
  function gainUi() {
    const airspy = srcKind() === "airspy";
    $("gnRtl").style.display = airspy ? "none" : ""; $("gnAirspy").style.display = airspy ? "" : "none";
    const g = GAINS(); $("gnRtlGain").max = g.length - 1; $("gnRtlGainVal").textContent = (g[+$("gnRtlGain").value] ?? 0).toFixed(1) + " dB";
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
    const g = GAINS();
    s.gain = $("gnRtlAgc").checked ? null : (g[+$("gnRtlGain").value] ?? null);
    s.airspy_gain = $("gnAsEnabled").checked; s.airspy_mode = $("gnAsMode").value; s.airspy_preset = +$("gnAsPreset").value;
    s.airspy_lna = +$("gnAsLna").value; s.airspy_mixer = +$("gnAsMixer").value; s.airspy_vga = +$("gnAsVga").value; s.airspy_lna_agc = $("gnAsLnaAgc").checked; s.airspy_mixer_agc = $("gnAsMixerAgc").checked;
    return s;
  }
  function gainToUi(s) {
    const g = GAINS();
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
  // Usable width: the backend normalises 10 → 9.6 MSPS and 2.5 → 2.4 and decodes ±0.4 of that.
  const normRate = (r) => (r >= 9_000_000 ? 9_600_000 : r >= 2_450_000 && r < 2_550_000 ? 2_400_000 : r);
  const usable = (d) => { const r = (devView.settings[`${d.kind}|${d.id}`] || {}).rate || d.rates[0]; return { rate: r, width: normRate(r) * 0.8 / 1e6 }; };
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

  /* ---------- several sites on several radios ----------
     Every picked site needs its control channel inside some radio's band, and
     every run then decodes voice from every open radio (the backend pools
     them). So: hand the control channels to the radios (fewest radios, each
     one's spread within its width), centre each radio where it covers the
     most of the sites' spans while keeping its controls inside, and park the
     radios left over on whatever is still uncovered. */
  const MARGIN_MHZ = 0.025;
  window.planRadios = function planRadios(group, radiosIn) {
    const radios = radiosIn.map((r) => ({ ...r, width: r.width - MARGIN_MHZ, half: (r.width - MARGIN_MHZ) / 2 }));
    const ctrls = group.map((p) => p.control_mhz);
    const spans = group.map((p) => p.span_mhz || [p.control_mhz, p.control_mhz]);
    const lo = Math.min(...spans.map((x) => x[0])), hi = Math.max(...spans.map((x) => x[1]));
    // Coverage of [lo, hi] by a set of bands, in MHz.
    const covered = (bands) => { let gaps = [[lo, hi]]; for (const [a, b] of bands) gaps = gaps.flatMap(([x, y]) => (b <= x || a >= y) ? [[x, y]] : [[x, Math.min(y, a)], [Math.max(x, b), y]].filter(([p, q]) => q - p > 1e-6)); return { len: (hi - lo) - gaps.reduce((t, [x, y]) => t + (y - x), 0), gaps }; };
    // Try every assignment of sites to radios (a few sites × a few radios), keep the best.
    let best = null;
    const n = group.length, k = radios.length;
    const assign = new Array(n).fill(0);
    const total = Math.pow(k, n);
    for (let code = 0; code < total; code++) {
      let c = code; for (let i = 0; i < n; i++) { assign[i] = c % k; c = Math.floor(c / k); }
      // Feasible: each radio's controls fit in its width. Candidate centres per radio.
      const per = radios.map(() => []); assign.forEach((r, i) => per[r].push(i));
      let ok = true; const ranges = [];
      for (let r = 0; r < k; r++) {
        if (!per[r].length) { ranges.push(null); continue; }
        const cs = per[r].map((i) => ctrls[i]), cmin = Math.min(...cs), cmax = Math.max(...cs);
        if (cmax - cmin > radios[r].width) { ok = false; break; }
        ranges.push([cmax - radios[r].half, cmin + radios[r].half]);
      }
      if (!ok) continue;
      const used = ranges.filter(Boolean).length;
      // Centres: each radio tries its low end, high end and the middle of its allowed range; pick the combination covering most.
      const clamp = (v, rg) => Math.min(rg[1], Math.max(rg[0], v));
      const opts = ranges.map((rg, r) => rg ? [...new Set([rg[0], (rg[0] + rg[1]) / 2, rg[1], clamp(lo + radios[r].half, rg), clamp(hi - radios[r].half, rg)])] : [null]);
      const walk = (r, chosen) => {
        if (r === k) {
          const bands = chosen.map((c, i) => c == null ? null : [c - radios[i].half, c + radios[i].half]).filter(Boolean);
          const cov = covered(bands);
          // Fewest radios, then most of the span covered, then the least band wasted outside it.
          const outside = bands.reduce((t, [a, b]) => t + Math.max(0, lo - a) + Math.max(0, b - hi), 0);
          const score = [-used, cov.len, -outside];
          const better = (x, y) => x[0] !== y[0] ? x[0] > y[0] : Math.abs(x[1] - y[1]) > 1e-9 ? x[1] > y[1] : x[2] > y[2] + 1e-9;
          if (!best || better(score, best.score)) best = { score, assign: [...assign], centres: [...chosen] };
          return;
        }
        for (const c of opts[r]) walk(r + 1, [...chosen, c]);
      };
      walk(0, []);
    }
    if (!best) return { ok: false, lo, hi };
    // Radios left without a control channel park on the biggest gaps.
    const plan = radios.map((r, i) => ({ ...r, center: best.centres[i], sites: group.filter((_, j) => best.assign[j] === i) }));
    let { gaps } = covered(plan.filter((r) => r.center != null).map((r) => [r.center - r.half, r.center + r.half]));
    for (const r of plan.filter((x) => x.center == null)) {
      if (!gaps.length) break;
      gaps.sort((a, b) => (b[1] - b[0]) - (a[1] - a[0]));
      const [x, y] = gaps[0];
      r.center = (y - x) <= r.width ? (x + y) / 2 : x + r.half;
      gaps = covered(plan.filter((z) => z.center != null).map((z) => [z.center - z.half, z.center + z.half])).gaps;
    }
    return { ok: true, lo, hi, gaps, radios: plan.filter((r) => r.center != null).map((r) => ({ ...r, lo: r.center - r.half, hi: r.center + r.half })) };
  };
  // The radios Start may use: the picked one first, then every other radio not switched off for coverage.
  function radiosForStart() {
    const primaryKey = $("source").value;
    const list = [];
    for (const d of devView.devices) {
      const k = `${d.kind}|${d.id}`, u = usable(d), st = devView.settings[k] || {};
      if (k !== primaryKey && (roles[k] === "off" || !$("cpEnabled").checked)) continue;
      list.push({ key: k, source: d.kind, device: d.id, rate: u.rate, width: u.width, gain: st.gain ?? null, ppm: st.ppm || null, label: st.nickname || d.label, primary: k === primaryKey });
    }
    list.sort((a, b) => (b.primary - a.primary) || (b.width - a.width));
    return list;
  }
  $("source").addEventListener("change", () => setTimeout(coveragePlan, 0));
  $("rate").addEventListener("change", () => setTimeout(coveragePlan, 0));

  /* ---------- shared rule settings: Telegram, destinations, Ollama, cloud ---------- */
  // Rules are tripwires now (tripwires.js). What they share — the bot's
  // defaults, named destinations, discovered chats and the local model —
  // lives in the alerts settings object held here; the Connections page
  // (connections.js) edits it through alertsSettings() / alertsPersist().
  listen("alert", (e) => alertFired(e.payload));
  listen("alert_error", (e) => { logEvent(`tripwire failed: ${e.payload}`, "warn"); uiToast(`Tripwire failed: ${e.payload}`, "err"); });
  let akSettings = null, akView = null;
  async function akPersist() {
    if (!akSettings) return false;
    akSettings.ollama = { url: $("olUrl").value.trim() || "http://localhost:11434", model: $("olModel").value, timeout_secs: parseInt($("olTimeout").value, 10) || 60, fail_open: $("olFailOpen").checked };
    try { await invoke("alerts_set", { settings: akSettings }); const v = await invoke("alerts_get"); akSettings = v.settings; akView = v; return true; } catch (e) { log(`alerts_set failed: ${e}`); uiToast(`Could not save: ${e}`, "err"); return false; }
  }
  async function akRefresh() {
    try {
      const v = await invoke("alerts_get"); akSettings = v.settings; akView = v;
      if (typeof window.connectionsRender === "function") window.connectionsRender();
      $("olUrl").value = v.settings.ollama.url; $("olTimeout").value = v.settings.ollama.timeout_secs; $("olFailOpen").checked = v.settings.ollama.fail_open;
      if (v.settings.ollama.model) $("olModel").innerHTML = `<option value="${esc(v.settings.ollama.model)}">${esc(v.settings.ollama.model)}</option>`;
      olRefresh(true);
    } catch (e) { log(`alerts_get: ${e}`); }
  }
  /* Reasoning ("think") is only offered for a model that has a thinking mode,
     per Ollama's /api/show capabilities. Cached per model; a saved rule keeps
     its flag either way — the backend simply sends think=false to a model
     that lacks the mode. */
  const olCaps = new Map();   // model → capabilities[] (or null while loading)
  async function olCapsOf(model) {
    if (!model) return [];
    if (olCaps.has(model)) return olCaps.get(model) || [];
    olCaps.set(model, null);
    try { const caps = await invoke("ollama_capabilities", { url: $("olUrl").value.trim() || "http://localhost:11434", model }); olCaps.set(model, caps || []); return caps || []; }
    catch (e) { log(`ollama_capabilities: ${e}`); olCaps.delete(model); return []; }
  }
  async function olThinkUi() {
    const model = $("olModel").value;
    const caps = await olCapsOf(model);
    const can = caps.includes("thinking");
    for (const [box, hint] of [["twThink", "twThinkHint"]]) {
      const b = $(box), h = $(hint); if (!b) continue;
      b.disabled = !can;
      h.textContent = !model ? "pick an Ollama model" : can ? "" : `${model} has no thinking mode`;
    }
  }
  window.olThinkUi = olThinkUi;
  async function olRefresh(quiet) {
    try { const models = await invoke("ollama_models", { url: $("olUrl").value.trim() || "http://localhost:11434" }); const cur = (akSettings && akSettings.ollama.model) || $("olModel").value; $("olModel").innerHTML = '<option value="">—</option>' + models.map((m) => `<option value="${esc(m)}">${esc(m)}</option>`).join(""); $("olModel").value = models.includes(cur) ? cur : ""; $("olMeta").textContent = `${models.length} models`; }
    catch (e) { $("olMeta").textContent = "not reachable"; if (!quiet) uiToast(`Ollama: ${e}`, "err"); }
  }
  $("olRefresh").onclick = () => olRefresh(false);
  ["olModel", "olTimeout", "olFailOpen", "olUrl"].forEach((id) => $(id).onchange = akPersist);
  $("olModel").addEventListener("change", () => olThinkUi());
  window.alertsSettings = () => akSettings;
  window.alertsView = () => akView;
  window.alertsPersist = akPersist;
  window.alertsReload = akRefresh;
  akRefresh();

  /* cloud model settings (for tripwires whose check runs on the cloud model) */
  async function azcLoad() {
    try {
      const [cloud, hasKey] = await invoke("analyzer_cloud_get");
      $("azcProvider").value = cloud.provider || "openrouter"; $("azcModel").value = cloud.model || "";
      $("azcBase").value = cloud.base_url || ""; $("azcTimeout").value = cloud.timeout_secs || 60;
      $("azcKeyState").textContent = hasKey ? "key saved" : "no key saved";
      $("azcMeta").textContent = cloud.model ? `${cloud.provider} · ${cloud.model}` : "";
    } catch (e) { log(`analyzer_cloud_get: ${e}`); }
  }
  $("azcSave").onclick = async () => {
    const cloud = { provider: $("azcProvider").value, model: $("azcModel").value.trim(), base_url: $("azcBase").value.trim(), timeout_secs: Math.max(5, parseInt($("azcTimeout").value, 10) || 60) };
    const key = $("azcKey").value;
    try { await invoke("analyzer_cloud_save", { cloud, key: key || null }); $("azcKey").value = ""; uiToast("Cloud model saved"); azcLoad(); }
    catch (e) { uiToast(`Could not save: ${e}`, "err"); }
  };
  $("azcClear").onclick = async () => { if (!(await uiConfirm("Forget the saved cloud API key?", "Forget"))) return; try { await invoke("analyzer_cloud_clear_key"); $("azcKey").value = ""; uiToast("Key forgotten"); azcLoad(); } catch (e) { uiToast(`${e}`, "err"); } };
  azcLoad();

  /* ---------- file name template ---------- */
  async function fnRefresh() {
    try { const v = await invoke("names_get"); $("fnTemplate").value = v.settings.template; $("fnExample").textContent = v.example; $("fnTokens").innerHTML = v.tokens.map(([t, d]) => `<b>${esc(t)}</b> ${esc(d)}`).join(" · "); } catch (e) { log(`names_get: ${e}`); }
  }
  $("fnTemplate").oninput = async () => { try { $("fnExample").textContent = await invoke("names_preview", { template: $("fnTemplate").value }); } catch (_) {} };
  $("fnSave").onclick = async () => { try { $("fnExample").textContent = await invoke("names_set", { template: $("fnTemplate").value }); logEvent("file name template saved"); } catch (e) { alert(e); } };
  fnRefresh();

  /* ---------- remote access (phone over Tailscale) ---------- */
  async function webRefresh() {
    try {
      const v = await invoke("web_access_get");
      $("webUrl").value = v.url;
      $("webToken").value = v.token;
      $("webToken").type = "password";
      $("webTokenShow").textContent = "Show";
      $("webMeta").textContent = `port ${v.port} · listening`;
    } catch (e) { log(`web_access_get: ${e}`); $("webMeta").textContent = "unavailable"; }
  }
  function copyText(s) {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      return navigator.clipboard.writeText(s).catch(() => {});
    }
    if (typeof document.execCommand === "function") {
      try {
        const ta = document.createElement("textarea"); ta.value = s; ta.style.position = "fixed"; ta.style.opacity = "0";
        document.body.appendChild(ta); ta.select(); document.execCommand("copy"); document.body.removeChild(ta);
      } catch (_) {}
    }
    return Promise.resolve();
  }
  $("webUrlCopy").onclick = () => copyText($("webUrl").value).then(() => uiToast("Address copied"));
  $("webTokenCopy").onclick = () => copyText($("webToken").value).then(() => uiToast("Token copied"));
  $("webTokenShow").onclick = () => { const el = $("webToken"); const show = el.type === "password"; el.type = show ? "text" : "password"; $("webTokenShow").textContent = show ? "Hide" : "Show"; };
  webRefresh();

  /* ---------- instances on the tailnet ---------- */
  let tnRows = [];
  async function tnRefresh() {
    try {
      const v = await invoke("remotes_get");
      if (!v) return;
      $("tnTrust").checked = !!(v.settings && v.settings.trust_tailnet);
      if (v.tailnet) {
        const t = v.tailnet;
        $("tnAccount").innerHTML = `Linked as <b>${esc(t.login || "this Tailscale account")}</b> · this Mac is <span class="mono">${esc(t.dns || t.host)}</span>${t.ip ? ` (${esc(t.ip)})` : ""} · ${t.online} of ${t.devices} device${t.devices === 1 ? "" : "s"} online`;
        $("tnMeta").textContent = "";
      } else {
        $("tnAccount").textContent = v.error ? `Tailscale unavailable: ${v.error}` : "Tailscale unavailable";
        $("tnMeta").textContent = "not linked";
      }
    } catch (e) { log(`remotes_get: ${e}`); $("tnAccount").textContent = "Tailscale unavailable"; }
  }
  $("tnTrust").onchange = async () => {
    try { await invoke("remotes_set", { settings: { trust_tailnet: $("tnTrust").checked } }); uiToast($("tnTrust").checked ? "Trusting devices on your account" : "Token required again"); }
    catch (e) { alert(e); }
  };
  function tnStateHtml(r) {
    if (r.found) {
      const bits = [];
      bits.push(r.authorized ? (r.running === true ? "running" : r.running === false ? "standby" : "reachable") : (r.trusts_tailnet ? "trusts this account" : "token needed"));
      if (r.authorized && r.catalog_len != null) bits.push(`${r.catalog_len} talkgroups`);
      if (r.version) bits.push(`v${r.version}`);
      return `<span class="tn-state on${r.authorized ? " ok" : ""}">${esc(bits.join(" · "))}</span>`;
    }
    if (r.error) return `<span class="tn-state err" title="${esc(r.error)}">error</span>`;
    return `<span class="tn-state">${r.online ? "no HoosierSDR" : "offline"}</span>`;
  }
  let tnScanned = false;
  function tnRender() {
    const el = $("tnList");
    if (!tnRows.length) { el.innerHTML = `<div class="tn-empty">${tnScanned ? "No online devices found. Click Scan to look again." : "Not scanned yet."}</div>`; return; }
    el.innerHTML = tnRows.map((r, i) => `
      <div class="tn-row${r.found ? " found" : ""}" data-i="${i}">
        <div class="tn-who">
          <div class="tn-name">${esc(r.host || r.dns)}${r.is_self ? ` <span class="faint">this Mac</span>` : ""}${r.same_account ? "" : ` <span class="faint">other account</span>`}</div>
          <div class="mono faint">${esc(r.dns)}${r.ip ? ` · ${esc(r.ip)}` : ""}${r.os ? ` · ${esc(r.os)}` : ""}</div>
        </div>
        <div class="tn-status">${tnStateHtml(r)}</div>
        <div class="tn-act">${r.found && !r.is_self ? `
          <input type="password" class="tn-token" placeholder="${r.has_token ? "token saved" : "paste token"}" spellcheck="false" autocomplete="off" />
          <button class="btn ghost sm" data-tnsave>Save</button>
          <button class="btn sm" data-tnopen>Open</button>` : r.found && r.is_self ? `<button class="btn ghost sm" data-tnopen>Open</button>` : ""}
        </div>
      </div>`).join("");
  }
  async function tnScan() {
    const btn = $("tnScan"); btn.disabled = true; btn.textContent = "Scanning…"; tnScanned = true;
    $("tnMeta").textContent = "scanning";
    try {
      const rows = await invoke("remotes_scan");
      tnRows = Array.isArray(rows) ? rows : [];
      const found = tnRows.filter((r) => r.found).length;
      $("tnMeta").textContent = `${found} instance${found === 1 ? "" : "s"} · ${tnRows.length} device${tnRows.length === 1 ? "" : "s"} checked`;
    } catch (e) { log(`remotes_scan: ${e}`); $("tnMeta").textContent = `scan failed: ${e}`; tnRows = []; }
    tnRender();
    btn.disabled = false; btn.textContent = "Scan";
  }
  $("tnScan").onclick = tnScan;
  window.remoteOnShow = () => { tnRefresh(); if (!tnScanned) tnScan(); };
  $("tnList").onclick = async (e) => {
    const b = e.target.closest("button"); if (!b) return;
    const row = b.closest(".tn-row"); const r = row && tnRows[parseInt(row.dataset.i, 10)]; if (!r) return;
    if (b.hasAttribute("data-tnsave")) {
      const inp = row.querySelector(".tn-token");
      try { await invoke("remote_token_set", { dns: r.dns, token: inp.value }); uiToast(inp.value.trim() ? "Token saved" : "Token cleared"); inp.value = ""; tnScan(); }
      catch (err) { alert(err); }
    } else if (b.hasAttribute("data-tnopen")) {
      try { await invoke("remote_open", { dns: r.dns, url: r.url, host: r.host }); }
      catch (err) { alert(err); }
    }
  };
  tnRefresh();
  tnRender();

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

  /* ---------- radio-ID aliases (per system: a radio ID is only unique within one) ---------- */
  let unitSystems = null;   // sid → system name, from the playlists
  async function unitSystemNames() {
    if (unitSystems) return unitSystems;
    unitSystems = new Map();
    try { for (const p of (await invoke("playlists_list")) || []) if (p.sid && !unitSystems.has(p.sid)) unitSystems.set(p.sid, p.system_name || `system ${p.sid}`); } catch (_) {}
    const sel = $("unitSid");
    if (sel && sel.options.length <= 1) for (const [sid, name] of unitSystems) sel.add(new Option(name, sid));
    return unitSystems;
  }
  const unitSid = () => { const v = $("unitSid") ? $("unitSid").value : ""; return v ? +v : null; };
  async function renderUnits() {
    try {
      const names = await unitSystemNames();
      const u = await invoke("units_list");
      $("unitsMeta").textContent = u.length ? `${u.length} named` : "";
      $("unitsBody").innerHTML = u.map((r) => `<tr><td class="mono">${r.id}</td><td>${esc(r.name)}</td><td class="faint">${r.sid != null ? esc(names.get(r.sid) || `system ${r.sid}`) : "any system"}</td></tr>`).join("");
    } catch (e) { log(`units: ${e}`); }
  }
  $("unitSave").onclick = async () => {
    const id = parseInt($("unitId").value, 10); if (!Number.isFinite(id)) return;
    try { await invoke("unit_set", { id, name: $("unitName").value, sid: unitSid() }); $("unitId").value = ""; $("unitName").value = ""; renderUnits(); } catch (e) { alert(e); }
  };
  $("unitsImport").onclick = async () => {
    const path = $("unitsCsv").value.trim(); if (!path) return;
    try { const n = await invoke("units_import", { path, sid: unitSid() }); $("unitsMeta").textContent = `${n} named`; renderUnits(); } catch (e) { alert(e); }
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
    // One cell per header: blank (select), Time, Talkgroup, Description,
    // Unit, Length, Transcript, Actions. The Time cell was missing, so
    // every header sat one column left of its data.
    return `<tr data-id="${r.id}" class="${libSel === r.id ? "sel" : ""}"><td><input type="checkbox" data-sel="${r.id}" ${cart.has(r.id) ? "checked" : ""}></td>` +
      `<td class="time">${esc(fmtT(r.start))}</td>` +
      `<td class="tg">${esc(r.tg_name)}<span class="num">TG ${r.tg}${r.emergency ? " · EMERGENCY" : ""}</span>${r.encrypted ? '<span class="badge enc">Encrypted</span>' : ""}${r.service ? `<span class="svc">${esc(r.service)}</span>` : ""}${r.category ? `<span class="cat">${esc(r.category)}</span>` : ""}${(r.fired || []).length ? `<span class="twbox">${firedBadges(r.fired)}</span>` : ""}</td>` +
      `<td class="desc">${esc(r.tg_desc || "")}</td>` +
      `<td class="src">${r.unit_name ? `${esc(r.unit_name)}<span class="num" style="display:block;font-size:10.5px;color:var(--ink-faint)">UID ${r.unit}</span>` : (r.unit ? `UID ${r.unit}` : "—")}</td><td class="len">${r.secs.toFixed(1)} s</td>` +
      `<td class="tr ${r.transcript_edited ? "edited" : ""}" title="${esc(t)}">${esc(t) || (r.audio ? '<span class="faint">not transcribed</span>' : '<span class="faint">no audio</span>')}</td>` +
      `<td class="act">${r.audio ? `<button title="Play" data-lplay="${r.id}">▶</button>` : ""}<button title="Star" data-lstar="${r.id}" class="${r.starred ? "pri-h" : ""}">★</button>` +
      `<button title="Run the transcriber on this call now (again, if it already has a transcript)" data-ltr="${r.id}">Transcribe</button></td></tr>`;
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
    if (h) { const td = h.el.querySelector("td.tr"); if (td && !td.dataset.edited && !td.classList.contains("editing")) { td.textContent = text; td.title = text; } h.text += " " + text.toLowerCase(); applyHistFilter(); }
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
      $("detMeta").textContent = `#${r.id} · ${r.sha256 ? "sha256 " + r.sha256.slice(0, 12) + "…" : "no audio"}${r.dropped_blocks ? ` · ${r.dropped_blocks} stream drop(s)` : ""}${r.poor_frames ? ` · ${r.poor_frames}/${Math.round(r.secs * 50)} frames concealed` : ""}`;
      $("detBody").innerHTML = `<div class="det">
        <div><b>${esc(r.tg_name)}</b> <span class="faint">TG ${r.tg}</span>${r.service ? ` · <span class="svc">${esc(r.service)}</span>` : ""}${r.category ? ` · <span class="cat">${esc(r.category)}</span>` : ""}${r.encrypted ? ' · <span class="badge enc">Encrypted</span>' : ""} · unit ${r.unit_name ? esc(r.unit_name) + " (" + r.unit + ")" : r.unit} · ${(r.freq_hz / 1e6).toFixed(4)} MHz · ${r.modulation} · ${r.secs.toFixed(1)}s${r.emergency ? ' · <span class="badge emg">EMERGENCY</span>' : ""}</div>
        <div class="faint">${fmtT(r.start)} · ${esc([r.system, r.site].filter(Boolean).join(" · "))} ${r.patched_with.length ? "· patched " + r.patched_with.join(",") : ""}</div>
        ${(r.fired || []).length ? `<div class="k">Tripwires</div><div class="twlist">${r.fired.map((f) => `<div>${firedBadges([f])} <span class="faint">${esc(f.status === "quiet" ? "looked, stayed quiet" : f.status)} · ${esc(fmtT(f.at))}</span></div>`).join("")}</div>` : ""}
        <div class="xport" style="margin:8px 0">${r.audio ? `<button class="btn sm" id="detPlay">▶ Play</button>` : ""}<button class="btn sm" id="detCart">${cart.has(r.id) ? "Remove from cart" : "Add to cart"}</button><button class="btn sm" id="detTr">Transcribe${r.transcript ? " again" : ""}</button>${r.audio ? `<button class="btn sm" id="detUp" title="Send to the enabled sharing services">Upload</button>` : ""}</div>
        <div class="k">Machine transcript ${r.transcript_model ? "· " + r.transcript_model : ""}</div>
        <div class="machine">${esc(r.transcript || "—")}</div>
        <div class="k">Transcript · edit in place${r.transcript_edited ? " · edited" : ""} <span class="faint">(kept beside the machine text above, which is never changed)</span></div>
        <textarea id="detEdit" placeholder="Nothing transcribed yet — type what was said…">${esc(r.transcript_edited || r.transcript || "")}</textarea>
        <div class="xport" style="margin-top:6px"><button class="btn primary sm" id="detSave">Save edit</button><button class="btn ghost sm" id="detClearEdit">Clear edit</button><span class="meta" id="detSaved">${r.edited_at ? "edited " + fmtT(r.edited_at) : ""}</span></div>
      </div>`;
      const play = $("detPlay"); if (play) play.onclick = () => invoke("library_play", { id }).catch((e) => alert(e));
      $("detCart").onclick = () => { cartToggle(r.id, `${fmtT(r.start)} ${r.tg_name} · ${r.secs.toFixed(1)}s`); libSelect(id); };
      $("detTr").onclick = () => invoke("transcribe_call", { id }).then(() => $("detSaved").textContent = "transcribing…").catch((e) => alert(e));
      const up = $("detUp"); if (up) up.onclick = () => invoke("upload_call", { id }).then(() => $("detSaved").textContent = "queued for upload").catch((e) => alert(e));
      // The box starts with whatever is best known, so a one-word fix is a
      // one-word edit. Saving text identical to the machine transcript keeps
      // the call unedited rather than storing a copy.
      $("detSave").onclick = async () => { try { const v = $("detEdit").value; const text = v.trim() === (r.transcript || "").trim() ? "" : v; await invoke("library_set_edited", { id, text }); $("detSaved").textContent = text ? "saved" : "same as the machine text — no edit kept"; libSearchRefreshRow(id); } catch (e) { alert(e); } };
      $("detClearEdit").onclick = async () => { $("detEdit").value = r.transcript || ""; await invoke("library_set_edited", { id, text: "" }); $("detSaved").textContent = "edit cleared"; libSearchRefreshRow(id); };
    } catch (e) { alert(e); }
  }
  window.libRefreshRow = (id) => libSearchRefreshRow(id);
  // A rule fired about a call on screen: badge it without a reload.
  window.libMarkFired = (id, f) => {
    const r = libRows.find((x) => x.id === id); if (!r) return;
    r.fired = [f, ...(r.fired || []).filter((x) => x.rule_name !== f.rule_name)];
    const tr = $("libBody").querySelector(`tr[data-id="${id}"]`); if (tr) { tr.outerHTML = libRowHtml(r); wireLibRows(); }
  };
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
    try { await invoke("transcribe_configure", { settings: { enabled: $("trEnabled").checked, engine: $("trEngine").value, model: $("trModel").value, language: $("trLang").value.trim() || "en", device: $("trDevice").value } }); $("trMeta").textContent = "saved"; setTimeout(trRefresh, 800); setTimeout(trModelsRender, 900); }
    catch (e) { alert(e); }
  };
  $("trEnabled").onchange = $("trSave").onclick;
  async function libStatsRefresh() {
    try { const st = await invoke("library_stats"); const [n, secs, tr, dir] = Array.isArray(st) ? st : [st.count, st.seconds, st.transcribed, st.dir]; $("libStats").textContent = `${n} calls · ${(secs / 60).toFixed(0)} min · ${tr} transcribed`; $("libDir").textContent = dir; } catch (e) { log(`library_stats: ${e}`); }
  }
  // Retention (what the library keeps) lives in retention.js and the
  // backend's hourly timer; the page no longer prunes by itself.
  trRefresh(); libStatsRefresh(); window.libStatsRefresh = libStatsRefresh;

  /* ---------- transcript corrections (global + per-talkgroup) ---------- */
  let tcRules = [];
  async function tcLoad() {
    try { const v = await invoke("corrections_get"); tcRules = (v || []).map(([tg, from, to]) => ({ tg, from, to })); }
    catch (e) { log(`corrections_get: ${e}`); }
    tcRender();
  }
  function tcPersist() {
    invoke("corrections_set", { entries: tcRules.map((r) => [r.tg, r.from, r.to]) }).catch((e) => log(`corrections_set: ${e}`));
  }
  function tcRender() {
    $("tcList").innerHTML = tcRules.length ? tcRules.map((r, i) => `<div class="row"><span class="grow"><span class="mono">${r.tg ? `TG ${r.tg}` : "All TGs"}</span> <b>${esc(r.from)}</b> → ${esc(r.to)}</span><button class="btn ghost" data-tcdel="${i}">✕</button></div>`).join("") : '<div class="row"><span class="grow" style="color:var(--ink-faint)">No corrections yet — add the words your channels keep getting wrong. Leave TG blank to apply everywhere.</span></div>';
    $("tcList").querySelectorAll("[data-tcdel]").forEach((b) => b.onclick = () => { tcRules.splice(+b.dataset.tcdel, 1); tcPersist(); tcRender(); });
  }
  $("tcAdd").onclick = () => {
    const tgRaw = $("tcTg").value.trim(), from = $("tcFrom").value.trim(), to = $("tcTo").value.trim();
    const tg = tgRaw === "" ? null : parseInt(tgRaw, 10);
    if (tgRaw !== "" && !Number.isFinite(tg)) { alert("TG must be a number, or blank for every talkgroup."); return; }
    if (!from || !to) { alert("Enter the misheard and correct words (TG blank → every talkgroup)."); return; }
    tcRules.push({ tg, from, to });
    $("tcFrom").value = $("tcTo").value = "";
    tcPersist(); tcRender();
  };
  tcLoad();

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
    const pl = plForSid(r.sid), f = filtFor(pl), p = priOf(r.id, pl), c = colorOf(r.id), rule = ruleFor(r.id);
    const pol = (k, glyph, title) => `<button data-pol="${k}:${r.id}" class="${polAllows(k, r.id) ? "on-ok" : "off"}" title="${title}: ${polAllows(k, r.id) ? "yes" : "no"} — click to toggle">${glyph}</button>`;
    return `<tr class="${r.encrypted ? "enc" : ""}" data-tg="${r.id}" ${c ? `data-color="${c}" style="--tgc:${c}"` : ""}><td><input type="checkbox" data-tick="${r.id}" ${alTicked.has(r.id) ? "checked" : ""}></td><td class="mono">${r.id}</td><td>${esc(r.alias)}</td><td>${esc(r.description)}${rule ? ` <small class="faint" title="range rule">▸ ${esc(rule.name || rule.lo + "–" + rule.hi)}</small>` : ""}</td><td><small>${esc(r.tag)}</small></td><td><small>${esc(r.category)}</small></td><td><small class="mono">${esc(srcLabel(r.source))}</small></td>` +
      `<td class="act">${pol("record", "●", "Record audio")}${pol("stream", "▶", "Stream live")}${pol("upload", "↑", "Upload to sharing services")}</td>` +
      `<td class="act"><button class="swatch" data-color="${r.id}" title="Colour — click to cycle" style="background:${c || "transparent"}"></button><input type="number" class="priin" data-pri="${r.id}" data-pl="${esc(pl)}" min="1" max="99" value="${p}" title="Priority 1–99 (1 = highest)${plName(pl) ? " on " + esc(plName(pl)) : ""}">` +
      `<button data-bell="${r.id}" class="${bells.has(r.id) ? "bell" : ""}">🔔</button><button data-avoid="${r.id}" data-pl="${esc(pl)}" class="${f.avoid.has(r.id) ? "on" : ""}">⏱</button><button data-lock="${r.id}" data-pl="${esc(pl)}" class="${f.lockout.has(r.id) || (rule && rule.lock) ? "on" : ""}" title="Lock out${plName(pl) ? " on " + esc(plName(pl)) : ""}">⊘</button></td></tr>`;
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
    document.querySelectorAll("#set-aliases th[data-sort]").forEach((th) => { th.classList.toggle("asc", th.dataset.sort === k && d > 0); th.classList.toggle("desc", th.dataset.sort === k && d < 0); });
    const tb = $("alBody");
    tb.querySelectorAll("input[data-tick]").forEach((c) => c.onchange = () => { c.checked ? alTicked.add(+c.dataset.tick) : alTicked.delete(+c.dataset.tick); $("grpMeta").textContent = alTicked.size ? `${alTicked.size} ticked` : ""; });
    tb.querySelectorAll("button[data-pol]").forEach((b) => b.onclick = () => { const [k, tg] = b.dataset.pol.split(":"); polSet(k, +tg, !polAllows(k, +tg)); pushPolicies(); alRender(); });
    tb.querySelectorAll("input[data-pri]").forEach((b) => b.onchange = () => { setPriority(+b.dataset.pri, b.value, b.dataset.pl); alRender(); });
    tb.querySelectorAll("button[data-bell]").forEach((b) => b.onclick = () => { toggleBell(+b.dataset.bell); alRender(); });
    tb.querySelectorAll("button[data-avoid]").forEach((b) => b.onclick = () => { avoidFor(+b.dataset.avoid, b.dataset.pl); alRender(); });
    tb.querySelectorAll("button[data-lock]").forEach((b) => b.onclick = () => { toggleLock(+b.dataset.lock, b.dataset.pl); alRender(); });
    tb.querySelectorAll("button[data-color]").forEach((b) => b.onclick = () => { cycleColor(+b.dataset.color); alRender(); });
    renderTagChips();
  }
  $("alFilter").oninput = alRender;
  $("alSource").onchange = alRender;
  document.querySelectorAll("#set-aliases th[data-sort]").forEach((th) => th.onclick = () => { alSort = { key: th.dataset.sort, dir: alSort.key === th.dataset.sort ? -alSort.dir : 1 }; save("hs.alsort", alSort); alRender(); });
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

  /* ---------- tag (service-type) filter for the live follow ---------- */
  const tagSel = new Set(store("hs.tags", []));
  function renderTagChips() {
    const tags = [...new Set(alRows.map((r) => r.tag).filter(Boolean))].sort();
    [...tagSel].forEach((t) => { if (!tags.includes(t)) tagSel.delete(t); });
    $("tagChips").innerHTML = tags.length ? tags.map((t) => `<span class="chip ${tagSel.has(t) ? "on" : ""}" data-tag="${esc(t)}">${esc(t)}</span>`).join("") : '<span class="faint">load a catalog to filter by tag</span>';
    $("tagChips").querySelectorAll(".chip").forEach((ch) => ch.onclick = () => { const t = ch.dataset.tag; tagSel.has(t) ? tagSel.delete(t) : tagSel.add(t); save("hs.tags", [...tagSel]); renderTagChips(); pushTagAllowlist(); });
    $("tagSummary").textContent = tagSel.size ? `${tagSel.size} of ${tags.length}` : "all";
  }
  // The effective allowlist is the playlist's talkgroups (if any) intersected
  // with the chosen tags. Re-pushed after every playlist activation,
  // since activation sets the backend allowlist to the playlist alone.
  async function pushTagAllowlist() {
    // Every playlist a picked site follows (plus the unscoped set) is narrowed to the chosen tags.
    const picked = [$("playlist").value, ...plAlsoIds()].map((id) => sites.find((s) => s.id === id)).filter(Boolean);
    const plIds = [...new Set(picked.map((s) => s.playlist || ""))];
    if (!plIds.length) plIds.push("");
    const inTags = new Set(alRows.filter((r) => tagSel.has(r.tag)).map((r) => r.id));
    let shown = 0;
    for (const id of plIds) {
      if (!tagSel.size) { await invoke("set_allowlist", { tgs: null, playlist: id || null }).catch((e) => log(`allowlist: ${e}`)); continue; }
      const pl = playlists.find((p) => p.id === id), plSet = pl && pl.tgs.length ? new Set(pl.tgs) : null;
      const tgs = plSet ? [...plSet].filter((t) => inTags.has(t)) : [...inTags];
      shown += tgs.length;
      await invoke("set_allowlist", { tgs, playlist: id || null }).catch((e) => log(`allowlist: ${e}`));
    }
    if (tagSel.size) $("followMeta").textContent = `tags: ${[...tagSel].join(", ")} → ${shown} talkgroups`;
  }
  window.pushTagAllowlist = pushTagAllowlist;

  /* ---------- talkgroup range rules ---------- */
  $("rgColor").innerHTML = '<option value="">none</option>' + PALETTE.map((c) => `<option value="${c}" style="color:${c}">${c}</option>`).join("");
  window.renderRules = function renderRules() {
    $("rgList").innerHTML = tgRules.length ? tgRules.map((r, i) => `<div class="row"><span class="swatch" style="background:${r.color || "transparent"}"></span><span class="grow"><b>${esc(r.name || "")}</b> <span class="mono">${r.lo}–${r.hi}</span><br><small>${[r.pri ? `priority ${r.pri}` : "", r.lock ? "locked out" : "", r.bell ? "alert" : ""].filter(Boolean).join(" · ") || "colour only"}</small></span><button class="btn ghost" data-rgdel="${i}">✕</button></div>`).join("") : '<div class="row"><span class="grow" style="color:var(--ink-faint)">No range rules.</span></div>';
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
  $("urTry").oninput = async () => { const id = parseInt($("urTry").value, 10); if (!Number.isFinite(id)) { $("urTryOut").textContent = ""; return; } try { const n = await invoke("unit_resolve", { id, sid: unitSid() }); $("urTryOut").textContent = n ? `${id} → ${n}` : `${id} → no alias or rule matches`; } catch (_) {} };
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
  const gb = (b) => b >= 1e9 ? `${(b / 1e9).toFixed(1)} GB` : `${Math.max(1, Math.round(b / 1e6))} MB`;
  async function trModelsRender() {
    try {
      const rows = await invoke("transcribe_models"); const names = [...new Set(rows.map((r) => r.model))];
      const total = rows.filter((r) => r.downloaded).reduce((a, r) => a + (r.bytes || 0), 0);
      const meta = $("trModelsMeta"); if (meta) meta.textContent = total ? `${gb(total)} on disk` : "";
      $("trModels").innerHTML = names.map((m) => {
        const cell = (eng) => { const r = rows.find((x) => x.model === m && x.engine === eng); const key = `${eng}/${m}`;
          if (!r) return "";
          if (r.downloaded) return `<span class="badge clear" title="${esc(r.path || "")}">✔ ${gb(r.bytes || 0)}</span>${r.in_use ? ' <span class="badge">in use</span>' : ` <button class="btn ghost sm" data-rmmodel="${key}" title="Delete this model from disk">Delete</button>`}`;
          return downloading.has(key) ? `<span class="meta">downloading…</span>` : `<button class="btn ghost sm" data-dl="${key}">Download</button>`; };
        return `<tr><td class="mono">${m} <small class="faint">${MODEL_SIZES[m] || ""}</small></td><td>${cell("faster-whisper")}</td><td>${cell("openai-whisper")}</td><td>${cell("mlx-whisper")}</td></tr>`;
      }).join("");
      $("trModels").querySelectorAll("[data-dl]").forEach((b) => b.onclick = () => { const [engine, model] = b.dataset.dl.split("/"); downloading.add(b.dataset.dl); trModelsRender(); invoke("transcribe_download", { engine, model }).catch((e) => { downloading.delete(b.dataset.dl); alert(e); trModelsRender(); }); });
      $("trModels").querySelectorAll("[data-rmmodel]").forEach((b) => b.onclick = async () => {
        const [engine, model] = b.dataset.rmmodel.split("/"); const r = rows.find((x) => x.engine === engine && x.model === model);
        if (!(await uiConfirm(`Delete ${engine} ${model} (${gb((r && r.bytes) || 0)}) from this Mac? It downloads again if you choose it later.`, "Delete"))) return;
        try { const freed = await invoke("transcribe_delete", { engine, model }); uiToast(`Deleted ${engine} ${model} — ${gb(freed)} freed`); } catch (e) { uiToast(`${e}`, "err"); }
        trModelsRender();
      });
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
      $("fmtReencodeCodec").textContent = f.format.codec.toUpperCase();
      [...$("fmtCodec").options].forEach((o) => { if (o.value !== "wav") o.disabled = !f.ffmpeg; });
    } catch (e) { log(`format_get: ${e}`); }
  }
  const fmtSave = async () => {
    try { await invoke("format_set", { format: { codec: $("fmtCodec").value, bitrate_kbps: +$("fmtKbps").value, mode: $("fmtMode").value } }); $("fmtMeta").textContent = "saved"; setTimeout(fmtRefresh, 700); }
    catch (e) { alert(e); fmtRefresh(); }
  };
  $("fmtCodec").onchange = $("fmtKbps").onchange = $("fmtMode").onchange = fmtSave;
  fmtRefresh();

  /* ---------- re-encode existing files ---------- */
  const fmtReencode = async () => {
    const btn = $("fmtReencode"), meta = $("fmtReencodeMeta");
    btn.disabled = true; meta.textContent = "starting…";
    try { await invoke("library_reencode"); }
    catch (e) { btn.disabled = false; meta.textContent = `error: ${e}`; }
  };
  $("fmtReencode").onclick = fmtReencode;
  if (listen) {
    listen("reencode_progress", (e) => {
      const p = e.payload;
      $("fmtReencodeMeta").textContent = `${p.converted} converted · ${p.skipped} skipped · ${p.failed} failed · ${p.done}/${p.total}`;
    });
    listen("reencode_done", (e) => {
      const p = e.payload;
      $("fmtReencode").disabled = false;
      $("fmtReencodeMeta").textContent = `done — ${p.converted} converted, ${p.skipped} skipped, ${p.failed} failed`;
    });
    listen("reencode_error", (e) => {
      log(`reencode: ${e.payload.path}: ${e.payload.error}`);
    });
  }

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
      $("rrPass").placeholder = st.has_password ? "saved on this Mac" : "";
      const missing = [];
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
    await invoke("rr_save", { username: $("rrUser").value, password: $("rrPass").value, sid: sidVal() });
    $("rrPass").value = "";
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

  /* ---------- a loaded system → sites to hear, and a playlist of its talkgroups ---------- */
  let sys = null, picked = new Set();
  async function loadSystem(sid) {
    try {
      $("rrDownload").disabled = true; $("findMeta").textContent = "downloading system…";
      if ($("rrPass").value) await rrSaveCreds();
      sys = await invoke("rr_download", { sid });
      $("findMeta").textContent = "";
      $("sysPanel").style.display = "";
      $("sysName").textContent = sys.name;
      $("sysMeta").textContent = `sid ${sys.sid} · ${sys.talkgroups} talkgroups · ${sys.sites.length} sites`;
      $("loadcat").textContent = sys.talkgroups + " TGs";
      picked = new Set();
      renderSiteAddPl(); renderSites(); renderCats(); renderTgs();
      if (!playlists.some((p) => p.sid === sys.sid)) $("plName").value = shortSystemName(sys.name);
      $("sysLoaded").textContent = `✔ Loaded ${sys.talkgroups} talkgroups from ${sys.name} — they now name calls; see the Aliases tab to check any talkgroup.`;
      logEvent(`loaded ${sys.talkgroups} talkgroups from ${sys.name}`);
      rrRefresh(); aliasesRefresh();
      $("sysPanel").scrollIntoView({ behavior: "smooth", block: "start" });
    } catch (e) { $("findMeta").textContent = ""; alert(e); }
    finally { $("rrDownload").disabled = false; }
  }
  // "Example Emergency Services Agency (EESA) (Formerly XYZ)" → "EESA"
  const shortSystemName = (n) => { const m = /\(([A-Z0-9-]{2,8})\)/.exec(n || ""); return m ? m[1] : (n || "Playlist"); };
  $("rrDownload").onclick = () => { const sid = sidVal(); if (sid == null) { alert("Enter a system ID."); return; } $("rrProg").style.display = ""; $("rrProgBar").style.width = "3%"; $("rrProgText").textContent = "connecting…"; loadSystem(sid); };
  const siteRate = (s) => ((s.span_mhz ? s.span_mhz[1] - s.span_mhz[0] : 0) <= 1.9 ? 2500000 : 10000000);
  // The playlist a newly added site will follow: one of the loaded system's.
  function renderSiteAddPl() {
    if (!sys) return;
    const mine = playlists.filter((p) => p.sid === sys.sid), cur = $("siteAddPl").value;
    $("siteAddPl").innerHTML = '<option value="">every talkgroup (no playlist)</option>' + mine.map((p) => `<option value="${esc(p.id)}">${esc(p.name)} · ${p.tgs.length ? p.tgs.length + " TGs" : "all TGs"}</option>`).join("");
    $("siteAddPl").value = mine.some((p) => p.id === cur) ? cur : (mine.length ? mine[0].id : "");
  }
  function renderSites() {
    const saved = new Set(sites.filter((s) => s.sid === sys.sid).map((s) => s.site_id));
    $("siteList").innerHTML = sys.sites.map((s) =>
      `<div class="row ${saved.has(s.site_id) ? "on" : ""}" data-site="${s.site_id}">` +
      `<span class="grow"><b>${s.site_id}</b> ${esc(s.name)}${s.tdma_control ? ' <small style="color:var(--enc)">TDMA CC — not decodable yet</small>' : ""}${saved.has(s.site_id) ? ' <small class="faint">· added</small>' : ""}</span>` +
      (s.nac != null ? `<span class="mono">NAC 0x${s.nac.toString(16).toUpperCase().padStart(3, "0")}</span>` : "") +
      `<span class="mono">${s.control_mhz[0].toFixed(4)} MHz</span>` +
      (s.span_mhz ? `<span class="mono">${(s.span_mhz[1] - s.span_mhz[0]).toFixed(2)} MHz span</span>` : "") +
      `<button class="btn ${saved.has(s.site_id) ? "ghost" : "primary"} sm" data-addsite="${s.site_id}" title="Save this site so it can be picked in the top bar; it follows the playlist chosen above">${saved.has(s.site_id) ? "Add again" : "Add site"}</button></div>`).join("");
    $("siteList").querySelectorAll("[data-addsite]").forEach((b) => b.onclick = () => addSite(sys.sites.find((s) => s.site_id === +b.dataset.addsite)));
  }
  async function addSite(s) {
    if (!s) return;
    const lo = s.span_mhz ? s.span_mhz[0] : s.control_mhz[0], hi = s.span_mhz ? s.span_mhz[1] : s.control_mhz[0];
    const site = { id: "", name: s.name, sid: sys.sid, system_name: sys.name, site_id: s.site_id, site_name: s.name, nac: s.nac,
      control_mhz: s.control_mhz[0], center_mhz: +((lo + hi) / 2).toFixed(4), rate: siteRate(s),
      span_mhz: s.span_mhz ? [s.span_mhz[0], s.span_mhz[1]] : null, playlist: $("siteAddPl").value || null };
    try { renderSavedSites(await invoke("site_save", { site })); renderSites(); $("stMeta").textContent = `added ${s.name}`; logEvent(`added site ${s.name}`); } catch (e) { alert(e); }
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
    if (!sys) { alert("Load a system first."); return; }
    const name = $("plName").value.trim(); if (!name) { alert("Give the playlist a name."); return; }
    // Saving under an existing name of the same system replaces its talkgroups (lockout and priorities stay).
    const existing = playlists.find((p) => p.sid === sys.sid && p.name.toLowerCase() === name.toLowerCase());
    const playlist = { id: existing ? existing.id : "", name, sid: sys.sid, system_name: sys.name, tgs: [...picked].sort((a, b) => a - b), lockout: [], priorities: [] };
    try { renderPlaylists(await invoke("playlist_save", { playlist })); $("plMeta").textContent = existing ? `updated ${name}` : "saved"; renderSiteAddPl(); if (!existing) $("siteAddPl").value = playlists.find((p) => p.name === name && p.sid === sys.sid)?.id || ""; } catch (e) { alert(e); }
  };

  /* ---------- playlists (a system's talkgroups + its lockout and priorities) and sites (what the radio tunes) ---------- */
  let playlists = [], sites = [];
  // The band centre that reaches every control channel on one radio, if one
  // exists: the radio decodes ±0.4 of its (normalised) rate around the centre.
  function commonCentre(controls_hz, rate) {
    const norm = rate >= 9_000_000 ? 9_600_000 : rate >= 2_450_000 && rate < 2_550_000 ? 2_400_000 : rate;
    const half = norm * 0.4, lo = Math.min(...controls_hz), hi = Math.max(...controls_hz);
    return { ok: hi - lo < 2 * half - 25_000, center: Math.round((lo + hi) / 2), span: hi - lo, half };
  }
  window.commonCentre = commonCentre;
  // Sites picked alongside the main one (their ids, in preferences).
  const plAlsoIds = () => (store("hs.prefs", {}).alsoPlaylists || []);
  function plAlsoSelected() { const ids = plAlsoIds(); return sites.filter((p) => ids.includes(p.id)); }
  function plMoreRender() {
    const cur = $("playlist").value;
    const ids = plAlsoIds();
    const others = sites.filter((p) => p.id !== cur);
    $("plMoreList").innerHTML = others.length ? others.map((p) =>
      `<label><input type="checkbox" data-also="${esc(p.id)}" ${ids.includes(p.id) ? "checked" : ""} /> <span>${esc(p.name)} <small>${esc(plName(p.playlist) || "all talkgroups")} · ${p.control_mhz.toFixed(4)} MHz</small></span></label>`).join("")
      : `<div class="none">No other sites yet — add one under Settings → Playlists.</div>`;
    $("plMoreList").querySelectorAll("[data-also]").forEach((cb) => cb.onchange = () => {
      const set = new Set(plAlsoIds()); cb.checked ? set.add(cb.dataset.also) : set.delete(cb.dataset.also);
      save("hs.prefs", { ...store("hs.prefs", {}), alsoPlaylists: [...set] }); plMoreBadge();
    });
    plMoreBadge();
  }
  function plMoreBadge() {
    const n = plAlsoSelected().filter((p) => p.id !== $("playlist").value).length;
    const b = $("plMore"); if (!b) return;
    b.classList.toggle("on", n > 0); b.innerHTML = n ? `+<span class="n">${n}</span>` : "+";
  }
  if ($("plMore")) {
    $("plMore").onclick = (e) => { if (e && e.stopPropagation) e.stopPropagation(); const pop = $("plMorePop"); pop.hidden = !pop.hidden; if (!pop.hidden) plMoreRender(); };
    document.addEventListener("click", (e) => { const pop = $("plMorePop"); if (!pop.hidden && !e.target.closest("#plMorePop")) pop.hidden = true; });
    $("playlist").addEventListener("change", plMoreBadge);
  }
  const siteMeta = (s) => { const p = playlists.find((x) => x.id === s.playlist); return `site: ${s.name} · ${p ? `playlist ${p.name} (${p.tgs.length ? p.tgs.length + " talkgroups" : "all talkgroups"})` : "all talkgroups"}`; };
  function renderPlaylists(list) {
    playlists = list;
    loadPlaylistFilters(list);
    $("plEmpty").style.display = list.length ? "none" : "";
    $("plList").innerHTML = list.map((p) => {
      const users = sites.filter((s) => s.playlist === p.id);
      return `<div class="row" data-id="${esc(p.id)}"><span class="grow"><input class="plname" data-plname="${esc(p.id)}" value="${esc(p.name)}" title="Rename — press Enter" spellcheck="false" /><br><small>${esc(p.system_name)} · ${p.tgs.length ? p.tgs.length + " TGs" : "all TGs"} · ${(p.lockout || []).length} locked out · ${(p.priorities || []).length} priorities · ${users.length ? "sites: " + users.map((s) => esc(s.name)).join(", ") : "no site follows it yet"}</small></span>` +
        `<button class="btn ghost" data-del="${esc(p.id)}">Delete</button></div>`;
    }).join("");
    $("plList").querySelectorAll("[data-plname]").forEach((inp) => inp.onchange = async () => {
      const p = playlists.find((x) => x.id === inp.dataset.plname), name = inp.value.trim(); if (!p || !name || name === p.name) return;
      try { renderPlaylists(await invoke("playlist_save", { playlist: { ...p, name } })); renderSavedSites(sites); } catch (e) { alert(e); }
    });
    $("plList").querySelectorAll("[data-del]").forEach((b) => b.onclick = async () => {
      const p = playlists.find((x) => x.id === b.dataset.del), users = sites.filter((s) => s.playlist === b.dataset.del);
      if (!(await uiConfirm(`Delete playlist “${p ? p.name : ""}”?${users.length ? ` ${users.length} site${users.length === 1 ? "" : "s"} will follow every talkgroup instead.` : ""}`, "Delete"))) return;
      try { renderPlaylists(await invoke("playlist_delete", { id: b.dataset.del })); renderSavedSites(await invoke("sites_list")); } catch (e) { alert(e); }
    });
    if (sys) renderSiteAddPl();
    if (typeof renderSavedSites === "function" && sites.length) renderSavedSites(sites);
  }
  function renderSavedSites(list) {
    sites = list;
    $("stEmpty").style.display = list.length ? "none" : "";
    const plOpts = (s) => '<option value="">every talkgroup</option>' + playlists.filter((p) => p.sid === s.sid).map((p) => `<option value="${esc(p.id)}" ${p.id === s.playlist ? "selected" : ""}>${esc(p.name)}</option>`).join("");
    $("stList").innerHTML = list.map((s) =>
      `<div class="row" data-id="${esc(s.id)}"><span class="grow"><input class="plname" data-stname="${esc(s.id)}" value="${esc(s.name)}" title="Rename — press Enter" spellcheck="false" /><br><small>${esc(s.system_name)} · site ${s.site_id} ${esc(s.site_name)} · ${s.control_mhz.toFixed(4)} MHz</small></span>` +
      `<label class="tb"><span>Playlist</span><select data-stpl="${esc(s.id)}">${plOpts(s)}</select></label>` +
      `<button class="btn primary" data-act="${esc(s.id)}">Use</button><button class="btn ghost" data-stdel="${esc(s.id)}">Delete</button></div>`).join("");
    const saveSite = async (id, patch) => { const s = sites.find((x) => x.id === id); if (!s) return; try { renderSavedSites(await invoke("site_save", { site: { ...s, ...patch } })); renderPlaylists(playlists); } catch (e) { alert(e); } };
    $("stList").querySelectorAll("[data-stname]").forEach((inp) => inp.onchange = () => { const name = inp.value.trim(); if (name) saveSite(inp.dataset.stname, { name }); });
    $("stList").querySelectorAll("[data-stpl]").forEach((sel) => sel.onchange = () => saveSite(sel.dataset.stpl, { playlist: sel.value || null }));
    $("stList").querySelectorAll("[data-act]").forEach((b) => b.onclick = () => activateSite(b.dataset.act));
    $("stList").querySelectorAll("[data-stdel]").forEach((b) => b.onclick = async () => {
      if (!(await uiConfirm("Delete this site?", "Delete"))) return;
      try { renderSavedSites(await invoke("site_delete", { id: b.dataset.stdel })); renderPlaylists(playlists); if (sys) renderSites(); } catch (e) { alert(e); }
    });
    const cur = $("playlist").value;
    $("playlist").innerHTML = '<option value="">— every talkgroup —</option>' + list.map((p) => `<option value="${esc(p.id)}">${esc(p.name)}${p.playlist ? ` · ${esc(plName(p.playlist))}` : ""}</option>`).join("");
    $("playlist").value = list.some((p) => p.id === cur) ? cur : "";
    plMoreBadge();
  }
  // Pick a site: its tuning goes into the top bar; Start follows it with its playlist.
  function activateSite(id) {
    const p = sites.find((s) => s.id === id);
    $("playlist").value = p ? p.id : "";
    if (typeof pushTagAllowlist === "function") pushTagAllowlist();
    if (p) {
      modeSel = "follow"; setSeg($("modeSeg"), "follow"); applyMode();
      $("freq").value = p.control_mhz.toFixed(4) + "M"; $("center").value = p.center_mhz.toFixed(4) + "M";
      if (p.span_mhz) { $("cpLo").value = p.span_mhz[0].toFixed(4); $("cpHi").value = p.span_mhz[1].toFixed(4); save("hs.span", p.span_mhz); if (typeof coveragePlan === "function") coveragePlan(); }
      if ($("source").value === "rtlsdr" && p.rate > 2400000) {
        $("rate").value = "2400000"; $("center").value = p.control_mhz.toFixed(4) + "M";
        logEvent(`RTL-SDR covers ±1.2 MHz: centred on the control channel; calls outside that span will be skipped — pick Airspy R2 for the whole site`, "warn");
      } else { $("rate").value = String(p.rate); }
      syncRate();
      if ($("pillText").textContent !== "standby") logEvent("site changed — press Stop, then Start to retune", "warn");
      $("followMeta").textContent = siteMeta(p);
      showView("monitor");
    } else { $("followMeta").textContent = ""; }
  }
  $("playlist").onchange = () => activateSite($("playlist").value);
  // Lockouts and priorities kept globally by earlier versions move onto every
  // playlist that has none of its own — once.
  function migrateLocalFilters() {
    if (store("hs.filtersMigrated", false)) return;
    const f0 = filtFor("");
    if (f0.lockout.size || f0.prio.size) for (const p of playlists) {
      if ((p.lockout || []).length || (p.priorities || []).length) continue;
      const f = filtFor(p.id); f.lockout = new Set(f0.lockout); f.prio = new Map(f0.prio);
      pushLockout(p.id); pushPriorities(p.id);
    }
    save("hs.filtersMigrated", true);
  }
  Promise.all([invoke("playlists_list"), invoke("sites_list")]).then(async ([pl, st]) => {
    sites = st || [];
    renderPlaylists(pl || []);
    renderSavedSites(sites);
    migrateLocalFilters();
    renderLockout();
    // Now that the playlists are known, send their filters again. The first
    // push happened before this list arrived, so it only reached the
    // unscoped set — and the part that carries muted listen groups (`extra`)
    // is deliberately never saved on the Rust side, so a run started now
    // would have nothing muted at all.
    pushLockout(); pushPriorities(); pushMuted();
    // Auto-start: opt-in, and only if the last-used site still exists.
    const pr = store("hs.prefs", {});
    if (pr.autostart && pr.lastPlaylist && sites.some((p) => p.id === pr.lastPlaylist) && !location.hash.startsWith("#autostart")) {
      // The detected radio decides the rate and centre a site gets (an RTL-SDR
      // is centred on the control channel); wait, briefly, until the radio list
      // has replaced the placeholder options before tuning.
      await new Promise((done) => { const t0 = Date.now(); (function poll() { if ($("source").value.includes("|") || Date.now() - t0 > 4000) done(); else setTimeout(poll, 50); })(); });
      activateSite(pr.lastPlaylist);
      logEvent(`auto-start: ${sites.find((p) => p.id === pr.lastPlaylist).name}`);
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
  const TGS = [[10103,"Police Dispatch NW"],[10106,"Police Dispatch SE"],[10147,"Fire Dispatch"],[10202,"County EMS"],[10308,"Sheriff Patrol"]];
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
    if (tick === 200) handleFollow({ kind: "talker_alias", tg: 10147, name: "Fire Dispatch", alias: "ENG 21" });
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
    handleFollow({ kind: "call_start", tg: 10147, name: "Fire Dispatch", freq_mhz: 857.3875, priority: 10 });
    handleFollow({ kind: "call", tg: 10103, name: "Police Dispatch NW", source: 4910003, unit_name: "Car 12", talker_alias: "ENG 21", freq_mhz: 851.8125, modulation: "CQPSK", secs: 6.4, wav: null, emergency: false, patched_with: [] });
    handleFollow({ kind: "call", tg: 10308, name: "Sheriff Patrol", source: 4910008, freq_mhz: 858.3375, modulation: "CQPSK", secs: 3.1, wav: null, emergency: true, patched_with: [10204] });
    [["affiliated", 4910003, 10103], ["registered", 4910008, null], ["located", 4910011, 10147], ["refused", 4910012, 10202]].forEach(([what, unit, tg]) => handleFollow({ kind: "mobility", what, unit, unit_name: unit === 4910003 ? "Car 12" : null, tg, name: tg ? (TGS.find((t) => t[0] === tg) || [])[1] : null }));
    handleFollow({ kind: "location", unit: 4910003, unit_name: "Car 12", lat: 39.7684, lon: -86.1581 });
    handleFollow({ kind: "location", unit: 4910011, unit_name: null, lat: 39.79, lon: -86.17 });
    handleFollow({ kind: "talker_alias", tg: 10147, name: "Fire Dispatch", alias: "ENG 21" });
    handleFollow({ kind: "status", control_syncs: 412, calls: 2, out_of_band: 3, encrypted: 1, locked: 0, msps: 9.6, want_msps: 9.6, dropped: 0, elapsed_secs: 42 });
    if (!groups.length) { groups.push({ id: "gdemo1", name: "Hospitals", tgs: [10202], listen: true }, { id: "gdemo2", name: "EMS / Fire", tgs: [10147, 10202], listen: false }); renderGroupChips(); }
    { const h = history[0]; if (h) { const td = h.el.querySelector("td.tr"); td.textContent = "Engine 21 on scene, working structure fire, requesting second alarm."; } }
    colors.set(10147, "#f5b544"); tgRules.push({ lo: 10100, hi: 10199, name: "Police", pri: 10, color: "#7aa2ff", lock: false, bell: false });
    if (location.hash === "#discovery") { showView("settings"); setPage("discovery"); }
  }
}

/* ================= conversations tab ================= */
// Stored conversations: what a conversation rule stitched, summarised and sent.
let convRows = [], convSel = null, convStats = null;
const convAgo = (t) => { const s = Math.max(0, Math.round(Date.now() / 1000 - t)); return s < 60 ? `${s} s ago` : s < 3600 ? `${Math.round(s / 60)} min ago` : s < 86400 ? `${Math.round(s / 3600)} h ago` : `${Math.round(s / 86400)} d ago`; };
const convWhen = (t) => new Date(t * 1000).toLocaleString("en-US", { month: "numeric", day: "numeric", year: "numeric", hour: "numeric", minute: "2-digit", second: "2-digit" });
const convClock = (t) => new Date(t * 1000).toLocaleTimeString("en-US", { hour: "numeric", minute: "2-digit", second: "2-digit" });
const convDur = (s) => { s = Math.max(0, Math.round(s)); return s < 60 ? `${s} s` : `${Math.floor(s / 60)} min ${s % 60} s`; };
const convBadge = (st) => `<span class="badge ${st === "sent" ? "clear" : st === "failed" ? "enc" : ""}">${esc(st || "?")}</span>`;
const convUnitName = (p) => p.unit_name || `radio ${p.unit}`;
function convArgs() { const tg = $("cvTgFilter").value; return { q: $("cvSearch").value.trim() || null, tg: tg ? +tg : null, before: null, limit: 100 }; }
async function convLoad(more) {
  if (!TAURI) return;
  try {
    const args = convArgs(); if (more && convRows.length) args.before = convRows[convRows.length - 1].last_at;
    const rows = (await invoke("conversations_list", args)) || [];
    convRows = more ? convRows.concat(rows) : rows;
    $("cvOlder").disabled = rows.length < 100;
    convRenderList();
  } catch (e) { log(`conversations_list: ${e}`); }
  convLoadStats();
}
async function convLoadStats() {
  if (!TAURI) return;
  try {
    const st = (await invoke("conversations_stats")) || {};
    convStats = st;
    $("cvStTotal").textContent = st.total || 0; $("cvStSent").textContent = st.sent || 0; $("cvStFailed").textContent = st.failed || 0; $("cvStSkipped").textContent = st.skipped || 0;
    $("cvCount").textContent = st.total ? `(${st.total})` : "";
    const by = st.by_tg || [];
    $("cvStEmpty").style.display = by.length ? "none" : "";
    $("cvStBody").innerHTML = by.map(([tg, name, n]) => `<tr data-cvtg="${tg}" title="Show only this talkgroup"><td>${esc(name)} <span class="faint mono">${tg}</span></td><td>${n}</td></tr>`).join("");
    $("cvStBody").querySelectorAll("[data-cvtg]").forEach((tr) => tr.onclick = () => { $("cvTgFilter").value = tr.dataset.cvtg; convShowPage("list"); convLoad(false); });
    const sel = $("cvTgFilter"); const cur = sel.value;
    sel.innerHTML = `<option value="">All talkgroups</option>` + by.map(([tg, name]) => `<option value="${tg}">${esc(name)} (${tg})</option>`).join("");
    sel.value = cur; if (sel.value !== cur) sel.value = "";
  } catch (e) { log(`conversations_stats: ${e}`); }
}
function convRenderList() {
  $("cvCardsEmpty").style.display = convRows.length ? "none" : "";
  $("cvListMeta").textContent = convRows.length ? `${convRows.length} shown` : "";
  $("cvCards").innerHTML = convRows.map((r) => `<div class="cvcard ${esc(r.status)}" data-cvid="${r.id}">
    <div class="top"><span class="cid">${esc(r.conv_id)}</span>${convBadge(r.status)}${r.revision ? `<span class="badge">rev ${r.revision}</span>` : ""}${r.source === "test" ? `<span class="badge">test</span>` : ""}<span class="tgpill" title="TG ${r.tg} · ${esc(r.tg_name)}">${esc(r.tg_desc || r.tg_name)}</span></div>
    <div class="when">${esc(convWhen(r.first_at))} · ${esc(r.rule_name)} · ${r.calls} transmission${r.calls === 1 ? "" : "s"} · ${convDur(r.last_at - r.first_at)}</div>
    <div class="tags">${(r.units || []).map((u) => `<span class="cvunit">${esc(u)}</span>`).join("")}</div>
    <div class="summ"><span class="eyebrow">AI summary</span>${esc(r.summary || r.detail || "(no summary)")}</div>
    <div class="acts"><button class="btn ghost sm" data-cvopen="${r.id}">View details</button><button class="btn ghost sm" data-cvlisten="${r.id}">▶ Listen</button><button class="btn ghost sm" data-cvcopy="${r.id}">Copy message</button></div>
  </div>`).join("");
  const cards = $("cvCards");
  cards.querySelectorAll("[data-cvopen]").forEach((b) => b.onclick = () => convOpen(+b.dataset.cvopen));
  cards.querySelectorAll("[data-cvlisten]").forEach((b) => b.onclick = () => { const r = convRows.find((x) => x.id === +b.dataset.cvlisten); if (r) convListen(r); });
  cards.querySelectorAll("[data-cvcopy]").forEach((b) => b.onclick = () => { const r = convRows.find((x) => x.id === +b.dataset.cvcopy); if (r) convCopy(r); });
}
function convCopy(r) { const cb = navigator.clipboard; if (!cb) return; cb.writeText(r.message || r.summary || "").then(() => uiToast("Message copied"), () => {}); }
async function convListen(r) {
  const pieces = (r.pieces || []).filter((p) => p.id != null || p.audio);
  if (!pieces.length) { uiToast("No audio stored for this conversation", "err"); return; }
  uiToast(`Queued ${pieces.length} transmission${pieces.length === 1 ? "" : "s"}`);
  for (const p of pieces) { try { if (p.id != null) await invoke("library_play", { id: p.id }); else await invoke("play_wav", { path: p.audio }); } catch (e) { uiToast(`${e}`, "err"); return; } }
}
function convShowPage(which) {
  $("cvListPage").style.display = which === "list" ? "" : "none";
  $("cvDetailPage").style.display = which === "detail" ? "" : "none";
  $("cvSeg").querySelectorAll("button").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.v === which)));
}
async function convOpen(id) {
  let r = convRows.find((x) => x.id === id);
  if (TAURI) { try { r = (await invoke("conversation_get", { id })) || r; } catch (e) { uiToast(`${e}`, "err"); } }
  if (!r) return;
  convSel = r;
  $("cvSeg").querySelector('[data-v="detail"]').disabled = false;
  convShowPage("detail");
  $("cvdStatus").innerHTML = convBadge(r.status) + (r.revision ? ` <span class="badge">revised ×${r.revision}</span>` : "") + (r.source === "test" ? ` <span class="badge">test run</span>` : "");
  $("cvdAgo").textContent = r.sent_at ? `${convAgo(r.sent_at)}` : "";
  const pieces = r.pieces || [];
  const kv = [
    ["Talkgroup", `${esc(r.tg_desc || r.tg_name)} <span class="faint mono">${esc(r.tg_name)} · TG ${r.tg}</span>`],
    ["Conversation ID", `<span class="mono">${esc(r.conv_id)}</span>`],
    ["Rule", esc(r.rule_name)],
    ["Transmissions", `${pieces.length}`],
    ["Started", esc(convWhen(r.first_at))],
    ["Ended", `${esc(convWhen(r.last_at))} <span class="faint">· ${convDur(r.last_at - r.first_at)}</span>`],
    ["Sent", r.sent_at ? esc(convWhen(r.sent_at)) : "—"],
    ["Telegram chat", r.chat ? `<span class="mono">${esc(r.chat)}</span>` : "—"],
  ];
  $("cvdInfo").innerHTML = kv.map(([k, v]) => `<div><span class="k">${k}</span><span class="v">${v}</span></div>`).join("");
  // Participants: the mobile units in order of first appearance (the first
  // is the primary), then the fixed party's consoles.
  const seen = new Set(); const parts = [];
  for (const p of pieces) { if (seen.has(p.unit)) continue; seen.add(p.unit); parts.push(p); }
  const mobiles = parts.filter((p) => !p.fixed), fixed = parts.filter((p) => p.fixed);
  $("cvdParts").innerHTML = mobiles.map((p, i) => `<div class="p">${i === 0 ? '<span class="star" title="Primary unit">★</span>' : '<span style="width:1em"></span>'}<span class="mono faint">UID ${p.unit}</span><b>${esc(convUnitName(p))}</b><span class="cvunit">unit</span>${i === 0 ? '<span class="badge">primary</span>' : ""}</div>`).join("")
    + fixed.map((p) => `<div class="p"><span style="width:1em"></span><span class="mono faint">UID ${p.unit}</span><b>${esc(p.unit_name || "fixed party")}</b><span class="cvunit fixed">hospital</span></div>`).join("")
    || `<div class="faint small">No radios recorded.</div>`;
  $("cvdSummary").textContent = r.summary || (r.status === "skipped" ? "(skipped — no transcript arrived, so nothing was summarised)" : "(no summary)");
  $("cvdSummMeta").textContent = r.summary ? `${r.summary.length} chars` : "";
  $("cvdMessage").textContent = r.message || "(nothing was sent)";
  $("cvdSentMeta").textContent = r.status === "sent" ? `sent ${r.sent_at ? convWhen(r.sent_at) : ""}` : r.status;
  $("cvdDetail").textContent = r.detail || "";
  $("cvdTranscript").textContent = r.transcript || "(empty)";
  $("cvdPrompt").textContent = r.prompt || "(no prompt — no transcript, so the model was not asked)";
  $("cvdSegMeta").textContent = `${pieces.length} · ${convDur(pieces.reduce((a, p) => a + (p.secs || 0), 0))} of audio`;
  $("cvdSegs").innerHTML = pieces.map((p) => `<tr><td class="mono time">${esc(convClock(p.at))}<br><small class="faint">${(p.secs || 0).toFixed(1)} s</small></td><td class="who"><span class="cvunit ${p.fixed ? "fixed" : ""}">${p.fixed ? "hospital" : "unit"}</span><br><span class="mono faint">UID ${p.unit}</span> ${esc(p.fixed ? (p.unit_name || "") : convUnitName(p))}</td><td class="tr">${p.transcript ? esc(p.transcript) : '<span class="faint">[no transcript]</span>'}</td><td class="act">${p.id != null ? `<button class="btn ghost sm" data-cvplay="${p.id}" title="Play this transmission">▶</button>` : p.audio ? `<button class="btn ghost sm" data-cvwav="${esc(p.audio)}" title="Play this transmission">▶</button>` : ""}</td></tr>`).join("")
    || `<tr><td colspan="4" class="faint">No transmissions recorded.</td></tr>`;
  $("cvdSegs").querySelectorAll("[data-cvplay]").forEach((b) => b.onclick = () => invoke("library_play", { id: +b.dataset.cvplay }).catch((e) => uiToast(`${e}`, "err")));
  $("cvdSegs").querySelectorAll("[data-cvwav]").forEach((b) => b.onclick = () => invoke("play_wav", { path: b.dataset.cvwav }).catch((e) => uiToast(`${e}`, "err")));
}
$("cvSeg").querySelectorAll("button").forEach((b) => b.onclick = () => { if (b.dataset.v === "detail" && !convSel) return; convShowPage(b.dataset.v); });
$("cvdBack").onclick = () => convShowPage("list");
$("cvdListen").onclick = () => { if (convSel) convListen(convSel); };
$("cvdCopy").onclick = () => { if (convSel) convCopy(convSel); };
$("cvdDelete").onclick = async () => {
  if (!convSel) return;
  if (!(await uiConfirm(`Delete stored conversation ${convSel.conv_id}? The library calls it points at are kept.`, "Delete"))) return;
  try { await invoke("conversation_delete", { id: convSel.id }); convRows = convRows.filter((x) => x.id !== convSel.id); convSel = null; $("cvSeg").querySelector('[data-v="detail"]').disabled = true; convShowPage("list"); convRenderList(); convLoadStats(); } catch (e) { uiToast(`${e}`, "err"); }
};
$("cvRefresh").onclick = () => convLoad(false);
$("cvOlder").onclick = () => convLoad(true);
$("cvSearch").onkeydown = (e) => { if (e.key === "Enter") convLoad(false); };
$("cvTgFilter").onchange = () => { convShowPage("list"); convLoad(false); };
if (listen) listen("conversations", () => { if ($("view-conversations").style.display !== "none" && $("cvListPage").style.display !== "none") convLoad(false); });
window.conversationsOnShow = () => convLoad(false);

/* ---------- inline transcript editing (right-click a transcript) ---------- */
// Right-click the transcript of a stored call — in the Monitor history or
// the Library list — to edit it where it is. Enter saves (Shift+Enter for a
// new line), Esc cancels, clicking away saves. The edit is kept beside the
// machine transcript, never over it (library_set_edited); saving text equal
// to what was there leaves the call as it was.
function editTranscriptCell(td, id, current, onSaved) {
  if (td.classList.contains("editing")) return;
  td.classList.add("editing"); hideTip();
  const prev = td.innerHTML, prevTitle = td.getAttribute("title") || "";
  const ta = document.createElement("textarea"); ta.className = "tr-edit"; ta.value = current; ta.rows = Math.min(8, Math.max(2, Math.ceil(current.length / 60)));
  td.innerHTML = ""; td.appendChild(ta); ta.focus(); ta.setSelectionRange(ta.value.length, ta.value.length);
  let done = false;
  const finish = async (saveIt) => {
    if (done) return; done = true;
    const text = ta.value.trim();
    td.classList.remove("editing");
    if (!saveIt || text === current.trim()) { td.innerHTML = prev; td.setAttribute("title", prevTitle); return; }
    try {
      await invoke("library_set_edited", { id, text });
      td.textContent = text; td.setAttribute("title", text); td.classList.add("edited"); td.dataset.edited = "1";
      uiToast("Transcript saved");
      if (onSaved) onSaved(text);
    } catch (e) { td.innerHTML = prev; td.setAttribute("title", prevTitle); uiToast(`${e}`, "err"); }
  };
  ta.onkeydown = (e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); finish(true); } else if (e.key === "Escape") { e.preventDefault(); finish(false); } };
  ta.onblur = () => finish(true);
}
// A small menu at the pointer: [[label, fn], …].
function uiMenu(x, y, items) {
  document.querySelectorAll(".ctxmenu").forEach((m) => m.remove());
  const m = document.createElement("div"); m.className = "ctxmenu";
  m.innerHTML = items.map(([label], i) => `<button data-i="${i}">${esc(label)}</button>`).join("");
  document.body.appendChild(m);
  const r = m.getBoundingClientRect();
  m.style.left = `${Math.min(x, innerWidth - r.width - 6)}px`; m.style.top = `${Math.min(y, innerHeight - r.height - 6)}px`;
  const close = () => { m.remove(); document.removeEventListener("mousedown", away, true); document.removeEventListener("keydown", esc_, true); };
  const away = (e) => { if (!m.contains(e.target)) close(); };
  const esc_ = (e) => { if (e.key === "Escape") close(); };
  m.querySelectorAll("[data-i]").forEach((b) => b.onclick = () => { close(); items[+b.dataset.i][1](); });
  setTimeout(() => { document.addEventListener("mousedown", away, true); document.addEventListener("keydown", esc_, true); }, 0);
}
// Right-click a call (in the live list or the library): edit its
// transcript, or start a tripwire from it.
document.addEventListener("contextmenu", (e) => {
  if (!e.target.closest) return;
  const td = e.target.closest("td.tr");
  const tr = e.target.closest("tr");
  const libRow = tr && tr.closest("#libBody") ? tr : null;
  const cell = td || (tr && tr.querySelector("td.tr[data-trid]"));
  let id = null;
  if (td) id = td.dataset.trid !== undefined ? td.dataset.trid : (tr && tr.dataset.id);
  else if (libRow) id = libRow.dataset.id;
  else if (cell) id = cell.dataset.trid;
  if (!td && !id) return;
  e.preventDefault();
  if (!TAURI) return;
  if (!id) { uiToast("This call was not stored, so its transcript cannot be edited", "err"); return; }
  const items = [];
  if (td) items.push(["✎ Edit the transcript", () => {
    const current = (td.getAttribute("title") || "").trim();
    editTranscriptCell(td, +id, current, (text) => {
      if (typeof window.libRefreshRow === "function" && tr && tr.dataset.id) window.libRefreshRow(+id);
      if (Array.isArray(history)) { const h = history.find((x) => x.el === tr); if (h) { h.text += " " + text.toLowerCase(); } }
    });
  }]);
  items.push(["⚡ Tell me when something like this happens…", () => { if (typeof window.tripwireFromCall === "function") window.tripwireFromCall(+id); }]);
  uiMenu(e.clientX, e.clientY, items);
});

/* ================= onboarding / setup wizard ================= */
/* A short guided setup: radio → RadioReference → find system → start.
   Auto-opens on first run (unless skipped); re-open anytime via the "?" button. */
let obStep = 0, obWrap = null, obDevices = [], obCreds = false, obCatalog = 0, obPlaylists = 0, obZipMsg = "";

async function obGather() {
  obDevices = []; obCreds = false; obCatalog = 0; obPlaylists = 0;
  if (!invoke) return;
  try { obDevices = (await invoke("devices_list")).devices || []; } catch (_) {}
  try { const st = await invoke("rr_settings"); obCreds = !!(st && st.has_password); obCatalog = (st && st.catalog_len) || 0; } catch (_) {}
  try { const st = await invoke("sites_list"); obPlaylists = Array.isArray(st) ? st.length : 0; } catch (_) {}
}

const obTick = (ok) => `<span class="obchk${ok ? " ok" : ""}">${ok ? "✓" : "○"}</span>`;
const obBtn = (id, label, cls) => `<button class="btn ${cls || ""}" data-ob="${id}">${label}</button>`;

function obOpen() {
  obStep = 0; obZipMsg = "";
  if (!obWrap) {
    obWrap = document.createElement("div");
    obWrap.className = "modal-wrap";
    obWrap.innerHTML = `<div class="modal ob"><div class="obdots" id="obDots"></div><div class="obbody" id="obBody"></div><div class="xport obfoot" id="obFoot"></div></div>`;
    document.body.appendChild(obWrap);
    obWrap.addEventListener("keydown", (e) => { if (e.key === "Escape") obFinish(); });
    obWrap.onclick = (e) => { if (e.target === obWrap && obStep === 0) obFinish(); };
  }
  obGather().then(obRender);
}

function obClose() { if (obWrap) { obWrap.remove(); obWrap = null; } }
function obFinish() { save("hs.onboarded", true); obClose(); }
function obGo(n) { obStep = Math.max(0, Math.min(4, n)); obZipMsg = ""; obRender(); }

function obWire() {
  obWrap.querySelectorAll("[data-ob]").forEach((b) => {
    b.onclick = () => {
      const a = b.dataset.ob;
      if (a === "next") return obGo(obStep + 1);
      if (a === "back") return obGo(obStep - 1);
      if (a === "skip") return obFinish();
      if (a === "finish") return obFinish();
      if (a === "rescan") return obGather().then(obRender);
      if (a === "savecreds") return obSaveCreds();
      if (a === "ziplookup") return obZipLookup();
      if (a === "goplaylists") return obGoPlaylists();
      if (a === "gomonitor") { obFinish(); showView("monitor"); }
      if (a === "close") return obFinish();
    };
  });
}

async function obSaveCreds() {
  const u = obWrap.querySelector("#obUser").value.trim();
  const p = obWrap.querySelector("#obPass").value;
  if (!u) { uiToast("Enter your RadioReference username.", "err"); return; }
  if (!invoke) { uiToast("No backend — enter your account in the Playlists tab.", "err"); return; }
  try { await invoke("rr_save", { username: u, password: p, sid: null }); obCreds = true; uiToast("RadioReference account saved."); obRender(); }
  catch (e) { uiToast(`Could not save: ${e}`, "err"); }
}

async function obZipLookup() {
  const zip = parseInt(obWrap.querySelector("#obZip").value, 10);
  if (!Number.isFinite(zip)) { obZipMsg = ""; obRender(); return; }
  obZipMsg = "Looking up…"; obRender();
  if (!invoke) { obZipMsg = "No backend — find your system in the Playlists tab."; obRender(); return; }
  try { const z = await invoke("rr_zip", { zip }); obZipMsg = `Found systems near ${z.city || "ZIP " + zip} — open the browser to pick one.`; }
  catch (e) { obZipMsg = `ZIP lookup failed: ${e}`; }
  obRender();
}

function obGoPlaylists() {
  const zip = parseInt((obWrap.querySelector("#obZip") || {}).value, 10);
  obFinish();
  showView("settings"); setPage("playlists");
  if (Number.isFinite(zip) && $("bZip")) { $("bZip").value = String(zip); const g = $("bZipGo"); if (g && g.onclick) g.onclick(); }
}

function obDots() {
  return [0, 1, 2, 3, 4].map((i) => `<i class="${i === obStep ? "on" : i < obStep ? "done" : ""}"></i>`).join("");
}

function obBody() {
  const devs = obDevices.map((d) => `<div class="obdev"><span>${esc(d.label || d.id || "radio")}</span><span class="mono">${esc(d.kind || "")}</span></div>`).join("") || `<div class="obdev"><span style="color:var(--ink-faint)">No radio detected yet.</span></div>`;
  switch (obStep) {
    case 0:
      return `<h2>Welcome to HoosierSDR</h2>
        <p>Decode your city's P25 trunked radio — police, fire, EMS — with a software-defined radio. Four quick steps and you're listening.</p>
        <ul class="obchecks">
          <li>${obTick(obDevices.length > 0)} <span><b>Plug in a radio</b> — an Airspy R2 or RTL-SDR dongle.</span></li>
          <li>${obTick(obCreds)} <span><b>RadioReference account</b> — names the talkgroups.</span></li>
          <li>${obTick(obCatalog > 0)} <span><b>Find your system</b> — ${obCatalog ? `${obCatalog} talkgroups loaded` : "the trunked system near you"}.</span></li>
          <li>${obTick(obPlaylists > 0)} <span><b>Press Start</b> — ${obPlaylists ? `${obPlaylists} site${obPlaylists === 1 ? "" : "s"} saved` : "begin decoding"}.</span></li>
        </ul>`;
    case 1:
      return `<h2>1 · Plug in a radio</h2>
        <p>HoosierSDR reads an Airspy R2 or an RTL-SDR USB dongle. Plug one in and it appears below.</p>
        ${devs}
        <p style="font-size:12px">If nothing shows up, check the cable and press <b>Rescan</b>.</p>`;
    case 2:
      return `<h2>2 · Name your talkgroups</h2>
        <p>A RadioReference account turns raw talkgroup numbers into names ("Fire Dispatch"). Free accounts work; a Premium account lets us auto-download the full system. Your password stays on this Mac.</p>
        <label class="field" style="margin:0"><span class="lab">Username</span><input id="obUser" type="text" autocomplete="username" spellcheck="false" placeholder="yourname" /></label>
        <label class="field" style="margin:0"><span class="lab">Password</span><input id="obPass" type="password" autocomplete="current-password" placeholder="${obCreds ? "saved on this Mac" : ""}" /></label>
        <div class="obresult">${obCreds ? "✓ Account saved" : ""}</div>`;
    case 3:
      return `<h2>3 · Find your system</h2>
        <p>Pick the trunked system in your area. Easiest: enter your ZIP code, then browse the matching systems and choose a site.</p>
        <label class="field" style="margin:0"><span class="lab">ZIP code</span><span class="inline"><input id="obZip" type="text" inputmode="numeric" placeholder="46204" spellcheck="false" /><button class="btn ghost" data-ob="ziplookup">Look up</button></span></label>
        <div class="obresult">${esc(obZipMsg)}</div>
        <p style="font-size:12px">This opens the <b>Playlists</b> tab where you pick a site and save a playlist.</p>`;
    default:
      return `<h2>4 · Start listening</h2>
        <p>You're set. Go to the <b>Monitor</b> tab, pick your playlist in the top bar (if you saved one), and press <b>▶ Start</b> — the app tunes the control channel and begins decoding.</p>
        <ul class="obchecks">
          <li>${obTick(obDevices.length > 0)} Radio ready</li>
          <li>${obTick(obCreds)} Account saved</li>
          <li>${obTick(obCatalog > 0)} ${obCatalog ? `${obCatalog} talkgroups` : "Talkgroups loaded"}</li>
        </ul>`;
  }
}

function obFoot() {
  const back = obBtn("back", "← Back", "ghost");
  switch (obStep) {
    case 0: return `<div class="grp">${obBtn("skip", "Skip for now", "ghost")}</div><div class="grp">${obBtn("next", "Start setup →", "primary")}</div>`;
    case 1: return `<div class="grp">${back}</div><div class="grp">${obBtn("rescan", "Rescan", "ghost")}${obBtn("next", "Next →", "primary")}</div>`;
    case 2: return `<div class="grp">${back}</div><div class="grp">${obBtn("savecreds", "Save account", "ghost")}${obBtn("next", "Next →", "primary")}</div>`;
    case 3: return `<div class="grp">${back}</div><div class="grp">${obBtn("next", "Next →", "ghost")}${obBtn("goplaylists", "Open system browser →", "primary")}</div>`;
    default: return `<div class="grp">${back}</div><div class="grp">${obBtn("gomonitor", "Go to Monitor", "primary")}${obBtn("finish", "Done", "ghost")}</div>`;
  }
}

function obRender() {
  if (!obWrap) return;
  obWrap.querySelector("#obDots").innerHTML = obDots();
  obWrap.querySelector("#obBody").innerHTML = obBody();
  obWrap.querySelector("#obFoot").innerHTML = obFoot();
  obWire();
}

$("help").onclick = obOpen;
/* first run: open the guide once */
setTimeout(() => { if (!store("hs.onboarded", false)) obOpen(); }, 700);

/* ---------- dispatch bridge: the Tauri backend, or canned data when the page is opened standalone ---------- */
const dpDemoNow = Math.floor(Date.now() / 1000);
const dpDemoIncidents = [
  { id: 1, created: dpDemoNow - 240, updated: dpDemoNow - 60, tg: 10147, tg_name: "Fire/EMS Dispatch", call_type: "Cardiac Arrest", emoji: "🫀", address: "8241 East 41st Street", validated: "8241 East 41st Street, Testville, EX", lat: 39.8318, lon: -86.0223, geocode: "ok", units: ["Ladder 38", "Medic 21"], summary: "Cardiac arrest, CPR in progress on arrival", confidence: 95, calls: 3, revision: 2, pathway: "Cardiac arrest", targets: [{ label: "Closest hospital", place_id: "p1", place_name: "Example General", lat: 39.8100, lon: -86.0500, meters: 4800, secs: 480, how: "road" }, { label: "ECMO centre", place_id: "p2", place_name: "Example Heart", lat: 39.7800, lon: -86.1500, meters: 11000, secs: 900, how: "road" }] },
  { id: 2, created: dpDemoNow - 600, updated: dpDemoNow - 600, tg: 10147, tg_name: "Fire/EMS Dispatch", call_type: "Structure Fire", emoji: "🔥", address: "2000 South Fourth Street", validated: "2000 South Fourth Street, Testville, EX", lat: 39.7452, lon: -86.1579, geocode: "ok", units: ["Engine 23", "Engine 35", "Ladder 5", "Battalion 4"], summary: "Smoke showing from a two-storey residence", confidence: 92, calls: 1, revision: 0 },
  { id: 3, created: dpDemoNow - 900, updated: dpDemoNow - 900, tg: 10202, tg_name: "County EMS", call_type: "Vehicle Accident", emoji: "🚗", address: "38th and Sixth", validated: "East 38th Street & North Sixth Avenue, Testville, EX", lat: 39.8264, lon: -86.1178, geocode: "ok", units: ["Medic 63"], summary: "Two-vehicle crash, one patient complaining of neck pain", confidence: 88, calls: 1, revision: 0 },
  { id: 4, created: dpDemoNow - 1500, updated: dpDemoNow - 1500, tg: 10202, tg_name: "County EMS", call_type: "Stroke/CVA", emoji: "🧠", address: "9111 Avenue", validated: "", lat: null, lon: null, geocode: "none", units: ["Medic 42", "Engine 42"], summary: "Possible stroke, facial droop", confidence: 61, calls: 1, revision: 0 },
  { id: 5, created: dpDemoNow - 2400, updated: dpDemoNow - 2000, tg: 10147, tg_name: "Fire/EMS Dispatch", call_type: "Gas Odor", emoji: "⚠️", address: "4232 Cardinal Drive", validated: "4232 Cardinal Drive, Testville, EX", lat: 39.7043, lon: -86.2137, geocode: "ok", units: ["Engine 23", "Engine 46"], summary: "Odor of natural gas inside the residence", confidence: 90, calls: 2, revision: 1 },
  { id: 6, created: dpDemoNow - 3300, updated: dpDemoNow - 3300, tg: 10202, tg_name: "County EMS", call_type: "Sick Person", emoji: "🤒", address: "1124 North Elm Avenue", validated: "1124 North Elm Avenue, Testville, EX", lat: 39.7817, lon: -86.2418, geocode: "ok", units: ["Medic 85"], summary: "Sick person, weakness", confidence: 95, calls: 1, revision: 0, pathway: "Any other medical run", targets: [{ label: "Closest hospital", place_id: "p1", place_name: "Example General", lat: 39.7900, lon: -86.2200, meters: 3100, secs: 0, how: "straight" }] },
];
const dpDemo = async (cmd, args) => {
  switch (cmd) {
    case "dispatch_get": return { channels: [{ tg: 10147, name: "Fire/EMS Dispatch", role: "dispatch", fixed_call_type: "", enabled: true }, { tg: 10202, name: "County EMS", role: "dispatch", fixed_call_type: "", enabled: true }, { tg: 10150, name: "Fire Tac 1", role: "tactical", fixed_call_type: "", enabled: true }], call_types: [["Cardiac Arrest", "🫀"], ["Chest Pain", "❤️‍🩹"], ["Difficulty Breathing", "😮‍💨"], ["Stroke/CVA", "🧠"], ["Unconscious", "😵"], ["Sick Person", "🤒"], ["Injured Person", "🤕"], ["Overdose", "💊"], ["Mental-Emotional", "😰"], ["Vehicle Accident", "🚗"], ["Structure Fire", "🔥"], ["Fire Alarm", "🚨"], ["Gas Odor", "⚠️"], ["Residence Alarm", "🔔"], ["Water Rescue", "🌊"], ["Hazmat", "☣️"], ["Assault", "👊"], ["Unknown", "📍"]].map(([name, emoji]) => ({ name, emoji })), home_lat: 39.7684, home_lon: -86.1581, region_hint: "Testville, EX", search_radius_km: 40, geocoder_url: "https://nominatim.openstreetmap.org", geocoder_email: "", engine: "ollama", group_window_secs: 2700, group_radius_m: 150, retention_days: 14, extra_instructions: "", grid_fallback: true, calibration: { lat0: 39.77, lon0: -86.16, lat_per: 1.5e-5, lon_per: 1.7e-5, samples: 42, median_m: 480, at: dpDemoNow } };
    case "incidents_list": return dpDemoIncidents.filter((i) => !args || !args.since || i.updated >= args.since);
    case "incident_get": { const i = dpDemoIncidents.find((x) => x.id === (args && args.id)); return i ? { incident: i, reports: i.id === 1 ? [{ id: 9, at: i.created + 1500, tg: 10259, tg_name: "MED-06", tg_desc: "Example General ER", place: "Example General", summary: "Medic 21 inbound with a 68-year-old in cardiac arrest, ROSC achieved.", how: "Medic 21 was sent to this run" }] : [], calls: [{ call: 700 + i.id, at: i.created, tg: i.tg, role: "dispatch", summary: i.summary, extracted: "", tg_name: i.tg_name, unit_name: null, secs: 6.4, audio: null, transcript: `${i.units[0] || "Medic 1"}, ${i.address}, ${i.call_type.toLowerCase()}. ${i.units[0] || "Medic 1"}, ${i.address}, ${i.call_type.toLowerCase()}. 11:20 hours.` }] } : null; }
    case "dispatch_log": return dpDemoIncidents.map((i) => ({ at: i.updated, tg: i.tg, tg_name: i.tg_name, call: 700 + i.id, outcome: "new", detail: `#${i.id} ${i.call_type} · ${i.address}`, incident: i.id }));
    case "dispatch_set": case "incident_delete": return null;
    case "incident_locate": { const i = dpDemoIncidents.find((x) => x.id === args.id); if (i && args.lat != null) { i.lat = args.lat; i.lon = args.lon; i.geocode = "manual"; } return i; }
    case "dispatch_test": return "demo mode — no radio";
    case "dispatch_backfill": return 0;
    case "dispatch_regeocode": return [0, 0];
    // The shape of a drive, for the map. Two bends so a polyline has
    // something to show, and the straight-line case is exercised too.
    case "incident_route": {
      const i = dpDemoIncidents.find((x) => x.id === (args && args.id));
      if (!i || !(i.targets || []).length) return null;
      const want = String((args && args.which) || "").toLowerCase();
      const t = i.targets.find((x) => !want || x.label.toLowerCase() === want) || i.targets[0];
      const line = t.how === "road"
        ? [[i.lat, i.lon], [(i.lat + t.lat) / 2, i.lon], [(i.lat + t.lat) / 2, (i.lon + t.lon) / 2], [t.lat, t.lon]]
        : [[i.lat, i.lon], [t.lat, t.lon]];
      const say = t.how === "road" && t.secs > 0
        ? `${t.place_name} — ${(t.meters / 1609.344).toFixed(1)} mi, ${Math.round(t.secs / 60)} min by road`
        : `${t.place_name} — ${(t.meters / 1609.344).toFixed(1)} mi direct (straight line, no road route)`;
      return { label: t.label, place_name: t.place_name, to: [t.lat, t.lon], meters: t.meters, secs: t.secs, how: t.how, line, say };
    }
    case "dispatch_calibrate": return { lat0: 39.77, lon0: -86.16, lat_per: 1.5e-5, lon_per: 1.7e-5, samples: 42, median_m: 480, at: dpDemoNow };
    default: return null;
  }
};
const dpInvoke = TAURI ? invoke : dpDemo;
const dpListen = TAURI ? listen : async () => () => {};

/* ---------- dispatch: live incident map (Leaflet + OSM/CARTO tiles) ---------- */
// Everything drawn here came from the model or the geocoder: it is escaped
// before it meets innerHTML, and the emoji was checked in Rust to be one.
let dpMap = null, dpTiles = null, dpLayer = null, dpSettings = null, dpSel = null, dpChBuf = [];
// The drawn route to a run's facility, and which run/facility it is for, so
// clicking the same marker twice does not redraw and clicking another one
// clears the first.
let dpRouteLayer = null, dpRouteFor = null, dpRoutes = new Map();
let dpWinHours = store("hs.dp.win", 6), dpChanFilter = "", dpQuery = "";
const dpInc = new Map();       // id → incident
const dpMarkers = new Map();   // id → L.marker
const dpHidden = new Set(store("hs.dp.hidden", []));   // call types unticked in Layers
const dpNow = () => Math.floor(Date.now() / 1000);
const dpAgo = (t) => ago(t * 1000);
const dpShown = () => $("view-dispatch").style.display !== "none" && $("dpMain").style.display !== "none";
const dpIsDark = () => !isLightTheme(document.documentElement.getAttribute("data-theme"));
// OpenStreetMap tiles. In the app they come through the Rust `tiles://` scheme (fetched
// with the app's User-Agent and cached on disk); standalone they load straight from OSM.
// The dark theme is a CSS filter over them.
const dpTileUrl = () => `${tileBase()}/{z}/{x}/{y}.png`;
const dpHome = () => (dpSettings ? [dpSettings.home_lat, dpSettings.home_lon] : [39.7684, -86.1581]);

function dpInitMap() {
  if (dpMap || typeof L === "undefined") return;
  try {
    dpMap = L.map("dpMap", { zoomControl: false, attributionControl: false }).setView(dpHome(), 11);
    L.control.zoom({ position: "bottomright" }).addTo(dpMap);
    dpTiles = L.tileLayer(dpTileUrl(), { maxZoom: 19, className: "dptiles" }).addTo(dpMap);
    dpLayer = L.layerGroup().addTo(dpMap);
    dpRouteLayer = L.layerGroup().addTo(dpMap);
    dpMap.on("click", () => dpSelect(null));
    dpMap.on("popupopen", (e) => { const el = e.popup.getElement(); if (!el) return; el.querySelectorAll("[data-det]").forEach((b) => b.onclick = () => dpDetails(+b.dataset.det));
      el.querySelectorAll("[data-route]").forEach((b) => b.onclick = () => dpDrawRoute(+b.dataset.route, b.dataset.which)); });
    new MutationObserver(() => { $("dpMap").classList.toggle("light", !dpIsDark()); }).observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
    $("dpMap").classList.toggle("light", !dpIsDark());
    if (typeof ResizeObserver !== "undefined") {
      let raf = 0;
      new ResizeObserver(() => { if (raf) return; raf = requestAnimationFrame(() => { raf = 0; if (dpMap && dpShown()) dpMap.invalidateSize({ animate: false }); }); }).observe($("dpMap"));
    }
  } catch (e) { log(`map init: ${e}`); dpMap = null; }
}

function dpVisible(i) {
  if (dpWinHours && i.updated < dpNow() - dpWinHours * 3600) return false;
  if (dpChanFilter && String(i.tg) !== dpChanFilter) return false;
  if (dpHidden.has(i.call_type)) return false;
  if (dpQuery) { const hay = `${i.call_type} ${i.address} ${i.validated} ${(i.units || []).join(" ")} ${i.summary} ${i.tg_name}`.toLowerCase(); if (!hay.includes(dpQuery)) return false; }
  return true;
}
const dpUnits = (i, cls) => (i.units || []).map((u) => `<span class="unit ${cls || ""}">${esc(u)}</span>`).join("");
function dpCard(i) {
  const fresh = i.updated > dpNow() - 300, stale = i.updated < dpNow() - 3 * 3600;
  return `<div class="dpcard ${dpSel === i.id ? "on" : ""} ${fresh ? "fresh" : ""} ${stale ? "stale" : ""}" data-id="${i.id}">
    <div class="top"><span class="chan" title="${esc(i.tg_name)}">${esc(i.tg_name || "TG " + i.tg)}</span>${i.calls > 1 ? `<span class="linked">Linked · ${i.calls - 1} update${i.calls === 2 ? "" : "s"}</span>` : ""}<span class="conf ${i.confidence < 50 ? "low" : ""}" title="model confidence">${i.confidence}%</span><span class="ago" data-t="${i.updated}">${dpAgo(i.updated)}</span><button class="btn ghost sm" data-det="${i.id}" title="Details">⤢</button></div>
    <div class="title"><span class="em">${esc(i.emoji)}</span><b>${esc(i.call_type)}</b>${i.address ? `<span class="addr">· ${esc(i.address)}</span>` : `<span class="noaddr">· no address heard</span>`}${dpPlacedBadge(i)}</div>
    ${(i.units || []).length ? `<div class="units">${dpUnits(i)}</div>` : ""}
    ${i.summary ? `<div class="summ" title="${esc(i.summary)}">${esc(i.summary)}</div>` : ""}
  </div>`;
}
// How the pin got where it is. "grid" means the dispatcher's grid
// reference placed it — a few hundred metres, not a doorstep.
// When a mis-heard street was put right, keep what was actually said in
// view — the listener is the one who can tell whether the swap was fair.
// The run as a story: dispatched, worked, and — when a crew was heard
// reading a report to a hospital — where the patient went. The hospital
// half is joined by the crew's callsign, so the reason is shown with it.
function dpStory(i, calls, reports, t) {
  if (!(reports || []).length) return "";
  const first = calls && calls.length ? calls[0].at : i.created;
  const step = (at, what, who, body, how) => `
    <div class="dpstep">
      <span class="mono when">${t(at)}</span>
      <span class="mins">${at > first ? "+" + Math.round((at - first) / 60) + " min" : ""}</span>
      <div class="what"><b>${esc(what)}</b>${who ? ` <span class="faint">${esc(who)}</span>` : ""}
        ${body ? `<div class="body">${esc(body)}</div>` : ""}
        ${how ? `<div class="how">joined because ${esc(how)}</div>` : ""}</div>
    </div>`;
  const rows = [step(i.created, `${i.emoji} ${i.call_type}`, (i.units || []).join(", "), i.address, "")];
  for (const r of reports) {
    rows.push(step(r.at, `🏥 ${r.place || r.tg_desc || r.tg_name}`, "", r.summary, r.how));
  }
  return `<div class="k" style="margin-top:10px">What happened</div><div class="dpstory">${rows.join("")}</div>`;
}
function dpHeardAs(i, calls) {
  if (i.geocode !== "corrected") return "";
  for (const c of calls || []) {
    let heard = "";
    try { heard = (JSON.parse(c.extracted || "{}").address || "").trim(); } catch (_) {}
    if (heard && heard.toLowerCase() !== String(i.address).toLowerCase()) {
      return `<div class="faint sm">heard as “${esc(heard)}”</div>`;
    }
  }
  return "";
}
function dpPlacedBadge(i) {
  if (!i.address) return "";
  if (i.lat == null) return `<span class="nogeo" title="address not found on the map — open details to fix it">⚠ unmapped</span>`;
  if (i.geocode === "grid") return `<span class="approx" title="placed from the grid reference the dispatcher read out, not from the address">≈ approximate</span>`;
  if (i.geocode === "corrected") return `<span class="fixedup" title="the street name was mis-heard; this is the one that sounds like it and sits where the grid reference says">✎ name corrected</span>`;
  return "";
}
function dpIcon(i) {
  const fresh = i.updated > dpNow() - 300;
  const html = `<div class="dpmk ${fresh ? "fresh" : ""} ${dpSel === i.id ? "sel" : ""} ${i.geocode === "grid" ? "approx" : ""}"><span class="em">${esc(i.emoji)}</span>${i.calls > 1 ? `<span class="cnt">${i.calls}</span>` : ""}${$("dpShowLabels").checked ? `<span class="lbl">${esc(i.call_type)}</span>` : ""}</div>`;
  return L.divIcon({ html, className: "dpmkwrap", iconSize: [34, 34], iconAnchor: [17, 17], popupAnchor: [0, -20] });
}
// What the care pathway worked out for this run: where its patient would
// go, and how far that is. The distances were worked out when the run came
// in; the one being drawn is refreshed from the route itself.
const dpWhere = (i) => {
  const ts = i.targets || [];
  if (!ts.length) return "";
  const drawn = dpRoutes.get(i.id);
  const rows = ts.map((t) => {
    const live = drawn && drawn.label === t.label ? drawn : null;
    const said = live ? live.say : dpSay(t);
    const on = !!live;
    return `<div class="dpwrow${on ? " on" : ""}"><button class="btn ghost sm" data-route="${i.id}" data-which="${esc(t.label)}" title="Show the way there">${on ? "◉" : "○"}</button><span class="grow"><b>${esc(t.label)}</b><br>${esc(said)}</span></div>`;
  }).join("");
  return `<div class="dpwhere">${i.pathway ? `<div class="small faint">${esc(i.pathway)}</div>` : ""}${rows}</div>`;
};
// The same wording the backend uses, for the case where a run was stored
// before this page reloaded.
const dpSay = (t) => t.how === "road" && t.secs > 0
  ? `${esc(t.place_name)} — ${(t.meters / 1609.344).toFixed(1)} mi, ${Math.max(1, Math.round(t.secs / 60))} min by road`
  : `${esc(t.place_name)} — ${(t.meters / 1609.344).toFixed(1)} mi direct (straight line, no road route)`;
const dpPopup = (i) => `<div class="pt">${esc(i.emoji)} ${esc(i.call_type)}</div><div class="pa">${esc(i.address || "no address heard")}</div>${(i.units || []).length ? `<div class="pu">${dpUnits(i)}</div>` : ""}${i.summary ? `<div>${esc(i.summary)}</div>` : ""}${dpWhere(i)}<div class="pl"><small class="faint">${esc(i.tg_name)} · ${dpAgo(i.updated)} · ${i.calls} transmission${i.calls === 1 ? "" : "s"}</small> <button class="btn ghost sm" data-det="${i.id}">Details</button></div>`;
window.dpPopup = dpPopup;
window.dpInc = dpInc;
function dpSyncMarker(i) {
  if (!dpMap) return;
  const show = dpVisible(i) && i.lat != null && i.lon != null && $("dpShowPins").checked;
  let m = dpMarkers.get(i.id);
  if (!show) { if (m) { dpLayer.removeLayer(m); dpMarkers.delete(i.id); } return; }
  if (!m) {
    m = L.marker([i.lat, i.lon], { icon: dpIcon(i), riseOnHover: true });
    m.on("click", () => { dpSelect(i.id); dpDrawRoute(i.id, ""); });
    m.bindPopup(() => dpPopup(dpInc.get(i.id) || i), { maxWidth: 300 });
    dpLayer.addLayer(m); dpMarkers.set(i.id, m);
  } else { m.setLatLng([i.lat, i.lon]); m.setIcon(dpIcon(i)); }
  m.setZIndexOffset(Math.round((i.updated - 1.7e9) / 10));
}
function dpRenderTypes() {
  const counts = new Map();
  for (const i of dpInc.values()) { if (dpWinHours && i.updated < dpNow() - dpWinHours * 3600) continue; counts.set(i.call_type, (counts.get(i.call_type) || 0) + 1); }
  const types = dpSettings ? dpSettings.call_types.map((t) => t.name) : [];
  for (const k of counts.keys()) if (!types.includes(k)) types.push(k);
  const emojiOf = (n) => { const t = dpSettings && dpSettings.call_types.find((x) => x.name === n); if (t) return t.emoji; const i = [...dpInc.values()].find((x) => x.call_type === n); return i ? i.emoji : "📍"; };
  $("dpTypes").innerHTML = types.map((n) => `<label><input type="checkbox" data-type="${esc(n)}" ${dpHidden.has(n) ? "" : "checked"}><span>${esc(emojiOf(n))}</span><span>${esc(n)}</span><span class="n">${counts.get(n) || ""}</span></label>`).join("");
  $("dpTypes").querySelectorAll("input[data-type]").forEach((c) => c.onchange = () => { if (c.checked) dpHidden.delete(c.dataset.type); else dpHidden.add(c.dataset.type); save("hs.dp.hidden", [...dpHidden]); dpRender(); });
}
function dpRender() {
  const list = [...dpInc.values()].filter(dpVisible).sort((a, b) => b.updated - a.updated);
  $("dpList").innerHTML = list.map(dpCard).join("");
  $("dpEmpty").style.display = list.length ? "none" : "";
  $("dpMeta").textContent = list.length ? `${list.length} incident${list.length === 1 ? "" : "s"}${dpWinHours ? ` · last ${dpWinHours >= 24 ? dpWinHours / 24 + "d" : dpWinHours + "h"}` : ""}` : "";
  $("dpList").querySelectorAll(".dpcard").forEach((c) => { c.onclick = (e) => { if (e.target.closest("[data-det]")) return; dpSelect(+c.dataset.id, { fly: true }); }; c.ondblclick = () => dpDetails(+c.dataset.id); });
  $("dpList").querySelectorAll("[data-det]").forEach((b) => b.onclick = () => dpDetails(+b.dataset.det));
  for (const i of dpInc.values()) dpSyncMarker(i);
  $("dpPinCount").textContent = dpMarkers.size;
  $("dpMapEmpty").style.display = dpMarkers.size ? "none" : "";
  dpRenderTypes();
}
// The route from a run to the facility its care pathway asks for. Drawn on
// click, because that is the moment somebody wants to know how far it is —
// and the map zooms out to hold both ends, since the useful thing about a
// route is seeing the whole of it.
async function dpDrawRoute(id, which) {
  const i = dpInc.get(id);
  if (!i || i.lat == null || !(i.targets || []).length) { dpClearRoute(); return null; }
  const key = `${id}:${which || ""}`;
  if (dpRouteFor === key) return dpRoutes.get(id) || null;
  dpRouteFor = key;
  if (dpRouteLayer) dpRouteLayer.clearLayers();
  let leg = null;
  try { leg = await dpInvoke("incident_route", { id, which: which || "" }); }
  catch (e) { log(`incident_route: ${e}`); }
  // A click elsewhere while the router was thinking wins.
  if (dpRouteFor !== key) return null;
  if (!leg || !(leg.line || []).length) { dpRouteFor = null; return null; }
  dpRoutes.set(id, leg);
  // The numbers are in hand even where there is no map to draw them on.
  if (!dpMap) return leg;
  const line = leg.line.map(([lat, lon]) => [lat, lon]);
  // A straight line is drawn dashed and thin: it is not a road, and it must
  // not be mistaken for one.
  const road = leg.how === "road";
  L.polyline(line, {
    color: "#f5b544", weight: road ? 5 : 2, opacity: road ? .85 : .7,
    dashArray: road ? null : "6 7", lineJoin: "round", lineCap: "round",
  }).addTo(dpRouteLayer);
  if (road) L.polyline(line, { color: "#000", weight: 8, opacity: .25 }).addTo(dpRouteLayer).bringToBack();
  L.marker(leg.to, {
    icon: L.divIcon({ className: "dpmkwrap", html: `<div class="dpmk dest" title="${esc(leg.place_name)}">🏥<span class="lbl">${esc(leg.place_name)}</span></div>`, iconSize: [34, 34], iconAnchor: [17, 17], popupAnchor: [0, -20] }),
  }).bindPopup(`<div class="pt">🏥 ${esc(leg.place_name)}</div><div class="pa">${esc(leg.label)}</div><div>${esc(leg.say)}</div>`).addTo(dpRouteLayer);
  // Hold the whole route, with room for the popup above the marker.
  dpMap.fitBounds(L.latLngBounds(line).extend([i.lat, i.lon]), { paddingTopLeft: [40, 90], paddingBottomRight: [40, 40], maxZoom: 15 });
  // The popup reads from dpInc, so re-opening it now shows the numbers.
  const m = dpMarkers.get(id);
  if (m && m.isPopupOpen()) m.setPopupContent(dpPopup(i));
  return leg;
}
window.dpDrawRoute = dpDrawRoute;
function dpClearRoute() {
  dpRouteFor = null;
  if (dpRouteLayer) dpRouteLayer.clearLayers();
}

function dpSelect(id, o) {
  if (id !== dpSel) dpClearRoute();
  dpSel = id;
  $("dpList").querySelectorAll(".dpcard").forEach((c) => c.classList.toggle("on", +c.dataset.id === id));
  for (const [k, m] of dpMarkers) { const i = dpInc.get(k); if (i) m.setIcon(dpIcon(i)); }
  const i = id != null ? dpInc.get(id) : null;
  if (i && dpMap && i.lat != null && o && o.fly) {
    dpMap.flyTo([i.lat, i.lon], Math.max(dpMap.getZoom(), 14), { duration: .6 });
    const m = dpMarkers.get(id); if (m) setTimeout(() => m.openPopup(), 650);
    const card = $("dpList").querySelector(`.dpcard[data-id="${id}"]`); if (card && card.scrollIntoView) card.scrollIntoView({ block: "nearest" });
  } else if (i && !i.lat && o && o.fly) uiToast("This incident has no map position yet — open details to fix the address");
}
async function dpLoad() {
  try {
    const since = dpWinHours ? dpNow() - dpWinHours * 3600 : 0;
    const rows = await dpInvoke("incidents_list", { since, limit: 2000 });
    dpInc.clear(); for (const i of rows || []) dpInc.set(i.id, i);
    dpRender();
  } catch (e) { log(`incidents_list: ${e}`); }
}
async function dpSettingsLoad() {
  try { dpSettings = await dpInvoke("dispatch_get"); } catch (e) { log(`dispatch_get: ${e}`); return; }
  if (!dpSettings) return;
  const sel = $("dpChannel"); const cur = sel.value;
  sel.innerHTML = '<option value="">All channels</option>' + dpSettings.channels.map((c) => `<option value="${c.tg}">${esc(c.name || "TG " + c.tg)}${c.role === "tactical" ? " (tac)" : ""}</option>`).join("");
  sel.value = cur;
}
dpListen("incident", (e) => {
  const i = e.payload; if (!i || i.id == null) return;
  const had = dpInc.has(i.id); dpInc.set(i.id, i);
  logEvent(`DISPATCH ${i.emoji} ${i.call_type} · ${i.address || "no address"}${had ? ` (update #${i.revision})` : ""}`, had ? "" : "alarm");
  if (!dpShown()) return;
  dpRender();
  if (!had && $("dpFollowNew").checked && dpMap && i.lat != null && dpVisible(i)) dpMap.flyTo([i.lat, i.lon], Math.max(dpMap.getZoom(), 13), { duration: .8 });
});
dpListen("incident_deleted", (e) => { dpInc.delete(e.payload); if (dpShown()) dpRender(); });
setInterval(() => { if (!dpShown()) return; $("dpList").querySelectorAll(".ago[data-t]").forEach((s) => { s.textContent = dpAgo(+s.dataset.t); }); }, 30000);
setInterval(() => { if (dpShown()) dpRender(); }, 300000);   // freshness rings / stale fade

/* feed controls */
$("dpSearch").oninput = () => { dpQuery = $("dpSearch").value.trim().toLowerCase(); dpRender(); };
$("dpChannel").onchange = () => { dpChanFilter = $("dpChannel").value; dpRender(); };
setSeg($("dpWindow"), String(dpWinHours));
wireSeg($("dpWindow"), (v) => { dpWinHours = +v; save("hs.dp.win", dpWinHours); dpLoad(); });
$("dpShowPins").onchange = dpRender; $("dpShowLabels").onchange = dpRender;
$("dpTypesAll").onclick = () => { dpHidden.clear(); save("hs.dp.hidden", []); dpRender(); };
$("dpTypesNone").onclick = () => { for (const t of [...dpInc.values()].map((i) => i.call_type).concat(dpSettings ? dpSettings.call_types.map((t) => t.name) : [])) dpHidden.add(t); save("hs.dp.hidden", [...dpHidden]); dpRender(); };
if (store("hs.dp.layers", "open") === "closed") $("dpLayers").classList.add("closed");
$("dpLayersHead").onclick = () => { const c = $("dpLayers").classList.toggle("closed"); save("hs.dp.layers", c ? "closed" : "open"); };

/* details */
async function dpDetails(id) {
  let d; try { d = await dpInvoke("incident_get", { id }); } catch (e) { uiToast(`${e}`, "err"); return; }
  if (!d) { uiToast("That incident is gone"); dpInc.delete(id); dpRender(); return; }
  const i = d.incident;
  const t = (s) => new Date(s * 1000).toLocaleTimeString("en-US", { hour12: false });
  const calls = d.calls.map((c) => `<div class="call"><div class="ch"><span class="mono">${t(c.at)}</span><span>${esc(c.tg_name || "TG " + c.tg)}</span><span class="badge ${c.role === "tactical" ? "clear" : ""}">${esc(c.role)}</span>${c.unit_name ? `<span>${esc(c.unit_name)}</span>` : ""}<span class="mono">${(+c.secs || 0).toFixed(1)}s</span><span class="spacer"></span>${c.audio ? `<button class="btn ghost sm" data-play="${c.call}" title="Play">▶</button>` : ""}</div>${c.summary ? `<div><b>${esc(c.summary)}</b></div>` : ""}<div class="tr">${esc(c.transcript || "(no transcript)")}</div></div>`).join("");
  const m = uiModal(`<div class="dpdet">
    <div class="inline"><span class="eyebrow">Incident #${i.id}</span><span class="badge ${i.confidence >= 50 ? "clear" : "enc"}">${i.confidence}% confidence</span><span class="spacer"></span>${d.calls.some((c) => c.audio) ? `<button class="btn ghost sm" data-playlast>▶ Play latest</button>` : ""}<button class="btn ghost sm danger" data-del>Delete</button><button class="btn ghost sm" data-close>✕</button></div>
    <div class="grid">
      <div><div class="k">Call type</div><div class="v big">${esc(i.emoji)} ${esc(i.call_type)}</div></div>
      <div><div class="k">First heard · last update</div><div class="v">${t(i.created)} · ${dpAgo(i.updated)} · ${i.calls} transmission${i.calls === 1 ? "" : "s"}</div></div>
      <div><div class="k">Units</div><div class="v units" style="display:flex;flex-wrap:wrap;gap:4px">${dpUnits(i) || "—"}</div></div>
      <div><div class="k">Channel</div><div class="v">${esc(i.tg_name)} <span class="mono faint">TG ${i.tg}</span></div></div>
      <div><div class="k">Address · as heard</div><div class="v">${esc(i.address) || "<span class='faint'>none</span>"}${dpHeardAs(i, d.calls)}</div></div>
      <div><div class="k">Validated</div><div class="v">${i.geocode === "grid" ? "<span class='warn'>≈ " + esc(i.validated) + " — approximate, from the grid reference</span>" : i.geocode === "corrected" ? "✎ " + esc(i.validated) + " <span class='faint'>(street name corrected)</span>" : i.validated ? "✓ " + esc(i.validated) : i.geocode === "manual" ? "placed by hand" : i.geocode === "none" ? "<span class='warn'>not found near home — fix the address below</span>" : i.geocode === "error" ? "<span class='warn'>geocoder error — try again</span>" : "<span class='faint'>—</span>"}</div></div>
      <div><div class="k">Coordinates</div><div class="v mono">${i.lat != null ? `${(+i.lat).toFixed(6)}, ${(+i.lon).toFixed(6)}` : "—"}</div></div>
      <div><div class="k">Summary</div><div class="v">${esc(i.summary) || "—"}</div></div>
    </div>
    <div class="fix"><input data-fixaddr type="text" value="${esc(i.address)}" placeholder="Corrected street address or intersection" spellcheck="false"><button class="btn ghost sm" data-fixgo>Re-geocode</button><button class="btn ghost sm" data-fixmap title="Place the pin at the current map centre">Use map centre</button></div>
    ${dpStory(i, d.calls, d.reports, t)}
    <div class="k" style="margin-top:10px">Transmissions</div>
    <div class="calls">${calls || '<div class="faint">none</div>'}</div>
  </div>`, { wide: true });
  m.querySelector("[data-close]").onclick = m.close;
  m.querySelectorAll("[data-play]").forEach((b) => b.onclick = () => dpInvoke("library_play", { id: +b.dataset.play }).catch((e) => uiToast(`${e}`, "err")));
  const pl = m.querySelector("[data-playlast]"); if (pl) pl.onclick = () => { const c = [...d.calls].reverse().find((x) => x.audio); if (c) dpInvoke("library_play", { id: c.call }).catch((e) => uiToast(`${e}`, "err")); };
  m.querySelector("[data-del]").onclick = async () => { if (!(await uiConfirm(`Delete incident #${i.id} and its ${i.calls} attached transmission${i.calls === 1 ? "" : "s"}? The recordings stay in the library.`, "Delete"))) return; try { await dpInvoke("incident_delete", { id: i.id }); dpInc.delete(i.id); m.close(); dpRender(); } catch (e) { uiToast(`${e}`, "err"); } };
  const relocate = async (args) => { try { const u = await dpInvoke("incident_locate", { id: i.id, ...args }); dpInc.set(u.id, u); m.close(); dpRender(); if (u.lat != null) { dpSelect(u.id, { fly: true }); uiToast(`Placed at ${u.validated || u.address || "the chosen point"}`); } else uiToast("Still not found near home — try an intersection or add the city", "err"); } catch (e) { uiToast(`${e}`, "err"); } };
  m.querySelector("[data-fixgo]").onclick = () => relocate({ address: m.querySelector("[data-fixaddr]").value.trim() || null, lat: null, lon: null });
  m.querySelector("[data-fixmap]").onclick = () => { if (!dpMap) return; const c = dpMap.getCenter(); relocate({ address: m.querySelector("[data-fixaddr]").value.trim() || null, lat: c.lat, lon: c.lng }); };
}

/* setup */
function dpSetupShow(on) { $("dpSetup").style.display = on ? "" : "none"; $("dpMain").style.display = on ? "none" : ""; if (on) { dpSetupFill(); dpLogRefresh(); } else { dpSettingsLoad().then(() => { setTimeout(() => { if (dpMap) dpMap.invalidateSize(); }, 30); dpRender(); }); } }
$("dpSetupBtn").onclick = () => dpSetupShow(true);
$("dpBack").onclick = () => dpSetupShow(false);
function dpChRender() {
  $("dpChannels").innerHTML = dpChBuf.map((c, k) => `<div class="row" data-k="${k}"><span class="grow inline" style="gap:6px;flex-wrap:wrap"><input data-ctg type="text" inputmode="numeric" placeholder="TG" value="${c.tg || ""}" style="width:64px"><input data-cname type="text" placeholder="name" value="${esc(c.name)}" style="width:170px"><select data-crole style="width:auto"><option value="dispatch" ${c.role !== "tactical" ? "selected" : ""}>dispatch</option><option value="tactical" ${c.role === "tactical" ? "selected" : ""}>tactical</option></select><input data-cfixed type="text" placeholder="fixed call type (optional)" value="${esc(c.fixed_call_type)}" style="width:150px" list="dpTypeList"><label class="check" style="margin:0"><input data-cen type="checkbox" ${c.enabled ? "checked" : ""}> on</label></span><button class="btn ghost sm" data-cdel="${k}" title="remove">✕</button></div>`).join("");
  $("dpChannels").querySelectorAll("[data-cdel]").forEach((b) => b.onclick = () => { dpChSync(); dpChBuf.splice(+b.dataset.cdel, 1); dpChRender(); });
  $("dpChannels").querySelectorAll("[data-ctg]").forEach((inp) => inp.onchange = () => { const row = inp.closest(".row"); const name = row.querySelector("[data-cname]"); if (!name.value.trim()) { const r = (typeof alRows !== "undefined" ? alRows : []).find((x) => x.id === parseInt(inp.value, 10)); if (r) name.value = r.alias || ""; } });
  $("dpChMeta").textContent = dpChBuf.length ? `${dpChBuf.filter((c) => c.enabled).length} of ${dpChBuf.length} on` : "";
}
function dpChSync() {
  dpChBuf = [...$("dpChannels").querySelectorAll(".row")].map((r) => ({ tg: parseInt(r.querySelector("[data-ctg]").value, 10) || 0, name: r.querySelector("[data-cname]").value.trim(), role: r.querySelector("[data-crole]").value, fixed_call_type: r.querySelector("[data-cfixed]").value.trim(), enabled: r.querySelector("[data-cen]").checked }));
}
function dpSetupFill() {
  const s = dpSettings; if (!s) return;
  dpChBuf = s.channels.map((c) => ({ ...c })); dpChRender();
  $("dpHomeLat").value = s.home_lat; $("dpHomeLon").value = s.home_lon; $("dpRegion").value = s.region_hint; $("dpRadiusKm").value = s.search_radius_km;
  $("dpGeoUrl").value = s.geocoder_url; $("dpGeoEmail").value = s.geocoder_email; $("dpEngine").value = s.engine;
  $("dpWindowMin").value = Math.round(s.group_window_secs / 60); $("dpRadiusM").value = s.group_radius_m; $("dpRetention").value = s.retention_days;
  $("dpTypesText").value = s.call_types.map((t) => `${t.emoji} ${t.name}`).join("\n"); $("dpExtra").value = s.extra_instructions;
  let dl = $("dpTypeList"); if (!dl) { dl = document.createElement("datalist"); dl.id = "dpTypeList"; document.body.appendChild(dl); } dl.innerHTML = s.call_types.map((t) => `<option value="${esc(t.name)}">`).join("");
  $("dpGeoMeta").textContent = s.geocoder_url.includes("nominatim.openstreetmap.org") ? "public Nominatim · 1 req/s" : "custom server";
  $("dpGridFallback").checked = s.grid_fallback !== false;
  dpCalShow(s.calibration);
}
function dpSetupRead() {
  dpChSync();
  const types = $("dpTypesText").value.split("\n").map((l) => l.trim()).filter(Boolean).map((l) => { const m = l.match(/^(\S+)\s+(.+)$/); return m && !/^[A-Za-z0-9]/.test(m[1]) ? { emoji: m[1], name: m[2].trim() } : { emoji: "📍", name: l }; });
  return { ...dpSettings, channels: dpChBuf.filter((c) => c.tg > 0), call_types: types,
    home_lat: parseFloat($("dpHomeLat").value), home_lon: parseFloat($("dpHomeLon").value), region_hint: $("dpRegion").value.trim(), search_radius_km: parseFloat($("dpRadiusKm").value) || 40,
    geocoder_url: $("dpGeoUrl").value.trim(), geocoder_email: $("dpGeoEmail").value.trim(), engine: $("dpEngine").value,
    grid_fallback: $("dpGridFallback").checked, calibration: (dpSettings && dpSettings.calibration) || {},
    group_window_secs: (parseInt($("dpWindowMin").value, 10) || 45) * 60, group_radius_m: parseInt($("dpRadiusM").value, 10) || 150, retention_days: parseInt($("dpRetention").value, 10) || 14,
    extra_instructions: $("dpExtra").value };
}
async function dpSave() {
  if (!dpSettings) { uiToast("Dispatch settings did not load — reopen the tab", "err"); return false; }
  const s = dpSetupRead();
  if (!Number.isFinite(s.home_lat) || !Number.isFinite(s.home_lon)) { uiToast("Home latitude/longitude must be numbers", "err"); return false; }
  try { await dpInvoke("dispatch_set", { settings: s }); } catch (e) { uiToast(`Could not save: ${e}`, "err"); return false; }
  await dpSettingsLoad(); dpSetupFill(); return true;
}
$("dpSave").onclick = async () => { if (await dpSave()) { uiToast("Dispatch settings saved"); if (!$("trEnabled").checked) uiToast("The dispatch map needs transcription — enable it in Settings → Transcription", "err"); if (dpSettings.engine === "ollama" && !$("olModel").value) uiToast("Pick a local model in Settings → Connections", "err"); } };
$("dpHomeFromMap").onclick = () => { if (!dpMap) { uiToast("Open the map first"); return; } const c = dpMap.getCenter(); $("dpHomeLat").value = c.lat.toFixed(5); $("dpHomeLon").value = c.lng.toFixed(5); };
$("dpChPick").onclick = async () => {
  if (typeof pickChannels !== "function") return;
  dpChSync();
  const got = await pickChannels({ title: "Dispatch and tactical channels", selected: dpChBuf.map((c) => c.tg).filter(Boolean) });
  if (!got) return;
  const keep = dpChBuf.filter((c) => got.includes(c.tg));
  for (const tg of got) if (!keep.some((c) => c.tg === tg)) keep.push({ tg, name: channelSummary([tg], 1), role: "dispatch", fixed_call_type: "", enabled: true });
  dpChBuf = keep; dpChRender();
};
$("dpChAdd").onclick = () => { dpChSync(); dpChBuf.push({ tg: 0, name: "", role: "dispatch", fixed_call_type: "", enabled: true }); dpChRender(); const last = $("dpChannels").querySelector(".row:last-child [data-ctg]"); if (last) last.focus(); };
$("dpTest").onclick = async () => { if (!(await dpSave())) return; uiToast("Running the extractor on the latest dispatch call…"); try { await uiConfirm(await dpInvoke("dispatch_test", { tg: null }), "OK"); dpLogRefresh(); } catch (e) { uiToast(`Run failed: ${e}`, "err"); } };
// What the grid fit is worth, in the listener's own terms.
function dpCalShow(c) {
  const el = $("dpCalMeta"); if (!el) return;
  if (!c || !c.samples) { el.textContent = "not fitted yet — it learns from calls that do place"; return; }
  el.textContent = `fitted from ${c.samples} placed call${c.samples === 1 ? "" : "s"} · typically ${Math.round(c.median_m)} m out`;
}
$("dpCalibrate").onclick = async () => {
  const b = $("dpCalibrate"); b.disabled = true;
  try { const c = await dpInvoke("dispatch_calibrate"); if (dpSettings) dpSettings.calibration = c; dpCalShow(c); uiToast("Grid re-fitted"); }
  catch (e) { uiToast(`${e}`, "err"); }
  finally { b.disabled = false; }
};
$("dpRegeocode").onclick = async () => {
  const b = $("dpRegeocode"), st = $("dpRegeocodeState");
  b.disabled = true; st.textContent = "retrying — about a second per address…";
  try { const [tried, placed] = await dpInvoke("dispatch_regeocode"); st.textContent = tried ? `${placed} of ${tried} placed` : "nothing unmapped"; if (placed) dpLoad(); }
  catch (e) { st.textContent = ""; uiToast(`${e}`, "err"); }
  finally { b.disabled = false; }
};
$("dpBackfill").onclick = async () => { if (!(await dpSave())) return; const hours = Math.max(1, parseInt($("dpBackfillHours").value, 10) || 24); try { const n = await dpInvoke("dispatch_backfill", { hours }); $("dpBackfillState").textContent = n ? `queued ${n} calls…` : "nothing new to process"; uiToast(n ? `Backfilling ${n} calls — one geocode per second, so give it a minute` : "No unprocessed transcribed calls on those channels in that window"); } catch (e) { uiToast(`${e}`, "err"); } };
dpListen("dispatch_progress", (e) => { const p = e.payload || {}; $("dpBackfillState").textContent = p.finished ? `done · ${p.total} processed` : `${p.done} / ${p.total}…`; });
async function dpLogRefresh() {
  try {
    const rows = await dpInvoke("dispatch_log");
    $("dpLogMeta").textContent = rows && rows.length ? `${rows.length} recent` : "";
    const badge = (o) => o === "new" ? '<span class="badge clear">new</span>' : o === "update" ? '<span class="badge clear">update</span>' : o === "error" ? '<span class="badge enc">error</span>' : '<span class="badge">skip</span>';
    $("dpLog").innerHTML = (rows || []).map((l) => `<tr><td class="mono">${new Date(l.at * 1000).toLocaleTimeString("en-US", { hour12: false })}</td><td><small>${esc(l.tg_name)} <span class="mono">${l.tg}</span></small></td><td>${badge(l.outcome)} <small>${esc(l.detail)}</small>${l.incident != null ? ` <button class="btn ghost sm" data-det="${l.incident}">⤢</button>` : ""}</td></tr>`).join("");
    $("dpLog").querySelectorAll("[data-det]").forEach((b) => b.onclick = () => dpDetails(+b.dataset.det));
  } catch (e) { log(`dispatch_log: ${e}`); }
}
$("dpLogRefresh").onclick = dpLogRefresh;
dpListen("dispatch", () => { if ($("dpSetup").style.display !== "none") dpLogRefresh(); });

window.dispatchOnShow = async () => {
  if (!dpSettings) await dpSettingsLoad();
  dpInitMap();
  setTimeout(() => { if (dpMap) { dpMap.invalidateSize(); if (!dpInc.size) dpMap.setView(dpHome(), 11); } }, 30);
  dpLoad();
};

