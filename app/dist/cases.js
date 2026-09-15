/* Cases: one patient's run as a timeline of what the radio said.
 *
 * Everything shown is built in `cases.rs` from the library. This page only
 * draws it. Every line names where it was heard (the page, the dispatcher's
 * readback, the crew, the hospital report) and plays the call it came from;
 * a line placed on a run by the clock alone says so.
 */
(() => {
  if (typeof invoke !== "function") return;

  const STATE = {
    dispatched: ["Dispatched", "amber"], working: ["Working", "enc"], rosc: ["ROSC", "clear"],
    transporting: ["Transporting", "signal"], reported: ["Reported to hospital", "signal"], arrived: ["At the hospital", "signal"],
    terminated: ["Efforts ceased", "faint"], downgraded: ["Not an arrest", "faint"],
  };
  const SOURCE = { page: "page", readback: "dispatcher", crew: "crew", report: "hospital report" };
  // The facts the summary call is asked for, in its order, as a clinician reads them.
  const FACT = [["age", "Age"], ["sex", "Sex"], ["witnessed", "Witnessed"], ["bystander cpr", "Bystander CPR"], ["rhythm", "Rhythm"],
    ["rosc", "ROSC"], ["downtime", "Downtime, as said"], ["history", "History"], ["eta", "ETA, as said"]];
  let data = { cases: [], unplaced: [] }, curId = null, timer = null;

  const shown = () => $("view-cases") && $("view-cases").style.display !== "none";
  const hm = (epoch) => new Date(epoch * 1000).toLocaleTimeString("en-US", { hour12: false, hour: "2-digit", minute: "2-digit" });
  const hms = (epoch) => new Date(epoch * 1000).toLocaleTimeString("en-US", { hour12: false });
  const when = (epoch) => new Date(epoch * 1000).toLocaleString("en-US", { hour12: false, month: "short", day: "2-digit", hour: "2-digit", minute: "2-digit" });
  const stateChip = (st) => { const [label, tone] = STATE[st] || [st, "faint"]; return `<span class="cs-state ${tone}">${esc(label)}</span>`; };
  const firstOf = (k, kind) => k.lines.find((l) => l.kind === kind);

  function card(k) {
    const last = k.lines[k.lines.length - 1];
    return `<button class="cs-card ${k.id === curId ? "sel" : ""} ${k.open ? "open" : ""}" data-case="${k.id}">`
      + `<span class="cs-stripe ${(STATE[k.state] || [])[1] || "faint"}"></span>`
      + `<span class="cs-cardbody"><span class="cs-cardtop"><b>${esc(k.title)}</b>${stateChip(k.state)}</span>`
      + `<span class="cs-addr">${esc(k.address || "address not heard")}</span>`
      + `<span class="mono faint small">${esc(when(k.opened))}${k.units.length ? ` · ${esc(k.units.slice(0, 4).join(", "))}${k.units.length > 4 ? "…" : ""}` : ""}</span>`
      + (last ? `<span class="faint small">${esc(last.clock || hm(last.at))} ${esc(last.label)}</span>` : "")
      + `</span></button>`;
  }

  function facts(k) {
    const out = [];
    const d = firstOf(k, "dispatched");
    const w = firstOf(k, "working");
    const r = firstOf(k, "rosc");
    if (d) out.push(["Dispatched", d.clock || hm(d.at)]);
    if (w) out.push(["Working first said", w.clock || hm(w.at)]);
    if (r) out.push(["ROSC first said", r.clock || hm(r.at)]);
    if (k.open && d) out.push(["Since dispatch", `${Math.max(0, Math.round((Date.now() / 1000 - d.at) / 60))} min`]);
    const reports = k.lines.filter((l) => l.kind === "report");
    if (reports.length) out.push(["Last report", reports[reports.length - 1].label.replace(/^Report to /, "")]);
    return out.map(([a, b]) => `<div><span class="k">${esc(a)}</span><span class="v">${esc(b)}</span></div>`).join("");
  }

  // The latest value each report gave, so a later report's update wins.
  function patient(k) {
    const got = new Map();
    k.lines.filter((l) => l.kind === "report").forEach((l) => (l.facts || []).forEach((f) => got.set(f.key, f.value)));
    return FACT.filter(([key]) => got.has(key)).map(([key, label]) => [label, got.get(key)]);
  }

  const chips = (pairs) => pairs.map(([a, b]) => `<span class="cs-chip"><span class="k">${esc(a)}</span> ${esc(b)}</span>`).join("");

  function arrival(k) {
    const a = k.arrival;
    if (!a) return "";
    const rows = [];
    const window = a.from == null ? null : a.from === a.to ? `about ${hm(a.from)}` : `${hm(a.from)}–${hm(a.to)}`;
    rows.push([`Expected at ${a.place || "the hospital"}`, window || "no ETA said",
      a.said ? `said “${a.said}” at ${hm(a.anchor)}` : `report at ${hm(a.anchor)}`]);
    if (a.drive_min != null) rows.push(["Drive from the scene", `${a.drive_min} min ${a.drive_how}`, a.km != null ? `${a.km} km` : ""]);
    if (a.arrived != null) {
      const off = a.off_by_min;
      const vs = off == null ? "" : off === 0 ? "inside the window" : off > 0 ? `${off} min after the window` : `${-off} min before the window`;
      rows.push(["Said at the hospital", hm(a.arrived), vs]);
    }
    return `<div class="cs-arrival">${rows.map(([k2, v, sub]) => `<div><span class="k">${esc(k2)}</span><span class="v">${esc(v)}</span>${sub ? `<span class="sub">${esc(sub)}</span>` : ""}</div>`).join("")}`
      + (a.note ? `<div class="cs-note">${esc(a.note)}</div>` : "") + `</div>`;
  }

  function line(l) {
    const heard = l.clock && l.clock !== hm(l.at) ? ` title="The dispatcher's logged time; heard at ${esc(hms(l.at))}"` : ` title="${esc(hms(l.at))}"`;
    return `<div class="cs-line k-${esc(l.kind)}">`
      + `<span class="cs-time mono"${heard}>${esc(l.clock || hm(l.at))}</span>`
      + `<span class="cs-what"><span class="cs-top"><span class="cs-label">${esc(l.label)}</span>`
      + ` <span class="cs-src">${esc(SOURCE[l.source] || l.source)}</span>${l.inferred ? ' <span class="cs-inferred" title="Placed on this run without anything on the air naming it">inferred</span>' : ""}</span>`
      + (l.how ? `<span class="cs-how">${esc(l.how)}</span>` : "")
      + (l.facts && l.facts.length ? `<span class="cs-chips">${chips(FACT.filter(([key]) => l.facts.some((f) => f.key === key)).map(([key, label]) => [label, l.facts.find((f) => f.key === key).value]))}</span>` : "")
      + (l.detail ? `<span class="cs-detail">${esc(l.detail)}</span>` : "")
      + `</span>`
      + `<span class="cs-act">${l.call ? `<button class="btn ghost sm" data-csplay="${l.call}" title="Play the call">▶</button>` : ""}</span></div>`;
  }

  function drawTimeline() {
    const k = data.cases.find((x) => x.id === curId);
    $("csPick").style.display = k ? "none" : "";
    if (!k) { $("csTimeline").innerHTML = ""; $("csTitle").textContent = "Timeline"; $("csSub").textContent = ""; return; }
    $("csTitle").textContent = k.title;
    $("csSub").textContent = `Incident #${k.incident} · ${k.address || "address not heard"}${k.call_type && k.call_type !== k.title ? ` · dispatched as ${k.call_type}` : ""}`;
    $("csTimeline").innerHTML = `<div class="cs-head">${stateChip(k.state)}<span class="faint small">${esc(k.units.join(", "))}</span></div>`
      + `<div class="cs-facts">${facts(k)}</div>`
      + arrival(k)
      + (patient(k).length ? `<div class="cs-patient"><span class="cs-cap">From the hospital report</span><span class="cs-chips">${chips(patient(k))}</span></div>` : "")
      + `<div class="cs-lines">${k.lines.map(line).join("")}</div>`;
    $("csTimeline").querySelectorAll("[data-csplay]").forEach((b) => b.onclick = () => invoke("library_play", { id: +b.dataset.csplay }).catch((e) => uiToast(`${e}`, "err")));
  }

  function draw() {
    const open = data.cases.filter((k) => k.open).length;
    $("csMeta").textContent = data.cases.length ? `${data.cases.length} · ${open} open` : "";
    $("csEmpty").style.display = data.cases.length ? "none" : "";
    $("csCards").innerHTML = data.cases.map(card).join("");
    $("csCards").querySelectorAll("[data-case]").forEach((b) => b.onclick = () => { curId = +b.dataset.case; draw(); });
    if (curId == null && data.cases.length) curId = (data.cases.find((k) => k.open) || data.cases[0]).id;
    drawTimeline();
    $("csUnMeta").textContent = data.unplaced.length ? `${data.unplaced.length}` : "";
    $("csUnplaced").innerHTML = data.unplaced.length ? data.unplaced.map((u) =>
      `<div class="cs-line"><span class="cs-time mono">${esc(u.clock || hm(u.at))}</span><span class="cs-what"><span class="cs-top"><span class="cs-label">${esc(u.label)}</span> <span class="cs-src">${esc(SOURCE[u.source] || u.source)}</span></span><span class="cs-how">${esc(u.why)}</span><span class="cs-detail">${esc(u.detail)}</span></span>`
      + `<span class="cs-act">${u.call ? `<button class="btn ghost sm" data-csplay="${u.call}" title="Play the call">▶</button>` : ""}</span></div>`).join("")
      : '<div class="empty small">Everything heard was placed on a run.</div>';
    $("csUnplaced").querySelectorAll("[data-csplay]").forEach((b) => b.onclick = () => invoke("library_play", { id: +b.dataset.csplay }).catch((e) => uiToast(`${e}`, "err")));
  }

  async function load() {
    try {
      data = (await invoke("cases_list", { hours: +$("csHours").value })) || { cases: [], unplaced: [] };
      if (curId != null && !data.cases.some((k) => k.id === curId)) curId = null;
      draw();
    } catch (e) { uiToast(`${e}`, "err"); }
  }

  /* ---------- Telegram ---------- */
  const NOTIFY = [["working", "Working arrest"], ["rosc", "ROSC"], ["rearrest", "Lost pulses"], ["report", "Hospital report, or its ETA moving 3 min"], ["downgrade", "Not an arrest"], ["terminated", "Efforts ceased"]];
  let profile = null, dests = [];

  async function loadSend() {
    try {
      const s = (await invoke("cases_profiles")) || { profiles: [] };
      profile = (s.profiles || [])[0] || null;
      const a = await invoke("alerts_get");
      dests = (a && a.settings && a.settings.destinations) || [];
    } catch (e) { return; }
    drawSend();
  }

  function drawSend() {
    if (!profile || !$("csSendOn")) return;
    const t = profile.telegram || {};
    $("csSendOn").checked = !!t.enabled;
    $("csSendDest").innerHTML = `<option value="">— none —</option>` + dests.map((d) => `<option value="${esc(d.id)}" ${d.id === t.dest ? "selected" : ""}>${esc(d.name)}</option>`).join("")
      + (t.dest && !dests.some((d) => d.id === t.dest) ? `<option value="${esc(t.dest)}" selected>a destination that was removed</option>` : "");
    $("csSendHosp").checked = t.hospitals !== false;
    $("csSendMap").checked = t.map !== false;
    $("csSendAudio").checked = t.audio !== false;
    $("csNotify").innerHTML = NOTIFY.map(([k, label]) => `<label class="check"><input type="checkbox" data-notify="${k}" ${(t.notify || []).includes(k) ? "checked" : ""} /> ${esc(label)}</label>`).join("");
    $("csSendMeta").textContent = t.enabled ? "on" : "off";
  }

  function readSend() {
    return {
      enabled: $("csSendOn").checked,
      dest: $("csSendDest").value,
      hospitals: $("csSendHosp").checked,
      map: $("csSendMap").checked,
      audio: $("csSendAudio").checked,
      notify: [...$("csNotify").querySelectorAll("[data-notify]")].filter((i) => i.checked).map((i) => i.dataset.notify),
    };
  }

  async function saveSend() {
    if (!profile) return;
    const t = readSend();
    if (t.enabled && !(profile.telegram || {}).enabled && !(await uiConfirm("Cases will start sending to Telegram. Any tripwire that already announces arrests will keep sending too — turn those off once this looks right.", "Switch on"))) {
      $("csSendOn").checked = false; return;
    }
    try {
      const s = await invoke("cases_set_telegram", { profile: profile.id, telegram: t });
      profile = ((s && s.profiles) || []).find((p) => p.id === profile.id) || profile;
      drawSend();
      uiToast(t.enabled ? "Cases will be sent to Telegram" : "Saved — nothing is sent while it is off");
    } catch (e) { uiToast(`${e}`, "err"); }
  }

  async function preview() {
    if (!profile) return;
    const days = await uiAsk("Preview the cases already built over how many days? Nothing is sent. Save first to preview what you changed.", "3", "Preview");
    if (days == null || !(+days > 0)) return;
    try {
      const p = await invoke("cases_preview", { profile: profile.id, days: +days });
      if (!p) return;
      const rows = (p.days || []).map((d) => `<tr><td class="mono">${esc(d.date)}</td><td>${esc(d.dest)}</td><td class="mono">${d.threads}</td><td class="mono">${d.replies}</td></tr>`).join("");
      const sample = (p.cases || []).filter((k) => k.chats.length).slice(0, 5).map((k) =>
        `<details class="cs-sample"><summary>${esc(k.title)} · ${esc(k.address || "address not heard")} · ${esc(when(k.opened))} → ${esc(k.chats.join(", "))}</summary><pre>${esc(k.timeline)}</pre>${k.replies.length ? `<div class="lab small">Replies</div><pre>${esc(k.replies.join("\n"))}</pre>` : ""}</details>`).join("");
      $("csPreview").innerHTML = (p.warnings || []).map((w) => `<div class="cs-warn">${esc(w)}</div>`).join("")
        + (rows ? `<table class="cs-count"><thead><tr><th>Day</th><th>Chat</th><th>Cases</th><th>Replies</th></tr></thead><tbody>${rows}</tbody></table>` : '<div class="empty small">Nothing would have been sent.</div>')
        + sample;
    } catch (e) { uiToast(`${e}`, "err"); }
  }

  if ($("csSendSave")) {
    $("csSendSave").onclick = saveSend;
    $("csPreviewBtn").onclick = preview;
  }

  $("csHours").onchange = load;
  $("csRebuild").onclick = async () => {
    const days = await uiAsk("Build cases again from how many days of the library? Nothing is sent.", "3", "Rebuild");
    if (days == null || !(+days > 0)) return;
    const b = $("csRebuild"); b.disabled = true; b.textContent = "Rebuilding…";
    try {
      const r = (await invoke("cases_rebuild", { days: +days })) || { cases: 0, events: 0, inferred: 0, unplaced: 0 };
      uiToast(`${r.cases} cases, ${r.events} timeline lines, ${r.inferred} placed by inference, ${r.unplaced} not placed`);
      await load();
    } catch (e) { uiToast(`${e}`, "err"); }
    b.disabled = false; b.textContent = "Rebuild…";
  };
  if (listen) listen("cases", () => { if (!shown()) return; clearTimeout(timer); timer = setTimeout(load, 800); });
  // Keep "since dispatch" honest while the tab is open.
  setInterval(() => { if (shown() && data.cases.some((k) => k.open)) drawTimeline(); }, 30000);
  window.casesOnShow = () => { load(); loadSend(); };
})();
