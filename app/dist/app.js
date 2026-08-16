// HoosierSDR frontend — talks to the Rust backend over Tauri v2 globals.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const pill = $("statuspill");

function parseFreq(s) {
  s = s.trim();
  if (/[Mm]$/.test(s)) return parseFloat(s) * 1e6;
  if (/[kK]$/.test(s)) return parseFloat(s) * 1e3;
  return parseFloat(s);
}

function setLive(live) {
  pill.textContent = live ? "capturing" : "idle";
  pill.classList.toggle("live", live);
  $("start").disabled = live;
  $("stop").disabled = !live;
}

function opts() {
  return {
    freq: parseFreq($("freq").value),
    rate: parseFloat($("rate").value),
    gain: $("gain").value.trim() === "" ? null : parseFloat($("gain").value),
    cqpsk: $("mod").value === "cqpsk",
  };
}

// ---- calls table ----
const tbody = document.querySelector("#calls tbody");
function addCall(g) {
  $("callsEmpty").style.display = "none";
  const tr = document.createElement("tr");
  const t = new Date().toLocaleTimeString();
  tr.innerHTML =
    `<td>${t}</td><td>${g.name}</td><td>${g.source}</td>` +
    `<td>${g.freq_mhz.toFixed(4)} MHz</td>` +
    `<td>${g.encrypted ? '<span class="badge enc">ENCRYPTED</span>' : '<span class="badge clear">CLEAR</span>'}</td>`;
  tbody.prepend(tr);
  while (tbody.children.length > 300) tbody.removeChild(tbody.lastChild);
}

// ---- waterfall ----
const cv = $("waterfall");
const g = cv.getContext("2d");
function pushSpectrum(db) {
  const w = cv.width, h = cv.height;
  // scroll up 1px
  const img = g.getImageData(0, 1, w, h - 1);
  g.putImageData(img, 0, 0);
  // draw newest row at bottom
  const n = db.length;
  const row = g.createImageData(w, 1);
  for (let x = 0; x < w; x++) {
    const v = db[Math.floor((x / w) * n)];
    // map -90..-20 dB → 0..1
    const t = Math.max(0, Math.min(1, (v + 90) / 70));
    const [r, gr, b] = viridis(t);
    const i = x * 4;
    row.data[i] = r; row.data[i + 1] = gr; row.data[i + 2] = b; row.data[i + 3] = 255;
  }
  g.putImageData(row, 0, h - 1);
}
function viridis(t) {
  // cheap phosphor ramp: dark → cyan → green → amber
  const stops = [[10,16,22],[20,60,90],[47,143,99],[94,242,160],[255,194,75]];
  const p = t * (stops.length - 1);
  const i = Math.min(stops.length - 2, Math.floor(p));
  const f = p - i;
  const a = stops[i], b = stops[i + 1];
  return [a[0]+(b[0]-a[0])*f, a[1]+(b[1]-a[1])*f, a[2]+(b[2]-a[2])*f];
}

// ---- events from backend ----
listen("grant", (e) => addCall(e.payload));
listen("status", (e) => {
  const s = e.payload;
  $("t-syncs").textContent = s.syncs;
  $("t-grants").textContent = s.grants;
  $("t-voice").innerHTML = s.voice_secs.toFixed(1) + "<small>s</small>";
  $("t-blocks").textContent = s.blocks;
  $("t-mod").textContent = s.modulation;
});
listen("spectrum", (e) => pushSpectrum(e.payload.bins_db));
listen("stopped", () => { setLive(false); });
listen("error", (e) => { setLive(false); alert("Capture error:\n" + e.payload); });

// ---- controls ----
$("start").onclick = async () => {
  try {
    setLive(true);
    await invoke("start_capture", {
      ...opts(),
      recordIq: $("reciq").value.trim() || null,
      recordLog: $("reclog").value.trim() || null,
    });
  } catch (err) { setLive(false); alert(err); }
};
$("stop").onclick = () => invoke("stop_capture");
$("loadcat").onclick = async () => {
  const path = $("catalog").value.trim();
  if (!path) return;
  try { const n = await invoke("load_catalog", { path }); $("loadcat").textContent = n + " TGs"; }
  catch (err) { alert(err); }
};
$("decode").onclick = async () => {
  const path = $("decfile").value.trim();
  if (!path) return;
  const o = opts();
  try { await invoke("decode_file", { path, rate: o.rate, cqpsk: o.cqpsk }); }
  catch (err) { alert(err); }
};
$("clear").onclick = () => { tbody.innerHTML = ""; $("callsEmpty").style.display = ""; };

setLive(false);
