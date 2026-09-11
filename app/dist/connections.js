/* ================= connections: the Telegram bot, named destinations ================= */
// Loaded after app.js and shares its helpers ($, invoke, esc, uiToast,
// uiConfirm). The Telegram defaults, destinations and discovered chats live
// in the alerts settings object app.js already holds — reached through
// alertsSettings() / alertsPersist() so there is only ever one copy to save.
(() => {
  if (typeof invoke !== "function") return;   // standalone preview: no backend
  const S = () => (typeof window.alertsSettings === "function" ? window.alertsSettings() : null);
  const V = () => (typeof window.alertsView === "function" ? window.alertsView() : null);
  const dests = () => (S() && S().destinations) || [];
  const chats = () => (S() && S().known_chats) || [];
  const join = (chat, topic) => { chat = String(chat || "").trim(); topic = String(topic || "").trim(); return topic ? `${chat}:${topic}` : chat; };
  const split = (t) => { const m = /^(.+):(\d+)$/.exec(String(t || "").trim()); return m ? [m[1], m[2]] : [String(t || "").trim(), ""]; };
  const targetOf = (d) => join(d.chat_id, d.topic_id);
  const defaultTarget = () => (S() ? join(S().telegram.chat_id, S().telegram.topic_id) : "");
  const chatOf = (id) => chats().find((c) => c.id === String(id));
  // "Test Team › Arrests" from what discovery knows, else the raw ids.
  function describe(t) {
    const [chat, topic] = split(t); const c = chatOf(chat);
    const base = c ? c.title || chat : chat;
    if (!topic) return base;
    const tp = c && c.topics.find((x) => String(x.id) === topic);
    return `${base} › ${tp && tp.name ? tp.name : "topic " + topic}`;
  }
  // The name a rule's target goes by: its destination's, else a description.
  function nameOf(t) {
    if (!t) return "";
    const d = dests().find((x) => targetOf(x) === t);
    return d ? d.name : describe(t);
  }
  window.destName = nameOf;
  let bot = null, checking = false;

  async function persist() {
    if (typeof window.alertsPersist !== "function") return false;
    const ok = await window.alertsPersist();
    render();
    return ok;
  }

  /* ---------- the bot ---------- */
  function renderBot() {
    const v = V(); const has = !!(v && v.has_token);
    $("tgToken").placeholder = has ? "saved on this Mac" : "123456:ABC-DEF…";
    let html = "", meta = "no token";
    if (bot && bot.ok) {
      meta = `@${bot.username}`;
      html = `<span class="badge clear">✓ connected</span> <b>@${esc(bot.username)}</b> <span class="faint">${esc(bot.name)}</span>` +
        `<div class="help">${bot.reads_all ? "Privacy mode is off: the bot sees every message in its groups." : "In groups the bot only hears commands and mentions — so send <span class=\"mono\">/start@" + esc(bot.username) + "</span> in each topic you want to find."}</div>`;
      $("cxStartCmd").textContent = `/start@${bot.username}`;
    } else if (bot && bot.error) {
      meta = "check failed";
      html = `<span class="badge enc">✗</span> ${esc(bot.error)}`;
    } else if (has) {
      meta = "token saved";
      html = checking ? '<span class="faint">checking…</span>' : '<span class="faint">Token saved — press Save &amp; check to confirm it works.</span>';
    }
    $("tgMeta2").textContent = meta;
    $("cxBot").innerHTML = html;
  }
  async function verify(quiet) {
    checking = true; renderBot();
    try { bot = await invoke("telegram_verify"); }
    catch (e) { bot = { ok: false, error: String(e) }; }
    checking = false; renderBot();
    if (!quiet && bot && !bot.ok && bot.error) uiToast(bot.error, "err");
    return bot && bot.ok;
  }
  $("tgSave").onclick = async () => {
    const tok = $("tgToken").value.trim();
    try { if (tok) { await invoke("telegram_save", { token: tok }); $("tgToken").value = ""; } }
    catch (e) { uiToast(`${e}`, "err"); return; }
    if (typeof window.alertsReload === "function") await window.alertsReload();
    if (await verify(false)) { uiToast(`Connected to @${bot.username}`); if (!chats().length) find(true); }
  };

  /* ---------- chats the bot has seen ---------- */
  async function find(quiet) {
    const b = $("cxFind"); b.disabled = true; b.textContent = "Looking…";
    try {
      const list = await invoke("telegram_discover");
      if (S()) S().known_chats = list || [];
      render();
      if (!quiet) uiToast(list && list.length ? `${list.length} chat${list.length === 1 ? "" : "s"} found` : "Nothing yet — message the bot first, then look again");
    } catch (e) { if (!quiet) uiToast(`${e}`, "err"); }
    finally { b.disabled = false; b.textContent = "Find chats"; }
  }
  $("cxFind").onclick = () => find(false);
  const kindLabel = { private: "person", group: "group", supergroup: "group", channel: "channel" };
  function renderChats() {
    const list = chats();
    $("cxChatsEmpty").style.display = list.length ? "none" : "";
    $("cxChatsMeta").textContent = list.length ? `${list.length}` : "";
    const added = (t) => dests().find((d) => targetOf(d) === t);
    const row = (t, label, sub) => {
      const d = added(t);
      return `<div class="row cxrow ${sub ? "sub" : ""}"><span class="grow">${sub ? "↳ " : ""}<b>${esc(label)}</b>${sub ? "" : ""} <span class="mono faint">${esc(t)}</span></span>${d ? `<span class="faint small">✓ “${esc(d.name)}”</span>` : `<button class="btn ghost sm" data-cxadd="${esc(t)}" data-cxname="${esc(label)}">Add</button>`}</div>`;
    };
    $("cxChats").innerHTML = list.map((c) => {
      const title = c.title || c.id;
      let h = `<div class="cxchat"><div class="row cxrow"><span class="grow"><b>${esc(title)}</b> <span class="badge">${esc(kindLabel[c.kind] || c.kind || "chat")}</span>${c.is_forum ? ' <span class="badge">topics</span>' : ""}${c.seen ? ` <span class="faint small">seen ${esc(new Date(c.seen * 1000).toLocaleString("en-US", { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" }))}</span>` : ""}</span>`;
      if (!c.is_forum) { const d = added(c.id); h += d ? `<span class="faint small">✓ “${esc(d.name)}”</span>` : `<button class="btn ghost sm" data-cxadd="${esc(c.id)}" data-cxname="${esc(title)}">Add</button>`; }
      h += `</div>`;
      if (c.is_forum) {
        h += row(c.id, `${title} › General`, true);
        for (const t of c.topics) h += row(`${c.id}:${t.id}`, `${title} › ${t.name || "topic " + t.id}`, true);
      }
      return h + `</div>`;
    }).join("");
    $("cxChats").querySelectorAll("[data-cxadd]").forEach((b) => b.onclick = () => addDest(b.dataset.cxadd, b.dataset.cxname.replace(/ › General$/, "")));
  }

  /* ---------- destinations ---------- */
  async function addDest(t, name) {
    const s = S(); if (!s) return;
    const [chat, topic] = split(t);
    if (!chat) { uiToast("Enter a chat id", "err"); return; }
    if (dests().some((d) => targetOf(d) === t)) { uiToast("That one is already a destination"); return; }
    s.destinations = [...dests(), { id: `d${Date.now()}`, name: (name || describe(t)).trim(), chat_id: chat, topic_id: topic }];
    // The first destination becomes the default when there is none yet.
    if (!defaultTarget()) { s.telegram.chat_id = chat; s.telegram.topic_id = topic; }
    if (await persist()) uiToast(`Added “${name || describe(t)}”`);
  }
  $("cxManAdd").onclick = () => {
    const chat = $("cxManChat").value.trim(), topic = $("cxManTopic").value.replace(/\D/g, "");
    if (!/^-?\d+$|^@\w+$/.test(chat)) { uiToast("A chat id is a number like 123456789 or -1001234567890 (or @channelname)", "err"); return; }
    addDest(join(chat, topic), $("cxManName").value.trim());
    $("cxManName").value = $("cxManChat").value = $("cxManTopic").value = "";
  };
  function renderDests() {
    const list = dests(), def = defaultTarget();
    $("cxDestsEmpty").style.display = list.length ? "none" : "";
    $("cxDestMeta").textContent = list.length ? `${list.length}` : "";
    $("cxDests").innerHTML = list.map((d) => `<div class="row cxdest" data-cxid="${esc(d.id)}"><span class="grow"><input class="cxname" data-cxrename="${esc(d.id)}" value="${esc(d.name)}" spellcheck="false">${targetOf(d) === def ? ' <span class="badge clear">default</span>' : ""}<br><small class="faint">${esc(describe(targetOf(d)))} · <span class="mono">${esc(targetOf(d))}</span></small></span><button class="btn ghost sm" data-cxtest="${esc(d.id)}">Test</button><button class="btn ghost sm" data-cxdel="${esc(d.id)}" title="Remove">✕</button></div>`).join("");
    $("cxDests").querySelectorAll("[data-cxrename]").forEach((i) => i.onchange = async () => { const d = dests().find((x) => x.id === i.dataset.cxrename); if (!d) return; d.name = i.value.trim() || d.name; await persist(); });
    $("cxDests").querySelectorAll("[data-cxtest]").forEach((b) => b.onclick = async () => { const d = dests().find((x) => x.id === b.dataset.cxtest); if (!d) return; b.disabled = true; try { uiToast(await invoke("telegram_test_destination", { destination: d })); } catch (e) { uiToast(`${e}`, "err"); } finally { b.disabled = false; } });
    $("cxDests").querySelectorAll("[data-cxdel]").forEach((b) => b.onclick = async () => {
      const d = dests().find((x) => x.id === b.dataset.cxdel); if (!d) return;
      if (!(await uiConfirm(`Remove “${d.name}”? Tripwires that send there keep sending there; they just show the chat id instead of this name.`, "Remove"))) return;
      S().destinations = dests().filter((x) => x.id !== d.id); await persist();
    });
    // Default + status messages: every destination, plus whatever is set now.
    const opts = (cur, blank) => {
      const known = list.some((d) => targetOf(d) === cur);
      return (blank ? `<option value="">${esc(blank)}</option>` : "") + list.map((d) => `<option value="${esc(targetOf(d))}" ${targetOf(d) === cur ? "selected" : ""}>${esc(d.name)}</option>`).join("") + (cur && !known ? `<option value="${esc(cur)}" selected>${esc(describe(cur))} (unnamed)</option>` : "");
    };
    $("cxDefault").innerHTML = opts(def, list.length ? "" : "— add a destination first —");
    if (!def && list.length) $("cxDefault").insertAdjacentHTML("afterbegin", '<option value="" selected>— none —</option>');
    const s = S();
    if (s) { $("tgAnnounce").checked = !!s.telegram.announce; $("tgAnnounceDest").innerHTML = opts(s.telegram.announce_chat || "", `the default${def ? " (" + nameOf(def) + ")" : ""}`); }
  }
  $("cxDefault").onchange = async () => { const s = S(); if (!s) return; const [c, t] = split($("cxDefault").value); s.telegram.chat_id = c; s.telegram.topic_id = t; await persist(); };
  $("tgAnnounce").onchange = async () => { const s = S(); if (!s) return; s.telegram.announce = $("tgAnnounce").checked; await persist(); };
  $("tgAnnounceDest").onchange = async () => { const s = S(); if (!s) return; s.telegram.announce_chat = $("tgAnnounceDest").value; await persist(); };

  // Chats your tripwires already send to that have no name yet — one click
  // to name them, and those tripwires then send by that name.
  async function renderFound() {
    const s = S(); if (!s) { $("cxFound").innerHTML = ""; return; }
    const used = new Map(); // target → rule names
    const note = (t, who) => { t = String(t || "").trim(); if (!t) return; if (!used.has(t)) used.set(t, []); used.get(t).push(who); };
    note(defaultTarget(), "default");
    try { const v = await invoke("tripwires_get"); for (const t of (v && v.tripwires) || []) if (t.send.dest === "custom") note(t.send.chat, t.name); } catch (_) {}
    const loose = [...used].filter(([t]) => !dests().some((d) => targetOf(d) === t));
    $("cxFound").innerHTML = loose.length ? `<div class="cxfoundh">Already used by your tripwires — give them a name</div>` + loose.map(([t, who]) => { const d = describe(t); return `<div class="row cxfound"><span class="grow"><b>${esc(d === t ? "chat " + t : d)}</b>${d === t ? "" : ` <span class="mono faint">${esc(t)}</span>`}<br><small class="faint">used by ${esc(who.map((w) => (w === "default" ? "the default" : w)).join(", "))}</small></span><button class="btn ghost sm" data-cxname-found="${esc(t)}">Name it</button></div>`; }).join("") : "";
    $("cxFound").querySelectorAll("[data-cxname-found]").forEach((b) => b.onclick = () => {
      const t = b.dataset.cxnameFound;
      const m = uiModal(`<div class="eyebrow">Name this destination</div><p class="help mono">${esc(t)}</p><label class="field"><span class="lab">Name</span><input data-n type="text" value="${esc(describe(t))}" spellcheck="false"></label><div class="xport" style="justify-content:flex-end;margin:12px 0 0"><button class="btn ghost" data-no>Cancel</button><button class="btn primary" data-yes>Save</button></div>`);
      const inp = m.querySelector("[data-n]"); inp.focus(); inp.select();
      m.querySelector("[data-no]").onclick = m.close;
      m.querySelector("[data-yes]").onclick = () => { m.close(); addDest(t, inp.value.trim()); };
      inp.onkeydown = (e) => { if (e.key === "Enter") m.querySelector("[data-yes]").onclick(); };
    });
  }

  function render() {
    if (!$("set-connections")) return;
    renderBot(); renderChats(); renderDests();
    if (settingsPageVisible("connections")) renderFound();
  }
  window.connectionsRender = render;
  window.connectionsOnShow = async () => {
    if (!S() && typeof window.alertsReload === "function") await window.alertsReload();
    render(); renderFound();
    if (!bot && V() && V().has_token) verify(true);
  };

})();
