// HoosierSDR front end. Drives the real Tauri backend when present
// (start_capture / grant / status / spectrum), and falls back to a realistic
// demo driver when opened without a backend — so the same file previews the UI.
const $ = (id) => document.getElementById(id);
const TAURI = window.__TAURI__;
const invoke = TAURI ? TAURI.core.invoke : null;
const listen = TAURI ? TAURI.event.listen : null;

/* ---------- diagnostics: every JS error and key event goes to the launching terminal ---------- */
function log(m) {
  try { console.log(m); } catch (_) {}
  if (TAURI) invoke("ui_log", { msg: String(m) }).catch(() => {});
}
window.onerror = (m, src, line, col) => log(`JS error: ${m} @ ${line}:${col}`);
window.onunhandledrejection = (e) => log(`unhandled rejection: ${e.reason}`);
log(`page loaded; tauri=${!!TAURI}`);

/* ---------- theme toggle ---------- */
(() => {
  const root = document.documentElement;
  $("theme").onclick = () => {
    const cur = root.getAttribute("data-theme")
      || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
    root.setAttribute("data-theme", cur === "dark" ? "light" : "dark");
  };
})();

/* ---------- freq parsing / display ---------- */
function parseFreq(s) {
  s = String(s).trim();
  if (/[Mm]$/.test(s)) return parseFloat(s) * 1e6;
  if (/[kK]$/.test(s)) return parseFloat(s) * 1e3;
  return parseFloat(s);
}
function syncTuned() {
  const hz = parseFreq($("freq").value);
  if (isFinite(hz)) $("tunedHz").textContent = (hz / 1e6).toFixed(4);
  $("tunedSub").textContent = `NAC 0x261 · SITE 010 · ${modSel.toUpperCase()}`;
  const r = parseFloat($("rate").value);
  $("rateMeta").textContent = r >= 1e6 ? (r / 1e6).toFixed(1) + " MSPS" : (r / 1e3) + " kSPS";
}
$("freq").addEventListener("input", syncTuned);
$("rate").addEventListener("change", syncTuned);

/* ---------- segmented controls ---------- */
let modSel = "cqpsk", eqSel = "cma";
function wireSeg(el, onPick) {
  el.querySelectorAll("button").forEach((b) => {
    b.onclick = () => {
      el.querySelectorAll("button").forEach((x) => x.setAttribute("aria-pressed", "false"));
      b.setAttribute("aria-pressed", "true");
      onPick(b.dataset.v);
    };
  });
}
wireSeg($("modSeg"), (v) => { modSel = v; syncTuned(); });

/* ---------- views: Receiver / Config ---------- */
function showView(v) {
  $("view-receiver").style.display = v === "receiver" ? "" : "none";
  $("view-config").style.display = v === "config" ? "" : "none";
  $("navSeg").querySelectorAll("button").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.v === v)));
}
$("navSeg").querySelectorAll("button").forEach((b) => b.onclick = () => showView(b.dataset.v));

/* ---------- mode: follow a site vs. decode one channel ---------- */
let modeSel = "follow";
function applyMode() {
  const follow = modeSel === "follow";
  $("centerField").style.display = follow ? "" : "none";
  $("followOpts").style.display = follow ? "" : "none";
  $("channelOpts").style.display = follow ? "none" : "";
  // The follower picks modulation by measurement and uses the site's
  // equalizer; those controls only apply to a single named channel.
  $("modField").style.opacity = follow ? ".45" : "";
  $("eqField").style.opacity = follow ? ".45" : "";
  $("freqHint").textContent = follow ? "control ch" : "channel";
  $("empty").lastChild.textContent = follow
    ? "Press Start to follow the control channel, or decode a recording."
    : "Press Start to decode the channel, or decode a recording.";
}
wireSeg($("modeSeg"), (v) => { modeSel = v; applyMode(); });
applyMode();
wireSeg($("eqSeg"), (v) => { eqSel = v; $("r-eq").textContent = v === "bypass" ? "BARE" : v.toUpperCase(); });

/* ---------- state pill ---------- */
function setState(s) {
  const p = $("pill"), t = $("pillText");
  p.className = "pill" + (s === "capturing" ? " live" : s === "locked" ? " locked" : "");
  t.textContent = s;
  $("start").disabled = (s !== "standby");
  $("stop").disabled = (s === "standby");
}

