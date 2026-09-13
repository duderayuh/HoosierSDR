/* Dashboards: boards the listener composes out of panes.
 *
 * Two jobs in one file — drawing a board, and editing one. Matching happens
 * here rather than in Rust: the page already holds every incident it has been
 * told about, so an `incident` event re-renders from memory with no round
 * trip, which is what lets a board sit on a wall and keep up.
 */
(() => {
  if (typeof invoke !== "function") return;

  let cfg = null;              // the whole View from dashboards_get
  let boards = [];             // working copy, edited in place
  let places = [];             // enabled places, whole
  let curId = null;            // board on screen
  let sel = null;              // board open in the editor
  const inc = new Map();       // incidents by id
  const reports = new Map();   // stored conversations by id
  let loaded = false;

  const MI = 1609.344;
  const shown = () => $("view-dashboard").style.display !== "none";
  const board = () => boards.find((b) => b.id === curId) || boards[0] || null;

  /* ---------- matching ---------- */

  // Lower-case, punctuation to spaces, single-spaced — the same shape as the
  // Rust `alerts::normalize`, so a phrase typed here behaves as it does in a
  // tripwire.
  const norm = (s) => (" " + String(s || "").toLowerCase().replace(/[^a-z0-9]+/g, " ") + " ").replace(/\s+/g, " ");
  const hasWord = (hay, words) => words.some((w) => { const n = norm(w).trim(); return n && hay.includes(" " + n + " "); });
  const sameType = (a, b) => String(a || "").trim().toLowerCase() === String(b || "").trim().toLowerCase();

  function haversineM(a, b) {
    const R = 6371000, r = Math.PI / 180;
    const dLat = (b[0] - a[0]) * r, dLon = (b[1] - a[1]) * r;
    const s = Math.sin(dLat / 2) ** 2 + Math.cos(a[0] * r) * Math.cos(b[0] * r) * Math.sin(dLon / 2) ** 2;
    return 2 * R * Math.asin(Math.min(1, Math.sqrt(s)));
  }
  const placeById = (id) => places.find((p) => p.id === id) || null;
  const located = (p) => p && p.lat != null && p.lon != null;
  const hospitals = () => places.filter((p) => p.kind === "hospital" && located(p));

  // The hospital an incident is closest to, as the crow flies. Deliberately
  // not a drive time: a road route for every incident against every hospital
  // on every render is a lot of asking, and the answer to "whose patch is
  // this" does not turn on a minute either way.
  function closestHospital(i) {
    if (i.lat == null || i.lon == null) return null;
    let best = null, bestM = Infinity;
    for (const h of hospitals()) {
      const m = haversineM([i.lat, i.lon], [h.lat, h.lon]);
      if (m < bestM) { bestM = m; best = h; }
    }
    return best ? { place: best, meters: bestM } : null;
  }

  // Drive time is only ever shown when it was actually measured: the pathway
  // resolver stores a road route on the incident for the facilities it picked.
  // Anything else gets a distance and no minutes, rather than a guess.
  function roadTo(i, placeId) {
    const t = (i.targets || []).find((t) => t.place_id === placeId && t.how === "road" && t.secs > 0);
    return t ? Math.max(1, Math.round(t.secs / 60)) : null;
  }

  function incHay(i) {
    return norm([i.call_type, i.summary, i.address, (i.units || []).join(" ")].join(" "));
  }

  function matches(p, i) {
    const hay = incHay(i);
    if (p.except && p.except.length && hasWord(hay, p.except)) return false;
    if (p.call_types && p.call_types.length && !p.call_types.some((t) => sameType(t, i.call_type))) return false;
    if (p.phrases && p.phrases.length && !hasWord(hay, p.phrases)) return false;
    if (p.closest_place) {
      const c = closestHospital(i);
      if (!c || c.place.id !== p.closest_place) return false;
    }
    if (p.within_place && p.within_miles > 0) {
      const w = placeById(p.within_place);
      if (!located(w) || i.lat == null || i.lon == null) return false;
      if (haversineM([i.lat, i.lon], [w.lat, w.lon]) > p.within_miles * MI) return false;
    }
    return true;
  }

  // First rule wins, so the loudest emphasis goes at the top of the list.
  function emphasisFor(p, hay, callType) {
    for (const e of p.emphasis || []) {
      const w = e.when || {};
      if ((w.except_types || []).some((t) => sameType(t, callType))) continue;
      if ((w.except || []).length && hasWord(hay, w.except)) continue;
      const byType = (w.call_types || []).some((t) => sameType(t, callType));
      const byWord = (w.phrases || []).length && hasWord(hay, w.phrases);
      if (byType || byWord) return e;
    }
    return null;
  }

  /* ---------- rendering ---------- */

  const ago = (t) => {
    const s = Math.max(0, Math.floor(Date.now() / 1000) - t);
    if (s < 60) return s + "s ago";
    if (s < 3600) return Math.floor(s / 60) + " min ago";
    if (s < 86400) return Math.floor(s / 3600) + " h ago";
    return Math.floor(s / 86400) + " d ago";
  };

  function dispatchRows(p) {
    const out = [];
    for (const i of inc.values()) if (matches(p, i)) out.push(i);
    out.sort((a, b) => b.updated - a.updated);       // latest at the top
    return out.slice(0, p.limit || 25);
  }

  function reportRows(p) {
    const pl = placeById(p.place);
    const tgs = new Set((pl && pl.tgs) || []);
    const out = [];
    for (const r of reports.values()) if (tgs.has(r.tg)) out.push(r);
    out.sort((a, b) => b.last_at - a.last_at);
    return out.slice(0, p.limit || 25);
  }

  function dispatchCard(p, i) {
    const hay = incHay(i);
    const em = emphasisFor(p, hay, i.call_type);
    const bits = [];
    if (p.closest_place || p.within_place) {
      const ref = placeById(p.within_place || p.closest_place);
      if (located(ref) && i.lat != null && i.lon != null) {
        const mi = haversineM([i.lat, i.lon], [ref.lat, ref.lon]) / MI;
        const min = roadTo(i, ref.id);
        bits.push(`<span class="dbdist">${mi.toFixed(1)} mi</span>`);
        if (min != null) bits.push(`<span class="dbdist">${min} min</span>`);
      }
    }
    return `<article class="dbcard${em ? " em-" + esc(em.style) : ""}">
      <div class="dbcardhead">
        <span class="dbtitle">${esc(i.emoji || "")} ${esc(i.call_type || "Unknown")}</span>
        ${em && em.note ? `<span class="dbtag">${esc(em.note)}</span>` : ""}
        <span class="spacer"></span>
        <span class="dbid mono">#${i.id}</span>
      </div>
      <div class="dbmeta mono faint">
        <span class="ago" data-t="${i.updated}">${ago(i.updated)}</span>
        ${i.address ? " · " + esc(i.address) : ""}
        ${bits.length ? " · " + bits.join(" · ") : ""}
      </div>
      ${i.summary ? `<div class="dbbody">${esc(i.summary)}</div>` : ""}
    </article>`;
  }

  function reportCard(p, r) {
    const hay = norm([r.headline, r.summary, r.tg_name, (r.units || []).join(" ")].join(" "));
    const em = emphasisFor(p, hay, "");
    // Rows stored before headlines existed have none; the summary still reads
    // on its own, so the card simply loses its title rather than its meaning.
    const title = r.headline || (r.units || []).join(", ") || r.tg_name || "Report";
    return `<article class="dbcard${em ? " em-" + esc(em.style) : ""}">
      <div class="dbcardhead">
        <span class="dbtitle">${esc(title)}</span>
        ${em && em.note ? `<span class="dbtag">${esc(em.note)}</span>` : ""}
        <span class="spacer"></span>
        <span class="dbid mono">#${r.id}</span>
      </div>
      <div class="dbmeta mono faint">
        <span class="ago" data-t="${r.last_at}">${ago(r.last_at)}</span>
        ${r.units && r.units.length ? " · " + esc(r.units.join(", ")) : ""}
      </div>
      ${r.summary ? `<div class="dbbody">${esc(r.summary)}</div>` : ""}
    </article>`;
  }

  function paneHtml(p) {
    const isRep = p.kind === "reports";
    const rows = isRep ? reportRows(p) : dispatchRows(p);
    const sub = isRep ? (placeById(p.place) || {}).name || "" :
      (p.closest_place ? "closest to " + ((placeById(p.closest_place) || {}).name || "") : "");
    const cards = rows.map((r) => (isRep ? reportCard(p, r) : dispatchCard(p, r))).join("");
    return `<section class="dbpane" style="flex:${Math.max(1, p.width || 1)}">
      <div class="dbpanehead">
        <span class="eyebrow">${esc(p.title || (isRep ? "Reports" : "Dispatch"))}</span>
        ${sub ? `<span class="faint">${esc(sub)}</span>` : ""}
        <span class="spacer"></span>
        <span class="mono faint">${rows.length}</span>
      </div>
      <div class="dbpanebody">${cards || `<div class="empty small">Nothing matching yet.</div>`}</div>
    </section>`;
  }

  function render() {
    if (!shown()) return;
    const live = boards.filter((b) => b.enabled !== false);
    $("dbPick").innerHTML = live.map((b) =>
      `<button data-b="${esc(b.id)}"${b.id === curId ? ' aria-pressed="true"' : ""}>${esc(b.name)}</button>`).join("");
    $("dbPick").querySelectorAll("button").forEach((x) => x.onclick = () => { curId = x.dataset.b; render(); });
    const b = board();
    $("dbEmpty").style.display = live.length ? "none" : "";
    $("dbPanes").innerHTML = b ? (b.panes || []).map(paneHtml).join("") : "";
    $("dbFooter").style.display = b && b.footer ? "" : "none";
    if (b && b.footer) $("dbFooter").textContent = b.footer;
    $("dbClock").textContent = new Date().toLocaleTimeString();
  }

  /* ---------- data ---------- */

  async function loadIncidents() {
    try {
      const list = await invoke("incidents_list", { since: 0, limit: 2000 });
      inc.clear();
      (list || []).forEach((i) => inc.set(i.id, i));
    } catch (e) { log(`dashboards incidents: ${e}`); }
  }
  async function loadReports() {
    try {
      const rs = await invoke("conversations_list", { limit: 300 });
      reports.clear();
      (rs || []).forEach((r) => reports.set(r.id, r));
    } catch (e) { log(`dashboards reports: ${e}`); }
  }

  async function loadCfg() {
    const v = await invoke("dashboards_get");
    cfg = v;
    boards = JSON.parse(JSON.stringify(v.settings.dashboards || []));
    places = v.places || [];
    if (!curId || !boards.some((b) => b.id === curId)) curId = (boards[0] || {}).id || null;
  }

  async function show() {
    if (!loaded) { loaded = true; await loadCfg(); await loadIncidents(); await loadReports(); }
    render();
  }

  /* ---------- editor ---------- */

  const styleOpts = (cur) => (cfg.styles || []).map(([k, label]) =>
    `<option value="${esc(k)}"${k === cur ? " selected" : ""}>${esc(label)}</option>`).join("");
  const placeOpts = (cur, onlyHosp) => `<option value="">— none —</option>` +
    places.filter((p) => !onlyHosp || p.kind === "hospital")
      .map((p) => `<option value="${esc(p.id)}"${p.id === cur ? " selected" : ""}>${esc(p.name)}</option>`).join("");
  const words = (v) => (v || []).join(", ");
  const parseWords = (s) => String(s || "").split(",").map((x) => x.trim()).filter(Boolean);

  function paneEditor(b, p, j) {
    const isRep = p.kind === "reports";
    return `<div class="dbpedit" data-pane="${j}">
      <div class="dbprow">
        <select data-k="kind">
          <option value="dispatch"${!isRep ? " selected" : ""}>Dispatch calls</option>
          <option value="reports"${isRep ? " selected" : ""}>Hospital hand-off reports</option>
        </select>
        <input data-k="title" value="${esc(p.title || "")}" placeholder="Pane title" />
        <label class="inline">width <input data-k="width" type="number" min="1" max="6" value="${p.width || 1}" style="width:56px" /></label>
        <label class="inline">rows <input data-k="limit" type="number" min="1" max="200" value="${p.limit || 25}" style="width:64px" /></label>
        <button class="btn ghost sm" data-del-pane="${j}">Remove</button>
      </div>
      ${isRep ? `
      <div class="row2">
        <label class="field"><span class="lab">Hospital</span>
          <select data-k="place">${placeOpts(p.place, false)}</select></label>
        <div class="help">Matched by the place's talkgroups, from the place book.</div>
      </div>` : `
      <div class="row2">
        <label class="field"><span class="lab">Call types <span class="faint">(blank = any)</span></span>
          <input data-k="call_types" value="${esc(words(p.call_types))}" placeholder="Cardiac Arrest, Chest Pain" /></label>
        <label class="field"><span class="lab">Words anywhere</span>
          <input data-k="phrases" value="${esc(words(p.phrases))}" placeholder="entrapment, working arrest" /></label>
      </div>
      <div class="row3">
        <label class="field"><span class="lab">Closest hospital is</span>
          <select data-k="closest_place">${placeOpts(p.closest_place, true)}</select></label>
        <label class="field"><span class="lab">Within</span>
          <select data-k="within_place">${placeOpts(p.within_place, false)}</select></label>
        <label class="field"><span class="lab">Miles</span>
          <input data-k="within_miles" type="number" min="0" max="200" step="0.1" value="${p.within_miles || 0}" /></label>
      </div>
      <div class="row2">
        <label class="field"><span class="lab">But not</span>
          <input data-k="except" value="${esc(words(p.except))}" placeholder="cancelled, disregard" /></label>
      </div>`}
      <div class="lab" style="margin-top:8px">Emphasis <span class="faint">first rule that matches wins${isRep ? " · reports carry no call type, so these match on words" : ""}</span></div>
      ${(p.emphasis || []).map((e, k) => `
        <div class="dbemrow" data-em="${k}">
          <select data-e="style">${styleOpts(e.style)}</select>
          ${isRep ? "" : `<input data-e="call_types" value="${esc(words((e.when || {}).call_types))}" placeholder="call types" />`}
          <input data-e="phrases" value="${esc(words((e.when || {}).phrases))}" placeholder="words" />
          <input data-e="note" value="${esc(e.note || "")}" placeholder="tag (optional)" />
          <button class="btn ghost sm" data-del-em="${k}">×</button>
        </div>`).join("")}
      <button class="btn ghost sm" data-add-em="${j}">+ Emphasis</button>
    </div>`;
  }

  function editor(b, i) {
    return `<div class="dbedit" data-board="${i}">
      <div class="dbprow">
        <input data-b="name" value="${esc(b.name || "")}" placeholder="Dashboard name" style="flex:1" />
        <label class="check"><input data-b="enabled" type="checkbox"${b.enabled !== false ? " checked" : ""} /> shown</label>
        <button class="btn ghost sm" data-del-board="${i}">Delete</button>
      </div>
      ${(b.panes || []).map((p, j) => paneEditor(b, p, j)).join("")}
      <button class="btn ghost sm" data-add-pane="${i}">+ Pane</button>
      <label class="field" style="margin-top:10px"><span class="lab">Footer <span class="faint">pinned along the bottom — an attestation, a caveat, whatever this board is read under</span></span>
        <textarea data-b="footer" rows="3" placeholder="CONFIDENTIAL — for peer review and quality improvement use only.">${esc(b.footer || "")}</textarea></label>
    </div>`;
  }

  function renderEditor() {
    $("dbEditEmpty").style.display = boards.length ? "none" : "";
    $("dbMeta").textContent = boards.length ? `${boards.length} dashboard${boards.length > 1 ? "s" : ""}` : "";
    $("dbEditList").innerHTML = boards.map((b, i) =>
      `<div class="pwcard${sel === String(i) ? " on" : ""}">
        <div class="dbprow" data-open="${i}">
          <span class="grow"><b>${esc(b.name || "Dashboard")}</b>
            <span class="faint mono">${(b.panes || []).length} pane${(b.panes || []).length === 1 ? "" : "s"}</span></span>
          <span class="faint">${sel === b.id ? "▾" : "▸"}</span>
        </div>
        ${sel === b.id ? editor(b, i) : ""}
      </div>`).join("");
    wireEditor();
  }

  function wireEditor() {
    $("dbEditList").querySelectorAll("[data-open]").forEach((x) => x.onclick = (ev) => {
      if (ev.target.closest("input,select,textarea,button")) return;
      sel = sel === x.dataset.open ? null : x.dataset.open; renderEditor();
    });
    const boardOf = (el) => boards[+el.closest("[data-board]").dataset.board];
    const paneOf = (el) => boardOf(el).panes[+el.closest("[data-pane]").dataset.pane];

    $("dbEditList").querySelectorAll("[data-b]").forEach((x) => {
      const set = () => {
        const b = boardOf(x), k = x.dataset.b;
        b[k] = x.type === "checkbox" ? x.checked : x.value;
      };
      x.onchange = set; if (x.tagName !== "SELECT") x.oninput = set;
    });
    $("dbEditList").querySelectorAll("[data-k]").forEach((x) => {
      const set = () => {
        const p = paneOf(x), k = x.dataset.k;
        if (k === "call_types" || k === "phrases" || k === "except") p[k] = parseWords(x.value);
        else if (k === "width" || k === "limit") p[k] = Math.max(1, +x.value || 1);
        else if (k === "within_miles") p[k] = Math.max(0, +x.value || 0);
        else p[k] = x.value;
        if (k === "kind") renderEditor();
      };
      x.onchange = set; if (x.tagName !== "SELECT") x.oninput = set;
    });
    $("dbEditList").querySelectorAll("[data-e]").forEach((x) => {
      const set = () => {
        const p = paneOf(x), e = p.emphasis[+x.closest("[data-em]").dataset.em], k = x.dataset.e;
        e.when = e.when || {};
        if (k === "call_types" || k === "phrases") e.when[k] = parseWords(x.value);
        else e[k] = x.value;
      };
      x.onchange = set; if (x.tagName !== "SELECT") x.oninput = set;
    });
    $("dbEditList").querySelectorAll("[data-add-pane]").forEach((x) => x.onclick = () => {
      boardOf(x).panes.push({ kind: "dispatch", title: "", width: 1, limit: 25, emphasis: [] }); renderEditor();
    });
    $("dbEditList").querySelectorAll("[data-del-pane]").forEach((x) => x.onclick = () => {
      boardOf(x).panes.splice(+x.dataset.delPane, 1); renderEditor();
    });
    $("dbEditList").querySelectorAll("[data-add-em]").forEach((x) => x.onclick = () => {
      const p = paneOf(x); p.emphasis = p.emphasis || [];
      p.emphasis.push({ when: { call_types: [], phrases: [] }, style: "alarm", note: "" }); renderEditor();
    });
    $("dbEditList").querySelectorAll("[data-del-em]").forEach((x) => x.onclick = () => {
      paneOf(x).emphasis.splice(+x.dataset.delEm, 1); renderEditor();
    });
    $("dbEditList").querySelectorAll("[data-del-board]").forEach((x) => x.onclick = async () => {
      const b = boards[+x.dataset.delBoard];
      if (typeof uiConfirm === "function" && !(await uiConfirm(`Delete “${b.name}”?`))) return;
      boards.splice(+x.dataset.delBoard, 1); sel = null; renderEditor();
    });
  }

  /* ---------- wiring ---------- */

  $("dbSetupBtn").onclick = () => { $("dbBoard").style.display = "none"; $("dbSetup").style.display = ""; renderEditor(); };
  $("dbBack").onclick = () => { $("dbSetup").style.display = "none"; $("dbBoard").style.display = ""; render(); };
  // Built in memory, with no id: `sanitize` gives it one when Save writes it.
  // Nothing reaches dashboards.json until then, so backing out of a half-made
  // board is just leaving the editor.
  const blankBoard = (n) => ({
    id: "", name: `Dashboard ${n}`, enabled: true, footer: "", refresh_secs: 10,
    panes: [
      { id: "", kind: "dispatch", title: "Closest Dispatch Calls", width: 1, limit: 25,
        call_types: [], phrases: [], except: [],
        closest_place: "", within_place: "", within_miles: 3, place: "", emphasis: [] },
      { id: "", kind: "reports", title: "EMS Calls", width: 2, limit: 25,
        call_types: [], phrases: [], except: [],
        closest_place: "", within_place: "", within_miles: 0, place: "", emphasis: [] },
    ],
  });
  $("dbAdd").onclick = () => {
    boards.push(blankBoard(boards.length + 1));
    sel = null;                         // no id yet; opened by index after Save
    renderEditor();
    const last = $("dbEditList").lastElementChild;
    if (last) last.scrollIntoView({ block: "nearest" });
  };
  $("dbSave").onclick = async () => {
    try {
      const s = await invoke("dashboards_set", { settings: { dashboards: boards } });
      boards = JSON.parse(JSON.stringify(s.dashboards || []));
      if (!boards.some((b) => b.id === curId)) curId = (boards[0] || {}).id || null;
      renderEditor();
      if (typeof uiToast === "function") uiToast("Dashboards saved");
    } catch (e) { alert(e); }
  };

  // Live. Everything a board shows arrives as an event it is already told
  // about, so there is nothing to poll; the interval only ages the timestamps.
  if (typeof listen === "function") {
    listen("incident", (e) => { const i = e.payload; if (i && i.id != null) { inc.set(i.id, i); render(); } });
    listen("incident_deleted", (e) => { inc.delete(e.payload); render(); });
    listen("conversations", async () => { if (!shown()) return; await loadReports(); render(); });
    listen("places", async () => { await loadCfg(); render(); });
  }
  setInterval(() => {
    if (!shown()) return;
    $("dbClock").textContent = new Date().toLocaleTimeString();
    document.querySelectorAll("#dbPanes .ago[data-t]").forEach((n) => { n.textContent = ago(+n.dataset.t); });
  }, 10000);

  window.dashboardsOnShow = show;
})();
