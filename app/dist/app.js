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
const now = () => new Date().toLocaleTimeString("en-US", { hour12: false });
function wireSeg(el, onPick) {
  el.querySelectorAll("button").forEach((b) => {
    b.onclick = () => { setSeg(el, b.dataset.v); onPick(b.dataset.v); };
  });
}
function setSeg(el, v) { el.querySelectorAll("button").forEach((x) => x.setAttribute("aria-pressed", String(x.dataset.v === v))); }

/* ---------- views ---------- */
function showView(v) {
  ["monitor", "playlists", "settings"].forEach((n) => { $("view-" + n).style.display = n === v ? "" : "none"; });
  setSeg($("navSeg"), v);
}
$("navSeg").querySelectorAll("button").forEach((b) => b.onclick = () => showView(b.dataset.v));
if (["#playlists", "#settings"].includes(location.hash)) showView(location.hash.slice(1));

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
  $("emptyHint").textContent = follow ? "Pick a playlist or set a control channel, then press Start." : "Set a channel and press Start.";
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
function activeStart(ev) {
  const key = activeKey(ev.tg, ev.freq_mhz);
  if (activeCalls.has(key)) return;
  const el = document.createElement("div");
  el.className = "call";
  el.innerHTML = `<span class="tg">${ev.name}</span><span class="t">0:00</span><span class="sub">TG ${ev.tg} · ${ev.freq_mhz.toFixed(4)} MHz</span>`;
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
    `<td class="tg">${g.name}<span class="num">TG ${g.tg}</span></td>` +
    `<td class="src">${g.source ? g.source : "—"}</td>` +
    `<td class="dl">${g.freq_mhz.toFixed(4)}</td>` +
    `<td class="len">${len}</td>` +
    `<td>${g.encrypted ? '<span class="badge enc">Encrypted</span>' : `<span class="badge clear">${g.modulation || "clear"}</span>`}</td>` +
    `<td class="act">` +
      (g.wav ? `<button title="Replay" data-wav="${g.wav}">▶</button>` : "") +
      `<button title="Lock out TG ${g.tg}" data-lock="${g.tg}" class="${lockout.has(g.tg) ? "on" : ""}">⊘</button>` +
    `</td>`;
  tr.querySelectorAll("button[data-wav]").forEach((b) => b.onclick = () => replay(b.dataset.wav));
  tr.querySelectorAll("button[data-lock]").forEach((b) => b.onclick = () => toggleLock(+b.dataset.lock));
  const text = `${g.name} ${g.tg} ${g.source || ""} ${g.freq_mhz.toFixed(4)}`.toLowerCase();
  history.unshift({ el: tr, text });
  tbody.prepend(tr);
  while (history.length > 500) history.pop().el.remove();
  applyHistFilter();
}
function applyHistFilter() {
  const q = $("histFilter").value.trim().toLowerCase();
  let shown = 0;
  history.forEach((h) => { const on = !q || h.text.includes(q); h.el.style.display = on ? "" : "none"; if (on) shown++; });
  $("histMeta").textContent = history.length ? (q ? `${shown} of ${history.length}` : `${history.length} calls`) : "";
}
$("histFilter").oninput = applyHistFilter;
$("clear").onclick = () => { tbody.innerHTML = ""; history.length = 0; $("empty").style.display = ""; applyHistFilter(); };

