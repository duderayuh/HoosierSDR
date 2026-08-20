// HoosierSDR front end. Drives the real Tauri backend when present
// (start_capture / grant / status / spectrum), and falls back to a realistic
// demo driver when opened without a backend — so the same file previews the UI.
const $ = (id) => document.getElementById(id);
const TAURI = window.__TAURI__;
const invoke = TAURI ? TAURI.core.invoke : null;
const listen = TAURI ? TAURI.event.listen : null;

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
    : '<span class="badge clear">Clear</span>';
  tr.innerHTML =
    `<td class="time">${t}</td>` +
    `<td class="tg">${g.name}<span class="num">TG ${g.tg}</span></td>` +
    `<td class="src">${g.source ? g.source : "—"}</td>` +
    `<td class="dl">${g.freq_mhz.toFixed(4)}<span style="color:var(--ink-faint);font-size:11px"> MHz</span></td>` +
    `<td>${badge}</td>`;
  tbody.prepend(tr);
  while (tbody.children.length > 200) tbody.removeChild(tbody.lastChild);
}
$("clear").onclick = () => { tbody.innerHTML = ""; $("empty").style.display = ""; };

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
  listen("error", (e) => { setState("standby"); alert("Capture error:\n" + e.payload); });

  const opts = () => ({
    source: $("source").value,
    freq: parseFreq($("freq").value),
    rate: parseFloat($("rate").value),
    gain: $("gain").value.trim() === "" ? null : parseFloat($("gain").value),
    cqpsk: modSel === "cqpsk",
    eq: eqSel,
  });
  $("source").onchange = () => {
    $("rate").value = $("source").value === "airspy" ? "2500000" : "2400000";
  };
  $("start").onclick = async () => {
    try { setState("capturing");
      await invoke("start_capture", { ...opts(),
        recordIq: $("reciq").value.trim() || null, recordLog: $("reclog").value.trim() || null }); }
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
  setInterval(drawScope, 55);
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
