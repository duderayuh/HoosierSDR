/* Dashboards: boards the listener composes out of panes.
 *
 * Two jobs in one file — showing a board, and editing one. What belongs on a
 * board is decided in `dashboards::render`, not here: the same answer is
 * served to another machine over Tailscale, and that machine must be able to
 * see one board and nothing else, which is only true if the filtering happens
 * before anything leaves. Drawing is shared with that page too, in
 * board-render.js.
 */
(() => {
  if (typeof invoke !== "function") return;

  let cfg = null;              // the whole View from dashboards_get
  let boards = [];             // working copy, edited in place
  let places = [];             // enabled places, whole
  let curId = null;            // board on screen
  let sel = null;              // board open in the editor
  let loaded = false;
  let dirty = false;            // edited since the last Save
  let skew = 0;                 // this machine's clock, less the app's
  let remotes = null;           // tailnet name and port, for a share link

  const shown = () => $("view-dashboard").style.display !== "none";
  const board = () => boards.find((b) => b.id === curId) || boards[0] || null;

  // Draw. The board itself comes from Rust already matched, ordered and cut
  // to each pane's limit, so nothing here decides what is shown.
  async function render() {
    if (!shown()) return;
    const live = boards.filter((b) => b.enabled !== false);
    $("dbPick").innerHTML = live.map((b) =>
      `<button data-b="${esc(b.id)}"${b.id === curId ? ' aria-pressed="true"' : ""}>${esc(b.name)}</button>`).join("");
    $("dbPick").querySelectorAll("button").forEach((x) => x.onclick = () => { curId = x.dataset.b; render(); });
    $("dbEmpty").style.display = live.length ? "none" : "";
    $("dbClock").textContent = new Date().toLocaleTimeString();
    const b = board();
    if (!b || !b.id) { $("dbPanes").innerHTML = ""; $("dbFooter").style.display = "none"; return; }
    let view;
    try { view = await invoke("dashboards_render", { id: b.id }); }
    catch (e) { log(`dashboards render: ${e}`); return; }
    // A board saved a moment ago may not be the one just asked for, and a
    // call that answered with nothing must not blank a wall display.
    if (!view || !window.HSBoard || board() !== b) return;
    skew = HSBoard.paint($("dbPanes"), view);
    $("dbFooter").style.display = view.footer ? "" : "none";
    if (view.footer) $("dbFooter").textContent = view.footer;
  }

  // Runs arrive in bursts, and each draw is now a database scan rather than a
  // pass over memory. One draw per burst is enough to look live.
  let pending = null;
  function redraw() {
    if (!shown()) return;
    clearTimeout(pending);
    pending = setTimeout(() => { pending = null; render(); }, 300);
  }

  /* ---------- data ---------- */

  async function loadCfg() {
    // Only needed to spell a share link; a board still edits and draws
    // without it, so a failure here is not worth stopping for.
    try { remotes = await invoke("remotes_get"); } catch (e) { remotes = null; }
    const v = await invoke("dashboards_get");
    cfg = v;
    boards = JSON.parse(JSON.stringify(v.settings.dashboards || []));
    places = v.places || [];
    if (!curId || !boards.some((b) => b.id === curId)) curId = (boards[0] || {}).id || null;
  }

  async function show() {
    if (!loaded) { loaded = true; await loadCfg(); }
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
      ${shareEditor(b, i)}
    </div>`;
  }

  // Sharing one board over the tailnet.
  //
  // The link carries a key that opens this board and nothing else — not the
  // library, not another board, not the radio. That is worth saying here,
  // because the obvious alternative (hand someone the web token) would give
  // them the whole application.
  function shareEditor(b, i) {
    const on = !!b.shared;
    const url = on && b.share_key && b.id ? shareUrl(b) : "";
    return `<div class="dbshare" style="margin-top:10px">
      <label class="check"><input data-b="shared" type="checkbox"${on ? " checked" : ""} />
        share this board over Tailscale</label>
      ${!on ? `<div class="faint">Off. Nothing outside this machine can ask for it.</div>` : url
        ? `<div class="dbprow" style="margin-top:6px">
             <input class="mono" value="${esc(url)}" readonly onfocus="this.select()" style="flex:1" />
             <button class="btn ghost sm" data-copy-share="${i}">Copy</button>
             <button class="btn ghost sm" data-new-key="${i}">New link</button>
           </div>
           <div class="faint">Open this on any device signed in to your tailnet. It shows this
             board, read-only, and can reach nothing else. “New link” stops the old one working.</div>`
        : `<div class="faint">Press <b>Save</b> to get the link.</div>`}
      ${on && trustWarning() ? `<div class="dbwarn">${esc(trustWarning())}</div>` : ""}
    </div>`;
  }

  // Built from the tailnet name this machine answers to, so the link works
  // from the other laptop rather than only from here.
  function shareUrl(b) {
    const t = (remotes && remotes.tailnet) || {};
    const host = t.dns || t.ip || location.hostname;
    const port = (remotes && remotes.port) || 8042;
    return `http://${host}:${port}/board/${encodeURIComponent(b.id)}?key=${encodeURIComponent(b.share_key)}`;
  }

  // The board's own key is the narrow thing. Tailnet trust is not, and it is
  // a separate switch elsewhere that undoes the point of sharing carefully.
  function trustWarning() {
    return remotes && remotes.settings && remotes.settings.trust_tailnet
      ? "Tailnet trust is switched on in Connections, so every device on your account already has full control of this app — not just this board."
      : "";
  }

  // Anything that changes `boards` goes through here, so the Save reminder
  // cannot drift out of step with what is actually unsaved.
  function touch() { dirty = true; renderEditor(); }

  function renderEditor() {
    $("dbEditEmpty").style.display = boards.length ? "none" : "";
    markDirty();
    $("dbEditList").innerHTML = boards.map((b, i) =>
      `<div class="pwcard${sel === String(i) ? " on" : ""}">
        <div class="dbprow" data-open="${i}">
          <span class="grow"><b>${esc(b.name || "Dashboard")}</b>
            <span class="faint mono">${(b.panes || []).length} pane${(b.panes || []).length === 1 ? "" : "s"}</span></span>
          <span class="faint">${sel === String(i) ? "▾" : "▸"}</span>
        </div>
        ${sel === String(i) ? editor(b, i) : ""}
      </div>`).join("");
    wireEditor();
  }

  // The Save reminder. A board lives in memory until Save, so that a
  // half-made one can be abandoned — but nothing about the old screen said
  // so, and two boards were lost to that.
  function markDirty() {
    const n = boards.length;
    $("dbMeta").textContent = (n ? `${n} dashboard${n > 1 ? "s" : ""}` : "") + (dirty ? " · unsaved — press Save" : "");
    $("dbMeta").classList.toggle("dbunsaved", dirty);
    $("dbSave").classList.toggle("primary", true);
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
        // Turning sharing on or off changes what the panel offers, so this
        // one redraws the editor rather than only noting the change.
        if (k === "shared") { touch(); return; }
        dirty = true; markDirty();
      };
      x.onchange = set; if (x.tagName !== "SELECT") x.oninput = set;
    });
    $("dbEditList").querySelectorAll("[data-copy-share]").forEach((x) => x.onclick = async () => {
      const b = boards[+x.dataset.copyShare];
      const url = shareUrl(b);
      try { await navigator.clipboard.writeText(url); uiToast("Share link copied"); }
      catch (e) {
        // No clipboard in this webview; the field is right there and already
        // selects itself, so say that rather than failing silently.
        uiToast("Select the link and copy it", "err");
      }
    });
    // There is no command for this: clearing the key and saving is what mints
    // a new one, because a shared board is always given one on the way in.
    $("dbEditList").querySelectorAll("[data-new-key]").forEach((x) => x.onclick = async () => {
      const b = boards[+x.dataset.newKey];
      if (typeof uiConfirm === "function" &&
          !(await uiConfirm("Make a new link for this board? The link you have already shared will stop working."))) return;
      b.share_key = "";
      touch();
      uiToast("Press Save to issue the new link");
    });
    $("dbEditList").querySelectorAll("[data-k]").forEach((x) => {
      const set = () => {
        const p = paneOf(x), k = x.dataset.k;
        if (k === "call_types" || k === "phrases" || k === "except") p[k] = parseWords(x.value);
        else if (k === "width" || k === "limit") p[k] = Math.max(1, +x.value || 1);
        else if (k === "within_miles") p[k] = Math.max(0, +x.value || 0);
        else p[k] = x.value;
        dirty = true;
        if (k === "kind") renderEditor(); else markDirty();
      };
      x.onchange = set; if (x.tagName !== "SELECT") x.oninput = set;
    });
    $("dbEditList").querySelectorAll("[data-e]").forEach((x) => {
      const set = () => {
        const p = paneOf(x), e = p.emphasis[+x.closest("[data-em]").dataset.em], k = x.dataset.e;
        e.when = e.when || {};
        if (k === "call_types" || k === "phrases") e.when[k] = parseWords(x.value);
        else e[k] = x.value;
        dirty = true; markDirty();
      };
      x.onchange = set; if (x.tagName !== "SELECT") x.oninput = set;
    });
    $("dbEditList").querySelectorAll("[data-add-pane]").forEach((x) => x.onclick = () => {
      boardOf(x).panes.push({ kind: "dispatch", title: "", width: 1, limit: 25, emphasis: [] }); touch();
    });
    $("dbEditList").querySelectorAll("[data-del-pane]").forEach((x) => x.onclick = () => {
      boardOf(x).panes.splice(+x.dataset.delPane, 1); touch();
    });
    $("dbEditList").querySelectorAll("[data-add-em]").forEach((x) => x.onclick = () => {
      const p = paneOf(x); p.emphasis = p.emphasis || [];
      p.emphasis.push({ when: { call_types: [], phrases: [] }, style: "alarm", note: "" }); touch();
    });
    $("dbEditList").querySelectorAll("[data-del-em]").forEach((x) => x.onclick = () => {
      paneOf(x).emphasis.splice(+x.dataset.delEm, 1); touch();
    });
    $("dbEditList").querySelectorAll("[data-del-board]").forEach((x) => x.onclick = async () => {
      const b = boards[+x.dataset.delBoard];
      if (typeof uiConfirm === "function" && !(await uiConfirm(`Delete “${b.name}”?`))) return;
      boards.splice(+x.dataset.delBoard, 1); sel = null; touch();
    });
  }

  /* ---------- wiring ---------- */

  $("dbSetupBtn").onclick = () => { $("dbBoard").style.display = "none"; $("dbSetup").style.display = ""; renderEditor(); };
  $("dbBack").onclick = async () => {
    if (dirty && typeof uiConfirm === "function" &&
        !(await uiConfirm("There are unsaved dashboard changes. Leave without saving?"))) return;
    if (dirty) { await loadCfg(); dirty = false; }   // drop the edits, show what is stored
    $("dbSetup").style.display = "none"; $("dbBoard").style.display = ""; render();
  };
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
    sel = String(boards.length - 1);    // open it straight away
    dirty = true;
    renderEditor();
    const last = $("dbEditList").lastElementChild;
    if (last && last.scrollIntoView) last.scrollIntoView({ block: "nearest" });
  };
  $("dbSave").onclick = async () => {
    try {
      const s = await invoke("dashboards_set", { settings: { dashboards: boards } });
      boards = JSON.parse(JSON.stringify(s.dashboards || []));
      if (!boards.some((b) => b.id === curId)) curId = (boards[0] || {}).id || null;
      dirty = false;
      renderEditor();
      if (typeof uiToast === "function") uiToast("Dashboards saved");
    } catch (e) { uiToast(`${e}`, "err"); }
  };

  // Live. Everything a board shows arrives as an event it is already told
  // about, so there is nothing to poll; the interval only ages the timestamps.
  if (typeof listen === "function") {
    listen("incident", redraw);
    listen("incident_deleted", redraw);
    listen("conversations", redraw);
    listen("places", async () => { await loadCfg(); render(); });
  }
  // The clock and the "12 min ago" labels age between draws without asking
  // for the board again.
  setInterval(() => {
    if (!shown()) return;
    $("dbClock").textContent = new Date().toLocaleTimeString();
    if (window.HSBoard) HSBoard.tick($("dbPanes"), skew);
  }, 10000);

  window.dashboardsOnShow = show;
})();
