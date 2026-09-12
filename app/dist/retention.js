/* ================= retention: what the call library keeps ================= */
// The policy lives in the backend (retention.json) and runs on its hourly
// timer; this page edits it, previews exactly what a run would do, and
// shows where the disk goes. The old one-number setting in local storage is
// carried over once — "0" meant "delete every unstarred call at each
// start", so that one is not applied silently.
(() => {
  if (typeof invoke !== "function") return;
  let rt = null, rules = [];
  const intOrNull = (v) => { const n = parseInt(String(v).trim(), 10); return Number.isFinite(n) && n > 0 ? n : null; };
  const numOr = (v, d) => { const n = parseFloat(String(v).trim()); return Number.isFinite(n) && n > 0 ? n : d; };
  const gb = (b) => b >= 1e9 ? `${(b / 1e9).toFixed(1)} GB` : b >= 1e6 ? `${Math.round(b / 1e6)} MB` : `${Math.round(b / 1e3)} KB`;
  const days = (d) => d == null ? "forever" : `${d} day${d === 1 ? "" : "s"}`;

  function fill() {
    if (!rt) return;
    $("rtEnabled").checked = rt.enabled;
    $("rtRecord").value = rt.record_days ?? ""; $("rtAudio").value = rt.audio_days ?? "";
    $("rtStarred").checked = rt.keep_starred; $("rtFired").checked = rt.keep_fired; $("rtIncidents").checked = rt.keep_incidents;
    $("rtShortOn").checked = rt.min_audio_secs > 0; if (rt.min_audio_secs > 0) $("rtShort").value = rt.min_audio_secs;
    $("rtNoAudioOn").checked = rt.no_audio_hours != null; if (rt.no_audio_hours != null) $("rtNoAudio").value = rt.no_audio_hours;
    $("rtEncOn").checked = rt.encrypted_hours != null; if (rt.encrypted_hours != null) $("rtEnc").value = rt.encrypted_hours;
    $("rtCapOn").checked = rt.max_audio_gb != null; if (rt.max_audio_gb != null) $("rtCap").value = rt.max_audio_gb;
    $("rtHistory").value = rt.history_days;
    rules = (rt.rules || []).map((r) => ({ ...r }));
    renderRules();
    $("rtNotice").style.display = rt.notice ? "" : "none"; $("rtNotice").textContent = rt.notice || "";
    $("rtMeta").textContent = rt.enabled ? `on · calls ${days(rt.record_days)}` : "off";
  }
  function read() {
    return { ...rt, enabled: $("rtEnabled").checked, record_days: intOrNull($("rtRecord").value), audio_days: intOrNull($("rtAudio").value),
      keep_starred: $("rtStarred").checked, keep_fired: $("rtFired").checked, keep_incidents: $("rtIncidents").checked,
      min_audio_secs: $("rtShortOn").checked ? numOr($("rtShort").value, 1.5) : 0,
      no_audio_hours: $("rtNoAudioOn").checked ? intOrNull($("rtNoAudio").value) || 24 : null,
      encrypted_hours: $("rtEncOn").checked ? intOrNull($("rtEnc").value) || 24 : null,
      max_audio_gb: $("rtCapOn").checked ? numOr($("rtCap").value, 20) : null,
      history_days: intOrNull($("rtHistory").value) || 180,
      rules, notice: "" };
  }
  function renderRules() {
    $("rtRulesEmpty").style.display = rules.length ? "none" : "";
    $("rtRules").innerHTML = rules.map((r, i) => `<div class="row rtrule"><span class="grow"><input class="rtname" data-rn="${i}" value="${esc(r.name)}" spellcheck="false"> <button class="btn ghost sm" data-rpick="${i}">${r.tgs.length ? esc(channelSummary(r.tgs, 3)) : "choose talkgroups…"}</button><br>
      <span class="inline rtrulefields">calls <input class="rtnum" data-rrec="${i}" value="${r.record_days ?? ""}" placeholder="∞"> days · audio <input class="rtnum" data-raud="${i}" value="${r.audio_days ?? ""}" placeholder="∞"> days</span></span><button class="btn ghost sm" data-rdel="${i}" title="Remove">✕</button></div>`).join("");
    const box = $("rtRules");
    box.querySelectorAll("[data-rn]").forEach((x) => x.onchange = () => { rules[+x.dataset.rn].name = x.value.trim(); });
    box.querySelectorAll("[data-rrec]").forEach((x) => x.onchange = () => { rules[+x.dataset.rrec].record_days = intOrNull(x.value); });
    box.querySelectorAll("[data-raud]").forEach((x) => x.onchange = () => { rules[+x.dataset.raud].audio_days = intOrNull(x.value); });
    box.querySelectorAll("[data-rdel]").forEach((x) => x.onclick = () => { rules.splice(+x.dataset.rdel, 1); renderRules(); });
    box.querySelectorAll("[data-rpick]").forEach((x) => x.onclick = async () => { const r = rules[+x.dataset.rpick]; const got = await pickChannels({ title: `Talkgroups for “${r.name}”`, selected: r.tgs }); if (got) { r.tgs = got; renderRules(); } });
  }
  $("rtRuleAdd").onclick = async () => {
    const got = await pickChannels({ title: "Talkgroups for the new rule", selected: [] });
    if (!got || !got.length) return;
    rules.push({ id: "", name: channelSummary(got, 2), tgs: got, record_days: 90, audio_days: 30 });
    renderRules();
  };
  async function save() {
    try { rt = await invoke("retention_set", { settings: read() }); fill(); return true; }
    catch (e) { uiToast(`Could not save retention: ${e}`, "err"); return false; }
  }
  $("rtSave").onclick = async () => { if (await save()) uiToast(rt.enabled ? "Retention saved — cleanup runs every hour" : "Retention saved — automatic cleanup is off"); };
  $("rtPreview").onclick = async () => {
    $("rtPreviewOut").textContent = "working it out…";
    try { const p = await invoke("retention_preview", { settings: read() }); $("rtPreviewOut").innerHTML = `${esc(p.summary)}${p.reasons.length ? `<br><small class="faint">${p.reasons.map(([why, n]) => `${n} ${esc(why)}`).join(" · ")}</small>` : ""}`; }
    catch (e) { $("rtPreviewOut").textContent = ""; uiToast(`${e}`, "err"); }
  };
  $("rtApply").onclick = async () => {
    let p; try { p = await invoke("retention_preview", { settings: read() }); } catch (e) { uiToast(`${e}`, "err"); return; }
    if (!p.calls && !p.recordings) { uiToast("Nothing to clean up right now"); return; }
    if (!(await uiConfirm(`${p.summary} Save these settings and clean up now? This cannot be undone.`, "Clean up"))) return;
    if (!(await save())) return;
    try { const msg = await invoke("retention_apply"); uiToast(msg); usage(); if (window.libStatsRefresh) window.libStatsRefresh(); $("rtPreviewOut").textContent = ""; }
    catch (e) { uiToast(`${e}`, "err"); }
  };

  async function usage() {
    try {
      const u = await invoke("retention_usage");
      const bar = (b, max) => `<span class="rtbar"><span style="width:${max ? Math.max(2, Math.round((100 * b) / max)) : 0}%"></span></span>`;
      const maxTg = Math.max(1, ...u.by_tg.map((x) => x[3])), maxAge = Math.max(1, ...u.by_age.map((x) => x[2]));
      const fill_days = u.bytes_per_day > 0 && u.free_disk_bytes > 0 ? Math.floor(u.free_disk_bytes / u.bytes_per_day) : null;
      $("rtUsage").innerHTML = `
        <div class="rtstat"><b>${gb(u.audio_bytes)}</b> of audio in ${u.with_audio.toLocaleString()} recordings · <b>${gb(u.db_bytes)}</b> database · ${u.calls.toLocaleString()} calls${u.oldest ? ` since ${esc(new Date(u.oldest * 1000).toLocaleDateString("en-US", { month: "short", day: "numeric" }))}` : ""}</div>
        <div class="rtstat faint">≈ ${gb(u.bytes_per_day)} a day lately · ${gb(u.free_disk_bytes)} free on the disk${fill_days != null ? ` (≈ ${fill_days > 3650 ? "10+ years" : fill_days + " days"} at this rate)` : ""} · ${u.protected_calls.toLocaleString()} protected calls</div>
        <div class="k" style="margin-top:8px">By age</div>
        ${u.by_age.map(([l, n, b]) => `<div class="rtline"><span class="l">${esc(l)}</span>${bar(b, maxAge)}<span class="v">${gb(b)} · ${n.toLocaleString()}</span></div>`).join("")}
        <div class="k" style="margin-top:8px">Largest talkgroups</div>
        ${u.by_tg.map(([tg, name, n, b]) => `<div class="rtline"><span class="l" title="TG ${tg}">${esc(name || "TG " + tg)}</span>${bar(b, maxTg)}<span class="v">${gb(b)} · ${n.toLocaleString()}</span></div>`).join("")}`;
    } catch (e) { $("rtUsage").innerHTML = `<span class="faint">${esc(String(e))}</span>`; }
  }
  $("rtUsageRefresh").onclick = usage;

  (async () => {
    try { rt = await invoke("retention_get"); } catch (e) { log(`retention_get: ${e}`); return; }
    if (!rt.migrated) {
      // The old page setting: days in local storage ("" = never).
      let old = null; try { const v = JSON.parse(localStorage.getItem("hs.retention")); const d = parseInt(v, 10); if (Number.isFinite(d) && d >= 0) old = d; } catch (_) {}
      try { rt = await invoke("retention_migrate", { days: old }); try { localStorage.removeItem("hs.retention"); } catch (_) {} } catch (e) { log(`retention_migrate: ${e}`); }
    }
    fill();
  })();
  const prev = window.libraryOnShow;
  window.libraryOnShow = () => { if (prev) prev(); usage(); };
  if (typeof listen === "function") listen("retention", (e) => { logEvent(`library cleanup: ${e.payload}`); if (settingsPageVisible("library")) usage(); });
})();
