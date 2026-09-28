/* ================= Settings → Cases: the arrest messaging, in one place ================= */
// Everything about cases that used to be constants in code: what makes a
// run a case and which words are events (the profile), how an event is
// placed on a run, where cases are sent, what the Telegram message says,
// and what the hospital summary call asks for. The backend keeps it all in
// cases.json (see cases.rs and casesend.rs); this page edits a draft, tries
// a transmission against the draft's event rules, and renders the newest
// cases by the draft's wording before anything is saved.
// Loaded after app.js and cases.js and shares their helpers.
(() => {
  if (typeof invoke !== "function") return;
  const STATES = [["dispatched", "Dispatched"], ["working", "Working"], ["rosc", "ROSC"], ["transporting", "Transporting"], ["reported", "Reported to hospital"], ["arrived", "At the hospital"], ["terminated", "Efforts ceased"], ["downgraded", "Not an arrest"]];
  let s = null, saved = "", cur = 0, dests = [], tryTimer = null, trySeq = 0;
  const clone = (x) => JSON.parse(JSON.stringify(x));
  const lines = (v) => String(v || "").split(/\n+/).map((x) => x.trim()).filter(Boolean);
  const csv = (v) => String(v || "").split(/,/).map((x) => x.trim()).filter(Boolean);
  const int = (v, d) => { const n = parseInt(String(v).trim(), 10); return Number.isFinite(n) ? n : d; };
  const stable = (x) => JSON.stringify(x);
  const P = () => (s && s.profiles[cur]) || null;

  /* ---------- dirty ---------- */
  function markDirty() { $("ccDirty").textContent = s && stable(s) !== saved ? "unsaved changes" : ""; }
  function changed(redraw) { markDirty(); if (redraw) fill(); }

  /* ---------- the profile ---------- */
  function fillHead() {
    const p = P();
    $("ccProfile").innerHTML = s.profiles.map((x, i) => `<option value="${i}" ${i === cur ? "selected" : ""}>${esc(x.name || x.id)}</option>`).join("");
    $("ccProfileRow").querySelector("label").style.display = s.profiles.length > 1 ? "" : "none";
    $("ccName").value = p.name; $("ccEnabled").checked = !!p.enabled;
    $("ccMeta").textContent = `${p.name} · ${p.events.length} events · ${p.telegram.enabled ? "Telegram on" : "Telegram off"}`;
    $("ccCallTypes").value = (p.call_types || []).join(", ");
    $("ccPagePhrases").value = (p.page_phrases || []).join("\n");
    $("ccReportWords").value = (p.report_words || []).join("\n");
  }
  $("ccProfile").onchange = () => { cur = +$("ccProfile").value; fill(); };
  $("ccName").oninput = () => { P().name = $("ccName").value; markDirty(); };
  $("ccEnabled").onchange = () => { P().enabled = $("ccEnabled").checked; changed(false); };
  $("ccCallTypes").onchange = () => { P().call_types = csv($("ccCallTypes").value); changed(false); };
  $("ccPagePhrases").onchange = () => { P().page_phrases = lines($("ccPagePhrases").value); changed(false); };
  $("ccReportWords").onchange = () => { P().report_words = lines($("ccReportWords").value); changed(false); };

  /* ---------- events ---------- */
  function fillEvents() {
    const ev = P().events;
    $("ccEvents").innerHTML = ev.map((e, i) => `<div class="ccevent" data-ei="${i}">
      <div class="cctop">
        <input type="text" data-kind="${i}" value="${esc(e.kind)}" list="ccKindList" placeholder="kind" spellcheck="false" title="What the event is, to the state, the board and the email replies">
        <input type="text" data-label="${i}" value="${esc(e.label)}" placeholder="How the timeline names it" spellcheck="false">
        <span class="inline"><span class="ccmove"><button class="btn ghost sm" data-up="${i}" title="Try earlier" ${i === 0 ? "disabled" : ""}>↑</button><button class="btn ghost sm" data-down="${i}" title="Try later" ${i === ev.length - 1 ? "disabled" : ""}>↓</button></span><button class="btn ghost sm ccdel" data-del="${i}" title="Remove">✕</button></span>
      </div>
      <div class="ccbody">
        <label class="field" style="margin:0"><span class="lab">Said by anyone <span class="mono faint">one phrase per line</span></span><textarea data-phrases="${i}" spellcheck="false">${esc((e.phrases || []).join("\n"))}</textarea></label>
        <label class="field" style="margin:0"><span class="lab">Readback word <span class="mono faint">a dispatcher's short log only · one per line</span></span><textarea data-readback="${i}" spellcheck="false">${esc((e.readback || []).join("\n"))}</textarea></label>
      </div></div>`).join("");
    const box = $("ccEvents");
    const row = (i) => ev[+i] || {};
    box.querySelectorAll("[data-kind]").forEach((x) => x.onchange = () => { row(x.dataset.kind).kind = x.value.trim().toLowerCase(); changed(true); });
    box.querySelectorAll("[data-label]").forEach((x) => { x.oninput = () => { row(x.dataset.label).label = x.value; markDirty(); }; x.onchange = () => scheduleTry(); });
    box.querySelectorAll("[data-phrases]").forEach((x) => x.onchange = () => { row(x.dataset.phrases).phrases = lines(x.value); changed(false); scheduleTry(); });
    box.querySelectorAll("[data-readback]").forEach((x) => x.onchange = () => { row(x.dataset.readback).readback = lines(x.value); changed(false); scheduleTry(); });
    box.querySelectorAll("[data-del]").forEach((x) => x.onclick = () => { const i = +x.dataset.del; if (!ev[i]) return; ev.splice(i, 1); changed(true); });
    const swap = (i, j) => { if (!ev[i] || !ev[j]) return; [ev[i], ev[j]] = [ev[j], ev[i]]; changed(true); };
    box.querySelectorAll("[data-up]").forEach((x) => x.onclick = () => swap(+x.dataset.up, +x.dataset.up - 1));
    box.querySelectorAll("[data-down]").forEach((x) => x.onclick = () => swap(+x.dataset.down, +x.dataset.down + 1));
    $("ccEventsMeta").textContent = `${ev.length} rules, tried in this order`;
  }
  $("ccEventAdd").onclick = () => { P().events.push({ kind: "", label: "", phrases: [], readback: [] }); changed(true); const last = $("ccEvents").querySelector(`[data-kind="${P().events.length - 1}"]`); if (last) last.focus(); };
  function scheduleTry() { clearTimeout(tryTimer); tryTimer = setTimeout(runTry, 250); }
  async function runTry() {
    const text = $("ccTryText").value.trim();
    if (!text || !P()) { $("ccTryOut").textContent = "—"; return; }
    const seq = ++trySeq;
    let h; try { h = await invoke("cases_try_event", { profile: P(), text, console: $("ccTryConsole").checked }); } catch (e) { $("ccTryOut").textContent = String(e); return; }
    if (seq !== trySeq) return;
    if (!h) { $("ccTryOut").innerHTML = `<span class="faint">Not an event: no rule matched, or it reads as talk or a question.</span>`; return; }
    const who = h.source === "readback" ? "a dispatcher's readback" : "a crew statement";
    $("ccTryOut").innerHTML = `→ <b>${esc(h.label)}</b> <span class="faint">(${esc(h.kind)})</span> as ${who}${h.clock != null ? `, logged at ${esc(String(Math.floor(h.clock / 60)).padStart(2, "0"))}:${esc(String(h.clock % 60).padStart(2, "0"))}` : ""}`;
  }
  $("ccTryText").oninput = scheduleTry; $("ccTryConsole").onchange = scheduleTry;

  /* ---------- placement ---------- */
  const PL = [["ccOpen", "open_mins"], ["ccInfer", "infer_mins"], ["ccFork", "fork_mins"], ["ccPair", "pair_secs"], ["ccAgrees", "page_agrees_mins"], ["ccRbWords", "readback_words"], ["ccRbUntimed", "readback_words_untimed"]];
  const PLW = [["ccTalking", "talking"], ["ccAsking", "asking"], ["ccRequests", "requests"]];
  function fillPlacement() {
    const pl = P().placement;
    for (const [id, key] of PL) $(id).value = pl[key];
    for (const [id, key] of PLW) $(id).value = (pl[key] || []).join("\n");
  }
  for (const [id, key] of PL) $(id).onchange = () => { P().placement[key] = Math.max(1, int($(id).value, 1)); changed(false); scheduleTry(); };
  for (const [id, key] of PLW) $(id).onchange = () => { P().placement[key] = lines($(id).value); changed(false); scheduleTry(); };

  /* ---------- sending ---------- */
  function fillSend() {
    const t = P().telegram;
    $("csSendOn").checked = !!t.enabled;
    $("csSendDest").innerHTML = `<option value="">— none —</option>` + dests.map((d) => `<option value="${esc(d.id)}" ${d.id === t.dest ? "selected" : ""}>${esc(d.name)}</option>`).join("")
      + (t.dest && !dests.some((d) => d.id === t.dest) ? `<option value="${esc(t.dest)}" selected>a destination that was removed</option>` : "");
    $("csSendHosp").checked = t.hospitals !== false; $("csSendMap").checked = t.map !== false; $("csSendAudio").checked = t.audio !== false;
    $("csSendEmail").checked = !!t.email; $("csSendEmailTo").value = t.email_to || "";
    $("csSendMeta").textContent = [t.enabled ? "Telegram on" : "", t.email ? "email on" : ""].filter(Boolean).join(" · ") || "off";
  }
  const SEND = [["csSendOn", "enabled", "checked"], ["csSendDest", "dest", "value"], ["csSendHosp", "hospitals", "checked"], ["csSendMap", "map", "checked"], ["csSendAudio", "audio", "checked"], ["csSendEmail", "email", "checked"], ["csSendEmailTo", "email_to", "value"]];
  for (const [id, key, prop] of SEND) $(id).onchange = () => { P().telegram[key] = prop === "value" ? $(id).value.trim() : $(id).checked; changed(false); fillSend(); };
  $("csPreviewBtn").onclick = async () => {
    const p = P(); if (!p) return;
    const days = await uiAsk("Preview what the cases already built would have sent, by the settings as saved, over how many days? Nothing is sent.", "3", "Preview");
    if (days == null || !(+days > 0)) return;
    try {
      const r = await invoke("cases_preview", { profile: p.id, days: +days });
      if (!r) return;
      const when = (epoch) => new Date(epoch * 1000).toLocaleString("en-US", { hour12: false, month: "short", day: "2-digit", hour: "2-digit", minute: "2-digit" });
      const rows = (r.days || []).map((d) => `<tr><td class="mono">${esc(d.date)}</td><td>${esc(d.dest)}</td><td class="mono">${d.threads}</td><td class="mono">${d.replies}</td></tr>`).join("");
      const sample = (r.cases || []).filter((k) => k.chats.length).slice(0, 5).map((k) =>
        `<details class="cs-sample"><summary>${esc(k.title)} · ${esc(k.address || "address not heard")} · ${esc(when(k.opened))} → ${esc(k.chats.join(", "))}</summary><pre>${esc(k.timeline)}</pre>${k.replies.length ? `<div class="lab small">Reports heard, under it</div><pre>${esc(k.replies.join("\n"))}</pre>` : ""}</details>`).join("");
      $("csPreview").innerHTML = (r.warnings || []).map((w) => `<div class="cs-warn">${esc(w)}</div>`).join("")
        + (rows ? `<table class="cs-count"><thead><tr><th>Day</th><th>Chat</th><th>Cases</th><th>Reports heard</th></tr></thead><tbody>${rows}</tbody></table>` : '<div class="empty small">Nothing would have been sent.</div>') + sample;
    } catch (e) { uiToast(`${e}`, "err"); }
  };

  /* ---------- the message ---------- */
  function fillMessage() {
    const m = P().message;
    $("ccIcon").value = m.icon; $("ccUnitsIcon").value = m.units_icon; $("ccShowUnits").checked = !!m.show_units; $("ccShowUnstated").checked = !!m.show_unstated;
    $("ccBanners").innerHTML = `<div class="ccrow banner head"><span>State</span><span>Icon</span><span>Banner line</span><span>On a board</span></div>` + STATES.map(([st, name]) => {
      const b = (m.banners || []).find((x) => x.state === st) || { icon: "", words: "", label: "" };
      return `<div class="ccrow banner"><span class="k" title="${esc(st)}">${esc(name)}</span><input type="text" data-bicon="${st}" value="${esc(b.icon)}" spellcheck="false"><input type="text" data-bwords="${st}" value="${esc(b.words)}" spellcheck="false"><input type="text" data-blabel="${st}" value="${esc(b.label)}" spellcheck="false"></div>`;
    }).join("");
    const bannerOf = (st) => { let b = (m.banners || (m.banners = [])).find((x) => x.state === st); if (!b) { b = { state: st, icon: "", words: "", label: "" }; m.banners.push(b); } return b; };
    $("ccBanners").querySelectorAll("[data-bicon]").forEach((x) => x.onchange = () => { bannerOf(x.dataset.bicon).icon = x.value.trim(); changed(false); });
    $("ccBanners").querySelectorAll("[data-bwords]").forEach((x) => x.onchange = () => { bannerOf(x.dataset.bwords).words = x.value.trim(); changed(false); });
    $("ccBanners").querySelectorAll("[data-blabel]").forEach((x) => x.onchange = () => { bannerOf(x.dataset.blabel).label = x.value.trim(); changed(false); });
    const f = m.facts || (m.facts = []);
    $("ccFacts").innerHTML = `<div class="ccrow fact head"><span>Fact</span><span>Label</span><span>When not stated</span><span></span></div>` + f.map((x, i) => `<div class="ccrow fact">
      <input type="text" data-fkey="${i}" value="${esc(x.key)}" list="ccFactKeys" spellcheck="false"><input type="text" data-flabel="${i}" value="${esc(x.label)}" spellcheck="false"><input type="text" data-funstated="${i}" value="${esc(x.unstated)}" spellcheck="false">
      <span class="inline"><span class="ccmove"><button class="btn ghost sm" data-fup="${i}" ${i === 0 ? "disabled" : ""}>↑</button><button class="btn ghost sm" data-fdown="${i}" ${i === f.length - 1 ? "disabled" : ""}>↓</button></span><button class="btn ghost sm ccdel" data-fdel="${i}" title="Remove">✕</button></span></div>`).join("");
    const box = $("ccFacts"), row = (i) => f[+i] || {};
    box.querySelectorAll("[data-fkey]").forEach((x) => x.onchange = () => { row(x.dataset.fkey).key = x.value.trim().toLowerCase(); changed(false); });
    box.querySelectorAll("[data-flabel]").forEach((x) => x.onchange = () => { row(x.dataset.flabel).label = x.value.trim(); changed(false); });
    box.querySelectorAll("[data-funstated]").forEach((x) => x.onchange = () => { row(x.dataset.funstated).unstated = x.value.trim(); changed(false); });
    box.querySelectorAll("[data-fdel]").forEach((x) => x.onclick = () => { const i = +x.dataset.fdel; if (!f[i]) return; f.splice(i, 1); changed(true); });
    const swap = (i, j) => { if (!f[i] || !f[j]) return; [f[i], f[j]] = [f[j], f[i]]; changed(true); };
    box.querySelectorAll("[data-fup]").forEach((x) => x.onclick = () => swap(+x.dataset.fup, +x.dataset.fup - 1));
    box.querySelectorAll("[data-fdown]").forEach((x) => x.onclick = () => swap(+x.dataset.fdown, +x.dataset.fdown + 1));
    $("ccFactKeys").innerHTML = (s.report.facts || []).map((x) => `<option value="${esc(x.key)}">`).join("");
    for (const [id, key] of [["ccCaption", "caption_chars"], ["ccText", "text_chars"], ["ccSummaryMin", "summary_min"], ["ccStartWithin", "start_within_hours"], ["ccNotifyWithin", "notify_within_mins"], ["ccEtaMove", "eta_move_mins"]]) $(id).value = m[key];
    const kinds = []; for (const e of P().events) if (e.kind && !kinds.includes(e.kind)) kinds.push(e.kind);
    const on = m.email_events || (m.email_events = []);
    $("ccEmailEvents").innerHTML = kinds.map((k) => `<span class="chip ${on.includes(k) ? "on" : ""}" data-ek="${esc(k)}">${esc(k)}</span>`).join("") || `<span class="faint">no event kinds yet</span>`;
    $("ccEmailEvents").querySelectorAll("[data-ek]").forEach((x) => x.onclick = () => { const k = x.dataset.ek; const i = on.indexOf(k); if (i >= 0) on.splice(i, 1); else on.push(k); changed(true); });
    $("ccMsgMeta").textContent = `${m.caption_chars} / ${m.text_chars} chars`;
  }
  $("ccIcon").onchange = () => { P().message.icon = $("ccIcon").value.trim(); changed(false); };
  $("ccUnitsIcon").onchange = () => { P().message.units_icon = $("ccUnitsIcon").value.trim(); changed(false); };
  $("ccShowUnits").onchange = () => { P().message.show_units = $("ccShowUnits").checked; changed(false); };
  $("ccShowUnstated").onchange = () => { P().message.show_unstated = $("ccShowUnstated").checked; changed(false); };
  $("ccFactAdd").onclick = () => { P().message.facts.push({ key: "", label: "", unstated: "" }); changed(true); };
  for (const [id, key, lo, hi] of [["ccCaption", "caption_chars", 200, 1024], ["ccText", "text_chars", 500, 4096], ["ccSummaryMin", "summary_min", 20, 2000], ["ccStartWithin", "start_within_hours", 1, 240], ["ccNotifyWithin", "notify_within_mins", 1, 1440], ["ccEtaMove", "eta_move_mins", 1, 240]]) {
    $(id).onchange = () => { P().message[key] = Math.min(hi, Math.max(lo, int($(id).value, lo))); changed(true); };
  }
  $("ccPreview").onclick = async () => {
    const p = P(); if (!p) return;
    $("ccPreviewNote").textContent = "rendering…";
    try {
      const out = await invoke("cases_message_preview", { profile: p, days: 7 });
      const when = (epoch) => new Date(epoch * 1000).toLocaleString("en-US", { hour12: false, month: "short", day: "2-digit", hour: "2-digit", minute: "2-digit" });
      $("ccPreviewOut").innerHTML = out && out.length ? `<div class="ccprev">${out.map((c) => `<div class="cct">${esc(c.title)} · ${esc(when(c.opened))}</div><pre>${esc(c.text)}</pre>`).join("")}</div>` : `<div class="empty small">No case of this profile in the last 7 days. “Rebuild…” on the Cases tab builds them from calls already stored.</div>`;
      $("ccPreviewNote").textContent = out && out.length ? `${out.length} newest, as the draft would word them` : "";
    } catch (e) { $("ccPreviewNote").textContent = ""; uiToast(`${e}`, "err"); }
  };

  /* ---------- the report facts ---------- */
  function fillReport() {
    const f = s.report.facts || (s.report.facts = []);
    $("ccReport").innerHTML = `<div class="ccrow report head"><span>Key</span><span>What to put after it</span><span>Arrest only</span><span></span></div>` + f.map((x, i) => `<div class="ccrow report">
      <input type="text" data-rkey="${i}" value="${esc(x.key)}" spellcheck="false"><input type="text" data-rask="${i}" value="${esc(x.ask)}" spellcheck="false"><input type="checkbox" data-ronly="${i}" ${x.arrest_only ? "checked" : ""} title="“not stated” for a patient who was not in arrest">
      <span class="inline"><span class="ccmove"><button class="btn ghost sm" data-rup="${i}" ${i === 0 ? "disabled" : ""}>↑</button><button class="btn ghost sm" data-rdown="${i}" ${i === f.length - 1 ? "disabled" : ""}>↓</button></span><button class="btn ghost sm ccdel" data-rdel="${i}" title="Remove">✕</button></span></div>`).join("");
    const box = $("ccReport"), row = (i) => f[+i] || {};
    box.querySelectorAll("[data-rkey]").forEach((x) => x.onchange = () => { row(x.dataset.rkey).key = x.value.trim().toLowerCase(); changed(true); });
    box.querySelectorAll("[data-rask]").forEach((x) => x.onchange = () => { row(x.dataset.rask).ask = x.value.trim(); changed(true); });
    box.querySelectorAll("[data-ronly]").forEach((x) => x.onchange = () => { row(x.dataset.ronly).arrest_only = x.checked; changed(true); });
    box.querySelectorAll("[data-rdel]").forEach((x) => x.onclick = () => { const i = +x.dataset.rdel; if (!f[i]) return; f.splice(i, 1); changed(true); });
    const swap = (i, j) => { if (!f[i] || !f[j]) return; [f[i], f[j]] = [f[j], f[i]]; changed(true); };
    box.querySelectorAll("[data-rup]").forEach((x) => x.onclick = () => swap(+x.dataset.rup, +x.dataset.rup - 1));
    box.querySelectorAll("[data-rdown]").forEach((x) => x.onclick = () => swap(+x.dataset.rdown, +x.dataset.rdown + 1));
    // The FACTS block as the model will see it, from the list as it stands.
    const arrest = f.filter((x) => x.arrest_only).map((x) => x.key).filter(Boolean);
    const list = arrest.length > 1 ? `${arrest.slice(0, -1).join(", ")} and ${arrest[arrest.length - 1]}` : arrest[0] || "";
    const sentence = arrest.length ? ` ${list.charAt(0).toUpperCase()}${list.slice(1)} ${arrest.length === 1 ? "is" : "are"} about a cardiac arrest: for a patient who was not in arrest, each is "not stated".` : "";
    $("ccReportPrev").textContent = `Last, after the note, a blank line and the facts: a line "FACTS:" and then exactly these lines, in this order, each written "key: value". The value is "not stated" unless the transcript says it; never work one out.${sentence}\n` + f.map((x) => `${x.key}: ${x.ask}`).join("\n");
    $("ccReportMeta").textContent = `${f.length} facts`;
  }
  $("ccReportAdd").onclick = () => { s.report.facts.push({ key: "", ask: "", arrest_only: false }); changed(true); };

  /* ---------- fill / load / save ---------- */
  function fill() {
    if (!s || !P()) return;
    if (cur >= s.profiles.length) cur = 0;
    fillHead(); fillEvents(); fillPlacement(); fillSend(); fillMessage(); fillReport(); markDirty();
  }
  async function loadDests() { try { const a = await invoke("alerts_get"); dests = (a && a.settings && a.settings.destinations) || []; } catch (_) { dests = []; } }
  async function load() {
    let got; try { got = await invoke("cases_settings_get"); } catch (e) { log(`cases_settings_get: ${e}`); return; }
    if (!got || !got.profiles || !got.profiles.length) return;
    await loadDests();
    s = clone(got); saved = stable(s);
    fill();
  }
  async function save() {
    const p = P(); if (!p) return false;
    const was = JSON.parse(saved);
    const before = was.profiles.find((x) => x.id === p.id);
    if (p.telegram.enabled && !(before && before.telegram.enabled) && !(await uiConfirm("Cases will start sending to Telegram. Any tripwire that already announces arrests will keep sending too — turn those off once this looks right.", "Switch on"))) return false;
    try { const got = await invoke("cases_settings_set", { settings: s }); if (got) { s = clone(got); saved = stable(s); fill(); } return true; }
    catch (e) { uiToast(`Could not save: ${e}`, "err"); return false; }
  }
  $("ccSave").onclick = async () => { if (await save()) uiToast("Case settings saved — the cases are being built again by them"); };
  $("ccReset").onclick = async () => {
    if (!(await uiConfirm("Put every case setting back to the built-in cardiac arrest profile, including where cases are sent? Nothing changes until you Save.", "Reset"))) return;
    try { s = await invoke("cases_settings_defaults"); } catch (e) { uiToast(`${e}`, "err"); return; }
    if (!s) return;
    cur = 0; fill();
  };
  window.casesSettingsOnShow = async () => {
    if (s && stable(s) !== saved) { await loadDests(); fillSend(); return; }
    load();
  };
  // Destinations renamed or added on Connections show up in the picker.
  const prevRender = window.connectionsRender;
  window.connectionsRender = () => { if (prevRender) prevRender(); if (s) loadDests().then(fillSend); };
  load();
})();
