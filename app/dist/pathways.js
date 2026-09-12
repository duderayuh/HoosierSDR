/* ============ care pathways: if this, then the closest one of these ======== */
// The list lives in the backend (pathways.json) and is read by the dispatch
// map, the incident popup and any tripwire, so there is one answer to "which
// hospital" rather than three that can disagree. First match wins, so the
// order is the meaning — hence the drag.
(() => {
  if (typeof invoke !== "function") return;
  let pw = null, rules = [], features = [], have = {}, sel = null;
  let drag = null, dragging = false;
  const uid = () => `pw${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`;
  const words = (v) => String(v || "").split(",").map((x) => x.trim()).filter(Boolean);
  const list = (a) => (a || []).join(", ");

  async function load() {
    try {
      const v = await invoke("pathways_get");
      pw = v.settings || { enabled: true, pathways: [] };
      rules = (pw.pathways || []).map((p) => ({ ...p, when: { ...p.when }, needs: (p.needs || []).map((n) => ({ ...n })) }));
      features = v.features || [];
      have = Object.fromEntries(v.have || []);
      render(v);
    } catch (e) { log(`pathways_get: ${e}`); }
  }

  function read() {
    return { ...pw, enabled: $("pwEnabled").checked, pathways: rules };
  }

  // A pathway that asks for something no hospital here can do would quietly
  // return nothing, so it is said out loud instead.
  function warnings(v) {
    const out = [];
    if (v && v.unplaced) out.push(`${v.unplaced} hospital${v.unplaced === 1 ? " has" : "s have"} no location yet, so ${v.unplaced === 1 ? "it" : "they"} can never be chosen — set it in <b>Places</b>.`);
    const asked = new Set();
    rules.forEach((p) => (p.needs || []).forEach((n) => (n.features || []).forEach((f) => asked.add(f.toLowerCase()))));
    const missing = [...asked].filter((f) => !(have[f] > 0));
    if (missing.length) out.push(`Nothing is marked <b>${missing.map(esc).join(", ")}</b>, so those needs will come back empty. Tick what your hospitals can do in <b>Places</b>.`);
    return out;
  }

  function render(v) {
    if (!pw) return;
    $("pwEnabled").checked = pw.enabled !== false;
    $("pwEmpty").style.display = rules.length ? "none" : "";
    $("pwMeta").textContent = rules.length ? `${rules.filter((p) => p.enabled !== false).length} of ${rules.length} on` : "none";
    const warn = warnings(v);
    $("pwWarn").style.display = warn.length ? "" : "none";
    $("pwWarn").innerHTML = warn.join("<br>");
    if (dragging) return;
    $("pwList").innerHTML = rules.map((p, i) => {
      const w = p.when || {};
      const open = sel === p.id;
      const when = [
        (w.call_types || []).length ? `type is ${esc(list(w.call_types))}` : "",
        (w.phrases || []).length ? `words ${esc(list(w.phrases))}` : "",
      ].filter(Boolean).join(" or ") || "anything not already matched";
      const not = [
        (w.except_types || []).length ? `not ${esc(list(w.except_types))}` : "",
        (w.except || []).length ? `not ${esc(list(w.except))}` : "",
      ].filter(Boolean).join(", ");
      return `<div class="pwcard ${open ? "on" : ""} ${p.enabled === false ? "off" : ""}" data-pw="${esc(p.id)}" draggable="true">
        <div class="pwtop">
          <span class="pwgrip" title="Drag to reorder">⠿</span>
          <span class="pwno" title="First match wins">${i + 1}</span>
          <b class="grow">${esc(p.name)}</b>
          <label class="tw-switch sm" title="On or off"><input type="checkbox" data-pwen="${esc(p.id)}" ${p.enabled === false ? "" : "checked"}><span></span></label>
          <button class="btn ghost sm" data-pwdel="${esc(p.id)}" title="Remove">✕</button>
        </div>
        <div class="pwsaid">if ${when}${not ? ` <span class="faint">(${not})</span>` : ""} → ${(p.needs || []).map((n) => esc(n.label || "?")).join(", ") || '<span class="bad">nothing</span>'}</div>
        ${open ? editor(p) : ""}
      </div>`;
    }).join("");
    wire();
  }

  function editor(p) {
    const w = p.when || {};
    return `<div class="pwedit">
      <div class="row2">
        <label class="field"><span class="lab">Name</span><input data-pwname value="${esc(p.name)}" spellcheck="false" /></label>
        <label class="field"><span class="lab">Dispatch types <span class="mono faint">comma separated · matched whole</span></span><input data-pwtypes value="${esc(list(w.call_types))}" spellcheck="false" placeholder="Cardiac Arrest" /></label>
      </div>
      <label class="field"><span class="lab">Or these words anywhere <span class="mono faint">in the type, the summary or what was said on the radio</span></span><input data-pwwords value="${esc(list(w.phrases))}" spellcheck="false" placeholder="cardiac arrest, working arrest, cpr in progress" /></label>
      <div class="row2">
        <label class="field"><span class="lab">Never these types <span class="mono faint">refused outright</span></span><input data-pwnottypes value="${esc(list(w.except_types))}" spellcheck="false" placeholder="Fire Alarm, Gas Odor" /></label>
        <label class="field"><span class="lab">Never these words <span class="mono faint">read in the type and summary only</span></span><input data-pwnot value="${esc(list(w.except))}" spellcheck="false" placeholder="cancelled" /></label>
      </div>
      <div class="lab" style="margin-top:6px">Then find the closest…</div>
      <div class="pwneeds">${(p.needs || []).map((n, j) => `
        <div class="pwneed">
          <input data-pwnl="${j}" value="${esc(n.label)}" spellcheck="false" placeholder="Closest hospital" />
          <span class="pwfeats">${features.map(([k, lbl]) => `<label class="check sm" title="${esc(lbl)}${have[k] ? "" : " — nothing is marked this"}"><input type="checkbox" data-pwnf="${j}:${esc(k)}" ${(n.features || []).some((f) => f.toLowerCase() === k) ? "checked" : ""}> ${esc(k)}${have[k] ? "" : " ⚠"}</label>`).join("")}</span>
          <button class="btn ghost sm" data-pwndel="${j}" title="Remove">✕</button>
        </div>`).join("")}</div>
      <p class="help">Ticking two means <b>both</b> — <span class="mono">ecmo</span> and <span class="mono">peds</span> is a pediatric ECMO centre, not either one. Nothing ticked means any hospital. Only places marked <b>Hospital</b> in Places are ever chosen.</p>
      <button class="btn ghost sm" data-pwnadd>+ Facility</button>
    </div>`;
  }

  function wire() {
    const box = $("pwList");
    box.querySelectorAll(".pwcard").forEach((c) => c.onclick = (e) => {
      if (e.target.closest(".pwedit") || e.target.closest(".tw-switch") || e.target.closest("button") || e.target.closest(".pwgrip")) return;
      sel = sel === c.dataset.pw ? null : c.dataset.pw;
      render();
    });
    box.querySelectorAll("input[data-pwen]").forEach((x) => x.onchange = () => {
      const p = rules.find((r) => r.id === x.dataset.pwen); if (p) p.enabled = x.checked;
      render();
    });
    box.querySelectorAll("[data-pwdel]").forEach((b) => b.onclick = async () => {
      const p = rules.find((r) => r.id === b.dataset.pwdel); if (!p) return;
      if (!await uiConfirm(`Remove the “${p.name}” pathway?`, "Remove")) return;
      rules = rules.filter((r) => r.id !== p.id); if (sel === p.id) sel = null;
      render();
    });
    const cur = () => rules.find((r) => r.id === sel);
    const bind = (attr, set) => box.querySelectorAll(`[${attr}]`).forEach((x) => x.onchange = () => { const p = cur(); if (p) { set(p, x); render(); } });
    bind("data-pwname", (p, x) => { p.name = x.value.trim() || p.name; });
    bind("data-pwtypes", (p, x) => { p.when.call_types = words(x.value); });
    bind("data-pwwords", (p, x) => { p.when.phrases = words(x.value); });
    bind("data-pwnottypes", (p, x) => { p.when.except_types = words(x.value); });
    bind("data-pwnot", (p, x) => { p.when.except = words(x.value); });
    box.querySelectorAll("[data-pwnl]").forEach((x) => x.onchange = () => {
      const p = cur(); if (p) p.needs[+x.dataset.pwnl].label = x.value.trim();
    });
    box.querySelectorAll("[data-pwnf]").forEach((x) => x.onchange = () => {
      const p = cur(); if (!p) return;
      const [j, key] = x.dataset.pwnf.split(":");
      const n = p.needs[+j]; n.features = n.features || [];
      n.features = x.checked
        ? [...n.features.filter((f) => f.toLowerCase() !== key), key]
        : n.features.filter((f) => f.toLowerCase() !== key);
      render();
    });
    box.querySelectorAll("[data-pwndel]").forEach((b) => b.onclick = () => {
      const p = cur(); if (!p) return;
      p.needs.splice(+b.dataset.pwndel, 1); render();
    });
    box.querySelectorAll("[data-pwnadd]").forEach((b) => b.onclick = () => {
      const p = cur(); if (!p) return;
      p.needs = p.needs || []; p.needs.push({ label: "Closest hospital", features: [] });
      render();
    });
    wireDrag(box);
  }

  // Order is meaning here, so the drag is the important control: the same
  // gesture as the tripwire list, over the rules array.
  function wireDrag(box) {
    const rows = [...box.querySelectorAll(".pwcard")];
    rows.forEach((el) => {
      const id = el.dataset.pw;
      el.addEventListener("dragstart", (e) => {
        drag = id; dragging = true; el.classList.add("dragging");
        e.dataTransfer.effectAllowed = "move"; e.dataTransfer.setData("text/plain", id);
      });
      el.addEventListener("dragend", () => { el.classList.remove("dragging"); drag = null; dragging = false; render(); });
      el.addEventListener("dragover", (e) => {
        if (!drag || drag === id) return;
        e.preventDefault(); e.dataTransfer.dropEffect = "move";
        const r = el.getBoundingClientRect();
        el.dataset.drop = e.clientY < r.top + r.height / 2 ? "before" : "after";
        rows.forEach((o) => { if (o !== el) delete o.dataset.drop; });
      });
      el.addEventListener("dragleave", () => delete el.dataset.drop);
      el.addEventListener("drop", (e) => {
        e.preventDefault();
        const where = el.dataset.drop || "after";
        rows.forEach((o) => delete o.dataset.drop);
        const from = rules.findIndex((r) => r.id === drag);
        if (from < 0) return;
        const me = rules[from];
        rules.splice(from, 1);
        let to = rules.findIndex((r) => r.id === id) + (where === "before" ? 0 : 1);
        rules.splice(Math.max(0, Math.min(to, rules.length)), 0, me);
        dragging = false; render();
      });
    });
  }

  $("pwAdd").onclick = () => {
    const p = { id: uid(), name: "New pathway", enabled: true, when: { call_types: [], phrases: [], except: [], except_types: [] }, needs: [{ label: "Closest hospital", features: [] }] };
    // Above the catch-all, since a pathway below it would never be reached.
    const catchAll = rules.findIndex((r) => !(r.when.call_types || []).length && !(r.when.phrases || []).length);
    if (catchAll >= 0) rules.splice(catchAll, 0, p); else rules.push(p);
    sel = p.id; render();
  };

  $("pwSave").onclick = async () => {
    try {
      pw = await invoke("pathways_set", { settings: read() });
      rules = (pw.pathways || []).map((p) => ({ ...p, when: { ...p.when }, needs: (p.needs || []).map((n) => ({ ...n })) }));
      render(); uiToast("Care pathways saved.");
    } catch (e) { uiToast(`Could not save: ${e}`, "err"); }
  };

  $("pwReset").onclick = async () => {
    if (!await uiConfirm("Put back the standard set of pathways? Anything you have written here is replaced.", "Start over")) return;
    try { pw = await invoke("pathways_reset"); await load(); uiToast("The standard pathways are back."); }
    catch (e) { uiToast(`${e}`, "err"); }
  };

  // The same "against my own data" check the tripwire preview gives: a
  // pathway that never matches anything is worse than no pathway.
  $("pwPreview").onclick = async () => {
    const out = $("pwPreviewOut");
    out.style.display = ""; out.innerHTML = `<span class="faint">Reading the runs already recorded…</span>`;
    try {
      const p = await invoke("pathways_preview", { settings: read() });
      const rowsHtml = (p.per_pathway || []).map(([name, n]) =>
        `<div class="pwrow"><span class="grow">${esc(name)}</span><b class="${n ? "" : "faint"}">${n}</b></div>`).join("");
      const missed = (p.missed_types || []).map(([t, n]) => `${esc(t)} ${n}`).join(" · ");
      out.innerHTML = `<div class="small"><b>${p.incidents}</b> runs recorded, ${p.placed} of them placed on the map.</div>
        ${rowsHtml}
        <div class="pwrow"><span class="grow faint">no pathway</span><b class="${p.unmatched ? "bad" : ""}">${p.unmatched}</b></div>
        ${missed ? `<div class="small faint" style="margin-top:4px">Not matched, by type: ${missed}</div>` : ""}
        ${(p.samples || []).length ? `<div class="small faint" style="margin-top:4px">${p.samples.map(esc).join("<br>")}</div>` : ""}
        <div class="small faint" style="margin-top:4px">A pathway showing 0 is not wrong — it may be waiting for the run that needs it.</div>`;
    } catch (e) { out.innerHTML = `<span class="bad">${esc(String(e))}</span>`; }
  };

  window.pathwaysLoad = load;
  load();
})();
