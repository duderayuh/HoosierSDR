// Boots the real page under jsdom with a fake Tauri bridge, so a load-time
// JavaScript error — which leaves every list in the app empty and looks like
// a wiped install — fails here instead of on the radio.
//   cd app && npm i --no-save jsdom@24 && node scripts/pagecheck.js
// Exits non-zero on any thrown error; prints the startup commands it issued.
const { JSDOM } = require("jsdom"); const fs = require("fs");
const html = fs.readFileSync(require("path").join(__dirname, "..", "dist", "index.html"), "utf8").replace(/<script src="app.js"><\/script>/, "");
const dom = new JSDOM(html, { runScripts: "outside-only", pretendToBeVisual: true, url: "http://127.0.0.1/" });
const w = dom.window;
w.HTMLCanvasElement.prototype.getContext = () => new Proxy({}, { get: (t, p) => (p === "createImageData" ? () => ({ data: new Uint8ClampedArray(4) }) : p === "measureText" ? () => ({ width: 10 }) : () => {}), set: () => true });
w.matchMedia = () => ({ matches: false });
w.requestAnimationFrame = (f) => setTimeout(f, 16);
const canned = { playlists_list: [], units_list: [], unit_rules_list: [], catalog_rows: [], catalogs_list: [], alerts_get: { settings: { alerts: [], telegram: { chat_id: "" }, ollama: { url: "", model: "", timeout_secs: 60, fail_open: true } }, has_token: false, ffmpeg: true }, alerts_log: [], ollama_models: [], hook_get: { settings: { enabled: false, command: "", timeout_secs: 20, min_secs: 0, emergency_only: false }, status: { runs: 0, failures: 0 } }, names_get: { settings: { template: "x" }, tokens: [], example: "" }, transcribe_probe: { settings: { enabled: false, engine: "faster-whisper", model: "base", language: "en", device: "auto" }, engines: [] }, library_stats: [0, 0, 0, ""], transcribe_models: [], format_get: { format: { codec: "wav", bitrate_kbps: 32, mode: "vbr" }, ffmpeg: "" }, uploads_get: { settings: { rdio: { enabled: false, url: "", key: "", system: 0, system_label: "" }, openmhz: { enabled: false, url: "", short_name: "", api_key: "" }, broadcastify: { enabled: false, api_key: "", system_id: 0, format: "m4a" }, min_secs: 1 }, status: { sent: 0, failed: 0, queued: 0 }, ffmpeg: true }, stream_get: { settings: { enabled: false, host: "", port: 80, mount: "", user: "", password: "", codec: "mp3", bitrate_kbps: 16, name: "", description: "", tls: false }, status: {}, ffmpeg: true }, rr_settings: {}, sys_status: { cpu_app: 0, cpu_total: 0, cores: 1, mem_app_mb: 0, mem_used_mb: 0, mem_total_mb: 1, disk_free_gb: 1, disk_total_gb: 1, library_calls: 0, library_minutes: 0, uptime_secs: 0 }, audio_queued: { clips: 0, secs: 0, dropped: 0 }, get_volume: 1, devices_list: { devices: [{ kind: "airspy", id: "00000000A1B2C3D4", label: "Airspy · A1B2C3D4", rates: [10000000, 2500000] }, { kind: "rtlsdr", id: "driver=rtlsdr,serial=0001", label: "RTL-SDR · 0001", rates: [2400000] }], settings: { "airspy|00000000A1B2C3D4": { nickname: "Roof", ppm: 1.2, gain: null, rate: 10000000 } }, rtl_gains_db: [0, 0.9, 49.6] }, devices_get: {}, gain_live: "applied", tg_latest_call: null, conversations_get: { settings: { rules: [{ id: "c1", name: "Hospitals", enabled: true, tgs: [10202], fixed_units: [], learn_fixed: true, end_gap_secs: 90, late_window_secs: 180, max_secs: 900, min_calls: 1, summary_prompt: "x", message: "y", chat_id: "", attach_audio: true, send_without_transcript: false }], learned: {}, seen: {} }, proposed_fixed: { "c1:10202": [900001] } }, conversations_state: { open: [{ key: 1, rule_id: "c1", rule_name: "Hospitals", tg: 10202, tg_name: "ER", mobile_unit: 1, pieces: [{ id: 1, unit: 1, unit_name: "Medic 3", fixed: false, at: 0, secs: 3, audio: null, transcript: "hi" }], first_at: 0, last_at: 0, sent_ids: [], sent_chat: "", sent_at: 1, dirty: false, revision: 0, busy: false, last_summary: "sum", last_error: null }], log: [{ at: 0, rule: "Hospitals", tg_name: "ER", units: "Medic 3", calls: 1, revision: 0, ok: true, detail: "sent", summary: "s" }] } };
const calls = [];
const listeners = {};
w.__TAURI__ = { core: { invoke: async (cmd, args) => { calls.push(cmd); if (cmd in canned) return canned[cmd]; return null; } }, event: { listen: async (name, cb) => { (listeners[name] = listeners[name] || []).push(cb); return () => {}; } } };
w.__exercise = async () => {
  // Drive every handler the way the backend would, after the page has loaded.
  const fire = (name, payload) => (listeners[name] || []).forEach((cb) => { try { cb({ payload }); } catch (e) { console.log("PAGE ERROR: event " + name + ": " + e.stack); } });
  const follow = (p) => fire("follow", p);
  follow({ kind: "measured", control_mhz: 851.5375, modulation: "CQPSK", correction_hz: -412, rate: 9600000, center_mhz: 855, ppm: 0.48 });
  follow({ kind: "site", nac: 0x260, wacn: 0xBEE00, sys_id: 0x262, control_mhz: 851.5375, alternates_mhz: [851.2125], idens: [[1, 851.00625, 6.25]], patches: [[957, [10203, 10204]]], rfss: 1, site: 12, neighbours: [[0x262, 1, 13, 856.2375]] });
  follow({ kind: "spectrum", bins_db: new Array(256).fill(-90) });
  follow({ kind: "constellation", modulation: "CQPSK", points: [[1, 0], [0, 1]] });
  follow({ kind: "grant", tg: 10999, name: "TG 10999", named: false, freq_mhz: 857.3625, unit: 1, encrypted: false });
  follow({ kind: "call_start", tg: 10147, name: "Fire", freq_mhz: 857.3875, priority: 10 });
  follow({ kind: "call", tg: 10147, name: "Fire", source: 4910003, unit_name: "Car 12", talker_alias: "ENG 21", freq_mhz: 857.3875, modulation: "CQPSK", secs: 6.4, wav: "/tmp/x.wav", emergency: true, patched_with: [], id: 7, syncs_c4fm: 1, syncs_cqpsk: 9 });
  follow({ kind: "call", tg: 10103, name: "Police", source: 0, unit_name: null, talker_alias: null, freq_mhz: 851.8125, modulation: "?", secs: 0, wav: null, emergency: false, patched_with: [], id: 8, syncs_c4fm: 0, syncs_cqpsk: 0 });
  follow({ kind: "mobility", what: "affiliated", unit: 4910003, unit_name: "Car 12", tg: 10103, name: "Police" });
  follow({ kind: "location", unit: 4910003, unit_name: "Car 12", lat: 39.7684, lon: -86.1581 });
  follow({ kind: "talker_alias", tg: 10147, name: "Fire", alias: "ENG 21" });
  follow({ kind: "notice", text: "control channel moved" });
  follow({ kind: "status", control_syncs: 1, calls: 2, out_of_band: 0, encrypted: 0, locked: 0, busy: 0, msps: 9.6, want_msps: 9.6, dropped: 0, elapsed_secs: 1 });
  // With the receiver-health fields present (lock, multipath meter, clip flag) and again with them absent/null.
  follow({ kind: "status", control_syncs: 2, calls: 2, out_of_band: 0, encrypted: 0, locked: 0, busy: 0, msps: 9.6, want_msps: 9.6, dropped: 0, elapsed_secs: 2, signal_dbfs: -31.2, lock: 0.91, echo_frac: 0.033, echo_spread_us: 101.0, clip_pct: 1.4 });
  follow({ kind: "status", control_syncs: 3, calls: 2, out_of_band: 0, encrypted: 0, locked: 0, busy: 0, msps: 9.6, want_msps: 9.6, dropped: 0, elapsed_secs: 3, signal_dbfs: null, lock: null, echo_frac: null, echo_spread_us: null, clip_pct: 0.0 });
  fire("transcript", { id: 7, text: "working fire", model: "base" });
  fire("alert", { name: "Arrest", tg: 10147, message: "hi\nthere", tone: true });
  fire("alert_error", "x"); fire("hook_error", "y"); fire("transcribe_error", "z"); fire("transcribe_ready", "m");
  fire("grant", { tg: 1, name: "TG 1", source: 2, freq_mhz: 851.1, encrypted: false });
  fire("rr_progress", { step: "sites", done: 1, total: 3 }); fire("rr_progress", { step: "done" });
  fire("stopped", null); fire("conversations", null);
  for (const v of ["library", "playlists", "aliases", "discovery", "alerts", "devices", "settings", "monitor"]) { try { w.showView(v); } catch (e) { console.log("PAGE ERROR: view " + v + ": " + e.stack); } }
  // Click every button that has a handler, with dialogs auto-cancelled.
  w.uiConfirm = async () => false;
  for (const b of w.document.querySelectorAll("button")) { if (typeof b.onclick === "function") { try { const r = b.onclick({ target: b, preventDefault() {} }); if (r && r.catch) r.catch((e) => console.log("PAGE ERROR: async click " + (b.id || b.textContent.trim()) + ": " + e)); } catch (e) { console.log("PAGE ERROR: click " + (b.id || b.textContent.trim()) + ": " + e.stack); } } }
};
w.addEventListener("error", (e) => console.log("PAGE ERROR:", e.message, e.error && e.error.stack));
w.console.log = (m) => { if (/error|rejection/i.test(String(m))) console.log("LOG:", m); };
try { w.eval(fs.readFileSync(require("path").join(__dirname, "..", "dist", "app.js"), "utf8")); } catch (e) { console.log("THROW:", e.stack); }
let failed = false;
const origLog = console.log; console.log = (...a) => { if (/PAGE ERROR|THROW|LOG:/.test(String(a[0]))) failed = true; origLog(...a); };
setTimeout(async () => { await w.__exercise(); setTimeout(() => { console.log("invoked:", [...new Set(calls)].join(" ")); process.exit(failed ? 1 : 0); }, 800); }, 1500);