/* ---------- calls table ---------- */
const tbody = $("callBody");
function addCall(g) {
  $("empty").style.display = "none";
  const tr = document.createElement("tr");
  const t = new Date().toLocaleTimeString("en-US", { hour12: false });
  const badge = g.encrypted
    ? '<span class="badge enc">Encrypted</span>'
    : g.secs != null
      ? `<span class="badge clear">${g.secs.toFixed(1)}s · ${g.modulation || "?"}</span>`
      : g.live
        ? '<span class="badge clear">On air</span>'
        : '<span class="badge clear">Clear</span>';
  tr.innerHTML =
    `<td class="time">${t}</td>` +
    `<td class="tg">${g.name}<span class="num">TG ${g.tg}</span></td>` +
    `<td class="src">${g.source ? g.source : "—"}</td>` +
    `<td class="dl">${g.freq_mhz.toFixed(4)}<span style="color:var(--ink-faint);font-size:11px"> MHz</span></td>` +
    `<td>${badge}</td>` +
    `<td class="act">` +
      (g.wav ? `<button title="Replay" data-wav="${g.wav}">▶</button>` : "") +
      `<button title="Lock out TG ${g.tg}" data-lock="${g.tg}" class="${lockout.has(g.tg) ? "on" : ""}">⊘</button>` +
    `</td>`;
  tr.querySelectorAll("button[data-wav]").forEach((b) => b.onclick = () => replay(b.dataset.wav));
  tr.querySelectorAll("button[data-lock]").forEach((b) => b.onclick = () => toggleLock(+b.dataset.lock));
  tbody.prepend(tr);
  while (tbody.children.length > 200) tbody.removeChild(tbody.lastChild);
}
$("clear").onclick = () => { tbody.innerHTML = ""; $("empty").style.display = ""; };

/* ---------- lockout: remembered across runs, pushed to the follower live ---------- */
const lockout = new Set(JSON.parse(localStorage.getItem("hs.lockout") || "[]"));
function renderLockout() {
  const bar = $("lockbar"), chips = $("lockchips");
  bar.style.display = lockout.size ? "" : "none";
  chips.innerHTML = [...lockout].sort((a, b) => a - b)
    .map((tg) => `<span class="chip" data-tg="${tg}" title="Unlock">TG ${tg} ✕</span>`).join(" ");
  chips.querySelectorAll(".chip").forEach((c) => c.onclick = () => toggleLock(+c.dataset.tg));
  tbody.querySelectorAll("button[data-lock]").forEach((b) => b.classList.toggle("on", lockout.has(+b.dataset.lock)));
}
function toggleLock(tg) {
  if (lockout.has(tg)) lockout.delete(tg); else lockout.add(tg);
  localStorage.setItem("hs.lockout", JSON.stringify([...lockout]));
  renderLockout();
  if (TAURI) invoke("set_lockout", { tgs: [...lockout] }).catch((e) => alert(e));
}
function replay(path) {
  if (TAURI) invoke("play_wav", { path }).catch((e) => alert(e));
}
renderLockout();
if (TAURI) invoke("set_lockout", { tgs: [...lockout] }).catch(() => {});

/* ---------- readouts ---------- */
function setStatus(s) {
  if (s.syncs != null) $("r-syncs").textContent = s.syncs;
  if (s.grants != null) $("r-grants").textContent = s.grants;
  if (s.voice_secs != null) $("r-voice").innerHTML = s.voice_secs.toFixed(1) + "<small>s</small>";
  if (s.modulation) $("tunedSub").textContent = `NAC 0x261 · SITE 010 · ${s.modulation.toUpperCase()}`;
  if (s.lock != null) {
    if (s.lock >= 0) {                              // -1 = C4FM: no lock metric
      $("r-lock").textContent = s.lock.toFixed(2);
      $("r-lockbar").style.width = Math.max(0, Math.min(100, s.lock * 100)) + "%";
    } else {
      $("r-lock").textContent = "—";
      $("r-lockbar").style.width = "0%";
    }
  }
  if (s.sync_err != null) $("r-syncerr").textContent = s.sync_err.toFixed(2);
}

