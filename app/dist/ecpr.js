/* ================= Settings → ECPR: the whole process, in one place ================= */
// Everything about the ED-ECPR process that used to be constants in code:
// which tripwire is the screen (and the names of the fields research reads
// off it), the score's criteria, estimate table and age exclusion, the word
// lists that read a report's "unwitnessed" as no, and the score line in the
// case message. The backend keeps it in ecpr.json (see ecpr.rs); this page
// edits a draft and scores a made-up report by that draft as it is typed,
// so a change can be seen before it is saved.
// Loaded after app.js and shares its helpers ($, esc, invoke, uiToast).
(() => {
  if (typeof invoke !== "function") return;
  const KINDS = [["time", "time: age + minutes < limit"], ["yes_no", "yes / no"], ["no_disease", "no end-stage disease"]];
  const UNSTATED = [["unknown", "unknown (widens the range)"], ["met", "count it met"], ["not met", "count it not met"]];
  // What the try-it box starts with: a report every criterion can read.
  const SAMPLE = { age: "52", downtime: "about 20 minutes", witnessed: "yes", "bystander cpr": "yes", history: "hypertension" };
  let view = null, s = null, saved = "", tryVals = { ...SAMPLE }, tryTimer = null, trySeq = 0, lastScore = null;
  const clone = (x) => JSON.parse(JSON.stringify(x));
  const lines = (v) => String(v || "").split(/\n+/).map((x) => x.trim()).filter(Boolean);
  const int = (v, d) => { const n = parseInt(String(v).trim(), 10); return Number.isFinite(n) ? n : d; };
  const num = (v, d) => { const n = parseFloat(String(v).trim()); return Number.isFinite(n) ? n : d; };
  const onCount = () => (s.score.criteria || []).filter((c) => c.enabled).length;

  /* ---------- dirty ---------- */
  const stable = (x) => JSON.stringify(x);
  function markDirty() { $("ecDirty").textContent = s && stable(s) !== saved ? "unsaved changes" : ""; }
  // Every edit: note it, re-score the sample, and redraw what depends on it.
  function changed(redraw) { markDirty(); if (redraw) fill(); scheduleTry(); }

  /* ---------- the screen ---------- */
  function fillScreen() {
    const sel = $("ecScreen");
    const cand = s.screen.candidate || "candidate";
    const opts = [`<option value="">the first extraction that returns “${esc(cand)}”</option>`]
      .concat((view.screens || []).map((t) => `<option value="${esc(t.id)}">${esc(t.name)}${t.enabled ? "" : " (off)"}${t.keys.includes(cand) ? "" : ` — no “${esc(cand)}” field`}</option>`));
    // A saved id whose tripwire is gone still shows, so it can be seen and changed.
    if (s.screen_tripwire && !(view.screens || []).some((t) => t.id === s.screen_tripwire)) opts.push(`<option value="${esc(s.screen_tripwire)}">a removed tripwire (${esc(s.screen_tripwire)})</option>`);
    sel.innerHTML = opts.join("");
    sel.value = s.screen_tripwire || "";
    const picked = (view.screens || []).find((t) => t.id === s.screen_tripwire);
    const first = (view.screens || []).find((t) => t.keys.includes(cand));
    const target = picked || (s.screen_tripwire ? null : first);
    $("ecOpenScreen").disabled = !target;
    $("ecOpenScreen").dataset.id = target ? target.id : "";
    const n = (view.screens || []).length;
    $("ecScreenMeta").textContent = n ? `${n} extraction tripwire${n === 1 ? "" : "s"}` : "no extraction tripwire yet";
    $("ecScreenNote").textContent = !n ? "Nothing extracts fields yet — add the starter, then pick its talkgroups and destination and switch it on."
      : s.screen_tripwire && !picked ? "That tripwire is gone; pick another or leave it blank."
      : target && !target.enabled ? `“${target.name}” is switched off, so nobody is being paged.`
      : target && !target.keys.includes(cand) ? `“${target.name}” does not return a “${cand}” field; research will read blanks off it.`
      : target ? `“${target.name}” is the screen.` : "";
    $("ecFCandidate").value = s.screen.candidate; $("ecFMet").value = s.screen.criteria_met; $("ecFPct").value = s.screen.likelihood_pct; $("ecFReason").value = s.screen.reason;
  }
  $("ecScreen").onchange = () => { s.screen_tripwire = $("ecScreen").value; changed(true); };
  for (const [id, key] of [["ecFCandidate", "candidate"], ["ecFMet", "criteria_met"], ["ecFPct", "likelihood_pct"], ["ecFReason", "reason"]]) {
    $(id).oninput = () => { s.screen[key] = $(id).value.trim(); markDirty(); };
    $(id).onchange = () => fillScreen();
  }
  $("ecOpenScreen").onclick = () => { const id = $("ecOpenScreen").dataset.id; if (id && typeof window.tripwireOpen === "function") window.tripwireOpen(id); };
  $("ecCreateScreen").onclick = async () => {
    if (s && stable(s) !== saved && !(await uiConfirm("Save the changes on this page first? Adding the screen saves which tripwire is the screen.", "Save and add"))) return;
    if (s && stable(s) !== saved && !(await save())) return;
    try {
      const id = await invoke("ecpr_screen_create");
      await load();
      uiToast("Starter screen added, switched off — pick its talkgroups and destination, then switch it on");
      if (typeof window.tripwireOpen === "function") window.tripwireOpen(id);
    } catch (e) { uiToast(`${e}`, "err"); }
  };

  /* ---------- the score ---------- */
  function fillCriteria() {
    const crit = s.score.criteria || [];
    const head = `<div class="eccrit echead"><span></span><span>Label</span><span>Kind</span><span>Reads the fact</span><span>Limit</span><span>If not stated</span><span></span></div>`;
    $("ecCriteria").innerHTML = head + crit.map((c, i) => `<div class="eccrit${c.enabled ? "" : " off"}" data-ci="${i}">
      <input type="checkbox" data-on="${i}" ${c.enabled ? "checked" : ""} title="On or off — a criterion that is off is not counted">
      <input type="text" data-label="${i}" value="${esc(c.label)}" placeholder="Witnessed arrest" spellcheck="false">
      <select data-kind="${i}">${KINDS.map(([v, l]) => `<option value="${v}" ${c.kind === v ? "selected" : ""}>${l}</option>`).join("")}</select>
      <input type="text" data-fact="${i}" value="${esc(c.fact)}" list="ecFactList" placeholder="${c.kind === "time" ? "downtime" : "witnessed"}" spellcheck="false" title="${c.kind === "time" ? "The fact holding the minutes; the age comes from the age fact below" : "The fact key in the report's FACTS block"}">
      <input type="text" class="eclimit" data-limit="${i}" value="${c.kind === "time" ? c.limit : ""}" inputmode="numeric" ${c.kind === "time" ? "" : "disabled"} title="Age + minutes must come in under this">
      <select data-unstated="${i}">${UNSTATED.map(([v, l]) => `<option value="${v}" ${c.unstated === v ? "selected" : ""}>${l}</option>`).join("")}</select>
      <span class="inline"><span class="ecmove"><button class="btn ghost sm" data-up="${i}" title="Move up" ${i === 0 ? "disabled" : ""}>↑</button><button class="btn ghost sm" data-down="${i}" title="Move down" ${i === crit.length - 1 ? "disabled" : ""}>↓</button></span><button class="btn ghost sm ecdel" data-del="${i}" title="Remove">✕</button></span>
    </div>`).join("");
    const box = $("ecCriteria");
    const row = (i) => crit[+i] || {};
    box.querySelectorAll("[data-on]").forEach((x) => x.onchange = () => { row(x.dataset.on).enabled = x.checked; resizeBands(); changed(true); });
    box.querySelectorAll("[data-label]").forEach((x) => { x.oninput = () => { row(x.dataset.label).label = x.value; markDirty(); }; x.onchange = () => scheduleTry(); });
    box.querySelectorAll("[data-kind]").forEach((x) => x.onchange = () => { row(x.dataset.kind).kind = x.value; changed(true); });
    box.querySelectorAll("[data-fact]").forEach((x) => { x.oninput = () => { row(x.dataset.fact).fact = x.value.trim().toLowerCase(); markDirty(); }; x.onchange = () => { fillTry(); scheduleTry(); }; });
    box.querySelectorAll("[data-limit]").forEach((x) => x.onchange = () => { row(x.dataset.limit).limit = Math.max(1, int(x.value, 100)); changed(false); });
    box.querySelectorAll("[data-unstated]").forEach((x) => x.onchange = () => { row(x.dataset.unstated).unstated = x.value; changed(false); });
    // A handler from a row drawn before a remove holds an index that may no
    // longer exist; it does nothing rather than leaving a hole in the list.
    box.querySelectorAll("[data-del]").forEach((x) => x.onclick = () => { const i = +x.dataset.del; if (!crit[i]) return; crit.splice(i, 1); resizeBands(); changed(true); });
    const swap = (i, j) => { if (!crit[i] || !crit[j]) return; [crit[i], crit[j]] = [crit[j], crit[i]]; changed(true); };
    box.querySelectorAll("[data-up]").forEach((x) => x.onclick = () => swap(+x.dataset.up, +x.dataset.up - 1));
    box.querySelectorAll("[data-down]").forEach((x) => x.onclick = () => swap(+x.dataset.down, +x.dataset.down + 1));
    $("ecScoreMeta").textContent = `${onCount()} of ${crit.length} criteria on`;
  }
  $("ecCritAdd").onclick = () => {
    s.score.criteria.push({ key: "", label: "", kind: "yes_no", fact: "", limit: 100, unstated: "unknown", enabled: true });
    resizeBands(); changed(true);
    const last = $("ecCriteria").querySelector(`[data-label="${s.score.criteria.length - 1}"]`); if (last) last.focus();
  };
  // One band per possible count of criteria met: the table follows the
  // criteria, padded with its last row and never left short.
  function resizeBands() {
    const n = onCount() + 1; const b = s.score.bands || (s.score.bands = []);
    if (!b.length) b.push([0, 5]);
    while (b.length < n) b.push([...b[b.length - 1]]);
    b.length = n;
  }
  function fillBands() {
    resizeBands();
    const b = s.score.bands, n = onCount();
    $("ecBands").innerHTML = b.map((row, i) => `<div class="ecband"><span class="k">${i} of ${n} met</span>
      <input type="text" data-lo="${i}" value="${row[0]}" inputmode="decimal" title="Low"><span class="faint">to</span><input type="text" data-hi="${i}" value="${row[1]}" inputmode="decimal" title="High">
      <span class="ecbar" title="${row[0]}–${row[1]}%"><span style="left:${Math.min(100, Math.max(0, row[0]))}%;width:${Math.max(0, Math.min(100, row[1]) - Math.max(0, row[0]))}%"></span></span></div>`).join("");
    const box = $("ecBands");
    box.querySelectorAll("[data-lo]").forEach((x) => x.onchange = () => { b[+x.dataset.lo][0] = Math.min(100, Math.max(0, num(x.value, 0))); changed(true); });
    box.querySelectorAll("[data-hi]").forEach((x) => x.onchange = () => { b[+x.dataset.hi][1] = Math.min(100, Math.max(0, num(x.value, 0))); changed(true); });
  }
  $("ecName").oninput = () => { s.score.name = $("ecName").value; markDirty(); };
  $("ecName").onchange = () => scheduleTry();
  $("ecAgeFact").onchange = () => { s.score.age_fact = $("ecAgeFact").value.trim().toLowerCase() || "age"; fillTry(); changed(false); };
  $("ecMaxAge").onchange = () => { s.score.max_age = Math.max(0, int($("ecMaxAge").value, 0)); changed(false); };
  $("ecNote").onchange = () => { s.score.not_stated_note = $("ecNote").checked; changed(false); };

  /* ---------- the words ---------- */
  const WORDS = [["ecWUnstated", "unstated_words"], ["ecWEnd", "end_stage_words"], ["ecWNo", "no_words"], ["ecWNoP", "no_phrases"], ["ecWYes", "yes_words"], ["ecWYesP", "yes_phrases"]];
  for (const [id, key] of WORDS) {
    $(id).oninput = () => { s.score[key] = lines($(id).value); markDirty(); };
    $(id).onchange = () => scheduleTry();
  }

  /* ---------- the case message ---------- */
  $("ecCaseLine").onchange = () => { s.case_line = $("ecCaseLine").checked; changed(false); renderCasePrev(); };
  $("ecPrefix").oninput = () => { s.case_prefix = $("ecPrefix").value.trim(); markDirty(); renderCasePrev(); };
  function renderCasePrev() {
    if (!s.case_line) { $("ecCasePrev").innerHTML = `<span class="faint">The score is left out of case messages.</span>`; return; }
    const sc = lastScore;
    const lead = s.case_prefix ? s.case_prefix + " " : "";
    // Under the facts line the ED sees, as the case sender lays it out.
    $("ecCasePrev").innerHTML = `🩺 Witnessed: <b>yes</b> · Bystander CPR: <b>yes</b> · Downtime (as said): <b>about 20 minutes</b>\n${esc(lead)}${esc(sc ? sc.name : s.score.name)}: <b>${esc(sc ? sc.estimate : "…")}</b>`;
  }

  /* ---------- try it ---------- */
  // The facts worth a box: the age, and whatever the criteria read.
  function tryKeys() {
    const keys = [s.score.age_fact || "age"];
    for (const c of s.score.criteria || []) if (c.enabled && c.fact && !keys.includes(c.fact)) keys.push(c.fact);
    return keys;
  }
  function fillTry() {
    const keys = tryKeys();
    $("ecTryFacts").innerHTML = keys.map((k) => `<label class="ectryfact"><span class="k">${esc(k)}</span><input type="text" data-tk="${esc(k)}" value="${esc(tryVals[k] ?? "")}" placeholder="not stated" spellcheck="false"></label>`).join("");
    $("ecTryFacts").querySelectorAll("[data-tk]").forEach((x) => x.oninput = () => { tryVals[x.dataset.tk] = x.value; scheduleTry(); });
  }
  function scheduleTry() { clearTimeout(tryTimer); tryTimer = setTimeout(runTry, 250); }
  async function runTry() {
    if (!s) return;
    const seq = ++trySeq;
    const facts = {}; for (const k of tryKeys()) if ((tryVals[k] || "").trim()) facts[k] = tryVals[k].trim();
    let sc; try { sc = await invoke("ecpr_try", { settings: s, facts }); } catch (e) { $("ecTryOut").innerHTML = `<span class="bad">${esc(String(e))}</span>`; return; }
    if (seq !== trySeq || !sc) return;
    lastScore = sc;
    const v = (c) => c.verdict === "not met" ? "not" : c.verdict;
    $("ecTryOut").innerHTML = `<div class="est">${esc(sc.name)}: ${esc(sc.estimate)}</div>` +
      (sc.excluded ? `<div class="excl">Hard exclusion — ${esc(sc.excluded)}</div>` : "") +
      (sc.criteria || []).map((c) => `<div><span class="verdict ${v(c)}">${esc(c.verdict)}</span>${esc(c.label)} <span class="faint">— ${esc(c.why)}</span></div>`).join("") +
      `<div class="faint" style="margin-top:4px">${sc.met} met (${sc.assumed} assumed) · ${sc.unknown} unknown · ${sc.complete ? "every input stated" : "some inputs not stated"}</div>`;
    $("ecTryMeta").textContent = sc.estimate;
    renderCasePrev();
  }

  /* ---------- fill / load / save ---------- */
  function fill() {
    if (!s || !view) return;
    $("ecFactList").innerHTML = (view.facts || []).map((k) => `<option value="${esc(k)}">`).join("");
    fillScreen();
    $("ecName").value = s.score.name; $("ecAgeFact").value = s.score.age_fact; $("ecMaxAge").value = s.score.max_age; $("ecNote").checked = !!s.score.not_stated_note;
    fillCriteria(); fillBands();
    for (const [id, key] of WORDS) $(id).value = (s.score[key] || []).join("\n");
    $("ecCaseLine").checked = !!s.case_line; $("ecPrefix").value = s.case_prefix || "";
    fillTry(); renderCasePrev(); markDirty();
    $("ecMeta").textContent = `${onCount()} criteria · ${s.score.name}`;
  }
  async function load() {
    try { view = await invoke("ecpr_get"); } catch (e) { log(`ecpr_get: ${e}`); return; }
    if (!view) return;
    s = clone(view.settings); saved = stable(s);
    fill(); scheduleTry();
  }
  async function save() {
    try { view = await invoke("ecpr_set", { settings: s }); s = clone(view.settings); saved = stable(s); fill(); scheduleTry(); return true; }
    catch (e) { uiToast(`Could not save: ${e}`, "err"); return false; }
  }
  $("ecSave").onclick = async () => { if (await save()) uiToast("ECPR settings saved — the next case message and research row use them"); };
  $("ecReset").onclick = async () => {
    if (!(await uiConfirm("Put every ECPR setting back to the program's ED-ECPR gate? Nothing changes until you Save.", "Reset"))) return;
    try { s = await invoke("ecpr_defaults"); } catch (e) { uiToast(`${e}`, "err"); return; }
    fill(); scheduleTry();
  };
  // Coming to the page refreshes the tripwire list, which may have changed
  // on Tripwires; an unsaved draft is kept.
  window.ecprOnShow = async () => {
    if (s && stable(s) !== saved) { try { const v = await invoke("ecpr_get"); if (v) { view.screens = v.screens; fillScreen(); } } catch (_) {} return; }
    load();
  };
  if (typeof listen === "function") listen("tripwires", () => { if (typeof settingsPageVisible === "function" && settingsPageVisible("ecpr")) window.ecprOnShow(); });
  load();
})();
