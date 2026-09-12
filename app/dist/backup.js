/* ================= backups: a copy that survives this machine ============= */
// The policy lives in the backend (backup.json) and runs on its own timer;
// this page edits it, tests a destination before the first real backup, and
// drives a restore. Two things it is careful about: the passphrase, which is
// the only way back into a sealed archive, and the access keys, which go
// into the secret store and never come back out to the page.
(() => {
  if (typeof invoke !== "function") return;
  let bk = null, dests = [], keys = {}, passSet = false, pending = null;
  const uid = () => `d${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`;
  const intOr0 = (v) => { const n = parseInt(String(v).trim(), 10); return Number.isFinite(n) && n > 0 ? n : 0; };
  const gb = (b) => b >= 1e9 ? `${(b / 1e9).toFixed(2)} GB` : b >= 1e6 ? `${(b / 1e6).toFixed(1)} MB` : `${Math.round(b / 1e3)} KB`;
  const when = (t) => t ? new Date(t * 1000).toLocaleString() : "never";

  // The presets only fill the endpoint in; every one of them is the same
  // code path underneath.
  const PRESETS = {
    aws: { name: "Amazon S3", endpoint: "https://s3.us-east-2.amazonaws.com", region: "us-east-2", path_style: false,
      help: "Endpoint is https://s3.<region>.amazonaws.com. The key needs s3:PutObject, GetObject, ListBucket and DeleteObject on this bucket." },
    supabase: { name: "Supabase Storage", endpoint: "https://YOUR-PROJECT.supabase.co/storage/v1/s3", region: "us-east-1", path_style: true,
      help: "Project Settings → Storage → S3 access keys. The endpoint and region are shown there; the bucket is the one you made in Storage." },
    b2: { name: "Backblaze B2", endpoint: "https://s3.us-west-004.backblazeb2.com", region: "us-west-004", path_style: false,
      help: "Use an application key scoped to the one bucket. The endpoint and region are on the bucket's page." },
    r2: { name: "Cloudflare R2", endpoint: "https://YOUR-ACCOUNT.r2.cloudflarestorage.com", region: "auto", path_style: true,
      help: "R2 → Manage API tokens. The region is auto." },
    wasabi: { name: "Wasabi", endpoint: "https://s3.us-east-1.wasabisys.com", region: "us-east-1", path_style: false, help: "" },
    minio: { name: "MinIO or another S3 server", endpoint: "http://192.168.1.10:9000", region: "us-east-1", path_style: true,
      help: "Your own server on the network. Path style is required." },
  };

  function fill() {
    if (!bk) return;
    $("bkTier").value = bk.tier || "kept";
    $("bkEvery").value = bk.every_hours || "";
    dests = (bk.destinations || []).map((d) => ({ ...d }));
    renderDests();
    const on = dests.filter((d) => d.enabled).length;
    const last = Object.values(bk.last || {}).filter((o) => o.ok).map((o) => o.at).sort().pop();
    $("bkMeta").textContent = !dests.length ? "not set up" : `${on} of ${dests.length} on · last ${when(last)}`;
    const p = $("bkPending");
    if (pending) {
      p.style.display = "";
      p.innerHTML = `A restore from <b>${esc(pending.from)}</b> is staged and goes in at the next start. <button class="btn ghost sm" id="bkCancelRestore">Cancel it</button>`;
      $("bkCancelRestore").onclick = async () => {
        if (!await uiConfirm("Throw away the staged restore? The library stays as it is now.", "Cancel restore")) return;
        try { await invoke("backup_cancel_restore"); uiToast("The staged restore was thrown away."); await load(); }
        catch (e) { uiToast(`Could not cancel: ${e}`, "err"); }
      };
    } else p.style.display = "none";
  }

  function read() {
    return { ...bk, tier: $("bkTier").value, every_hours: intOr0($("bkEvery").value), destinations: dests };
  }

  function renderDests() {
    $("bkEmpty").style.display = dests.length ? "none" : "";
    $("bkDests").innerHTML = dests.map((d, i) => {
      const s3 = d.kind === "s3";
      const o = (bk.last || {})[d.id];
      const state = !o ? "<span class='faint'>never run</span>"
        : o.ok ? `<span class="ok">${esc(o.name)} · ${gb(o.bytes)} · ${esc(when(o.at))}</span>`
        : `<span class="bad" title="${esc(o.error)}">failed ${esc(when(o.at))} — ${esc(String(o.error).slice(0, 120))}</span>`;
      return `<div class="panel bkdest"><div class="body">
        <div class="row2">
          <label class="field"><span class="lab">Name</span><input data-bkname="${i}" value="${esc(d.name)}" spellcheck="false" /></label>
          <label class="field"><span class="lab">Keep the last <span class="mono faint">backups · blank = all</span></span><input data-bkkeep="${i}" value="${d.keep || ""}" placeholder="all" inputmode="numeric" /></label>
        </div>
        ${s3 ? `
        <div class="row2">
          <label class="field"><span class="lab">Endpoint</span><input data-bkendpoint="${i}" value="${esc(d.endpoint)}" spellcheck="false" placeholder="https://s3.us-east-2.amazonaws.com" /></label>
          <label class="field"><span class="lab">Region</span><input data-bkregion="${i}" value="${esc(d.region)}" spellcheck="false" /></label>
        </div>
        <div class="row2">
          <label class="field"><span class="lab">Bucket</span><input data-bkbucket="${i}" value="${esc(d.bucket)}" spellcheck="false" /></label>
          <label class="field"><span class="lab">Folder in the bucket <span class="mono faint">optional</span></span><input data-bkprefix="${i}" value="${esc(d.prefix)}" spellcheck="false" placeholder="hoosier/" /></label>
        </div>
        <div class="inline" style="gap:16px;flex-wrap:wrap">
          <label class="check" style="margin:0"><input type="checkbox" data-bkpath="${i}" ${d.path_style ? "checked" : ""} /> Bucket in the path <span class="faint">(Supabase, R2, MinIO)</span></label>
          <span class="${keys[d.id] ? "ok" : "bad"}">${keys[d.id] ? "access key saved" : "no access key yet"}</span>
          <button class="btn ghost sm" data-bkkey="${i}">${keys[d.id] ? "Replace key…" : "Add access key…"}</button>
        </div>
        <p class="help">Encrypted before it leaves, always — a bucket is somewhere else's disk.</p>`
        : `
        <label class="field"><span class="lab">Folder <span class="mono faint">an external disk, or a mounted share</span></span><input data-bkpath2="${i}" value="${esc(d.path)}" spellcheck="false" placeholder="/Volumes/backups/hoosier" /></label>
        <label class="check"><input type="checkbox" data-bkenc="${i}" ${d.encrypt ? "checked" : ""} /> Encrypt it <span class="faint">(leave on unless you need to open the archive with plain tar)</span></label>`}
        <div class="inline" style="margin-top:6px;gap:16px;flex-wrap:wrap">
          <label class="check" style="margin:0"><input type="checkbox" data-bkon="${i}" ${d.enabled ? "checked" : ""} /> Include in automatic backups</label>
          <span class="spacer"></span>
          <button class="btn ghost sm" data-bktest="${i}">Test</button>
          <button class="btn ghost sm" data-bkrun="${i}">Back up now</button>
          <button class="btn ghost sm" data-bkbrowse="${i}">Backups…</button>
          <button class="btn ghost sm" data-bkdel="${i}" title="Remove this destination">✕</button>
        </div>
        <div class="small" data-bkstate="${i}" style="margin-top:4px">${state}</div>
      </div></div>`;
    }).join("");
    const box = $("bkDests");
    box.querySelectorAll("[data-bkname]").forEach((x) => x.onchange = () => { dests[+x.dataset.bkname].name = x.value.trim(); });
    box.querySelectorAll("[data-bkkeep]").forEach((x) => x.onchange = () => { dests[+x.dataset.bkkeep].keep = intOr0(x.value); });
    box.querySelectorAll("[data-bkendpoint]").forEach((x) => x.onchange = () => { dests[+x.dataset.bkendpoint].endpoint = x.value.trim(); });
    box.querySelectorAll("[data-bkregion]").forEach((x) => x.onchange = () => { dests[+x.dataset.bkregion].region = x.value.trim(); });
    box.querySelectorAll("[data-bkbucket]").forEach((x) => x.onchange = () => { dests[+x.dataset.bkbucket].bucket = x.value.trim(); });
    box.querySelectorAll("[data-bkprefix]").forEach((x) => x.onchange = () => { dests[+x.dataset.bkprefix].prefix = x.value.trim(); });
    box.querySelectorAll("[data-bkpath]").forEach((x) => x.onchange = () => { dests[+x.dataset.bkpath].path_style = x.checked; });
    box.querySelectorAll("[data-bkpath2]").forEach((x) => x.onchange = () => { dests[+x.dataset.bkpath2].path = x.value.trim(); });
    box.querySelectorAll("[data-bkenc]").forEach((x) => x.onchange = () => { dests[+x.dataset.bkenc].encrypt = x.checked; });
    box.querySelectorAll("[data-bkon]").forEach((x) => x.onchange = () => { dests[+x.dataset.bkon].enabled = x.checked; });
    box.querySelectorAll("[data-bkdel]").forEach((x) => x.onclick = async () => {
      const d = dests[+x.dataset.bkdel];
      if (!await uiConfirm(`Stop backing up to “${d.name}”? Backups already there are left alone.`, "Remove")) return;
      dests.splice(+x.dataset.bkdel, 1); renderDests(); await save();
    });
    box.querySelectorAll("[data-bkkey]").forEach((x) => x.onclick = () => askKey(dests[+x.dataset.bkkey]));
    box.querySelectorAll("[data-bktest]").forEach((x) => x.onclick = async () => {
      const i = +x.dataset.bktest, d = dests[i], out = box.querySelector(`[data-bkstate="${i}"]`);
      if (!await save()) return;
      out.textContent = "Testing…";
      try { out.innerHTML = `<span class="ok">${esc(await invoke("backup_check", { dest: d.id }))}</span>`; }
      catch (e) { out.innerHTML = `<span class="bad">${esc(String(e))}</span>`; }
    });
    box.querySelectorAll("[data-bkrun]").forEach((x) => x.onclick = async () => {
      const d = dests[+x.dataset.bkrun];
      if (!await save()) return;
      try { await invoke("backup_run", { dest: d.id }); uiToast(`Backing up to ${d.name}…`); }
      catch (e) { uiToast(`Could not start: ${e}`, "err"); }
    });
    box.querySelectorAll("[data-bkbrowse]").forEach((x) => x.onclick = () => browse(dests[+x.dataset.bkbrowse]));
  }

  // The access key goes straight to the secret store. It is never read back
  // into the page, so the field always starts empty.
  function askKey(d) {
    const m = uiModal(`<div class="head"><span class="eyebrow">Access key for ${esc(d.name)}</span></div><div class="body">
      <label class="field"><span class="lab">Access key ID</span><input id="bkKeyId" spellcheck="false" autocomplete="off" /></label>
      <label class="field"><span class="lab">Secret access key</span><input id="bkKeySecret" type="password" spellcheck="false" autocomplete="off" /></label>
      <p class="help">Kept in this computer's secret store, the same place as the Telegram token — never in a settings file and never in a backup.</p>
      <div class="xport" style="justify-content:space-between;margin-top:10px">
        <button class="btn ghost" id="bkKeyForget">Forget the saved key</button>
        <span><button class="btn ghost" id="bkKeyCancel">Cancel</button> <button class="btn primary" id="bkKeySave">Save</button></span>
      </div></div>`, { wide: false });
    $("bkKeyCancel").onclick = () => m.close();
    $("bkKeySave").onclick = async () => {
      try {
        await invoke("backup_credentials", { dest: d.id, access: $("bkKeyId").value, secret: $("bkKeySecret").value });
        m.close(); uiToast("Key saved."); await load();
      } catch (e) { uiToast(`Could not save the key: ${e}`, "err"); }
    };
    $("bkKeyForget").onclick = async () => {
      try { await invoke("backup_credentials", { dest: d.id, access: "", secret: "" }); m.close(); uiToast("Key forgotten."); await load(); }
      catch (e) { uiToast(`Could not forget it: ${e}`, "err"); }
    };
  }

  // Without the passphrase a sealed archive is a wall of noise, so it is
  // shown once, plainly, and the listener says they have it. That
  // acknowledgement is what lets the first sealed backup go out.
  async function passphrase() {
    let text = "";
    try { text = await invoke("backup_passphrase", { set: "", show: true }); }
    catch (e) { uiToast(`${e}`, "err"); return; }
    // The backend refuses to hand the passphrase over the network, so on a
    // phone or another machine it comes back empty. It can still be set
    // from there; it just cannot be read anywhere but at the computer.
    const shown = text
      ? `<div class="bkpass mono" id="bkPassText">${esc(text)}</div>`
      : `<div class="bkpass" id="bkPassText">Open the app on the computer itself to see the passphrase — it is not sent over the network.</div>`;
    const m = uiModal(`<div class="head"><span class="eyebrow">Backup passphrase</span></div><div class="body">
      <p>This is the only way back into an encrypted backup. It is not sent anywhere, and nobody — not this app, not the place you back up to — can recover it for you.</p>
      ${shown}
      <p class="help"><b>Write it down somewhere that is not this computer.</b> A backup is for the day this computer is gone, and the passphrase has to outlive it too.</p>
      <label class="field"><span class="lab">Or use one of your own <span class="mono faint">12 characters or more · replaces the above</span></span><input id="bkPassOwn" spellcheck="false" autocomplete="off" placeholder="leave blank to keep the one above" /></label>
      <p class="help">Changing it does not re-encrypt the backups you already have — keep the old one as long as you want to be able to open them.</p>
      <div class="xport" style="justify-content:flex-end;margin-top:10px">
        <button class="btn ghost" id="bkPassCancel">Close</button>
        <button class="btn primary" id="bkPassOk">I have written it down</button>
      </div></div>`, { wide: false });
    $("bkPassCancel").onclick = () => m.close();
    $("bkPassOk").onclick = async () => {
      const own = $("bkPassOwn").value.trim();
      try {
        if (own) await invoke("backup_passphrase", { set: own, show: false });
        bk = await invoke("backup_set", { settings: { ...read(), passphrase_ack: true } });
        m.close(); await load(); uiToast("Saved. Encrypted backups can go out now.");
      } catch (e) { uiToast(`${e}`, "err"); }
    };
  }

  // What is actually at a destination, and the way back from it.
  async function browse(d) {
    const m = uiModal(`<div class="head"><span class="eyebrow">Backups at ${esc(d.name)}</span></div><div class="body" id="bkListBody"><span class="faint">Looking…</span></div>`, { wide: true });
    let list = [];
    try { list = await invoke("backup_list", { dest: d.id }); }
    catch (e) { $("bkListBody").innerHTML = `<span class="bad">${esc(String(e))}</span>`; return; }
    if (!list.length) { $("bkListBody").innerHTML = `<div class="empty small">Nothing there yet. Press <b>Back up now</b> to make the first one.</div>`; return; }
    $("bkListBody").innerHTML = `<div class="list">${list.map((a, i) => `<div class="row">
      <span class="grow"><span class="mono">${esc(a.name)}</span> <span class="faint">${gb(a.bytes)}${a.encrypted ? " · encrypted" : ""}${a.at ? ` · ${esc(when(a.at))}` : ""}</span></span>
      <button class="btn ghost sm" data-bkwhat="${i}">What's in it</button>
      <button class="btn ghost sm" data-bkrest="${i}">Restore…</button>
      <button class="btn ghost sm" data-bkdrop="${i}" title="Delete this backup">✕</button></div>`).join("")}</div>
      <div class="small faint" style="margin-top:6px" id="bkListOut"></div>`;
    const body = $("bkListBody");
    body.querySelectorAll("[data-bkwhat]").forEach((x) => x.onclick = async () => {
      const a = list[+x.dataset.bkwhat];
      $("bkListOut").textContent = "Reading the manifest…";
      try {
        const mf = await invoke("backup_peek", { dest: d.id, name: a.name });
        $("bkListOut").innerHTML = `<b>${esc(mf.name)}</b> · ${esc(mf.tier)} · ${mf.calls} calls · ${mf.recordings} recordings · ${mf.config_files} settings files · ${gb(mf.bytes)} unpacked`
          + (mf.redacted && mf.redacted.length ? `<br>Blanked on the way in: <span class="mono">${mf.redacted.map(esc).join(", ")}</span>` : "")
          + `<br><span class="faint">${esc(mf.note)}</span>`;
      } catch (e) { $("bkListOut").innerHTML = `<span class="bad">${esc(String(e))}</span>`; }
    });
    body.querySelectorAll("[data-bkdrop]").forEach((x) => x.onclick = async () => {
      const a = list[+x.dataset.bkdrop];
      if (!await uiConfirm(`Delete ${a.name}? This cannot be undone.`, "Delete")) return;
      try { await invoke("backup_forget", { dest: d.id, name: a.name }); m.close(); uiToast("Deleted."); browse(d); }
      catch (e) { uiToast(`Could not delete it: ${e}`, "err"); }
    });
    body.querySelectorAll("[data-bkrest]").forEach((x) => x.onclick = () => { m.close(); restore(d, list[+x.dataset.bkrest]); });
  }

  function restore(d, a) {
    const m = uiModal(`<div class="head"><span class="eyebrow">Restore from ${esc(a.name)}</span></div><div class="body">
      <p>Nothing that is open is overwritten. The database and settings are put beside the live ones and swapped in at the next start — what is there now is kept, so a restore from the wrong backup is not the end of the story.</p>
      <div class="inline" style="gap:16px;flex-wrap:wrap">
        <label class="check" style="margin:0"><input type="checkbox" id="bkRestDb" checked /> The call database</label>
        <label class="check" style="margin:0"><input type="checkbox" id="bkRestCfg" checked /> Settings and talkgroup catalogs</label>
        <label class="check" style="margin:0"><input type="checkbox" id="bkRestAudio" checked /> Recordings <span class="faint">(only ones missing here)</span></label>
      </div>
      ${a.encrypted ? `<label class="field" style="margin-top:8px"><span class="lab">Passphrase <span class="mono faint">blank = the one saved on this computer</span></span><input id="bkRestPass" spellcheck="false" autocomplete="off" placeholder="the saved one" /></label>` : ""}
      <div class="small" id="bkRestOut" style="margin-top:8px"></div>
      <div class="xport" style="justify-content:flex-end;margin-top:10px">
        <button class="btn ghost" id="bkRestCancel">Cancel</button>
        <button class="btn primary" id="bkRestGo">Restore</button>
      </div></div>`, { wide: true });
    $("bkRestCancel").onclick = () => m.close();
    $("bkRestGo").onclick = async () => {
      $("bkRestGo").disabled = true;
      $("bkRestOut").textContent = "Unpacking — a big archive takes a while…";
      try {
        const r = await invoke("backup_restore", {
          dest: d.id, name: a.name, passphrase: (($("bkRestPass") || {}).value || ""),
          database: $("bkRestDb").checked, config: $("bkRestCfg").checked, audio: $("bkRestAudio").checked,
        });
        const bad = r.bad && r.bad.length;
        $("bkRestOut").innerHTML = `<span class="${bad ? "bad" : "ok"}">${r.database ? "database · " : ""}${r.config_files} settings files · ${r.recordings} recordings</span><br>${esc(r.note)}`
          + (bad ? `<br><span class="mono">${r.bad.map(esc).join(", ")}</span>` : "");
        await load();
      } catch (e) { $("bkRestOut").innerHTML = `<span class="bad">${esc(String(e))}</span>`; }
      $("bkRestGo").disabled = false;
    };
  }

  function addDest(kind, preset) {
    const p = preset ? PRESETS[preset] : null;
    dests.push({
      id: uid(), name: p ? p.name : "Backup folder", kind,
      path: "", endpoint: p ? p.endpoint : "", region: p ? p.region : "us-east-1",
      bucket: "", prefix: "", path_style: p ? p.path_style : false,
      // A folder on a shared drive is somewhere people browse, so this
      // starts on there too; unlike a bucket, it can be turned off.
      encrypt: true, keep: 7, enabled: true, tier: "",
    });
    renderDests();
    if (p && p.help) uiToast(p.help);
  }

  async function save() {
    try { bk = await invoke("backup_set", { settings: read() }); fill(); return true; }
    catch (e) { uiToast(`Could not save the backup settings: ${e}`, "err"); return false; }
  }

  async function load() {
    try {
      const v = await invoke("backup_get");
      bk = v.settings; keys = v.keys || {}; passSet = !!v.passphrase_set; pending = v.pending || null;
      fill();
    } catch (e) { log(`backup_get: ${e}`); }
    // Weighing a backup stats every recording, so it comes after the draw.
    try {
      const s = await invoke("backup_sizes");
      $("bkSizes").innerHTML = `The database is ${gb(s.db_bytes)} and the settings ${gb(s.config_bytes)}. `
        + `Kept audio: ${s.kept_files} recordings, ${gb(s.kept_bytes)}. Everything: ${s.all_files} recordings, ${gb(s.all_bytes)}.`;
    } catch (e) { $("bkSizes").textContent = ""; }
  }

  $("bkSave").onclick = async () => { if (await save()) uiToast("Backup settings saved."); };
  $("bkPass").onclick = passphrase;
  $("bkAddFolder").onclick = () => addDest("folder", null);
  $("bkAddS3").onclick = () => {
    const m = uiModal(`<div class="head"><span class="eyebrow">Where to</span></div><div class="body">
      <div class="list">${Object.entries(PRESETS).map(([k, p]) => `<div class="row"><span class="grow"><b>${esc(p.name)}</b>${p.help ? `<br><span class="small faint">${esc(p.help)}</span>` : ""}</span><button class="btn ghost sm" data-bkpreset="${k}">Use this</button></div>`).join("")}</div>
      <p class="help">All of these speak the same protocol, so anything S3-compatible works even if it is not listed.</p></div>`, { wide: true });
    m.querySelectorAll("[data-bkpreset]").forEach((x) => x.onclick = () => { m.close(); addDest("s3", x.dataset.bkpreset); });
  };

  if (typeof listen === "function") {
    listen("backup", (e) => {
      const p = e.payload || {};
      const row = document.querySelector(`[data-bkstate="${dests.findIndex((d) => d.id === p.dest)}"]`);
      const pct = p.total ? ` ${Math.round((p.done / p.total) * 100)}%` : "";
      if (row) row.innerHTML = p.phase === "failed" ? `<span class="bad">${esc(p.detail)}</span>` : `${esc(p.detail || p.phase)}${pct}`;
      if (p.phase === "done") { uiToast(`Backup finished: ${p.detail}`); load(); }
      if (p.phase === "failed") uiToast(`Backup failed: ${p.detail}`, "err");
    });
  }

  window.bkLoad = load;
  load();
})();
