/* The place book — hospitals, stations and landmarks, with the talkgroups
   that belong to them. Lives in the Dispatch setup pane, because that is
   where the map and the geocoder already are.

   Nothing here decides what a hospital can do. The catalog can name a
   place, and the geocoder can put it on the map, but a feature is ticked by
   the listener or it is not true. */
(() => {
  const $ = (id) => document.getElementById(id);
  const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const invoke = (cmd, args) => window.__TAURI__ ? window.__TAURI__.core.invoke(cmd, args) : window.dpInvoke(cmd, args);

  let places = [];
  let features = [];
  let sel = null;

  const KIND = { hospital: "🏥", station: "🚒", landmark: "📍", other: "•" };
  const clone = (x) => JSON.parse(JSON.stringify(x));
  const find = (id) => places.find((p) => p.id === id);

  async function load() {
    try {
      const [s, f] = await Promise.all([invoke("places_get"), invoke("place_features")]);
      places = s.places || [];
      features = f || [];
      render();
    } catch (e) { /* the panel is only shown in the app */ }
  }

  function render() {
    const box = $("plList"); if (!box) return;
    $("plMeta").textContent = places.length ? `${places.length} place${places.length === 1 ? "" : "s"} · ${places.filter((p) => p.lat != null).length} on the map` : "";
    $("plEmpty").style.display = places.length ? "none" : "";
    box.innerHTML = places.map((p) => `
      <div class="plrow ${sel === p.id ? "on" : ""} ${p.enabled ? "" : "off"}" data-id="${esc(p.id)}">
        <span class="em">${KIND[p.kind] || KIND.other}</span>
        <b>${esc(p.name)}</b>
        ${p.lat == null ? '<span class="warn sm">not on the map</span>' : ""}
        ${(p.features || []).map((f) => `<span class="feat">${esc(label(f))}</span>`).join("")}
        ${(p.tgs || []).length ? `<span class="tgs">${p.tgs.length} channel${p.tgs.length === 1 ? "" : "s"}</span>` : ""}
      </div>`).join("");
    box.querySelectorAll("[data-id]").forEach((el) => el.onclick = () => edit(el.dataset.id));
    $("plEdit").style.display = sel ? "" : "none";
    if (sel) fill();
  }

  const label = (key) => (features.find(([k]) => k === key) || [key, key])[1];

  function edit(id) {
    sel = id;
    render();
  }

  function fill() {
    const p = find(sel); if (!p) { sel = null; return; }
    $("plName").value = p.name || "";
    $("plKind").value = p.kind || "other";
    $("plAddress").value = p.address || "";
    $("plNotes").value = p.notes || "";
    $("plEnabled").checked = p.enabled !== false;
    $("plWhere").textContent = p.lat != null ? `${(+p.lat).toFixed(5)}, ${(+p.lon).toFixed(5)}` : "not on the map yet";
    $("plTgs").value = (p.tgs || []).join(", ");
    $("plSystem").value = p.system || "";
    const own = (p.features || []).filter((f) => !features.some(([k]) => k === f));
    $("plFeatures").innerHTML = features.map(([k, lab]) =>
      `<label class="check"><input type="checkbox" data-feat="${esc(k)}" ${(p.features || []).includes(k) ? "checked" : ""} /> ${esc(lab)}</label>`).join("");
    $("plOwnFeatures").value = own.join(", ");
  }

  function read() {
    const p = find(sel); if (!p) return null;
    p.name = $("plName").value.trim();
    p.kind = $("plKind").value;
    p.address = $("plAddress").value.trim();
    p.notes = $("plNotes").value.trim();
    p.enabled = $("plEnabled").checked;
    p.tgs = $("plTgs").value.split(/[^0-9]+/).filter(Boolean).map(Number).slice(0, 32);
    p.system = $("plSystem").value.trim();
    const ticked = [...$("plFeatures").querySelectorAll("[data-feat]")].filter((i) => i.checked).map((i) => i.dataset.feat);
    const own = $("plOwnFeatures").value.split(",").map((s) => s.trim().toLowerCase()).filter(Boolean);
    p.features = [...new Set([...ticked, ...own])];
    return p;
  }

  async function save(note) {
    try {
      const s = await invoke("places_set", { settings: { places } });
      places = s.places || [];
      if (sel && !find(sel)) sel = null;
      render();
      if (note) uiToast(note);
      return true;
    } catch (e) { uiToast(`${e}`, "err"); return false; }
  }

  /* ---------- wiring ---------- */
  function wire() {
    if (!$("plList")) return;
    $("plAdd").onclick = () => {
      places.push({ id: "", name: "New place", kind: "hospital", features: [], tgs: [], enabled: true });
      save().then(() => { sel = places[places.length - 1].id; render(); $("plName").select(); });
    };
    $("plSave").onclick = () => { if (!read()) return; save("Saved"); };
    $("plDelete").onclick = async () => {
      const p = find(sel); if (!p) return;
      if (!(await uiConfirm(`Delete “${p.name}”?`, "Delete"))) return;
      places = places.filter((x) => x.id !== sel); sel = null; save("Deleted");
    };
    $("plLocate").onclick = async () => {
      const p = read(); if (!p) return;
      const q = p.address || p.name;
      const b = $("plLocate"); b.disabled = true; b.textContent = "Looking…";
      try {
        const got = await invoke("place_locate", { query: q });
        if (!got) { uiToast("The geocoder didn't find that — try the street address", "err"); }
        else { p.lat = got[0]; p.lon = got[1]; if (!p.address) p.address = got[2]; await save(`Placed ${p.name}`); }
      } catch (e) { uiToast(`${e}`, "err"); }
      finally { b.disabled = false; b.textContent = "Find on the map"; }
    };
    $("plPickTgs").onclick = async (e) => {
      e.preventDefault();
      const cur = $("plTgs").value.split(/[^0-9]+/).filter(Boolean).map(Number);
      const got = await pickChannels({ title: "Channels that belong to this place", selected: cur });
      if (got) { $("plTgs").value = got.join(", "); }
    };
    $("plSuggest").onclick = suggest;
    load();
  }

  /* ---------- what the catalog already knows ---------- */
  async function suggest() {
    const b = $("plSuggest"); b.disabled = true;
    try {
      const list = await invoke("places_suggest", {});
      if (!list.length) { uiToast("Nothing new — every hospital channel your catalog names is already here"); return; }
      const picked = await ask(list);
      if (!picked || !picked.length) return;
      for (const s of picked) {
        places.push({ id: "", name: s.name, kind: s.kind, features: [], tgs: [s.tg], system: s.system, enabled: true, notes: `From the catalog: ${s.from}` });
      }
      await save(`Added ${picked.length} place${picked.length === 1 ? "" : "s"} — now find each on the map and tick what it can do`);
    } catch (e) { uiToast(`${e}`, "err"); }
    finally { b.disabled = false; }
  }

  // System names off RadioReference are a mouthful ("… (Formerly IDPS)");
  // the part before the first bracket is the name people use.
  const shortSystem = (s) => String(s).split("(")[0].trim() || s;

  // The app's own modal, so this looks like every other sheet: everything
  // ticked, the listener unticks what they don't want, and each row shows
  // the catalog line it was read from so they can judge it.
  function ask(list) {
    return new Promise((resolve) => {
      const html = `
        <div class="xport" style="margin:0 0 10px"><b>Places your catalog names</b><span class="spacer"></span><span class="faint sm">${list.length} hospital channel${list.length === 1 ? "" : "s"}</span></div>
        <div class="plsuggest">${list.map((s, i) => `
          <label class="plsug"><input type="checkbox" data-i="${i}" checked />
            <span class="who"><b>${esc(s.name)}</b><span class="mono faint">${esc(s.tg_name)}${s.system ? " · " + esc(shortSystem(s.system)) : ""}</span></span>
            <span class="from">${esc(s.from)}</span></label>`).join("")}</div>
        <div class="xport" style="margin:12px 0 0"><span class="faint sm">Each arrives unplaced, with nothing ticked about what it can do.</span><span class="spacer"></span>
          <button class="btn ghost" data-no>Cancel</button><button class="btn primary" data-yes>Add these</button></div>`;
      const wrap = uiModal(html, { wide: true });
      const close = (v) => { wrap.close(); resolve(v); };
      wrap.querySelector("[data-no]").onclick = () => close(null);
      wrap.querySelector("[data-yes]").onclick = () => close(
        [...wrap.querySelectorAll("[data-i]")].filter((i) => i.checked).map((i) => list[+i.dataset.i]));
      wrap.onclick = (e) => { if (e.target === wrap) close(null); };
    });
  }

  window.placesReload = load;
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", wire);
  else wire();
})();