/* ---------- waterfall ---------- */
const wf = $("waterfall"), wctx = wf.getContext("2d");
wctx.fillStyle = "#05090a"; wctx.fillRect(0, 0, wf.width, wf.height);
function phosphor(t) {
  // dark teal → cyan → green → amber → hot white
  const stops = [[6,14,16],[14,58,74],[26,140,150],[74,214,180],[150,240,120],[245,200,90],[255,246,225]];
  t = Math.max(0, Math.min(1, t));
  const p = t * (stops.length - 1), i = Math.min(stops.length - 2, Math.floor(p)), f = p - i;
  const a = stops[i], b = stops[i + 1];
  return [a[0]+(b[0]-a[0])*f, a[1]+(b[1]-a[1])*f, a[2]+(b[2]-a[2])*f];
}
function pushSpectrum(db) {
  const w = wf.width, h = wf.height;
  wctx.drawImage(wf, 0, 0, w, h - 1, 0, 1, w, h - 1); // scroll down 1px
  const n = db.length, row = wctx.createImageData(w, 1);
  for (let x = 0; x < w; x++) {
    const v = db[Math.floor((x / w) * n)];
    const [r, g, b] = phosphor((v + 92) / 74);      // -92..-18 dB
    const i = x * 4;
    row.data[i] = r; row.data[i+1] = g; row.data[i+2] = b; row.data[i+3] = 255;
  }
  wctx.putImageData(row, 0, 0);
}

/* ---------- constellation scope ---------- */
const sc = $("scope"), sctx = sc.getContext("2d");
const pts = [];
function drawScope() {
  const w = sc.width, h = sc.height, cx = w/2, cy = h/2, R = Math.min(w,h)*0.36;
  sctx.fillStyle = "#05090a"; sctx.fillRect(0,0,w,h);
  sctx.strokeStyle = "rgba(46,120,112,.28)"; sctx.lineWidth = 1;
  sctx.beginPath(); sctx.moveTo(cx,8); sctx.lineTo(cx,h-8); sctx.moveTo(14,cy); sctx.lineTo(w-14,cy); sctx.stroke();
  sctx.beginPath(); sctx.arc(cx,cy,R,0,Math.PI*2); sctx.stroke();
  for (let k=0;k<8;k++){ const a=k*Math.PI/4; sctx.fillStyle="rgba(120,180,172,.35)";
    sctx.beginPath(); sctx.arc(cx+Math.cos(a)*R, cy-Math.sin(a)*R, 2, 0, Math.PI*2); sctx.fill(); }
  for (const p of pts) {
    sctx.fillStyle = `rgba(52,224,207,${p.a})`;
    sctx.beginPath(); sctx.arc(cx+p.x*R, cy-p.y*R, 1.6, 0, Math.PI*2); sctx.fill();
    p.a *= 0.94;
  }
  while (pts.length && pts[0].a < 0.05) pts.shift();
}
function pushSymbols(spread) {
  for (let i=0;i<7;i++){
    const k = Math.floor(Math.random()*8), a = k*Math.PI/4;
    const r = 1 + (Math.random()*2-1)*spread*0.5;
    const th = a + (Math.random()*2-1)*spread;
    pts.push({ x: Math.cos(th)*r, y: Math.sin(th)*r, a: 0.9 });
  }
  if (pts.length > 260) pts.splice(0, pts.length-260);
}

