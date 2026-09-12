/* ================= talkgroup picker ================= */
// One picker for every place a rule names talkgroups. It searches alpha tag,
// description and number; lists what was actually heard lately first, with
// counts; groups the catalog by alpha-tag prefix ("49M", "IMPD MET") since
// RadioReference exports often carry no Tag/Category; and saves named sets
// ("Hospitals") in the backend for reuse. By default it shows only the
// systems the listener follows, so TG 10202 on some other system does not
// sit beside the one they hear.
//
//   pickChannels({ title, selected: [tg…] }) → Promise<[tg…] | null>
//   channelSummary([tg…]) → "49M-M03, 49M-M02 +3 more"
(() => {
  let rows = null, rowsAt = 0, act = new Map(), actAt = 0, sets = [], followed = new Set();
  const now = () => Math.floor(Date.now() / 1000);
  // The family a talkgroup belongs to: the alpha tag up to its first "-",
  // else its first word ("49M-M03" → "49M", "IMPD MET-CO6" → "IMPD MET").
  const prefixOf = (alias) => { const a = String(alias || "").trim(); if (!a) return "?"; const d = a.indexOf("-"); if (d > 0) return a.slice(0, d).trim(); const s = a.indexOf(" "); return s > 0 ? a.slice(0, s) : a; };
  window.tgPrefixOf = prefixOf;
  const ago = (t) => { const s = Math.max(0, now() - t); return s < 90 ? "just now" : s < 3600 ? `${Math.round(s / 60)} min ago` : s < 86400 ? `${Math.round(s / 3600)} h ago` : `${Math.round(s / 86400)} d ago`; };

  async function load(force) {
    if (typeof invoke !== "function") return;
    const t = Date.now();
    if (force || !rows || t - rowsAt > 5 * 60e3) {
      try { rows = (await invoke("catalog_rows")) || []; rowsAt = t; } catch (e) { rows = rows || []; }
      try {
        const pls = (await invoke("playlists_list")) || [];
        followed = new Set(pls.map((p) => p.sid).filter(Boolean));
      } catch (_) {}
      try { sets = ((await invoke("channel_sets_get")) || {}).sets || []; } catch (_) { sets = []; }
    }
    if (force || t - actAt > 60e3) {
      try {
        const a = (await invoke("channel_activity", { hours: 24 })) || [];
        act = new Map();
        for (const x of a) { const cur = act.get(x.tg); if (!cur) act.set(x.tg, { ...x }); else { cur.calls += x.calls; cur.transcribed += x.transcribed; cur.last_at = Math.max(cur.last_at, x.last_at); } }
        actAt = t;
      } catch (_) {}
    }
  }
  // One row per talkgroup number: the followed system's name wins.
  function catalog(onlyFollowed) {
    const byTg = new Map();
    for (const r of rows || []) {
      const mine = r.sid == null || followed.has(r.sid);
      if (onlyFollowed && followed.size && !mine) continue;
      const cur = byTg.get(r.id);
      if (!cur || (mine && !cur.mine)) byTg.set(r.id, { ...r, mine });
    }
    // Heard but not in any catalog: still pickable.
    for (const [tg] of act) if (!byTg.has(tg)) byTg.set(tg, { id: tg, alias: `TG ${tg}`, description: "(not in a catalog)", source: "", mine: true });
    return [...byTg.values()];
  }
  const nameOf = (tg) => { const r = (rows || []).find((x) => x.id === tg && (x.sid == null || followed.has(x.sid))) || (rows || []).find((x) => x.id === tg); return r ? r.alias || `TG ${tg}` : `TG ${tg}`; };
  window.channelSummary = (tgs, max = 3) => {
    const list = [...new Set(tgs || [])];
    if (!list.length) return "";
    const names = list.slice(0, max).map(nameOf);
    return names.join(", ") + (list.length > max ? ` +${list.length - max} more` : "");
  };
  window.channelsWarm = () => load(false);

  window.pickChannels = (opts) => new Promise(async (resolve) => {
    await load(false);
    const sel = new Set((opts && opts.selected) || []);
    let q = "", only = followed.size > 0, open = new Set();
    const m = uiModal(`<div class="pk">
      <div class="pkhead"><span class="eyebrow">${esc((opts && opts.title) || "Choose talkgroups")}</span><span class="spacer"></span>
        <label class="check" style="margin:0" title="Hide talkgroups from catalogs of systems you don't follow"><input type="checkbox" data-only ${only ? "checked" : ""} ${followed.size ? "" : "disabled"}> only systems I follow</label></div>
      <input data-q type="text" placeholder="Search alpha tag, description or number…" spellcheck="false" autocomplete="off">
      <div class="pksel" data-sel></div>
      <div class="pklist" data-list></div>
      <div class="xport" style="justify-content:flex-end;margin:10px 0 0"><button class="btn ghost" data-saveset>Save as set…</button><span class="spacer"></span><button class="btn ghost" data-no>Cancel</button><button class="btn primary" data-yes>Done</button></div>
    </div>`, { wide: true });
    const $m = (s) => m.querySelector(s);
    const done = (v) => { m.close(); resolve(v); };
    const actText = (tg) => { const a = act.get(tg); return a ? `${a.calls}${a.calls === 1 ? " call" : " calls"} in 24 h · ${ago(a.last_at)}` : ""; };
    const row = (r) => `<label class="pkrow ${sel.has(r.id) ? "on" : ""}"><input type="checkbox" data-tg="${r.id}" ${sel.has(r.id) ? "checked" : ""}><b>${esc(r.alias || "TG " + r.id)}</b><span class="d" title="${esc(r.description)}">${esc(r.description || "")}</span><span class="n mono">TG ${r.id}</span><span class="a">${esc(actText(r.id))}</span></label>`;
    const groupHead = (key, label, members, extra) => {
      const n = members.filter((r) => sel.has(r.id)).length;
      return `<div class="pkgroup ${open.has(key) ? "open" : ""}"><div class="pkgh" data-open="${esc(key)}"><span class="car">${open.has(key) ? "▾" : "▸"}</span><b>${esc(label)}</b><span class="faint">${members.length}${n ? ` · ${n} chosen` : ""}${extra || ""}</span><span class="spacer"></span><button class="btn ghost sm" data-all="${esc(key)}">${n === members.length ? "none" : "all"}</button></div>${open.has(key) ? `<div class="pkgb">${members.map(row).join("")}</div>` : ""}</div>`;
    };
    let groups = new Map();
    function render() {
      const cat = catalog(only);
      const byTg = new Map(cat.map((r) => [r.id, r]));
      groups = new Map();
      let html = "";
      if (q) {
        const words = q.toLowerCase().split(/\s+/).filter(Boolean);
        const hits = cat.filter((r) => { const hay = `${r.id} ${r.alias} ${r.description} ${r.tag} ${r.category}`.toLowerCase(); return words.every((w) => hay.includes(w)); })
          .sort((a, b) => ((act.get(b.id) || {}).calls || 0) - ((act.get(a.id) || {}).calls || 0) || a.id - b.id);
        html = hits.length ? hits.slice(0, 400).map(row).join("") + (hits.length > 400 ? `<div class="faint small">${hits.length - 400} more — narrow the search</div>` : "") : '<div class="faint">Nothing matches.</div>';
        if (hits.length > 1 && hits.length <= 400) { groups.set("__hits", hits); html = `<div class="pkgh flat"><span class="faint">${hits.length} match${hits.length === 1 ? "" : "es"}</span><span class="spacer"></span><button class="btn ghost sm" data-all="__hits">${hits.every((r) => sel.has(r.id)) ? "none" : "all"}</button></div>` + html; }
      } else {
        const heard = [...act.values()].sort((a, b) => b.calls - a.calls).map((a) => byTg.get(a.tg)).filter(Boolean);
        if (heard.length) { groups.set("__heard", heard); html += groupHead("__heard", "Heard in the last 24 hours", heard); }
        for (const s of sets) {
          const members = s.tgs.map((tg) => byTg.get(tg) || { id: tg, alias: nameOf(tg), description: "" });
          const key = "set:" + s.id; groups.set(key, members);
          html += groupHead(key, `★ ${s.name}`, members, ` <button class="btn ghost sm" data-delset="${esc(s.id)}" title="Delete this set">✕</button>`);
        }
        const fam = new Map();
        for (const r of cat) { const p = prefixOf(r.alias); if (!fam.has(p)) fam.set(p, []); fam.get(p).push(r); }
        // Families heard lately first (busiest first), then the rest A→Z.
        const heardIn = (v) => v.reduce((n, r) => n + ((act.get(r.id) || {}).calls || 0), 0);
        const big = [...fam].filter(([, v]) => v.length > 1).sort((a, b) => heardIn(b[1]) - heardIn(a[1]) || a[0].localeCompare(b[0], undefined, { numeric: true }));
        const loose = [...fam].filter(([, v]) => v.length === 1).flatMap(([, v]) => v);
        if (big.length) html += `<div class="pksec">By alpha-tag prefix</div>`;
        for (const [p, members] of big) { members.sort((a, b) => a.id - b.id); const key = "p:" + p; groups.set(key, members); const n = heardIn(members); html += groupHead(key, p, members, (n ? ` · <span class="pkheard">${n} heard in 24 h</span>` : "") + (members[0].description ? ` · ${esc(members[0].description)}…` : "")); }
        if (loose.length) { loose.sort((a, b) => a.id - b.id); groups.set("__other", loose); html += groupHead("__other", "Other", loose); }
        if (!cat.length) html = '<div class="faint">No talkgroups known yet — load a system (Playlists) or a catalog (Aliases).</div>';
      }
      $m("[data-list]").innerHTML = html;
      wire();
    }
    function wire() {
      m.querySelectorAll("[data-tg]").forEach((c) => c.onchange = () => { const tg = +c.dataset.tg; c.checked ? sel.add(tg) : sel.delete(tg); c.closest(".pkrow").classList.toggle("on", c.checked); renderSel(); });
      m.querySelectorAll("[data-open]").forEach((h) => h.onclick = (e) => { if (e.target.closest("button")) return; const k = h.dataset.open; open.has(k) ? open.delete(k) : open.add(k); render(); });
      m.querySelectorAll("[data-all]").forEach((b) => b.onclick = () => { const members = groups.get(b.dataset.all) || []; const allOn = members.every((r) => sel.has(r.id)); for (const r of members) allOn ? sel.delete(r.id) : sel.add(r.id); render(); });
      m.querySelectorAll("[data-delset]").forEach((b) => b.onclick = async () => { const s = sets.find((x) => x.id === b.dataset.delset); if (!s || !(await uiConfirm(`Delete the set “${s.name}”? Rules that used it keep their talkgroups.`, "Delete"))) return; sets = sets.filter((x) => x.id !== s.id); try { sets = ((await invoke("channel_sets_set", { settings: { sets } })) || {}).sets || sets; } catch (e) { uiToast(`${e}`, "err"); } render(); });
      renderSel();
    }
    function renderSel() {
      const chosen = [...sel];
      $m("[data-sel]").innerHTML = chosen.length ? `<span class="faint">${chosen.length} chosen:</span> ` + chosen.slice(0, 40).map((tg) => `<span class="chip on" data-rm="${tg}" title="remove">${esc(nameOf(tg))} ✕</span>`).join("") + (chosen.length > 40 ? ` <span class="faint">+${chosen.length - 40}</span>` : "") + ` <button class="btn ghost sm" data-clear>clear</button>` : '<span class="faint">None chosen — an empty list means “any talkgroup”.</span>';
      m.querySelectorAll("[data-rm]").forEach((c) => c.onclick = () => { sel.delete(+c.dataset.rm); render(); });
      const cl = $m("[data-clear]"); if (cl) cl.onclick = () => { sel.clear(); render(); };
      $m("[data-yes]").textContent = chosen.length ? `Use ${chosen.length} talkgroup${chosen.length === 1 ? "" : "s"}` : "Any talkgroup";
    }
    $m("[data-q]").oninput = () => { q = $m("[data-q]").value.trim(); render(); };
    $m("[data-only]").onchange = () => { only = $m("[data-only]").checked; render(); };
    $m("[data-no]").onclick = () => done(null);
    $m("[data-yes]").onclick = () => done([...sel].sort((a, b) => a - b));
    $m("[data-saveset]").onclick = async () => {
      if (!sel.size) { uiToast("Choose some talkgroups first", "err"); return; }
      const name = await askName();
      if (!name) return;
      const existing = sets.find((s) => s.name.toLowerCase() === name.toLowerCase());
      if (existing) existing.tgs = [...sel]; else sets.push({ id: `s${Date.now()}`, name, tgs: [...sel] });
      try { sets = ((await invoke("channel_sets_set", { settings: { sets } })) || {}).sets || sets; uiToast(`Saved “${name}” (${sel.size})`); } catch (e) { uiToast(`${e}`, "err"); }
      render();
    };
    function askName() {
      return new Promise((res) => {
        const n = uiModal(`<div class="eyebrow">Save these ${sel.size} talkgroups as a set</div><label class="field" style="margin-top:8px"><span class="lab">Name</span><input data-n type="text" placeholder="Hospitals" spellcheck="false"></label><div class="xport" style="justify-content:flex-end;margin:12px 0 0"><button class="btn ghost" data-no>Cancel</button><button class="btn primary" data-yes>Save</button></div>`);
        const inp = n.querySelector("[data-n]"); setTimeout(() => inp.focus(), 20);
        const fin = (v) => { n.close(); res(v); };
        n.querySelector("[data-no]").onclick = () => fin(null);
        n.querySelector("[data-yes]").onclick = () => fin(inp.value.trim() || null);
        inp.onkeydown = (e) => { if (e.key === "Enter") fin(inp.value.trim() || null); };
      });
    }
    // Recent traffic open, and the family of what is already chosen, so the
    // choice shows in context.
    open.add("__heard");
    for (const tg of sel) open.add("p:" + prefixOf(nameOf(tg)));
    render();
    setTimeout(() => $m("[data-q]").focus(), 30);
  });

  // "Choose…" beside a comma-separated talkgroup field: the picker writes the
  // list back, and a line under the field names what it holds.
  const shows = {};
  window.pickerRefresh = (id) => { if (shows[id]) shows[id](); };
  window.pickerBind = (inputId, btnId, title) => {
    const inp = $(inputId), btn = $(btnId); if (!inp || !btn) return;
    let line = $(inputId + "Names");
    if (!line) { line = document.createElement("div"); line.id = inputId + "Names"; line.className = "pknames"; (inp.closest("label") || inp).insertAdjacentElement("afterend", line); }
    const parse = () => inp.value.split(/[\s,;]+/).map((x) => parseInt(x, 10)).filter(Number.isFinite);
    const show = () => { const t = parse(); line.textContent = t.length ? channelSummary(t, 6) : "any talkgroup"; };
    btn.onclick = async (e) => { if (e && e.preventDefault) e.preventDefault(); const got = await pickChannels({ title, selected: parse() }); if (got) { inp.value = got.join(", "); inp.dispatchEvent(new Event("change")); show(); } };
    inp.addEventListener("input", show); inp.addEventListener("change", show);
    load(false).then(show);
    shows[inputId] = show;
    return show;
  };
  if (typeof invoke === "function") {
    pickerBind("akTgs", "akPickTg", "Talkgroups this alert watches");
    pickerBind("azTgs", "azPickTg", "Talkgroups this analyzer watches");
    pickerBind("cvTgs", "cvPickTg", "Talkgroups where these exchanges happen");
    pickerBind("dgTgs", "dgPickTg", "Talkgroups this digest rolls up");
  }
})();
