/* ================= Tripwires: when · check · send ================= */
// One kind of rule for what used to be alerts, analyzers, conversation
// summaries and digests. The list on the left (with a week of results per
// tripwire), a recipe gallery to start from, and an editor that reads as a
// sentence — WHEN something is heard, CHECK it, SEND it — beside a live
// preview that runs the draft over the library: how often it would have
// fired, which calls, which phrases are never heard (and what is heard
// instead), and what the AI check would have said.
// Loaded after app.js / picker.js / connections.js and shares their helpers.
(() => {
  if (typeof invoke !== "function") { window.tripwireFromCall = () => {}; return; }
  const QUIET = [[0, "every time"], [120, "once per 2 min per talkgroup"], [300, "once per 5 min per talkgroup"], [900, "once per 15 min per talkgroup"], [3600, "once per hour per talkgroup"]];
  const FOLLOW_MINS = [10, 20, 30, 60, 120];
  const EARLIER = [[0, "just this call"], [1, "+ the call before"], [2, "+ 2 calls before"], [3, "+ 3 calls before"]];
  const OPS = ["==", "!=", ">", ">=", "<", "<=", "contains"];
  const KINDS = ["string", "number", "bool"];
  const TOKENS = {
    call: ["{name}", "{tgname}", "{tgdesc}", "{unitname}", "{time}", "{transcript}", "{keywords}", "{ai}"],
    conversation: ["{rule}", "{summary}", "{tgname}", "{tgdesc}", "{unitnames}", "{calls}", "{duration}", "{started}", "{transcript}", "{revision}"],
    digest: ["{name}", "{summary}", "{count}", "{window}", "{time}", "{transcript}"],
    incident: ["{name}", "{calltype}", "{address}", "{units}", "{summary}", "{where}", "{pathway}", "{maps}", "{place}", "{nearest}", "{km}", "{mins}", "{hospital}", "{report}", "{time}", "{ai}"],
  };
  let list = [], folders = [], stats = {}, view = null, recipes = null;
  // Which folders are rolled up, per listener rather than per install.
  let shut = new Set(store("hs.twfolders", []));
  // What is being dragged, and a guard so a backend event cannot
  // re-render the list out from under the gesture.
  let drag = null, dragging = false;
  let sel = null, draft = null, saved = "", words = null;
  let days = 7, preview = null, previewSeq = 0, previewTimer = null, tried = new Map();
  let activity = [];   // [{tg, system, calls}] over the last week, for the system picker
  const clone = (x) => JSON.parse(JSON.stringify(x));
  const lines = (v) => String(v || "").split(/\n+/).map((x) => x.trim()).filter(Boolean);
  const nums = (v) => String(v || "").split(/[\s,;]+/).map((x) => parseInt(x, 10)).filter(Number.isFinite);
  const int = (v, d) => { const n = parseInt(String(v).trim(), 10); return Number.isFinite(n) ? n : d; };
  const now = () => Math.floor(Date.now() / 1000);
  const ago = (t) => { if (!t) return ""; const s = Math.max(0, now() - t); return s < 90 ? "just now" : s < 3600 ? `${Math.round(s / 60)} min ago` : s < 86400 ? `${Math.round(s / 3600)} h ago` : `${Math.round(s / 86400)} d ago`; };
  const clock = (t) => new Date(t * 1000).toLocaleTimeString("en-US", { hour: "numeric", minute: "2-digit" });
  const dayClock = (t) => new Date(t * 1000).toLocaleString("en-US", { weekday: "short", hour: "numeric", minute: "2-digit" });
  const visible = () => $("view-tripwires") && $("view-tripwires").style.display !== "none";
  const S = () => (typeof window.alertsSettings === "function" ? window.alertsSettings() : null);

  /* ---------- names for things ---------- */
  function destLabel(send) {
    const s = S();
    if (!send.telegram) return "the app only";
    if (send.dest === "custom") return (window.destName && window.destName(send.chat)) || send.chat || "a chat";
    if (!send.dest) {
      const def = s ? [s.telegram.chat_id, s.telegram.topic_id].filter(Boolean).join(":") : "";
      return def ? `${(window.destName && window.destName(def)) || def}` : "the default chat";
    }
    const d = s && (s.destinations || []).find((x) => x.id === send.dest);
    return d ? d.name : "a removed destination";
  }
  const chans = (t) => (t.when.tgs.length ? (window.channelSummary ? channelSummary(t.when.tgs, 2) : t.when.tgs.join(", ")) : "any talkgroup");
  function sentence(t) {
    const w = t.when; let s;
    if (w.kind === "conversation") s = `Summarises conversations on ${chans(t)}`;
    else if (w.kind === "digest") s = `Every ${w.digest.every_mins} min, what was said on ${chans(t)}`;
    else {
      const bits = [];
      if (w.emergency) bits.push("an emergency");
      if (w.phrases.length) bits.push(w.phrases.slice(0, 3).map((p) => `“${p}”`).join(", ") + (w.phrases.length > 3 ? ` +${w.phrases.length - 3}` : ""));
      if (w.units.length) bits.push(`radio ${w.units.slice(0, 2).join(", ")}${w.units.length > 2 ? "…" : ""}`);
      s = `${bits.length ? bits.join(" · ") : "any call"} on ${chans(t)}`;
      if (t.check.kind === "ask") s += " · AI asks first";
      if (t.check.kind === "extract") s += " · AI pulls details";
    }
    return `${s} → ${destLabel(t.send)}`;
  }

  /* ---------- loading ---------- */
  async function load() {
    try { view = await invoke("tripwires_get"); list = view.tripwires || []; folders = view.folders || []; stats = view.stats || {}; }
    catch (e) { log(`tripwires_get: ${e}`); return; }
    renderList();
    if (sel && sel !== "new" && !list.some((t) => t.id === sel)) closeEditor();
  }
  async function loadActivity() {
    try { activity = (await invoke("channel_activity", { hours: 24 * 7 })) || []; } catch (_) { activity = []; }
  }

  /* ---------- the list: a tree of folders, dragged into shape ---------- */
  // Display order is the stored order: within one parent, folders first,
  // then tripwires, each in the order the arrays hold them. Dragging
  // rewrites those arrays, so what is on screen is what is saved.
  // An id-less folder is nobody's child: without this guard one would be
  // its own parent at the top level, and counting the tree would not end.
  const kidFolders = (parent) => folders.filter((f) => f.id && (f.parent || "") === parent);
  const kidWires = (parent) => list.filter((t) => (t.parent || "") === parent);
  const folderById = (id) => folders.find((f) => f.id === id);

  // A folder is only really on if every folder above it is too — the same
  // rule the backend applies, so the list cannot disagree with what runs.
  function folderOn(id) {
    let seen = 0;
    while (id) {
      const f = folderById(id);
      if (!f) return true;
      if (!f.enabled) return false;
      id = f.parent || "";
      if (++seen > 8) return false;
    }
    return true;
  }
  const countIn = (parent) => kidWires(parent).length + kidFolders(parent).reduce((n, f) => n + countIn(f.id), 0);
  const liveIn = (parent) =>
    kidWires(parent).filter((t) => t.enabled && folderOn(parent)).length +
    kidFolders(parent).reduce((n, f) => n + liveIn(f.id), 0);

  function twCard(t, depth) {
    const st = stats[t.id] || {};
    const res = [st.sent ? `${st.sent} sent` : "", st.quiet ? `${st.quiet} quiet` : "", st.failed ? `<span class="bad">${st.failed} failed</span>` : ""].filter(Boolean).join(" · ");
    const icon = t.when.kind === "conversation" ? "🏥" : t.when.kind === "digest" ? "🗞️" : t.when.kind === "incident" ? "🚑" : t.check.kind === "none" ? "⚡" : "🔎";
    // A tripwire switched on inside a shut-off folder is drawn as off,
    // because that is what it is — with a word on why.
    const muted = t.enabled && !folderOn(t.parent || "");
    return `<div class="twcard ${sel === t.id ? "on" : ""} ${t.enabled && !muted ? "" : "off"}" data-tw="${esc(t.id)}" draggable="true" style="--twdepth:${depth}">
      <div class="twcard-top"><span class="twgrip" title="Drag to move">⠿</span><span class="twicon">${icon}</span><b class="grow">${esc(t.name)}</b>${muted ? `<span class="twmuted" title="The folder it is in is switched off">folder off</span>` : ""}<label class="tw-switch sm" title="On or off"><input type="checkbox" data-twen="${esc(t.id)}" ${t.enabled ? "checked" : ""}><span></span></label></div>
      <div class="twcard-sent">${esc(sentence(t))}</div>
      <div class="twcard-stats">${res || "nothing this week"}${st.last_at ? ` · last ${ago(st.last_at)}` : ""}</div></div>`;
  }

  function folderRow(f, depth) {
    const n = countIn(f.id), on = liveIn(f.id);
    const isShut = shut.has(f.id);
    const dim = f.enabled && !folderOn(f.parent || "");
    return `<div class="twfolder ${f.enabled && !dim ? "" : "off"}" data-twf="${esc(f.id)}" draggable="true" style="--twdepth:${depth}">
        <span class="twgrip" title="Drag to move">⠿</span>
        <button class="twcaret" data-twfold="${esc(f.id)}" title="${isShut ? "Open" : "Close"}">${isShut ? "▸" : "▾"}</button>
        <span class="twfname grow" data-twren="${esc(f.id)}" title="Click to rename">${esc(f.name)}</span>
        <small class="faint">${n ? `${on} of ${n} on` : "empty"}</small>
        <label class="tw-switch sm" title="Switch off everything in here"><input type="checkbox" data-twfen="${esc(f.id)}" ${f.enabled ? "checked" : ""}><span></span></label>
        <button class="btn ghost sm" data-twfdel="${esc(f.id)}" title="Remove the folder and keep what is in it">✕</button>
      </div>` + (isShut ? "" : branch(f.id, depth + 1));
  }

  function branch(parent, depth) {
    if (depth > 6) return "";
    return kidFolders(parent).map((f) => folderRow(f, depth)).join("")
      + kidWires(parent).map((t) => twCard(t, depth)).join("");
  }

  function renderList() {
    // Never redraw mid-gesture: the list is server-backed and a tripwires
    // event would otherwise wipe the node being dragged.
    if (dragging) return;
    const q = ($("twFilter").value || "").trim().toLowerCase();
    $("twEmpty").style.display = list.length || folders.length ? "none" : "";
    $("twMeta").textContent = list.length ? `${list.filter((t) => t.enabled && folderOn(t.parent || "")).length} of ${list.length} on` : "";
    // While filtering, the tree gets out of the way: matches are shown flat,
    // because a hit three folders down is otherwise invisible.
    $("twList").innerHTML = q
      ? list.filter((t) => `${t.name} ${sentence(t)}`.toLowerCase().includes(q)).map((t) => twCard(t, 0)).join("")
      : branch("", 0);
    wireList(!!q);
  }

  function wireList(filtering) {
    const box = $("twList");
    box.querySelectorAll(".twcard").forEach((c) => c.onclick = (e) => {
      if (e.target.closest(".tw-switch") || e.target.closest(".twgrip")) return;
      pick(c.dataset.tw);
    });
    box.querySelectorAll("input[data-twen]").forEach((c) => c.onchange = async () => {
      const t = list.find((x) => x.id === c.dataset.twen); if (!t) return;
      t.enabled = c.checked;
      if (await persist(list)) { uiToast(`${t.name} is ${t.enabled ? "on" : "off"}`); if (sel === t.id && draft) { draft.enabled = t.enabled; $("twEnabled").checked = t.enabled; if (saved) { const o = JSON.parse(saved); o.enabled = t.enabled; saved = stable(o); } markDirty(); } }
    });
    box.querySelectorAll("[data-twfold]").forEach((b) => b.onclick = () => {
      const id = b.dataset.twfold;
      if (shut.has(id)) shut.delete(id); else shut.add(id);
      save("hs.twfolders", [...shut]); renderList();
    });
    box.querySelectorAll("input[data-twfen]").forEach((c) => c.onchange = async () => {
      const f = folderById(c.dataset.twfen); if (!f) return;
      f.enabled = c.checked;
      if (await persist(list)) uiToast(`${f.name} and everything in it is ${f.enabled ? "on" : "off"}`);
    });
    box.querySelectorAll("[data-twren]").forEach((el) => el.onclick = async () => {
      const f = folderById(el.dataset.twren); if (!f) return;
      const name = await uiAsk("Folder name", f.name);
      if (name === null) return;
      f.name = String(name).trim() || f.name;
      await persist(list);
    });
    box.querySelectorAll("[data-twfdel]").forEach((b) => b.onclick = async () => {
      const f = folderById(b.dataset.twfdel); if (!f) return;
      const n = countIn(f.id);
      if (!await uiConfirm(n
        ? `Remove the folder “${f.name}”? The ${n} tripwire(s) and folder(s) in it move up a level — nothing is deleted.`
        : `Remove the empty folder “${f.name}”?`, "Remove folder")) return;
      // Contents move to where the folder was, so nothing disappears.
      const up = f.parent || "";
      list.forEach((t) => { if ((t.parent || "") === f.id) t.parent = up; });
      folders.forEach((x) => { if ((x.parent || "") === f.id) x.parent = up; });
      folders = folders.filter((x) => x.id !== f.id);
      shut.delete(f.id); save("hs.twfolders", [...shut]);
      await persist(list);
    });
    if (!filtering) wireDrag(box);
  }

  /* ---------- dragging ---------- */
  // Same gesture as the nav tabs, but this list is server-backed and
  // re-rendered, so the arrays are reordered and then redrawn rather than
  // live DOM nodes being shuffled.
  function wireDrag(box) {
    const rows = [...box.querySelectorAll(".twcard, .twfolder")];
    rows.forEach((el) => {
      const kind = el.classList.contains("twfolder") ? "folder" : "tw";
      const id = kind === "folder" ? el.dataset.twf : el.dataset.tw;
      el.addEventListener("dragstart", (e) => {
        drag = { kind, id }; dragging = true; el.classList.add("dragging");
        e.dataTransfer.effectAllowed = "move";
        e.dataTransfer.setData("text/plain", id);
      });
      el.addEventListener("dragend", () => { el.classList.remove("dragging"); drag = null; dragging = false; renderList(); });
      el.addEventListener("dragover", (e) => {
        if (!drag || drag.id === id) return;
        // A folder may not be dropped into itself or its own descendants.
        if (drag.kind === "folder" && kind === "folder" && inside(id, drag.id)) return;
        e.preventDefault(); e.dataTransfer.dropEffect = "move";
        const r = el.getBoundingClientRect();
        const third = r.height / 3;
        // Top third = before it, bottom third = after it, middle of a
        // folder header = into it.
        const where = e.clientY < r.top + third ? "before" : e.clientY > r.bottom - third ? "after" : (kind === "folder" ? "in" : "after");
        el.dataset.drop = where;
        rows.forEach((o) => { if (o !== el) delete o.dataset.drop; });
      });
      el.addEventListener("dragleave", () => delete el.dataset.drop);
      el.addEventListener("drop", async (e) => {
        e.preventDefault(); e.stopPropagation();
        const where = el.dataset.drop || "after";
        rows.forEach((o) => delete o.dataset.drop);
        const target = { kind, id };
        drag && (await moveTo(drag, target, where));
      });
    });
    // Dropping on the empty space below the list means the top level.
    box.addEventListener("dragover", (e) => { if (drag) e.preventDefault(); });
    box.addEventListener("drop", async (e) => {
      if (!drag || e.target.closest(".twcard, .twfolder")) return;
      e.preventDefault();
      await moveTo(drag, { kind: "root", id: "" }, "in");
    });
  }

  // Is `id` inside `maybeAncestor` (or the same folder)?
  function inside(id, maybeAncestor) {
    let seen = 0;
    while (id) {
      if (id === maybeAncestor) return true;
      id = (folderById(id) || {}).parent || "";
      if (++seen > 8) return true;
    }
    return false;
  }

  async function moveTo(what, target, where) {
    const arr = what.kind === "folder" ? folders : list;
    const i = arr.findIndex((x) => x.id === what.id);
    if (i < 0) return;
    const me = arr[i];
    let parent, at;
    if (target.kind === "root") {
      parent = ""; at = arr.length;
    } else if (target.kind === "folder" && where === "in") {
      parent = target.id; at = arr.length;
      shut.delete(target.id); save("hs.twfolders", [...shut]);
    } else {
      // Beside the row it was dropped on, in that row's own parent.
      const t = (target.kind === "folder" ? folders : list).find((x) => x.id === target.id);
      if (!t) return;
      parent = t.parent || "";
      if (target.kind === what.kind) {
        at = arr.findIndex((x) => x.id === target.id) + (where === "before" ? 0 : 1);
      } else {
        at = arr.length;
      }
    }
    // A folder cannot be moved inside itself, whatever the drop said.
    if (what.kind === "folder" && inside(parent, what.id)) return;
    arr.splice(i, 1);
    if (at > i) at -= 1;
    me.parent = parent;
    arr.splice(Math.max(0, Math.min(at, arr.length)), 0, me);
    dragging = false;
    await persist(list);
  }

  $("twFolderAdd").onclick = async () => {
    const name = await uiAsk("New folder", "");
    if (name === null) return;
    // A new folder lands beside whatever is selected, so it appears where
    // the listener is looking rather than at the bottom.
    const near = list.find((t) => t.id === sel);
    folders.push({ id: `f${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`, name: String(name).trim() || "Folder", enabled: true, parent: near ? (near.parent || "") : "" });
    await persist(list);
  };

  $("twFilter").oninput = renderList;

  async function persist(next) {
    try {
      const out = await invoke("tripwires_set", { tripwires: next, folders });
      list = out;
      // The backend settles folder ids, names and any impossible nesting,
      // so the page adopts what was stored rather than what it sent.
      try { const v = await invoke("tripwires_get"); folders = v.folders || []; } catch (_) {}
      renderList(); return true;
    }
    catch (e) { uiToast(`Could not save: ${e}`, "err"); await load(); return false; }
  }

  /* ---------- recipe gallery ---------- */
  async function showGallery() {
    if (!(await leaveOk())) return;
    sel = null; draft = null; words = null; renderList();
    $("twEdit").style.display = "none"; $("twGallery").style.display = "";
    $("twGalleryClose").style.display = list.length ? "" : "none";
    if (!recipes) { try { recipes = await invoke("tripwire_recipes"); } catch (e) { recipes = []; uiToast(`${e}`, "err"); } }
    $("twRecipes").innerHTML = recipes.map((r, i) => `<button class="twrecipe" data-ri="${i}"><span class="twrecipe-icon">${esc(r.icon)}</span><b>${esc(r.title)}</b><small>${esc(r.blurb)}</small></button>`).join("") +
      `<button class="twrecipe blank" data-ri="-1"><span class="twrecipe-icon">＋</span><b>Start blank</b><small>Everything from scratch.</small></button>`;
    $("twRecipes").querySelectorAll("[data-ri]").forEach((b) => b.onclick = () => {
      const i = +b.dataset.ri;
      const t = i >= 0 ? clone(recipes[i].tripwire) : clone((recipes[0] || {}).tripwire || {});
      if (i < 0) { t.name = "New tripwire"; t.recipe = ""; t.when.phrases = []; t.when.except = []; t.check = { ...t.check, kind: "none", prompt: "" }; }
      t.id = ""; t.enabled = true;
      if (S() && S().ollama && t.check.kind === "ask") t.check.if_unavailable = S().ollama.fail_open ? "send" : "hold";
      edit("new", t, null);
      // Runs already come from the channels the Dispatch tab watches, so
      // there is nothing to pick.
      if (!t.when.tgs.length && t.when.kind !== "incident") setTimeout(() => $("twTgs").click(), 150);
    });
  }
  $("twNew").onclick = showGallery;
  $("twGalleryClose").onclick = () => { if (list.length) pick(list[0].id); };

  /* ---------- the editor ---------- */
  // The same tripwire as one string, keys sorted (the backend's key order
  // is not the page's), for "has it changed?".
  const stable = (x) => JSON.stringify(x, (k, v) => (v && typeof v === "object" && !Array.isArray(v) ? Object.keys(v).sort().reduce((o, key) => { o[key] = v[key]; return o; }, {}) : v));
  // A copy without half-typed rows: a field whose name is still empty would
  // make the backend refuse the whole draft mid-keystroke.
  const tidy = (t) => { const c = clone(t); c.check.fields = (c.check.fields || []).filter((f) => (f.key || "").trim()); c.check.conditions = (c.check.conditions || []).filter((x) => (x.field || "").trim()); return c; };
  async function leaveOk() {
    if (!draft || !isDirty()) return true;
    return uiConfirm("Discard the changes to this tripwire?", "Discard");
  }
  async function pick(id) {
    if (sel === id) return;
    if (!(await leaveOk())) return;
    const t = list.find((x) => x.id === id); if (!t) return;
    edit(id, clone(t), null);
  }
  function edit(id, t, w) {
    sel = id; draft = t; words = w; tried = new Map(); preview = null;
    $("twGallery").style.display = "none"; $("twEdit").style.display = "";
    renderList(); fill();
    // What "unchanged" looks like is whatever the form itself reads back
    // from the tripwire it was just filled with. Comparing against the
    // stored object instead would call a tripwire dirty the moment the
    // form writes a field the stored one happened not to carry.
    saved = id === "new" ? "" : stable(tidy(read()));
    markDirty();
    schedulePreview(0);
  }
  window.twEdit = edit;   // so the page check can open the editor
  function closeEditor() { sel = null; draft = null; words = null; $("twEdit").style.display = "none"; $("twGallery").style.display = ""; showGallery(); }
  const isDirty = () => draft && stable(tidy(read())) !== saved;
  function markDirty() { $("twDirty").textContent = sel === "new" ? "not saved yet" : isDirty() ? "unsaved changes" : ""; }

  function fillDest() {
    const s = S(); const d = draft.send;
    const def = s ? [s.telegram.chat_id, s.telegram.topic_id].filter(Boolean).join(":") : "";
    const dests = (s && s.destinations) || [];
    $("twDest").innerHTML = `<option value="">Default${def ? " — " + esc((window.destName && window.destName(def)) || def) : " (none set)"}</option>` +
      dests.map((x) => `<option value="${esc(x.id)}">${esc(x.name)}</option>`).join("") +
      (d.dest && d.dest !== "custom" && !dests.some((x) => x.id === d.dest) ? `<option value="${esc(d.dest)}">⚠ removed destination</option>` : "") +
      `<option value="custom">Other chat…</option>`;
    $("twDest").value = d.dest || "";
    $("twChat").value = d.chat || "";
    $("twChatWrap").style.display = d.dest === "custom" ? "" : "none";
  }
  function fillSystem() {
    const names = [...new Set(activity.map((a) => a.system).filter(Boolean))].sort();
    if (draft.when.system && !names.includes(draft.when.system)) names.unshift(draft.when.system);
    $("twSystem").innerHTML = `<option value="">any system</option>` + names.map((n) => `<option value="${esc(n)}">${esc(n)}</option>`).join("");
    $("twSystem").value = draft.when.system || "";
    $("twSystem").style.display = names.length > 1 || draft.when.system ? "" : "none";
  }
  function fill() {
    const t = draft, w = t.when, k = t.check, s = t.send;
    $("twName").value = t.name; $("twEnabled").checked = t.enabled;
    setSeg($("twKind"), w.kind);
    $("twTgs").textContent = chans(t);
    $("twTgHint").textContent = w.tgs.length ? `${w.tgs.length} talkgroup${w.tgs.length === 1 ? "" : "s"}` : "";
    fillSystem();
    $("twPhrases").value = w.phrases.join("\n"); $("twExcept").value = w.except.join("\n");
    $("twUnits").value = w.units.join(", "); $("twEmergency").checked = w.emergency;
    const c = w.conversation;
    $("twFixed").value = c.fixed_units.join(", "); $("twLearn").checked = c.learn_fixed;
    $("twGap").value = c.end_gap_secs; $("twReply").value = c.reply_gap_secs; $("twLate").value = c.late_window_secs;
    $("twMax").value = c.max_secs; $("twMin").value = c.min_calls; $("twNoTr").checked = c.send_without_transcript;
    $("twEvery").value = w.digest.every_mins; $("twWindow").value = w.digest.window_mins;
    fillIncident(w.incident || {});
    setSeg($("twCheck"), k.kind === "summarize" ? "none" : k.kind);
    $("twPrompt").value = k.prompt; $("twEngine").value = k.engine; $("twThink").checked = k.think; $("twUnavail").value = k.if_unavailable;
    $("twMatch").value = k.match_mode;
    renderFields(); renderConds();
    fillDest();
    $("twQuiet").innerHTML = QUIET.map(([v, l]) => `<option value="${v}">${l}</option>`).join("") + (QUIET.some(([v]) => v === s.quiet_secs) ? "" : `<option value="${s.quiet_secs}">once per ${s.quiet_secs} s per talkgroup</option>`);
    $("twQuiet").value = String(s.quiet_secs);
    $("twMessage").value = s.message; $("twAudio").checked = s.audio; $("twMap").checked = !!s.map; $("twTone").checked = s.tone; $("twTelegram").checked = s.telegram;
    $("twEarlier").innerHTML = EARLIER.map(([v, l]) => `<option value="${v}">${l}</option>`).join("") + (s.earlier_calls > 3 ? `<option value="${s.earlier_calls}">+ ${s.earlier_calls} calls before</option>` : "");
    $("twEarlier").value = String(s.earlier_calls);
    $("twFollow").value = s.follow;
    $("twFollowMins").innerHTML = FOLLOW_MINS.map((m) => `<option value="${m}">${m} min</option>`).join("") + (FOLLOW_MINS.includes(s.follow_mins) ? "" : `<option value="${s.follow_mins}">${s.follow_mins} min</option>`);
    $("twFollowMins").value = String(s.follow_mins);
    $("twDelete").style.display = $("twDup").style.display = $("twTest").style.display = sel === "new" ? "none" : "";
    renderWords(); kindUi(); markDirty();
    if (typeof window.olThinkUi === "function") window.olThinkUi();
  }
  // The place book, for "near …". Loaded once and left alone: a listener
  // who adds a hospital re-opens the editor anyway.
  let places = null;
  async function loadPlaces() {
    if (places) return places;
    try { places = (await invoke("places_get")).places || []; } catch (e) { places = []; }
    return places;
  }
  function fillIncident(o) {
    $("twIncTypes").value = (o.call_types || []).join(", ");
    $("twIncKm").value = o.within_km || 0;
    $("twIncPlaced").checked = !!o.placed_only;
    $("twIncLinked").checked = !!o.linked_only;
    loadPlaces().then((list) => {
      const feats = [...new Set(list.flatMap((p) => p.features || []))];
      const opts = [`<option value="">anywhere</option>`]
        .concat(feats.map((f) => `<option value="f:${esc(f)}">the nearest that can do ${esc(f)}</option>`))
        .concat(list.map((p) => `<option value="p:${esc(p.id)}">${esc(p.name)}</option>`));
      $("twIncNear").innerHTML = opts.join("");
      $("twIncNear").value = o.near_feature ? `f:${o.near_feature}` : o.near_place ? `p:${o.near_place}` : "";
    });
  }
  function readIncident(w) {
    const near = $("twIncNear").value || "";
    w.incident = {
      call_types: $("twIncTypes").value.split(",").map((x) => x.trim()).filter(Boolean),
      near_place: near.startsWith("p:") ? near.slice(2) : "",
      near_feature: near.startsWith("f:") ? near.slice(2) : "",
      within_km: +$("twIncKm").value || 0,
      placed_only: $("twIncPlaced").checked,
      linked_only: $("twIncLinked").checked,
    };
  }
  function read() {
    if (!draft) return null;
    const t = draft, w = t.when, k = t.check, s = t.send;
    t.name = $("twName").value.trim(); t.enabled = $("twEnabled").checked;
    w.system = $("twSystem").value;
    readIncident(w);
    w.phrases = lines($("twPhrases").value); w.except = lines($("twExcept").value);
    w.units = nums($("twUnits").value); w.emergency = $("twEmergency").checked;
    const c = w.conversation;
    c.fixed_units = nums($("twFixed").value); c.learn_fixed = $("twLearn").checked;
    c.end_gap_secs = int($("twGap").value, 90); c.reply_gap_secs = int($("twReply").value, 45); c.late_window_secs = int($("twLate").value, 180);
    c.max_secs = int($("twMax").value, 900); c.min_calls = int($("twMin").value, 1); c.send_without_transcript = $("twNoTr").checked;
    w.digest.every_mins = Math.max(1, int($("twEvery").value, 15)); w.digest.window_mins = Math.max(1, int($("twWindow").value, 15));
    k.prompt = $("twPrompt").value; k.engine = $("twEngine").value; k.think = $("twThink").checked; k.if_unavailable = $("twUnavail").value;
    syncRows(); k.match_mode = $("twMatch").value;
    s.dest = $("twDest").value; s.chat = $("twChat").value.trim();
    s.quiet_secs = int($("twQuiet").value, 300); s.message = $("twMessage").value;
    s.audio = $("twAudio").checked; s.map = $("twMap").checked; s.tone = $("twTone").checked; s.telegram = $("twTelegram").checked;
    s.earlier_calls = int($("twEarlier").value, 0);
    s.follow = $("twFollow").value; s.follow_mins = int($("twFollowMins").value, 30);
    return t;
  }
  function kindUi() {
    const kind = draft.when.kind, check = draft.check.kind;
    $("twCallWhen").style.display = kind === "call" ? "" : "none";
    $("twConvWhen").style.display = kind === "conversation" ? "" : "none";
    $("twIncWhen").style.display = kind === "incident" ? "" : "none";
    $("twTgRow").style.display = kind === "incident" ? "none" : "";
    $("twDigestWhen").style.display = kind === "digest" ? "" : "none";
    const asksModel = kind === "call" || kind === "incident";
    $("twCheck").style.display = asksModel ? "" : "none";
    const showBody = !asksModel || check !== "none";
    $("twCheckBody").style.display = showBody ? "" : "none";
    $("twExtract").style.display = asksModel && check === "extract" ? "" : "none";
    $("twUnavail").style.display = asksModel ? "" : "none";
    $("twEngine").style.display = asksModel ? "" : "none";
    $("twThinkWrap").style.display = asksModel && $("twEngine").value === "local" ? "" : "none";
    $("twPromptLab").textContent = !asksModel ? "What the summary should say" : check === "ask" ? "The question — answered yes (send) or no (stay quiet)" : "What to look for and how to judge it";
    $("twPrompt").placeholder = check === "ask" ? "Is this a cardiac arrest happening now — not a history of one, a training call or a cancelled response?" : "";
    $("twCheckHelp").textContent = kind === "conversation" ? "When the exchange goes quiet, the transcripts are stitched with who said what and the local model writes a hand-off note. A late transmission revises it." :
      kind === "digest" ? "On each run the transcripts from the window are rolled up and the local model says what is happening." :
      check === "none" ? "Every matching call is sent (after the quiet window below)." :
      check === "ask" ? "The model reads the transcript and your question; “no” keeps it quiet. Its one-line reason fills {ai}." :
      "The model returns the details you list as fields; the message is sent only when your conditions over them hold. Each detail is also a {token}.";
    $("twQuietField").style.display = asksModel ? "" : "none";
    $("twFollowRow").style.display = kind === "call" ? "" : "none";
    $("twFollowFor").style.display = $("twFollow").value === "off" ? "none" : "";
    $("twToneWrap").style.display = kind === "call" ? "" : "none";
    $("twAudioWrap").style.display = kind === "incident" ? "none" : "";
    $("twEarlier").style.display = kind === "call" && $("twAudio").checked ? "" : "none";
    // A map needs a place, and only a run has one.
    $("twMapWrap").style.display = kind === "incident" ? "" : "none";
    $("twChatWrap").style.display = $("twDest").value === "custom" ? "" : "none";
    const toks = [...TOKENS[kind] || TOKENS.call, ...(asksModel && check === "extract" ? draft.check.fields.filter((f) => f.key).map((f) => `{${f.key}}`) : [])];
    $("twTokens").innerHTML = toks.map((x) => `<button class="tw-token" data-tok="${esc(x)}" title="Insert">${esc(x)}</button>`).join("");
    $("twTokens").querySelectorAll("[data-tok]").forEach((b) => b.onclick = () => { const ta = $("twMessage"); const at = ta.selectionStart ?? ta.value.length; ta.value = ta.value.slice(0, at) + b.dataset.tok + ta.value.slice(ta.selectionEnd ?? at); ta.focus(); ta.selectionStart = ta.selectionEnd = at + b.dataset.tok.length; changed(); });
    renderMsgPreview();
  }
  wireSeg($("twKind"), (v) => {
    read();
    const was = draft.when.kind; draft.when.kind = v;
    if (v !== "call") draft.check.kind = "summarize";
    else if (was !== "call") draft.check.kind = "none";
    // A fresh draft takes the kind's usual message and prompt.
    if (sel === "new" && was !== v && recipes) {
      const r = recipes.find((x) => x.tripwire.when.kind === v && (v !== "call" || x.id === "words"));
      if (r) { draft.send.message = r.tripwire.send.message; if (v !== "call") draft.check.prompt = r.tripwire.check.prompt; draft.send.tone = r.tripwire.send.tone; draft.send.quiet_secs = r.tripwire.send.quiet_secs; }
    }
    fill(); changed();
  });
  wireSeg($("twCheck"), (v) => {
    read(); draft.check.kind = v;
    if (v === "extract" && !draft.check.fields.length) draft.check.fields = [{ key: "match", kind: "bool", desc: "true if this call is what the listener is looking for" }, { key: "reason", kind: "string", desc: "one short sentence" }];
    if (v === "extract" && !draft.check.conditions.length) draft.check.conditions = [{ field: "match", op: "==", value: "true" }];
    if (v !== "none" && S() && S().ollama) draft.check.if_unavailable = v === "ask" && S().ollama.fail_open ? "send" : "hold";
    fill(); changed();
  });

  /* extract: fields and conditions */
  function renderFields() {
    $("twFields").innerHTML = draft.check.fields.map((f, i) => `<div class="tw-row" data-fi="${i}"><input data-fk placeholder="name" value="${esc(f.key)}" spellcheck="false"><select data-fkind>${KINDS.map((k) => `<option ${k === f.kind ? "selected" : ""}>${k}</option>`).join("")}</select><input data-fd class="grow" placeholder="how the model should fill it" value="${esc(f.desc)}"><button class="btn ghost sm" data-fdel="${i}" title="Remove">✕</button></div>`).join("");
    $("twFields").querySelectorAll("[data-fdel]").forEach((b) => b.onclick = () => { syncRows(); draft.check.fields.splice(+b.dataset.fdel, 1); renderFields(); changed(); });
  }
  function renderConds() {
    const keys = draft.check.fields.map((f) => f.key).filter(Boolean);
    $("twConds").innerHTML = draft.check.conditions.map((c, i) => `<div class="tw-row" data-ci="${i}"><select data-cf>${[...new Set([...keys, c.field].filter(Boolean))].map((k) => `<option ${k === c.field ? "selected" : ""}>${esc(k)}</option>`).join("")}</select><select data-cop>${OPS.map((o) => `<option ${o === c.op ? "selected" : ""}>${o}</option>`).join("")}</select><input data-cv class="grow" placeholder="value" value="${esc(c.value)}"><button class="btn ghost sm" data-cdel="${i}" title="Remove">✕</button></div>`).join("") ||
      '<div class="faint">No conditions — every call it looks at is sent, with the details filled in.</div>';
    $("twConds").querySelectorAll("[data-cdel]").forEach((b) => b.onclick = () => { syncRows(); draft.check.conditions.splice(+b.dataset.cdel, 1); renderConds(); changed(); });
  }
  function syncRows() {
    if (!draft) return;
    const fr = [...$("twFields").querySelectorAll(".tw-row")];
    draft.check.fields = fr.map((r) => ({ key: r.querySelector("[data-fk]").value.trim(), kind: r.querySelector("[data-fkind]").value, desc: r.querySelector("[data-fd]").value.trim() }));
    const cr = [...$("twConds").querySelectorAll(".tw-row")];
    draft.check.conditions = cr.map((r) => ({ field: r.querySelector("[data-cf]").value, op: r.querySelector("[data-cop]").value, value: r.querySelector("[data-cv]").value.trim() }));
  }
  $("twFieldAdd").onclick = () => { syncRows(); draft.check.fields.push({ key: "", kind: "string", desc: "" }); renderFields(); renderConds(); };
  $("twCondAdd").onclick = () => { syncRows(); const k = draft.check.fields.find((f) => f.key); draft.check.conditions.push({ field: k ? k.key : "", op: "==", value: "" }); renderConds(); changed(); };
  $("twFields").addEventListener("change", () => { syncRows(); renderConds(); kindUi(); changed(); });
  $("twConds").addEventListener("change", () => changed());

  /* words from the call a tripwire was drafted from */
  function renderWords() {
    if (!words || !words.length || draft.when.kind !== "call") { $("twWords").innerHTML = ""; return; }
    const have = new Set(lines($("twPhrases").value).map((x) => x.toLowerCase()));
    $("twWords").innerHTML = `<span class="lab" style="margin:0">Words from that call — tap to listen for them</span>` + words.slice(0, 24).map(([w, n]) => `<button class="tw-word ${have.has(w) ? "on" : ""}" data-w="${esc(w)}" title="in ${n} call${n === 1 ? "" : "s"}">${esc(w)} <small>${n}</small></button>`).join("");
    $("twWords").querySelectorAll("[data-w]").forEach((b) => b.onclick = () => {
      const cur = lines($("twPhrases").value), w = b.dataset.w, i = cur.findIndex((x) => x.toLowerCase() === w);
      if (i >= 0) cur.splice(i, 1); else cur.push(w);
      $("twPhrases").value = cur.join("\n"); renderWords(); changed();
    });
  }

  /* channels */
  $("twTgs").onclick = async () => {
    if (!draft) return;
    const got = await pickChannels({ title: `Talkgroups for “${$("twName").value.trim() || "this tripwire"}”`, selected: draft.when.tgs });
    if (!got) return;
    read(); draft.when.tgs = got;
    // One system among the chosen talkgroups' traffic: scope to it.
    if (!draft.when.system && got.length) {
      const sys = new Set(activity.filter((a) => got.includes(a.tg)).map((a) => a.system).filter(Boolean));
      if (sys.size === 1) draft.when.system = [...sys][0];
    }
    fill(); changed();
  };

  /* any change: dirty marker, dependent UI, preview */
  function changed() { if (!draft) return; read(); kindUi(); markDirty(); schedulePreview(); }
  $("twEdit").addEventListener("input", (e) => { if (e.target.closest(".tw-preview")) return; if (e.target.id === "twPhrases") renderWords(); changed(); });
  $("twEdit").addEventListener("change", (e) => { if (e.target.closest(".tw-preview")) return; changed(); });

  /* ---------- save, delete, duplicate, test ---------- */
  $("twSave").onclick = async () => {
    const t = read(); if (!t) return;
    if (!t.name) { uiToast("Give it a name", "err"); $("twName").focus(); return; }
    if (t.when.kind === "call" && !t.when.tgs.length && !t.when.units.length && !t.when.phrases.length && !t.when.emergency) { uiToast("Pick talkgroups, radios or phrases first — as it is, it would fire on every call", "err"); return; }
    if (t.check.kind === "ask" || t.check.kind === "extract") { if (!t.check.prompt.trim()) { uiToast("Write the question for the AI check (or choose Send every match)", "err"); return; } }
    const next = sel === "new" ? [...list, tidy(t)] : list.map((x) => (x.id === sel ? tidy(t) : x));
    if (!(await persist(next))) return;
    const got = sel === "new" ? list[list.length - 1] : list.find((x) => x.id === sel);
    sel = got.id; draft = clone(got); saved = stable(tidy(draft));
    renderList(); fill(); uiToast(`Saved “${got.name}”`);
    if (view && !view.transcribing && (t.when.phrases.length || t.check.kind !== "none" || t.when.kind !== "call")) uiToast("Transcription is off — this tripwire needs it (Settings → Transcription)", "err");
    loadStats();
  };
  $("twDelete").onclick = async () => {
    if (sel === "new" || !draft) return;
    if (!(await uiConfirm(`Delete “${draft.name}”? Its history stays in the library badges.`, "Delete"))) return;
    if (await persist(list.filter((x) => x.id !== sel))) { draft = null; sel = null; showGallery(); }
  };
  $("twDup").onclick = async () => {
    if (!draft || !(await leaveOk())) return;
    const t = clone(list.find((x) => x.id === sel) || draft); t.id = ""; t.name = `${t.name} (copy)`; t.enabled = false;
    edit("new", t, null);
  };
  $("twTest").onclick = async () => {
    if (!draft || sel === "new") return;
    if (isDirty()) { uiToast("Save first — the test runs the saved tripwire", "err"); return; }
    if (!(await uiConfirm(`Run “${draft.name}” now against the newest call it matches? This really sends, skipping the quiet window.`, "Run it"))) return;
    try { uiToast(await invoke("tripwire_test", { id: sel })); setTimeout(loadRecent, 4000); setTimeout(loadRecent, 20000); } catch (e) { uiToast(`${e}`, "err"); }
  };

  /* ---------- the live preview ---------- */
  wireSeg($("twDays"), (v) => { days = +v; schedulePreview(0); });
  function schedulePreview(delay = 450) {
    clearTimeout(previewTimer);
    previewTimer = setTimeout(runPreview, delay);
  }
  async function runPreview() {
    if (!draft) return;
    const seq = ++previewSeq, t = tidy(read());
    $("twHeadline").classList.add("busy");
    try {
      const p = await invoke("tripwire_preview", { tripwire: t, days });
      if (seq !== previewSeq) return;
      preview = p; renderPreview();
    } catch (e) { if (seq === previewSeq) { $("twHeadline").innerHTML = `<span class="faint">${esc(String(e))}</span>`; } }
    finally { if (seq === previewSeq) $("twHeadline").classList.remove("busy"); }
  }
  function renderPreview() {
    const p = preview, t = draft; if (!p || !t) return;
    const span = p.days === 1 ? "the last day" : `the last ${p.days} days`;
    let head;
    if (t.when.kind === "digest") head = `<b>${p.messages}</b> digests in ${span}, rolling up <b>${p.scanned.toLocaleString()}</b> calls (${p.transcribed.toLocaleString()} transcribed).`;
    else if (t.when.kind === "conversation") head = `About <b>${p.messages}</b> conversations in ${span} — ${p.scanned.toLocaleString()} transmissions on these talkgroups.`;
    else {
      const perDay = p.messages / p.days;
      const thing = t.when.kind === "incident" ? "run" : "call";
      head = p.matches ? `In ${span} it would have fired on <b>${p.matches}</b> ${thing}${p.matches === 1 ? "" : "s"} → <b>${p.messages}</b> message${p.messages === 1 ? "" : "s"}${t.check.kind !== "none" ? " before the AI check" : ""} <span class="faint">(≈ ${perDay < 1 ? perDay.toFixed(1) : Math.round(perDay)} a day)</span>.`
        : t.when.kind === "incident"
          ? `Nothing in ${span} would have tripped it <span class="faint">— looked at ${p.scanned.toLocaleString()} run${p.scanned === 1 ? "" : "s"} on the dispatch map</span>.`
          : `Nothing in ${span} would have tripped it <span class="faint">— looked at ${p.scanned.toLocaleString()} call${p.scanned === 1 ? "" : "s"}, ${p.transcribed.toLocaleString()} with transcripts</span>.`;
      if (p.excepted) head += ` <span class="faint">“But not” kept ${p.excepted} out.</span>`;
    }
    $("twHeadline").innerHTML = head;
    const max = Math.max(1, ...p.per_day.map((d) => d[1]));
    $("twBars").innerHTML = p.days > 1 && p.matches ? p.per_day.map(([l, n]) => `<span class="twbar" title="${esc(l)}: ${n}"><i style="height:${n ? Math.max(8, Math.round((100 * n) / max)) : 0}%"></i><small>${esc(p.days > 7 ? l.split(" ")[1] : l.split(" ")[0])}</small></span>`).join("") : "";
    $("twWarn").innerHTML = p.warnings.map((w) => `<div class="tw-warnline">⚠ ${esc(w)}</div>`).join("");
    $("twPhraseStats").innerHTML = p.phrases.map((ph) => {
      if (ph.hits > 0) return `<span class="tw-phrase ok" title="${ph.anywhere} on any talkgroup">${esc(ph.phrase)} <small>×${ph.hits}</small></span>`;
      const sug = ph.suggestions.map((s) => `<button class="tw-sug" data-from="${esc(ph.phrase)}" data-to="${esc(s.text)}" title="Replace with what is actually heard">→ ${esc(s.text)} <small>×${s.hits}</small></button>`).join("");
      return `<span class="tw-phrase dead" title="${ph.anywhere ? `heard ${ph.anywhere}× on other talkgroups` : "never heard in this window"}">${esc(ph.phrase)} <small>${ph.anywhere ? `0 here · ${ph.anywhere} elsewhere` : "never heard"}</small></span>${sug}`;
    }).join("");
    $("twPhraseStats").querySelectorAll("[data-to]").forEach((b) => b.onclick = () => {
      const cur = lines($("twPhrases").value);
      const i = cur.findIndex((x) => x === b.dataset.from);
      if (i >= 0) cur[i] = b.dataset.to; else cur.push(b.dataset.to);
      $("twPhrases").value = cur.join("\n"); changed();
    });
    $("twSamplesLab").textContent =
      t.when.kind === "incident" ? (p.samples.length ? `Runs it would have caught${p.matches > p.samples.length ? ` · newest ${p.samples.length}` : ""}` : "")
      : t.when.kind === "call" ? (p.samples.length ? `Calls it would have caught${p.matches > p.samples.length ? ` · newest ${p.samples.length}` : ""}` : "")
      : "Recent traffic it would summarise";
    const hl = (text, kws) => { let h = esc(text); for (const k of kws || []) { const re = new RegExp(`(${k.replace(/[.*+?^${}()|[\]\\]/g, "\\$&").replace(/[\s-]+/g, "[\\s\\W]+")})`, "ig"); h = h.replace(re, "<mark>$1</mark>"); } return h; };
    $("twSamples").innerHTML = p.samples.map((s) => {
      const tr = tried.get(s.id);
      const verdict = tr ? `<div class="tw-verdict ${tr.verdict}">${tr.verdict === "send" ? "✓ would send" : tr.verdict === "quiet" ? "· would stay quiet" : "⚠ check unavailable"}${tr.note ? ` — ${esc(tr.note)}` : ""}${tr.fields ? `<code>${esc(JSON.stringify(tr.fields))}</code>` : ""}</div>` : "";
      return `<div class="tw-sample"><div class="tw-sample-h"><span class="mono">${esc(dayClock(s.start))}</span><span>${esc(s.tg_name)}</span>${s.unit_name ? `<span class="faint">${esc(s.unit_name)}</span>` : ""}${s.emergency ? '<span class="badge emg">EMERG</span>' : ""}<span class="spacer"></span>${s.audio ? `<button class="btn ghost sm" data-play="${s.id}" title="Play">▶</button>` : ""}</div><div class="tw-sample-t">${s.transcript ? hl(s.transcript, s.keywords) : '<span class="faint">no transcript</span>'}</div>${verdict}</div>`;
    }).join("") || (t.when.kind === "call" && (t.when.phrases.length || t.when.tgs.length) ? '<div class="empty small">No calls to show.</div>' : "");
    $("twSamples").querySelectorAll("[data-play]").forEach((b) => b.onclick = () => invoke("library_play", { id: +b.dataset.play }).catch((e) => uiToast(`${e}`, "err")));
    $("twTry").style.display = t.when.kind === "call" && t.check.kind !== "none" && p.samples.some((s) => s.transcript) ? "" : "none";
    renderMsgPreview();
  }
  $("twTry").onclick = async () => {
    if (!preview || !draft) return;
    const ids = preview.samples.filter((s) => s.transcript).slice(0, 5).map((s) => s.id);
    const b = $("twTry"); b.disabled = true; b.textContent = "Asking the model…";
    try { const out = await invoke("tripwire_try", { tripwire: tidy(read()), ids }); for (const r of out) tried.set(r.id, r); renderPreview(); const n = out.filter((r) => r.verdict === "send").length; uiToast(`${n} of ${out.length} would send`); }
    catch (e) { uiToast(`${e}`, "err"); }
    finally { b.disabled = false; b.textContent = "Try the check on these"; }
  };

  /* what the message will look like, filled from the newest match */
  function renderMsgPreview() {
    if (!draft) return;
    const t = draft, tpl = $("twMessage").value;
    const s = preview && preview.samples[0];
    const kind = t.when.kind;
    if ((kind !== "call" && kind !== "incident") || !tpl.trim()) { $("twMsgPrev").innerHTML = ""; return; }
    // A run message is worth previewing too: it is the one with the
    // hospitals in it, and {where} is hard to picture from the token alone.
    // The run itself is made up — a tripwire is usually written before the
    // kind of run it waits for has happened.
    if (kind === "incident") {
      const ct = (t.when.incident && t.when.incident.call_types || [])[0] || "Cardiac Arrest";
      const out = tpl.replaceAll("{name}", t.name || "Tripwire").replaceAll("{calltype}", ct)
        .replaceAll("{address}", "1400 block of Example Street").replaceAll("{units}", "Medic 21, Engine 9")
        .replaceAll("{summary}", "…what the dispatcher said…").replaceAll("{time}", "12:34:56")
        .replaceAll("{pathway}", "‹the pathway that matched›")
        .replaceAll("{maps}", "https://www.google.com/maps/search/?api=1&query=…")
        .replaceAll("{where}", "Closest hospital: Example General — 3.0 mi, 8 min by road\nECMO centre: Example Heart — 6.8 mi, 15 min by road")
        .replaceAll("{place}", "Example General").replaceAll("{nearest}", "Example Heart").replaceAll("{km}", "10.9").replaceAll("{mins}", "15")
        .replaceAll("{hospital}", "Example General").replaceAll("{report}", "…the crew's report…").replaceAll("{ai}", "");
      $("twMsgPrev").innerHTML = `<div class="lab" style="margin:0 0 3px">Looks like (with a made-up run)</div><div class="tw-bubble">${esc(out.trim())}</div><div class="faint">to ${esc(destLabel(t.send))}${t.send.map ? " · with a map of the run" : ""}${/\{address\}/.test(tpl) ? " · the address opens Google Maps" : ""}</div>`;
      return;
    }
    const tr = s && tried.get(s.id);
    const f = { tg: s ? s.tg : 1234, tgname: s ? s.tg_name : "Talkgroup", unit: s ? s.unit_name : "Medic 1", time: s ? new Date(s.start * 1000).toLocaleTimeString("en-US", { hour12: false }) : "12:34:56", transcript: s ? s.transcript : "…the transcript…", keywords: s ? (s.keywords || []).join(", ") : "" };
    let out = tpl.replaceAll("{name}", t.name || "Tripwire").replaceAll("{alert}", t.name || "Tripwire").replaceAll("{tg}", String(f.tg)).replaceAll("{tgname}", f.tgname).replaceAll("{tgdesc}", "‹description›")
      .replaceAll("{unitname}", f.unit).replaceAll("{unit}", f.unit).replaceAll("{time}", f.time).replaceAll("{secs}", "6").replaceAll("{transcript}", f.transcript).replaceAll("{keywords}", f.keywords)
      .replaceAll("{ai}", tr && tr.note ? tr.note : t.check.kind === "ask" ? "‹the model's reason›" : "").replaceAll("{json}", tr && tr.fields ? JSON.stringify(tr.fields, null, 1) : "");
    for (const fld of t.check.fields || []) { if (!fld.key) continue; const v = tr && tr.fields && tr.fields[fld.key] != null ? String(tr.fields[fld.key]) : `‹${fld.key}›`; out = out.replaceAll(`{field.${fld.key}}`, v).replaceAll(`{${fld.key}}`, v); }
    $("twMsgPrev").innerHTML = `<div class="lab" style="margin:0 0 3px">Looks like${s ? "" : " (with made-up values)"}</div><div class="tw-bubble">${esc(out.trim())}</div><div class="faint">to ${esc(destLabel(t.send))}${t.send.audio ? " · with the audio" : ""}${t.send.follow !== "off" ? ` · follow-ups for ${t.send.follow_mins} min` : ""}</div>`;
  }

  /* ---------- conversations open right now ---------- */
  async function loadLive() {
    try {
      const st = await invoke("conversations_state");
      $("twLiveEmpty").style.display = st.open.length ? "none" : ""; $("twLiveMeta").textContent = st.open.length ? `${st.open.length}` : "";
      $("twLive").innerHTML = st.open.map((c) => { const units = [...new Set(c.pieces.filter((p) => !p.fixed).map((p) => p.unit_name || p.unit))].join(", "); const age = Math.max(0, Math.round(Date.now() / 1000 - c.last_at)); return `<div class="row"><span class="grow"><b>${esc(c.rule_name)}</b> · ${esc(c.tg_name)} <small>${esc(units) || "fixed party only"}</small><br><small>${c.pieces.length} transmissions · quiet ${age}s · ${c.busy ? "summarising…" : c.sent_at ? (c.dirty ? "reopened — will revise" : `sent${c.revision ? " (rev " + c.revision + ")" : ""}`) : "open"}${c.last_error ? ` · <span style="color:var(--enc)">${esc(c.last_error)}</span>` : ""}</small></span>${c.sent_at && !c.busy ? `<button class="btn ghost sm" data-resend="${c.key}" title="Summarise again and replace the Telegram message">resend</button>` : ""}</div>`; }).join("");
      $("twLive").querySelectorAll("[data-resend]").forEach((b) => b.onclick = async () => { try { await invoke("conversation_resend", { key: +b.dataset.resend }); uiToast("Summarising again…"); } catch (e) { uiToast(`${e}`, "err"); } });
    } catch (e) { log(`conversations_state: ${e}`); }
  }

  /* ---------- what fired lately ---------- */
  async function loadRecent() {
    try {
      const rows = await invoke("events_list", { query: { limit: 40 } });
      $("twRecentEmpty").style.display = rows.length ? "none" : "";
      $("twRecentMeta").textContent = rows.length ? `${rows.length}` : "";
      $("twRecent").innerHTML = rows.map((r) => `<div class="tw-ev ${esc(r.status)}" data-rule="${esc(r.rule_id)}" title="${esc(r.message || r.detail)}"><span class="mono">${esc(clock(r.at))}</span><span class="badge tw ${esc(r.status)}">${esc(r.status)}</span><b>${esc(r.rule_name)}</b><small>${esc(r.tg_name || "")}${r.detail && r.status !== "sent" ? " · " + esc(r.detail) : r.detail && r.detail.startsWith("follow-up") ? " · follow-up" : ""}</small></div>`).join("");
      $("twRecent").querySelectorAll("[data-rule]").forEach((d) => d.onclick = () => { if (list.some((t) => t.id === d.dataset.rule)) pick(d.dataset.rule); });
    } catch (e) { log(`events_list: ${e}`); }
  }
  $("twRecentRefresh").onclick = () => { loadRecent(); loadStats(); };
  async function loadStats() { try { const v = await invoke("tripwires_get"); stats = v.stats || {}; view = v; renderList(); } catch (_) {} }

  /* ---------- share: import and export ---------- */
  const slug = (t) => (t || "tripwires").toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "").slice(0, 40) || "tripwires";
  async function importText(text, m) {
    text = String(text || "").trim();
    if (!text) { uiToast("Choose a file or paste its JSON first", "err"); return; }
    let b;
    try { b = await invoke("tripwires_import", { text }); } catch (e) { uiToast(`Refused: ${e}`, "err"); return; }
    if (m) m.close();
    const rows = b.tripwires.map((t) => `<div class="row"><span class="grow"><b>${esc(t.name)}</b><br><small>${esc(sentence(t))}</small></span></div>`).join("");
    const rv = uiModal(`<div class="eyebrow">Review before adding</div>
      <p class="msg" style="margin:8px 0 2px"><b>${esc(b.name || "Untitled")}</b>${b.author ? ` <small class="faint">by ${esc(b.author)}</small>` : ""}</p>
      ${b.description ? `<p class="help" style="white-space:pre-wrap">${esc(b.description)}</p>` : ""}
      <div class="list" style="margin:10px 0;max-height:36vh">${rows}</div>
      <p class="help">They arrive <b>off</b>, sending to your default chat. Read each prompt before you turn one on — a prompt decides what gets sent.</p>
      <div class="xport" style="justify-content:flex-end;margin:12px 0 0"><button class="btn ghost" data-no>Cancel</button><button class="btn primary" data-yes>Add ${b.tripwires.length}</button></div>`, { wide: true });
    rv.querySelector("[data-no]").onclick = rv.close;
    rv.querySelector("[data-yes]").onclick = async () => { rv.close(); if (await persist([...list, ...b.tripwires])) uiToast(`Added ${b.tripwires.length} — they are off until you turn them on`); };
  }
  $("twImport").onclick = () => {
    const m = uiModal(`<div class="eyebrow">Import tripwires</div>
      <p class="help">A <span class="mono">.json</span> file exported from another HoosierSDR (old analyzer templates work too). It is checked before anything is added: sizes and names are limited, hidden characters are stripped, and everything arrives switched off.</p>
      <div class="inline" style="margin:10px 0"><button class="btn ghost" data-pick>Choose file…</button><span class="help" data-fname></span><input type="file" data-file accept=".json,application/json" style="display:none"></div>
      <label class="field"><span class="lab">or paste the JSON</span><textarea data-paste style="min-height:120px" spellcheck="false"></textarea></label>
      <div class="xport" style="justify-content:flex-end;margin:12px 0 0"><button class="btn ghost" data-no>Cancel</button><button class="btn primary" data-yes>Review…</button></div>`, { wide: true });
    const fileIn = m.querySelector("[data-file]");
    m.querySelector("[data-pick]").onclick = () => fileIn.click();
    fileIn.onchange = () => {
      const f = fileIn.files && fileIn.files[0]; if (!f) return;
      m.querySelector("[data-fname]").textContent = `${f.name} · ${Math.ceil(f.size / 1024)} KB`;
      if (f.size > 512 * 1024) { uiToast("That file is over 512 KB — not a tripwire file", "err"); return; }
      const rd = new FileReader(); rd.onload = () => { m.querySelector("[data-paste]").value = String(rd.result || ""); }; rd.readAsText(f);
    };
    m.querySelector("[data-no]").onclick = m.close;
    m.querySelector("[data-yes]").onclick = () => importText(m.querySelector("[data-paste]").value, m);
  };
  $("twExport").onclick = () => {
    if (!list.length) { uiToast("Nothing to export yet"); return; }
    const rows = list.map((t) => `<label class="row check" style="margin:0"><input type="checkbox" data-xid="${esc(t.id)}" ${sel === t.id || !sel || sel === "new" ? "checked" : ""}> <span class="grow"><b>${esc(t.name)}</b> <small>${esc(sentence(t))}</small></span></label>`).join("");
    const m = uiModal(`<div class="eyebrow">Export tripwires</div>
      <div class="row2" style="margin-top:8px"><label class="field"><span class="lab">Name</span><input data-xname type="text" placeholder="EMS screens" spellcheck="false"></label><label class="field"><span class="lab">Author <span class="mono faint">optional</span></span><input data-xauthor type="text" spellcheck="false"></label></div>
      <label class="field"><span class="lab">Description <span class="mono faint">optional</span></span><textarea data-xdesc style="min-height:56px"></textarea></label>
      <div class="lab">Include</div><div class="list" style="max-height:30vh;margin-bottom:8px">${rows}</div>
      <label class="field"><span class="lab">Save to</span><input data-xpath type="text" spellcheck="false"></label>
      <p class="help">Where they send and whether they are on stay out of the file. Talkgroups, phrases, prompts and messages go in as written.</p>
      <div class="xport" style="justify-content:flex-end;margin:12px 0 0"><button class="btn ghost" data-no>Cancel</button><button class="btn ghost" data-copy>Copy JSON</button><button class="btn primary" data-save>Save file</button></div>`, { wide: true });
    const nameIn = m.querySelector("[data-xname]"), pathIn = m.querySelector("[data-xpath]");
    const syncPath = () => { if (!pathIn.dataset.touched) pathIn.value = `~/Downloads/${slug(nameIn.value)}.hoosier-tripwires.json`; };
    nameIn.oninput = syncPath; pathIn.oninput = () => { pathIn.dataset.touched = "1"; }; syncPath();
    const build = async () => {
      const ids = [...m.querySelectorAll("input[data-xid]:checked")].map((c) => c.dataset.xid);
      if (!ids.length) { uiToast("Tick at least one", "err"); return null; }
      try { return await invoke("tripwires_export", { ids, name: nameIn.value.trim(), author: m.querySelector("[data-xauthor]").value.trim(), description: m.querySelector("[data-xdesc]").value.trim() }); }
      catch (e) { uiToast(`${e}`, "err"); return null; }
    };
    m.querySelector("[data-no]").onclick = m.close;
    m.querySelector("[data-copy]").onclick = async () => { const t = await build(); if (!t) return; try { await navigator.clipboard.writeText(t); uiToast("Copied"); } catch (_) { uiConfirm(t, "OK"); } };
    m.querySelector("[data-save]").onclick = async () => { const t = await build(); if (!t) return; try { const p = await invoke("save_text", { path: pathIn.value.trim(), text: t }); uiToast(`Saved ${p}`); m.close(); } catch (e) { uiToast(`${e}`, "err"); } };
  };

  /* ---------- "alert me when something like this happens" ---------- */
  window.tripwireFromCall = async (id) => {
    showView("tripwires");
    if (!(await leaveOk())) return;
    try {
      const d = await invoke("tripwire_draft", { id });
      if (!recipes) { try { recipes = await invoke("tripwire_recipes"); } catch (_) { recipes = []; } }
      if (!activity.length) await loadActivity();
      edit("new", d.tripwire, d.words);
      uiToast("Drafted from that call — tap words to listen for, watch the preview, then Save");
    } catch (e) { uiToast(`${e}`, "err"); }
  };

  /* ---------- wiring ---------- */
  window.tripwiresOnShow = async () => {
    if (!activity.length) loadActivity().then(() => { if (draft) fillSystem(); });
    await load(); loadLive(); loadRecent();
    if (!sel) { if (list.length) pick(list[0].id); else showGallery(); }
  };
  // Destinations renamed or added on Connections show up in the editor.
  const prevRender = window.connectionsRender;
  window.connectionsRender = () => { if (prevRender) prevRender(); if (draft) { fillDest(); renderMsgPreview(); } if (list.length) renderList(); };
  let evTimer = null;
  if (typeof listen === "function") {
    listen("tripwires", () => { if (!visible()) return; clearTimeout(evTimer); evTimer = setTimeout(() => { loadRecent(); loadStats(); }, 800); });
    listen("conversations", () => { if (visible()) loadLive(); });
  }
  setInterval(() => { if (visible()) loadLive(); }, 10000);
  window.addEventListener("beforeunload", (e) => { if (draft && isDirty()) { e.preventDefault(); e.returnValue = ""; } });
  load();
})();