/* ---------- backend wiring (real Tauri) or demo driver ---------- */
if (TAURI) {
  listen("grant", (e) => addCall(e.payload));
  listen("status", (e) => setStatus(e.payload));
  listen("spectrum", (e) => { pushSpectrum(e.payload.bins_db); pushSymbols(0.2); });
  listen("stopped", () => setState("standby"));
  let followVoice = 0;
  const evCounts = {};
  listen("follow", (e) => {
    const ev = e.payload;
    evCounts[ev.kind] = (evCounts[ev.kind] || 0) + 1;
    if (ev.kind !== "spectrum" && ev.kind !== "status") log(`follow ${ev.kind}: ${JSON.stringify(ev).slice(0, 160)}`);
    else if (evCounts[ev.kind] % 20 === 1) log(`follow ${ev.kind} #${evCounts[ev.kind]}`);
    switch (ev.kind) {
      case "measured":
        setState("locked");
        $("wfAxis").textContent = `${(parseFreq($("center").value) / 1e6).toFixed(2)} MHz ± ${(ev.rate / 2e6).toFixed(2)} MHz`;
        $("r-eq").textContent = "SITE";
        $("r-lock").textContent = "—"; $("r-lockbar").style.width = "0%";
        $("tunedHz").textContent = ev.control_mhz.toFixed(4);
        $("tunedSub").textContent = `CONTROL · ${ev.modulation} · tuner ${ev.correction_hz >= 0 ? "+" : ""}${ev.correction_hz.toFixed(0)} Hz`;
        $("followMeta").textContent = "following";
        break;
      case "call_start":
        $("followMeta").textContent = `on air: ${ev.name} · ${ev.freq_mhz.toFixed(4)} MHz`;
        break;
      case "call":
        followVoice += ev.secs;
        addCall({ tg: ev.tg, name: ev.name, source: ev.source, freq_mhz: ev.freq_mhz,
                  encrypted: false, secs: ev.secs, modulation: ev.modulation, wav: ev.wav });
        $("r-voice").innerHTML = followVoice.toFixed(1) + "<small>s</small>";
        $("followMeta").textContent = "following";
        break;
      case "notice":
        $("followMeta").textContent = ev.text;
        break;
      case "status":
        $("r-syncs").textContent = ev.control_syncs;
        $("r-grants").textContent = ev.calls;
        $("r-syncerr").textContent = ev.dropped ? `${ev.dropped} drop` : "0 drop";
        if (ev.locked) $("followMeta").textContent = `${ev.locked} locked-out call${ev.locked === 1 ? "" : "s"} skipped`;
        $("rateMeta").textContent = `${ev.msps.toFixed(2)}/${ev.want_msps.toFixed(2)} MSPS`;
        break;
      case "spectrum":
        pushSpectrum(ev.bins_db); pushSymbols(0.2);
        break;
    }
  });
  listen("error", (e) => { log(`backend error: ${e.payload}`); setState("standby"); alert("Capture error:\n" + e.payload); });

  const opts = () => ({
    source: $("source").value,
    freq: parseFreq($("freq").value),
    rate: parseFloat($("rate").value),
    gain: $("gain").value.trim() === "" ? null : parseFloat($("gain").value),
    cqpsk: modSel === "cqpsk",
    eq: eqSel,
  });
  $("source").onchange = () => {
    const a = $("source").value === "airspy";
    $("rate").value = a ? (modeSel === "follow" ? "10000000" : "2500000") : "2400000";
    syncTuned();
  };
  $("start").onclick = async () => {
    try { setState("capturing");
      log(`start: mode=${modeSel} source=${$("source").value} rate=${$("rate").value} freq=${$("freq").value} center=${$("center").value}`);
      if (modeSel === "follow") {
        const o = opts(); followVoice = 0;
        $("followMeta").textContent = "measuring the control channel…";
        await invoke("start_follow", { source: o.source, freq: parseFreq($("center").value), rate: o.rate,
          gain: o.gain, control: o.freq, callsDir: $("callsdir").value.trim() || null, play: $("play").checked });
      } else {
        await invoke("start_capture", { ...opts(),
          recordIq: $("reciq").value.trim() || null, recordLog: $("reclog").value.trim() || null });
      }
    }
    catch (err) { setState("standby"); alert(err); }
  };
  $("stop").onclick = () => invoke("stop_capture");
  $("loadcat").onclick = async () => {
    const path = $("catalog").value.trim(); if (!path) return;
    try { const n = await invoke("load_catalog", { path }); $("loadcat").textContent = n + " TGs"; }
    catch (err) { alert(err); }
  };
  $("decode").onclick = async () => {
    const path = $("decfile").value.trim(); if (!path) return;
    try { await invoke("decode_file", { path, rate: parseFloat($("rate").value), cqpsk: modSel === "cqpsk", eq: eqSel }); }
    catch (err) { alert(err); }
  };
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
      $("rrMeta").textContent = missing.length
        ? `missing: ${missing.join(", ")}`
        : st.catalog_len
          ? `${st.catalog_len} talkgroups loaded${st.system_name ? " · " + st.system_name : ""}`
          : "signed in";
      $("rrMeta").style.color = missing.length ? "var(--enc)" : "";
      if (st.catalog_len) $("loadcat").textContent = st.catalog_len + " TGs";
      return st;
    } catch (e) { log(`rr_settings: ${e}`); }
  }
  async function rrSaveCreds() {
    await invoke("rr_save", { appKey: $("rrKey").value, username: $("rrUser").value,
                              password: $("rrPass").value, sid: sidVal() });
    $("rrPass").value = ""; $("rrKey").value = "";
  }
  const sidVal = () => {
    const m = String($("rrSid").value).match(/(\d+)\s*$/); const v = m ? parseInt(m[1], 10) : NaN;
    return Number.isFinite(v) ? v : null;
  };
  $("rrSave").onclick = async () => {
    try { await rrSaveCreds(); $("rrMeta").textContent = "saved"; await rrRefresh(); } catch (e) { alert(e); }
  };

  /* ---------- find a system ---------- */
  let statesLoaded = false;
  async function loadStates() {
    if (statesLoaded) return;
    const st = await invoke("rr_states", {});
    $("bState").innerHTML = '<option value="">—</option>' +
      st.map((s) => `<option value="${s.stid}">${s.name}</option>`).join("");
    statesLoaded = true;
  }
  function renderSystems(list, label) {
    $("findMeta").textContent = label || "";
    $("sysList").innerHTML = list.length ? list.map((s) =>
      `<div class="row" data-sid="${s.sid}"><span class="grow">${s.name}${s.city ? ` <small>· ${s.city}</small>` : ""}</span><span class="mono">sid ${s.sid}</span></div>`
    ).join("") : '<div class="row"><span class="grow" style="color:var(--ink-faint)">No trunked systems listed here.</span></div>';
    $("sysList").querySelectorAll(".row[data-sid]").forEach((r) => r.onclick = () => { $("rrSid").value = r.dataset.sid; loadSystem(+r.dataset.sid); });
  }
  $("bState").onchange = async () => {
    const stid = +$("bState").value; $("bCounty").innerHTML = '<option value="">— statewide —</option>'; if (!stid) return;
    try {
      $("findMeta").textContent = "loading…";
      const v = await invoke("rr_state", { stid });
      $("bCounty").innerHTML += v.counties.map((c) => `<option value="${c.ctid}">${c.name}</option>`).join("");
      renderSystems(v.systems, `${v.systems.length} statewide system${v.systems.length === 1 ? "" : "s"}`);
    } catch (e) { $("findMeta").textContent = ""; alert(e); }
  };
  $("bCounty").onchange = async () => {
    const ctid = +$("bCounty").value; if (!ctid) { $("bState").onchange(); return; }
    try {
      $("findMeta").textContent = "loading…";
      const v = await invoke("rr_county", { ctid });
      renderSystems(v, `${v.length} system${v.length === 1 ? "" : "s"} in this county`);
    } catch (e) { $("findMeta").textContent = ""; alert(e); }
  };
  $("bZipGo").onclick = async () => {
    const zip = parseInt($("bZip").value, 10); if (!Number.isFinite(zip)) return;
    try {
      $("findMeta").textContent = "looking up ZIP…";
      await loadStates();
      const z = await invoke("rr_zip", { zip });
      $("bState").value = String(z.stid); await $("bState").onchange();
      $("bCounty").value = String(z.ctid); await $("bCounty").onchange();
      if (z.city) $("findMeta").textContent = `${z.city} · ` + $("findMeta").textContent;
    } catch (e) { $("findMeta").textContent = ""; alert(e); }
  };
  $("bState").onfocus = () => loadStates().catch((e) => alert(e));

  /* ---------- a loaded system: sites + talkgroups → playlist ---------- */
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
  function siteRate(s) { const span = s.span_mhz ? s.span_mhz[1] - s.span_mhz[0] : 0; return span <= 1.9 ? 2500000 : 10000000; }
  function renderSites() {
    $("siteList").innerHTML = sys.sites.map((s) =>
      `<div class="row ${pickedSite && s.site_id === pickedSite.site_id ? "on" : ""}" data-site="${s.site_id}">` +
      `<span class="grow"><b>${s.site_id}</b> ${s.name}${s.tdma_control ? ' <small style="color:var(--enc)">TDMA CC — not decodable yet</small>' : ""}</span>` +
      (s.nac != null ? `<span class="mono">NAC 0x${s.nac.toString(16).toUpperCase().padStart(3, "0")}</span>` : "") +
      `<span class="mono">${s.control_mhz[0].toFixed(4)} MHz</span>` +
      (s.span_mhz ? `<span class="mono">${(s.span_mhz[1] - s.span_mhz[0]).toFixed(2)} MHz span</span>` : "") +
      `</div>`).join("");
    $("siteList").querySelectorAll(".row").forEach((r) => r.onclick = () => { pickedSite = sys.sites.find((s) => s.site_id === +r.dataset.site); renderSites(); });
  }
  function renderCats() {
    const cats = [...new Set(sys.tgs.map((t) => t.category).filter(Boolean))].sort();
    $("tgCat").innerHTML = '<option value="">all</option>' + cats.map((c) => `<option>${c}</option>`).join("");
  }
  function shownTgs() {
    const q = $("tgFilter").value.trim().toLowerCase(), cat = $("tgCat").value;
    return sys.tgs.filter((t) => (!cat || t.category === cat) &&
      (!q || `${t.id} ${t.alias} ${t.description} ${t.category}`.toLowerCase().includes(q)));
  }
  function renderTgs() {
    const shown = shownTgs();
    $("tgBody").innerHTML = shown.map((t) =>
      `<tr class="${t.encrypted ? "enc" : ""}"><td><input type="checkbox" data-tg="${t.id}" ${picked.has(t.id) ? "checked" : ""} ${t.encrypted ? "disabled" : ""}></td>` +
      `<td class="mono">${t.id}</td><td>${t.alias}</td><td>${t.description}</td><td><small>${t.category}</small></td>` +
      `<td>${t.encrypted ? '<span class="badge enc">Encrypted</span>' : ""}</td></tr>`).join("");
    $("tgBody").querySelectorAll("input[data-tg]").forEach((c) => c.onchange = () => { c.checked ? picked.add(+c.dataset.tg) : picked.delete(+c.dataset.tg); tgMeta(); });
    tgMeta();
  }
  function tgMeta() { $("tgMeta").textContent = picked.size ? `${picked.size} selected` : "none selected → playlist follows every clear talkgroup"; }
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
    try { renderPlaylists(await invoke("playlist_save", { playlist })); $("plMeta").textContent = "saved"; }
    catch (e) { alert(e); }
  };

  /* ---------- playlists ---------- */
  let playlists = [];
  function renderPlaylists(list) {
    playlists = list;
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
    $("playlist").innerHTML = '<option value="">— none: follow every talkgroup —</option>' +
      list.map((p) => `<option value="${p.id}">${p.name}</option>`).join("");
    $("playlist").value = list.some((p) => p.id === cur) ? cur : "";
  }
  async function activatePlaylist(id) {
    try {
      const p = await invoke("playlist_activate", { id: id || null });
      $("playlist").value = p ? p.id : "";
      if (p) {
        modeSel = "follow"; $("modeSeg").querySelectorAll("button").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.v === "follow"))); applyMode();
        $("freq").value = p.control_mhz.toFixed(4) + "M"; $("center").value = p.center_mhz.toFixed(4) + "M";
        $("rate").value = String(p.rate); syncTuned();
        $("followMeta").textContent = `playlist: ${p.name} · ${p.tgs.length ? p.tgs.length + " talkgroups" : "all talkgroups"}`;
        showView("receiver");
      } else { $("followMeta").textContent = ""; }
    } catch (e) { alert(e); }
  }
  $("playlist").onchange = () => activatePlaylist($("playlist").value);
  invoke("playlists_list").then(renderPlaylists).catch((e) => log(`playlists: ${e}`));

  rrRefresh();

  setInterval(drawScope, 55);
  // Dev hook: open with #autostart=airspy to press Start for a 10 MSPS site follow.
  if (location.hash.startsWith("#autostart")) {
    $("source").value = location.hash.split("=")[1] || "airspy"; $("source").onchange();
    setTimeout(() => $("start").click(), 800);
    setTimeout(() => $("stop").click(), 40000);
  }
} else {
  /* ------- demo driver: realistic Marion County SAFE-T session ------- */
  const TGS = [
    { tg: 10103, name: "IMPD Dispatch NW", enc: false },
    { tg: 10106, name: "IMPD Dispatch SE", enc: false },
    { tg: 10128, name: "IMPD Operations 2", enc: false },
    { tg: 10147, name: "IFD Fire Dispatch", enc: false },
    { tg: 10202, name: "Marion Co EMS", enc: false },
    { tg: 10308, name: "Sheriff Patrol", enc: false },
    { tg: 11303, name: "Airport Operations", enc: false },
    { tg: 10255, name: "IMPD Tactical", enc: true },
    { tg: 10204, name: "Signal 13 — Emergency", enc: false },
  ];
  const DLS = [855.4875, 856.9875, 851.1375, 850.8750, 858.9375, 851.2750, 855.2125];
  const rnd = (a,b) => a + Math.random()*(b-a);
  const pick = (a) => a[Math.floor(Math.random()*a.length)];

  let running = false, syncs = 0, grants = 0, voice = 0, lock = 0, syncErr = 0.72, raf = 0, tick = 0;
  const bins = 256, ctrlBin = 168;
  let burst = 0, burstBin = 0, burstW = 0;

  function specRow() {
    const row = new Float32Array(bins);
    for (let i=0;i<bins;i++) row[i] = -88 + rnd(-3,3);
    for (let i=-6;i<=6;i++){ const d=Math.abs(i);
      row[ctrlBin+i] = Math.max(row[ctrlBin+i], -34 - d*3.2 + rnd(-2,2)); }
    if (burst > 0) { burst--;
      for (let i=-burstW;i<=burstW;i++){ const d=Math.abs(i);
        const b = burstBin+i; if (b>=0&&b<bins) row[b] = Math.max(row[b], -40 - d*2.4 + rnd(-3,3)); }
    } else if (Math.random() < 0.03) { burst = Math.floor(rnd(40,120)); burstBin = Math.floor(rnd(30,226)); burstW = Math.floor(rnd(5,9)); }
    return Array.from(row);
  }

  function loop() {
    if (!running) return;
    tick++;
    pushSpectrum(specRow());
    lock = Math.min(0.97, lock + (lock < 0.9 ? 0.02 : 0.002) + rnd(-.01,.01));
    const eqFloor = eqSel === "dfe" ? 0.05 : eqSel === "cma" ? 0.10 : 0.18;
    syncErr = Math.max(eqFloor, syncErr - 0.006 + rnd(-.01,.01));
    pushSymbols(0.10 + syncErr*0.55);
    drawScope();
    if (lock > 0.75) { setState("locked"); if (tick % 6 === 0) syncs++; }
    if (tick % 24 === 0) voice += rnd(0.4, 1.1);
    if (lock > 0.8 && Math.random() < 0.020) {
      const t = pick(TGS); grants++;
      addCall({ tg: t.tg, name: t.name, source: Math.floor(rnd(4910000,4914000)),
                freq_mhz: pick(DLS), encrypted: t.enc });
      burst = Math.floor(rnd(50,120)); burstBin = Math.floor(rnd(30,226)); burstW = Math.floor(rnd(5,9));
    }
    setStatus({ syncs, grants, voice_secs: voice, lock, sync_err: syncErr, modulation: modSel });
    raf = requestAnimationFrame(loop);
  }

  $("start").onclick = () => {
    if (running) return;
    running = true; setState("capturing");
    lock = 0; syncErr = 0.72;
    $("r-eq").textContent = eqSel === "bypass" ? "BARE" : eqSel.toUpperCase();
    loop();
  };
  $("stop").onclick = () => { running = false; cancelAnimationFrame(raf); setState("standby"); };
  $("loadcat").onclick = () => { $("loadcat").textContent = "406 TGs"; };
  $("decode").onclick = () => {
    if (running) return;
    setState("locked"); $("empty").style.display = "none";
    let g = 0, n = 0;
    setStatus({ syncs: 74, grants: 0, voice_secs: 0, lock: 0.94, sync_err: eqSel==="dfe"?0.05:0.10, modulation: modSel });
    const iv = setInterval(() => {
      const t = pick(TGS); g++;
      addCall({ tg: t.tg, name: t.name, source: Math.floor(rnd(4910000,4914000)),
                freq_mhz: pick(DLS), encrypted: t.enc });
      setStatus({ grants: g, voice_secs: (n+=rnd(2,5)) });
      if (g >= 12) { clearInterval(iv); setState("standby"); }
    }, 160);
  };
  drawScope();
}

/* initial paint */
setState("standby");
syncTuned();
$("r-lock").textContent = "0.00";
$("r-syncerr").textContent = "—";