/* ---------- lockout ---------- */
const lockout = new Set(JSON.parse(localStorage.getItem("hs.lockout") || "[]"));
function renderLockout() {
  $("lockbar").style.display = lockout.size ? "" : "none";
  $("lockchips").innerHTML = [...lockout].sort((a, b) => a - b).map((tg) => `<span class="chip" data-tg="${tg}" title="Unlock">TG ${tg} ✕</span>`).join(" ");
  $("lockchips").querySelectorAll(".chip").forEach((c) => c.onclick = () => toggleLock(+c.dataset.tg));
  tbody.querySelectorAll("button[data-lock]").forEach((b) => b.classList.toggle("on", lockout.has(+b.dataset.lock)));
}
function toggleLock(tg) {
  if (lockout.has(tg)) lockout.delete(tg); else lockout.add(tg);
  localStorage.setItem("hs.lockout", JSON.stringify([...lockout]));
  renderLockout();
  if (TAURI) invoke("set_lockout", { tgs: [...lockout] }).catch((e) => alert(e));
}
function replay(path) { if (TAURI) invoke("play_wav", { path }).catch((e) => alert(e)); }
renderLockout();

/* ---------- waterfall ---------- */
const wf = $("waterfall"), wctx = wf.getContext("2d");
wctx.fillStyle = "#05090a"; wctx.fillRect(0, 0, wf.width, wf.height);
function phosphor(t) {
  const stops = [[6,14,16],[14,58,74],[26,140,150],[74,214,180],[150,240,120],[245,200,90],[255,246,225]];
  t = Math.max(0, Math.min(1, t));
  const p = t * (stops.length - 1), i = Math.min(stops.length - 2, Math.floor(p)), f = p - i;
  const a = stops[i], b = stops[i + 1];
  return [a[0]+(b[0]-a[0])*f, a[1]+(b[1]-a[1])*f, a[2]+(b[2]-a[2])*f];
}
function pushSpectrum(db) {
  const w = wf.width, h = wf.height;
  wctx.drawImage(wf, 0, 0, w, h - 1, 0, 1, w, h - 1);
  const n = db.length, row = wctx.createImageData(w, 1);
  for (let x = 0; x < w; x++) {
    const v = db[Math.floor((x / w) * n)];
    const [r, g, b] = phosphor((v + 92) / 74);
    const i = x * 4; row.data[i] = r; row.data[i+1] = g; row.data[i+2] = b; row.data[i+3] = 255;
  }
  wctx.putImageData(row, 0, 0);
}

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
      $("tunedHz").textContent = ev.control_mhz.toFixed(4);
      $("tunedSub").textContent = `${ev.modulation} · tuner ${ev.correction_hz >= 0 ? "+" : ""}${ev.correction_hz.toFixed(0)} Hz`;
      $("wfAxis").textContent = `${(parseFreq($("center").value) / 1e6).toFixed(2)} MHz ± ${(ev.rate / 2e6).toFixed(2)} MHz`;
      $("followMeta").textContent = "";
      activeRefresh();
      break;
    case "call_start": activeStart(ev); break;
    case "call":
      activeEnd(ev);
      followVoice += ev.secs;
      addCall({ tg: ev.tg, name: ev.name, source: ev.source, freq_mhz: ev.freq_mhz, encrypted: false, secs: ev.secs, modulation: ev.modulation, wav: ev.wav });
      $("r-voice").innerHTML = followVoice.toFixed(1) + "<small>s</small>";
      break;
    case "notice": $("followMeta").textContent = ev.text; break;
    case "status":
      $("r-syncs").textContent = ev.control_syncs;
      $("r-grants").textContent = ev.calls;
      $("r-syncerr").textContent = ev.dropped ? `${ev.dropped}` : "0";
      $("r-stream").textContent = `${ev.msps.toFixed(2)}/${ev.want_msps.toFixed(2)}M`;
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
  listen("stopped", () => setState("standby"));
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
      setState(modeSel === "follow" ? "measuring" : "capturing");
      log(`start: mode=${modeSel} source=${$("source").value} rate=${$("rate").value} freq=${$("freq").value} center=${$("center").value}`);
      followVoice = 0;
      if (modeSel === "follow") {
        const o = opts();
        $("followMeta").textContent = "measuring the control channel…";
        $("tunedHz").textContent = mhz(o.freq);
        await invoke("start_follow", { source: o.source, freq: parseFreq($("center").value), rate: o.rate, gain: o.gain,
          control: o.freq, callsDir: $("callsdir").value.trim() || null, play: $("play").checked });
      } else {
        $("tunedHz").textContent = mhz(opts().freq);
        $("wfAxis").textContent = `${(opts().freq / 1e6).toFixed(4)} MHz ± ${(opts().rate / 2e6).toFixed(2)} MHz`;
        await invoke("start_capture", { ...opts(), recordIq: $("reciq").value.trim() || null, recordLog: $("reclog").value.trim() || null });
      }
    } catch (err) { setState("standby"); alert(err); }
  };
  $("stop").onclick = () => invoke("stop_capture");
  $("loadcat").onclick = async () => {
    const path = $("catalog").value.trim(); if (!path) return;
    try { const n = await invoke("load_catalog", { path }); $("loadcat").textContent = n + " TGs"; } catch (err) { alert(err); }
  };
  $("decode").onclick = async () => {
    const path = $("decfile").value.trim(); if (!path) return;
    try { setState("decoding"); await invoke("decode_file", { path, rate: parseFloat($("rate").value), cqpsk: modSel === "cqpsk", eq: eqSel }); }
    catch (err) { alert(err); } finally { setState("standby"); }
  };
  invoke("set_lockout", { tgs: [...lockout] }).catch(() => {});

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
    $("bState").innerHTML = '<option value="">—</option>' + st.map((s) => `<option value="${s.stid}">${s.name}</option>`).join("");
    statesLoaded = true;
  }
  function renderSystems(list, label) {
    $("findMeta").textContent = label || "";
    $("sysList").innerHTML = list.length ? list.map((s) =>
      `<div class="row" data-sid="${s.sid}"><span class="grow">${s.name}${s.city ? ` <small>· ${s.city}</small>` : ""}</span><span class="mono">sid ${s.sid}</span></div>`).join("")
      : '<div class="row"><span class="grow" style="color:var(--ink-faint)">No trunked systems listed here.</span></div>';
    $("sysList").querySelectorAll(".row[data-sid]").forEach((r) => r.onclick = () => { $("rrSid").value = r.dataset.sid; loadSystem(+r.dataset.sid); });
  }
  $("bState").onchange = async () => {
    const stid = +$("bState").value; $("bCounty").innerHTML = '<option value="">— statewide —</option>'; if (!stid) return;
    try { $("findMeta").textContent = "loading…"; const v = await invoke("rr_state", { stid });
      $("bCounty").innerHTML += v.counties.map((c) => `<option value="${c.ctid}">${c.name}</option>`).join("");
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
      rrRefresh();
      $("sysPanel").scrollIntoView({ behavior: "smooth", block: "start" });
    } catch (e) { $("findMeta").textContent = ""; alert(e); }
    finally { $("rrDownload").disabled = false; }
  }
  $("rrDownload").onclick = () => { const sid = sidVal(); if (sid == null) { alert("Enter a system ID."); return; } loadSystem(sid); };
  const siteRate = (s) => ((s.span_mhz ? s.span_mhz[1] - s.span_mhz[0] : 0) <= 1.9 ? 2500000 : 10000000);
  function renderSites() {
    $("siteList").innerHTML = sys.sites.map((s) =>
      `<div class="row ${pickedSite && s.site_id === pickedSite.site_id ? "on" : ""}" data-site="${s.site_id}">` +
      `<span class="grow"><b>${s.site_id}</b> ${s.name}${s.tdma_control ? ' <small style="color:var(--enc)">TDMA CC — not decodable yet</small>' : ""}</span>` +
      (s.nac != null ? `<span class="mono">NAC 0x${s.nac.toString(16).toUpperCase().padStart(3, "0")}</span>` : "") +
      `<span class="mono">${s.control_mhz[0].toFixed(4)} MHz</span>` +
      (s.span_mhz ? `<span class="mono">${(s.span_mhz[1] - s.span_mhz[0]).toFixed(2)} MHz span</span>` : "") + `</div>`).join("");
    $("siteList").querySelectorAll(".row").forEach((r) => r.onclick = () => { pickedSite = sys.sites.find((s) => s.site_id === +r.dataset.site); renderSites(); });
  }
  function renderCats() {
    const cats = [...new Set(sys.tgs.map((t) => t.category).filter(Boolean))].sort();
    $("tgCat").innerHTML = '<option value="">all</option>' + cats.map((c) => `<option>${c}</option>`).join("");
  }
  function shownTgs() {
    const q = $("tgFilter").value.trim().toLowerCase(), cat = $("tgCat").value;
    return sys.tgs.filter((t) => (!cat || t.category === cat) && (!q || `${t.id} ${t.alias} ${t.description} ${t.category}`.toLowerCase().includes(q)));
  }
  function renderTgs() {
    $("tgBody").innerHTML = shownTgs().map((t) =>
      `<tr class="${t.encrypted ? "enc" : ""}"><td><input type="checkbox" data-tg="${t.id}" ${picked.has(t.id) ? "checked" : ""} ${t.encrypted ? "disabled" : ""}></td>` +
      `<td class="mono">${t.id}</td><td>${t.alias}</td><td>${t.description}</td><td><small>${t.category}</small></td>` +
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
  function renderPlaylists(list) {
    $("plEmpty").style.display = list.length ? "none" : "";
    $("plList").innerHTML = list.map((p) =>
      `<div class="row" data-id="${p.id}"><span class="grow"><b>${p.name}</b><br><small>${p.system_name} · site ${p.site_id} ${p.site_name} · ${p.control_mhz.toFixed(4)} MHz · ${p.tgs.length ? p.tgs.length + " TGs" : "all TGs"}</small></span>` +
      `<button class="btn primary" data-act="${p.id}">Use</button><button class="btn ghost" data-del="${p.id}">Delete</button></div>`).join("");
    $("plList").querySelectorAll("[data-act]").forEach((b) => b.onclick = () => activatePlaylist(b.dataset.act));
    $("plList").querySelectorAll("[data-del]").forEach((b) => b.onclick = async () => {
      if (!confirm("Delete this playlist?")) return;
      try { renderPlaylists(await invoke("playlist_delete", { id: b.dataset.del })); } catch (e) { alert(e); }
    });
    const cur = $("playlist").value;
    $("playlist").innerHTML = '<option value="">— every talkgroup —</option>' + list.map((p) => `<option value="${p.id}">${p.name}</option>`).join("");
    $("playlist").value = list.some((p) => p.id === cur) ? cur : "";
  }
  async function activatePlaylist(id) {
    try {
      const p = await invoke("playlist_activate", { id: id || null });
      $("playlist").value = p ? p.id : "";
      if (p) {
        modeSel = "follow"; setSeg($("modeSeg"), "follow"); applyMode();
        $("freq").value = p.control_mhz.toFixed(4) + "M"; $("center").value = p.center_mhz.toFixed(4) + "M";
        $("rate").value = String(p.rate); syncRate();
        $("followMeta").textContent = `playlist: ${p.name} · ${p.tgs.length ? p.tgs.length + " talkgroups" : "all talkgroups"}`;
        showView("monitor");
      } else { $("followMeta").textContent = ""; }
    } catch (e) { alert(e); }
  }
  $("playlist").onchange = () => activatePlaylist($("playlist").value);
  invoke("playlists_list").then(renderPlaylists).catch((e) => log(`playlists: ${e}`));
  rrRefresh();

  // Dev hook: open with #autostart=airspy to press Start for a 10 MSPS site follow.
  if (location.hash.startsWith("#autostart")) {
    $("source").value = location.hash.split("=")[1] || "airspy"; $("source").onchange();
    setTimeout(() => $("start").click(), 800);
    setTimeout(() => $("stop").click(), 40000);
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
